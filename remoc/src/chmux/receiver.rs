use bytes::{Buf, Bytes, BytesMut};
use futures::{
    ready,
    stream::Stream,
    task::{Context, Poll},
};
use std::{collections::VecDeque, error::Error, fmt, mem, pin::Pin};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::ReusableBoxFuture;

use super::{
    AnyStorage, ForwardError, PortAllocator, Request, Sender,
    client::PreConnectState,
    credit::{PortCreditReturner, UsedGlobalCredit, UsedPortCredit},
    forward,
    mux::PortEvt,
    port_allocator::SidePort,
};
use wokio::runtime;

/// An error returned when a data message cannot be received.
///
/// Most applications encounter this type wrapped by
/// [`rch::base::RecvError`](crate::rch::base::RecvError).
///
/// A pre-connected port is provisionally opened so that data can arrive while
/// the remote listener's acceptance is still pending. The listener can still
/// reject the connection.
#[derive(Debug, Clone)]
pub enum RecvError {
    /// The underlying multiplexer terminated before the message was received.
    ChMux,
    /// The message exceeds the configured data-size limit.
    ExceedsMaxDataSize(usize),
    /// The message carries more ports than the configured limit permits.
    ExceedsMaxPortCount(usize),
    /// The remote listener rejected this port's pre-connection.
    Rejected {
        /// Whether the rejection occurred because the remote endpoint had no free ports.
        no_ports: bool,
    },
}

#[cfg(feature = "serde")]
crate::versioned::compact::impl_enum! {
    RecvError,
    variants {
        ChMux => "_0",
        ExceedsMaxDataSize(max_size: usize) => "_1",
        ExceedsMaxPortCount(max_count: usize) => "_2",
        Rejected { no_ports: bool } => "_3",
    }
}

impl RecvError {
    /// Returns true, if error is due to multiplexer being terminated.
    pub fn is_terminated(&self) -> bool {
        matches!(self, Self::ChMux)
    }

    /// Returns whether the remote endpoint rejected the channel or the connection failed.
    pub fn is_disconnected(&self) -> bool {
        matches!(self, Self::ChMux | Self::Rejected { .. })
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
            Self::Rejected { .. } => write!(f, "pre-connected port was rejected"),
        }
    }
}

impl Error for RecvError {}

impl From<RecvError> for std::io::Error {
    fn from(err: RecvError) -> Self {
        use std::io::ErrorKind;
        match err {
            RecvError::ChMux => Self::new(ErrorKind::ConnectionReset, err),
            RecvError::ExceedsMaxDataSize(_) => Self::new(ErrorKind::InvalidData, err),
            RecvError::ExceedsMaxPortCount(_) => Self::new(ErrorKind::InvalidData, err),
            RecvError::Rejected { .. } => Self::new(ErrorKind::ConnectionRefused, err),
        }
    }
}

/// An error returned while receiving from a multiplexed channel.
#[derive(Debug, Clone)]
pub enum RecvAnyError {
    /// The underlying multiplexer terminated.
    ChMux,
    /// The remote listener rejected this port's pre-connection.
    Rejected {
        /// Whether the rejection occurred because the remote endpoint had no free ports.
        no_ports: bool,
    },
}

#[cfg(feature = "serde")]
crate::versioned::compact::impl_enum! {
    RecvAnyError,
    variants {
        ChMux => "_0",
        Rejected { no_ports: bool } => "_1",
    }
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
            Self::Rejected { .. } => write!(f, "pre-connected port was rejected"),
        }
    }
}

impl Error for RecvAnyError {}

/// An error returned while receiving the remaining chunks of a message.
#[derive(Debug, Clone)]
pub enum RecvChunkError {
    /// The underlying multiplexer terminated before the message completed.
    ChMux,
    /// The remote endpoint cancelled this message before it completed.
    Cancelled,
}

#[cfg(feature = "serde")]
crate::versioned::compact::impl_enum! {
    RecvChunkError,
    variants {
        ChMux => "_0",
        Cancelled => "_1",
    }
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
    pre_connected_rx: Option<watch::Receiver<PreConnectState>>,
    handle: runtime::Handle,
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
    pub(super) fn new(
        local_port: SidePort, remote_port: SidePort, max_data_size: usize, max_port_count: usize,
        tx: mpsc::Sender<PortEvt>, high_priority_tx: mpsc::Sender<PortEvt>,
        rx: mpsc::UnboundedReceiver<PortReceiveMsg>, channel_credits: PortCreditReturner,
        port_allocator: PortAllocator, storage: AnyStorage,
        pre_connected_rx: Option<watch::Receiver<PreConnectState>>,
    ) -> Self {
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
            pre_connected_rx,
            handle: runtime::Handle::current(),
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
    ///
    /// Receiving a larger group of port requests fails with
    /// [`RecvError::ExceedsMaxPortCount`].
    pub fn set_max_ports(&mut self, max_ports: usize) {
        self.max_ports = max_ports;
    }

    /// Returns whether the remote endpoint may use global credits for sending on this channel.
    ///
    /// By default this is enabled.
    pub fn are_global_credits_allowed(&self) -> bool {
        self.channel_credits.are_global_credits_allowed()
    }

    /// Sets whether the remote endpoint may use global credits for sending on this channel.
    ///
    /// While disabled, the remote endpoint may only have as much data in-flight as the
    /// [port receive buffer](super::Cfg::port_receive_buffer) allows. This limits the
    /// achievable bandwidth of the channel, but keeps it from occupying buffer space
    /// shared with all other channels.
    ///
    /// Global credits are only used when both endpoints allow it; the sending side
    /// decides using
    /// [Sender::set_global_credits_allowed](super::Sender::set_global_credits_allowed),
    /// which it can disable when it knows that back pressure is to be expected.
    ///
    /// By default this is enabled.
    pub fn set_global_credits_allowed(&mut self, allowed: bool) {
        self.channel_credits.set_global_credits_allowed(allowed, self.remote_port, &self.high_priority_tx);
    }

    /// Waits that a pre-connected port is fully connected.
    ///
    /// The pre-connect state is kept afterwards, because a port that was
    /// [tentatively accepted](super::Request::accept_tentatively) can still be rejected
    /// after it has been connected.
    async fn wait_pre_connect_done(&mut self) -> Result<(), RecvError> {
        if let Some(pre_connected_rx) = &mut self.pre_connected_rx {
            let Ok(res) = pre_connected_rx.wait_for(|res| res.is_decided()).await else {
                return Err(RecvError::ChMux);
            };
            match &*res {
                PreConnectState::PreConnected => unreachable!(),
                PreConnectState::Accepted => (),
                PreConnectState::Rejected { no_ports } => {
                    return Err(RecvError::Rejected { no_ports: *no_ports });
                }
            }
        }

        Ok(())
    }

    /// The error to report when a tentatively accepted port has been rejected after
    /// it had been connected.
    ///
    /// The remote endpoint sends the rejection before it closes the port, thus it has
    /// always been processed once the closure of the port is observed.
    fn pre_connect_rejected(&self) -> Option<RecvError> {
        match &self.pre_connected_rx {
            Some(pre_connected_rx) => match &*pre_connected_rx.borrow() {
                PreConnectState::Rejected { no_ports } => Some(RecvError::Rejected { no_ports: *no_ports }),
                PreConnectState::PreConnected | PreConnectState::Accepted => None,
            },
            None => None,
        }
    }

    /// Receives data over the channel.
    ///
    /// Waits for data to become available.
    /// Received port open requests are silently rejected.
    /// The size of the received data is limited by [max_data_size](Self::max_data_size).
    pub async fn recv(&mut self) -> Result<Option<DataBuf>, RecvError> {
        self.wait_pre_connect_done().await?;

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
        self.wait_pre_connect_done().await.map_err(|err| match err {
            RecvError::ChMux => RecvChunkError::ChMux,
            _ => RecvChunkError::Cancelled,
        })?;

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
                        } else if self.pre_connect_rejected().is_some() {
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
    ///
    /// Returns binary data, a marker for chunked data, or port-opening requests;
    /// see [`Received`]. [`None`] indicates that the remote sender finished the
    /// channel and no further messages will arrive.
    pub async fn recv_any(&mut self) -> Result<Option<Received>, RecvError> {
        self.wait_pre_connect_done().await?;

        if self.finished {
            return Ok(None);
        }

        // Chunk if chunked reception is in progress.
        if let Receiving::Chunks { chunks, completed } = &self.receiving {
            if !chunks.is_empty() || !*completed {
                return Ok(Some(Received::Chunks));
            }
            self.receiving = Receiving::Nothing;
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
                    match self.pre_connect_rejected() {
                        Some(err) => return Err(err),
                        None => return Ok(None),
                    }
                }

                None => return Err(RecvError::ChMux),
            }
        }
    }

    /// Closes the sender at the remote endpoint, preventing it from sending new data.
    ///
    /// Messages sent before the close is processed can still be received.
    /// Calling this method again has no additional effect.
    pub async fn close(&mut self) {
        if !self.closed {
            let _ = self.tx.send(PortEvt::ReceiverClosed { local_port: self.local_port }).await;
            self.closed = true;
        }
    }

    /// Converts this receiver into a stream of binary messages.
    ///
    /// Port-opening requests are rejected by the stream, as they are by
    /// [`recv`](Self::recv).
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
        let tx = self.tx.clone();
        let local_port = self.local_port;
        self.handle.spawn(async move {
            let _ = tx.send(PortEvt::ReceiverDropped { local_port }).await;
        });
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
