use bytes::{Buf, Bytes, BytesMut};
use futures::{
    ready,
    stream::Stream,
    task::{Context, Poll},
};
use std::{collections::VecDeque, error::Error, fmt, mem, pin::Pin};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::ReusableBoxFuture;

use super::{
    AnyStorage, ForwardError, PortAllocator, Request, Sender,
    credit::{PortCreditReturner, UsedGlobalCredit, UsedPortCredit},
    forward,
    mux::PortEvt,
    port_allocator::SidePort,
};
use crate::exec;

/// An error occurred during receiving a data message.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum RecvError {
    /// Multiplexer terminated.
    ChMux,
    /// Data exceeds maximum size.
    ExceedsMaxDataSize(usize),
    /// Received ports exceed maximum count.
    ExceedsMaxPortCount(usize),
}

impl RecvError {
    /// Returns true, if error is due to multiplexer being terminated.
    pub fn is_terminated(&self) -> bool {
        matches!(self, Self::ChMux)
    }

    /// Returns whether the error is final, i.e. no further receive operation can succeed.
    pub fn is_final(&self) -> bool {
        self.is_terminated()
    }
}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::ChMux => write!(f, "multiplexer terminated"),
            Self::ExceedsMaxDataSize(max_size) => {
                write!(f, "data exceeds maximum allowed size of {max_size} bytes")
            }
            Self::ExceedsMaxPortCount(max_count) => {
                write!(f, "port message exceeds maximum allowed count of {max_count} ports")
            }
        }
    }
}

impl Error for RecvError {}

impl From<RecvError> for std::io::Error {
    fn from(err: RecvError) -> Self {
        use std::io::ErrorKind;
        match err {
            RecvError::ChMux => Self::new(ErrorKind::ConnectionReset, err.to_string()),
            RecvError::ExceedsMaxDataSize(_) => Self::new(ErrorKind::InvalidData, err.to_string()),
            RecvError::ExceedsMaxPortCount(_) => Self::new(ErrorKind::InvalidData, err.to_string()),
        }
    }
}

/// An error occurred during receiving a message.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum RecvAnyError {
    /// Multiplexer terminated.
    ChMux,
}

impl RecvAnyError {
    /// Returns true, if error is due to multiplexer being terminated.
    pub fn is_terminated(&self) -> bool {
        matches!(self, Self::ChMux)
    }
}

impl fmt::Display for RecvAnyError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::ChMux => write!(f, "multiplexer terminated"),
        }
    }
}

impl Error for RecvAnyError {}

/// An error occurred during receiving chunks of a message.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum RecvChunkError {
    /// Multiplexer terminated.
    ChMux,
    /// Remote endpoint cancelled transmission.
    Cancelled,
}

impl RecvChunkError {
    /// Returns true, if error is due to multiplexer being terminated.
    pub fn is_terminated(&self) -> bool {
        matches!(self, Self::ChMux)
    }
}

impl fmt::Display for RecvChunkError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::ChMux => write!(f, "multiplexer terminated"),
            Self::Cancelled => write!(f, "transmission cancelled"),
        }
    }
}

/// Container for received data.
pub(crate) struct ReceivedData {
    /// Received data.
    pub buf: Bytes,
    /// First chunk of data.
    pub first: bool,
    /// Last chunk of data.
    pub last: bool,
    /// Port flow-control credit.
    pub port_credit: UsedPortCredit,
    /// Global flow-control credit.
    #[expect(unused)]
    pub global_credit: UsedGlobalCredit,
}

/// Container for received port open requests.
pub(crate) struct ReceivedPortRequests {
    /// Port open requests.
    pub requests: Vec<Request>,
    /// First chunk of ports.
    pub first: bool,
    /// Last chunk of ports.
    pub last: bool,
    /// Flow-control credit.
    pub credit: UsedPortCredit,
}

/// Port receive message.
pub(crate) enum PortReceiveMsg {
    /// Data has been received.
    Data(ReceivedData),
    /// Ports have been received.
    PortRequests(ReceivedPortRequests),
    /// Request to report when messages have been processed up to this point.
    RequestReceivedReport,
    /// Sender has closed its end.
    Finished,
}

/// A buffer containing received data.
#[derive(Clone)]
pub struct DataBuf {
    bufs: VecDeque<Bytes>,
    remaining: usize,
}

impl fmt::Debug for DataBuf {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("DataBuf").field("remaining", &self.remaining).finish()
    }
}

impl DataBuf {
    fn new() -> Self {
        Self { bufs: VecDeque::new(), remaining: 0 }
    }

    fn try_push(&mut self, buf: Bytes, max_size: usize) -> Result<(), Bytes> {
        match self.remaining.checked_add(buf.len()) {
            Some(new_size) if new_size <= max_size => {
                self.bufs.push_back(buf);
                self.remaining = new_size;
                Ok(())
            }
            _ => Err(buf),
        }
    }
}

impl Default for DataBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl Buf for DataBuf {
    fn remaining(&self) -> usize {
        self.remaining
    }

    fn chunk(&self) -> &[u8] {
        match self.bufs.front() {
            Some(buf) => buf.chunk(),
            None => &[],
        }
    }

    fn advance(&mut self, mut cnt: usize) {
        while cnt > 0 {
            match self.bufs.front_mut() {
                Some(buf) if buf.len() > cnt => {
                    self.remaining -= cnt;
                    buf.advance(cnt);
                    cnt = 0;
                }
                Some(buf) => {
                    self.remaining -= buf.len();
                    cnt -= buf.len();
                    self.bufs.pop_front();
                }
                None => {
                    panic!("cannot advance beyond end of data");
                }
            }
        }
    }
}

impl From<DataBuf> for BytesMut {
    fn from(mut data: DataBuf) -> Self {
        let mut continuous = BytesMut::with_capacity(data.remaining);
        while let Some(buf) = data.bufs.pop_front() {
            continuous.extend_from_slice(&buf);
        }
        continuous
    }
}

impl From<DataBuf> for Bytes {
    fn from(mut data: DataBuf) -> Self {
        if data.bufs.len() == 1 { data.bufs.pop_front().unwrap() } else { BytesMut::from(data).into() }
    }
}

impl From<DataBuf> for Vec<u8> {
    fn from(mut data: DataBuf) -> Self {
        let mut continuous = Vec::with_capacity(data.remaining);
        while let Some(buf) = data.bufs.pop_front() {
            continuous.extend_from_slice(&buf);
        }
        continuous
    }
}

impl From<Bytes> for DataBuf {
    fn from(data: Bytes) -> Self {
        let remaining = data.len();
        let mut bufs = VecDeque::new();
        bufs.push_back(data);
        Self { bufs, remaining }
    }
}

/// Received data or port requests.
#[derive(Debug)]
pub enum Received {
    /// Binary data.
    Data(DataBuf),
    /// Data was received that exceeds the receive buffer size.
    ///
    /// Use [Receiver::recv_chunk] to stream the data in chunks.
    Chunks,
    /// Port open requests.
    Requests(Vec<Request>),
}

#[derive(Default)]
enum Receiving {
    #[default]
    Nothing,
    Data(DataBuf),
    Chunks {
        chunks: VecDeque<Bytes>,
        completed: bool,
    },
    Requests(Vec<Request>),
}

/// Receives byte data over a channel.
pub struct Receiver {
    local_port: SidePort,
    remote_port: SidePort,
    max_data_size: usize,
    max_ports: usize,
    tx: mpsc::Sender<PortEvt>,
    high_priority_tx: mpsc::Sender<PortEvt>,
    rx: mpsc::UnboundedReceiver<PortReceiveMsg>,
    receiving: Receiving,
    channel_credits: PortCreditReturner,
    closed: bool,
    finished: bool,
    port_allocator: PortAllocator,
    storage: AnyStorage,
    _drop_tx: oneshot::Sender<()>,
}

impl fmt::Debug for Receiver {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Receiver")
            .field("local_port", &self.local_port)
            .field("remote_port", &self.remote_port)
            .field("max_data_size", &self.max_data_size)
            .field("max_ports", &self.max_ports)
            .field("closed", &self.closed)
            .field("finished", &self.finished)
            .finish()
    }
}

impl Receiver {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        local_port: SidePort, remote_port: SidePort, max_data_size: usize, max_port_count: usize,
        tx: mpsc::Sender<PortEvt>, high_priority_tx: mpsc::Sender<PortEvt>,
        rx: mpsc::UnboundedReceiver<PortReceiveMsg>, channel_credits: PortCreditReturner,
        port_allocator: PortAllocator, storage: AnyStorage,
    ) -> Self {
        let (_drop_tx, drop_rx) = oneshot::channel();
        let tx_drop = tx.clone();
        exec::spawn(async move {
            let _ = drop_rx.await;
            let _ = tx_drop.send(PortEvt::ReceiverDropped { local_port }).await;
        });

        Self {
            local_port,
            remote_port,
            max_data_size,
            max_ports: max_port_count,
            tx,
            high_priority_tx,
            rx,
            receiving: Receiving::Nothing,
            channel_credits,
            closed: false,
            finished: false,
            port_allocator,
            storage,
            _drop_tx,
        }
    }

    /// The local port number.
    pub fn local_port(&self) -> SidePort {
        self.local_port
    }

    /// The remote port number.
    pub fn remote_port(&self) -> SidePort {
        self.remote_port
    }

    /// Maximum data size in bytes to receive per message.
    ///
    /// The default value is specified by [Cfg::max_data_size](super::Cfg::max_data_size).
    ///
    /// [recv_chunk](Self::recv_chunk) is not affected by this limit.
    pub fn max_data_size(&self) -> usize {
        self.max_data_size
    }

    /// Set maximum data size in bytes to receive per message.
    ///
    /// [recv_chunk](Self::recv_chunk) is not affected by this limit.
    pub fn set_max_data_size(&mut self, max_data_size: usize) {
        self.max_data_size = max_data_size;
    }

    /// Maximum port count per message.
    ///
    /// The default value is specified by [Cfg::max_received_ports](super::Cfg::max_received_ports).
    pub fn max_ports(&self) -> usize {
        self.max_ports
    }

    /// Set maximum port count per message.
    pub fn set_max_ports(&mut self, max_ports: usize) {
        self.max_ports = max_ports;
    }

    /// Receives data over the channel.
    ///
    /// Waits for data to become available.
    /// Received port open requests are silently rejected.
    /// The size of the received data is limited by [max_data_size](Self::max_data_size).
    pub async fn recv(&mut self) -> Result<Option<DataBuf>, RecvError> {
        loop {
            match self.recv_any().await? {
                Some(Received::Data(data)) => break Ok(Some(data)),
                Some(Received::Chunks) => break Err(RecvError::ExceedsMaxDataSize(self.max_data_size)),
                Some(Received::Requests(_)) => (),
                None => break Ok(None),
            }
        }
    }

    /// Receives chunks of data over the channel.
    ///
    /// This should be called when [recv_any](Self::recv_any) returns [Received::Chunks]
    /// to obtain the received data chunk by chunk.
    /// [None] is returned after the last chunk of a message.
    ///
    /// This is unlimited in size.
    pub async fn recv_chunk(&mut self) -> Result<Option<Bytes>, RecvChunkError> {
        if self.finished {
            return Ok(None);
        }

        loop {
            self.channel_credits.ready().await;

            match &mut self.receiving {
                // Chunks from receive operation started by recv_any available.
                Receiving::Chunks { chunks, .. } if !chunks.is_empty() => {
                    return Ok(Some(chunks.pop_front().unwrap()));
                }

                // Previous received chunk was last of message.
                Receiving::Chunks { completed: true, .. } => {
                    self.receiving = Receiving::Nothing;
                    return Ok(None);
                }

                // Try to receive next chunk.
                _ => match self.rx.recv().await {
                    Some(PortReceiveMsg::Data(data)) => {
                        self.channel_credits.start_return(
                            data.port_credit,
                            self.remote_port,
                            &self.high_priority_tx,
                        );

                        match (&self.receiving, data.first) {
                            // First segment without last segment indicates that last transmission
                            // was cancelled.
                            (Receiving::Chunks { .. }, true) => {
                                self.receiving =
                                    Receiving::Chunks { chunks: vec![data.buf].into(), completed: data.last };
                                return Err(RecvChunkError::Cancelled);
                            }
                            // Either continuation or start of transmission.
                            (Receiving::Chunks { .. }, false) | (_, true) => {
                                self.receiving =
                                    Receiving::Chunks { chunks: VecDeque::new(), completed: data.last };
                                return Ok(Some(data.buf));
                            }
                            // Ignore transmission without start.
                            (_, false) => (),
                        }
                    }

                    // Either aborted transmission or port data to ignore.
                    Some(PortReceiveMsg::PortRequests(req)) => {
                        self.channel_credits.start_return(req.credit, self.remote_port, &self.high_priority_tx);
                        if let Receiving::Chunks { .. } = &self.receiving {
                            self.receiving = Receiving::Nothing;
                            return Err(RecvChunkError::Cancelled);
                        }
                    }

                    // Report that message have been processed up to now.
                    Some(PortReceiveMsg::RequestReceivedReport) => {
                        self.channel_credits.start_report_processed(self.remote_port, &self.high_priority_tx);
                    }

                    // Port closure.
                    Some(PortReceiveMsg::Finished) => {
                        self.finished = true;
                        if let Receiving::Chunks { .. } = &self.receiving {
                            self.receiving = Receiving::Nothing;
                            return Err(RecvChunkError::Cancelled);
                        } else {
                            return Ok(None);
                        }
                    }

                    None => return Err(RecvChunkError::ChMux),
                },
            }
        }
    }

    /// Receives data or ports over the channel.
    pub async fn recv_any(&mut self) -> Result<Option<Received>, RecvError> {
        if self.finished {
            return Ok(None);
        }

        loop {
            self.channel_credits.ready().await;

            match self.rx.recv().await {
                // Data message.
                Some(PortReceiveMsg::Data(data)) => {
                    self.channel_credits.start_return(data.port_credit, self.remote_port, &self.high_priority_tx);

                    if data.first {
                        self.receiving = Receiving::Data(DataBuf::new());
                    }

                    if let Receiving::Data(mut data_buf) = mem::take(&mut self.receiving) {
                        // Try to add data to buffer.
                        match data_buf.try_push(data.buf, self.max_data_size) {
                            // Data fits into buffer.
                            Ok(()) => {
                                if data.last {
                                    return Ok(Some(Received::Data(data_buf)));
                                } else {
                                    self.receiving = Receiving::Data(data_buf);
                                }
                            }

                            // Maximum message size has been reached.
                            Err(buf) => {
                                data_buf.bufs.push_back(buf);
                                self.receiving =
                                    Receiving::Chunks { chunks: data_buf.bufs, completed: data.last };
                                return Ok(Some(Received::Chunks));
                            }
                        }
                    }
                }

                // Port connection requests.
                Some(PortReceiveMsg::PortRequests(req)) => {
                    self.channel_credits.start_return(req.credit, self.remote_port, &self.high_priority_tx);

                    if req.first {
                        self.receiving = Receiving::Requests(Vec::new());
                    }

                    if let Receiving::Requests(mut requests) = mem::take(&mut self.receiving) {
                        requests.extend(req.requests);

                        if requests.len() > self.max_ports {
                            self.receiving = Receiving::Nothing;
                            return Err(RecvError::ExceedsMaxPortCount(self.max_ports));
                        }

                        if req.last {
                            return Ok(Some(Received::Requests(requests)));
                        } else {
                            self.receiving = Receiving::Requests(requests);
                        }
                    }
                }

                // Report that message have been processed up to now.
                Some(PortReceiveMsg::RequestReceivedReport) => {
                    self.channel_credits.start_report_processed(self.remote_port, &self.high_priority_tx);
                }

                // Port closure.
                Some(PortReceiveMsg::Finished) => {
                    self.finished = true;
                    return Ok(None);
                }

                None => return Err(RecvError::ChMux),
            }
        }
    }

    /// Closes the sender at the remote endpoint, preventing it from sending new data.
    /// Already sent message will still be received.
    pub async fn close(&mut self) {
        if !self.closed {
            let _ = self.tx.send(PortEvt::ReceiverClosed { local_port: self.local_port }).await;
            self.closed = true;
        }
    }

    /// Convert this into a stream.
    pub fn into_stream(self) -> ReceiverStream {
        ReceiverStream::new(self)
    }

    /// Returns the port allocator of the channel multiplexer.
    pub fn port_allocator(&self) -> PortAllocator {
        self.port_allocator.clone()
    }

    /// Returns the arbitrary data storage of the channel multiplexer.
    pub fn storage(&self) -> AnyStorage {
        self.storage.clone()
    }

    /// Forwards all data received to the specified sender.
    ///
    /// This also recursively spawns background tasks for forwarding data on received ports.
    ///
    /// Returns when the channel is closed, but spawned tasks will continue forwarding until
    /// their channels are closed.
    ///
    /// Returns the total number of bytes forwarded on this channel,
    /// i.e. not counting forwarded bytes on recursively forwarded channel.
    pub async fn forward(&mut self, tx: &mut Sender) -> Result<usize, ForwardError> {
        forward::forward(self, tx).await
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        // required for correct drop order
    }
}

/// A stream receiving byte data over a channel.
///
/// No ports or data exceeding the maximum buffer size can be received.
///
/// This can be used together with [tokio_util::io::StreamReader] to obtain an
/// [AsyncRead](tokio::io::AsyncRead).
///
/// [tokio_util::io::StreamReader]: https://docs.rs/tokio-util/latest/tokio_util/io/struct.StreamReader.html
pub struct ReceiverStream {
    inner: ReusableBoxFuture<'static, (Result<Option<DataBuf>, RecvError>, Receiver)>,
    close: bool,
}

impl fmt::Debug for ReceiverStream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ReceiverStream").finish()
    }
}

impl ReceiverStream {
    /// Creates a new `ReceiverStream`.
    pub fn new(rx: Receiver) -> Self {
        Self { inner: ReusableBoxFuture::new(Self::make_future(rx, false)), close: false }
    }

    /// Closes the sender at the remote endpoint after the next value is received,
    /// preventing it from sending new data.
    ///
    /// Already sent data will still be received.
    pub fn close(&mut self) {
        self.close = true;
    }

    async fn make_future(mut rx: Receiver, close: bool) -> (Result<Option<DataBuf>, RecvError>, Receiver) {
        if close {
            // Subsequent closes after the first are ignored.
            rx.close().await;
        }

        let result = rx.recv().await;
        (result, rx)
    }
}

impl Stream for ReceiverStream {
    type Item = Result<DataBuf, RecvError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let (result, rx) = ready!(self.inner.poll(cx));

        let close = self.close;
        self.inner.set(Self::make_future(rx, close));

        Poll::Ready(result.transpose())
    }
}

impl Unpin for ReceiverStream {}

impl From<Receiver> for ReceiverStream {
    fn from(recv: Receiver) -> Self {
        Self::new(recv)
    }
}
