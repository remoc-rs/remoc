use bytes::Bytes;
use futures::{
    Future, FutureExt, future, ready,
    sink::Sink,
    task::{Context, Poll},
};
use std::{
    error::Error,
    fmt,
    mem::size_of,
    pin::Pin,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::ReusableBoxFuture;

use super::{
    AnyStorage, Connect, ConnectError, PortAllocator, PortReq,
    client::ConnectResponse,
    credit::{AssignedCredits, CreditPool, MixedAssignedCredits, MixedCreditUser},
    mux::PortEvt,
};
use crate::exec;

/// An error occurred during sending of a message.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SendError {
    /// The multiplexer terminated.
    ChMux,
    /// Other side closed receiving end of channel.
    Closed {
        /// True, if remote endpoint still processes messages that were already sent.
        gracefully: bool,
    },
}

impl SendError {
    /// Returns true, if error it due to channel being closed.
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Closed { gracefully: true })
    }

    /// True, if the remote endpoint closed the channel, was dropped or the connection failed.
    #[deprecated = "a chmux::SendError is always due to disconnection"]
    pub fn is_disconnected(&self) -> bool {
        true
    }

    /// Returns whether the error is final, i.e. no further send operation can succeed.
    #[deprecated = "a remoc::chmux::SendError is always final"]
    pub fn is_final(&self) -> bool {
        true
    }
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::ChMux => write!(f, "multiplexer terminated"),
            Self::Closed { gracefully } => write!(
                f,
                "remote endpoint closed channel{}",
                if *gracefully { " but still processes sent messages" } else { "" }
            ),
        }
    }
}

impl Error for SendError {}

impl<T> From<mpsc::error::SendError<T>> for SendError {
    fn from(_err: mpsc::error::SendError<T>) -> Self {
        Self::ChMux
    }
}

impl From<SendError> for std::io::Error {
    fn from(err: SendError) -> Self {
        use std::io::ErrorKind;
        match err {
            SendError::ChMux => Self::new(ErrorKind::ConnectionReset, err.to_string()),
            SendError::Closed { gracefully: false } => Self::new(ErrorKind::ConnectionReset, err.to_string()),
            SendError::Closed { gracefully: true } => Self::new(ErrorKind::ConnectionAborted, err.to_string()),
        }
    }
}

/// An error occurred during sending of a message.
#[derive(Debug)]
pub enum TrySendError {
    /// Channel queue is full.
    ///
    /// Sending should be retried.
    Full,
    /// Send error.
    Send(SendError),
}

impl TrySendError {
    /// True, if the remote endpoint closed the channel.
    pub fn is_closed(&self) -> bool {
        match self {
            Self::Full => false,
            Self::Send(err) => err.is_closed(),
        }
    }

    /// Returns whether the error is final, i.e. no further send operation can succeed.
    pub fn is_final(&self) -> bool {
        match self {
            Self::Full => false,
            Self::Send(_) => true,
        }
    }
}

impl fmt::Display for TrySendError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Full => write!(f, "channel queue is full"),
            Self::Send(err) => write!(f, "{err}"),
        }
    }
}

impl From<SendError> for TrySendError {
    fn from(err: SendError) -> Self {
        Self::Send(err)
    }
}

impl From<mpsc::error::TrySendError<PortEvt>> for TrySendError {
    fn from(err: mpsc::error::TrySendError<PortEvt>) -> Self {
        match err {
            mpsc::error::TrySendError::Full(_) => Self::Full,
            mpsc::error::TrySendError::Closed(_) => Self::Send(SendError::ChMux),
        }
    }
}

impl Error for TrySendError {}

/// This future resolves when the remote endpoint has closed its receiver.
///
/// It will also resolve when the channel is closed or the channel multiplexer
/// is shutdown.
pub struct Closed {
    fut: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl fmt::Debug for Closed {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("Closed").finish()
    }
}

impl Closed {
    fn new(hangup_notify: &Weak<std::sync::Mutex<Option<Vec<oneshot::Sender<()>>>>>) -> Self {
        match hangup_notify.upgrade() {
            Some(hangup_notify) => {
                if let Some(notifiers) = hangup_notify.lock().unwrap().as_mut() {
                    let (tx, rx) = oneshot::channel();
                    notifiers.push(tx);
                    Self {
                        fut: async move {
                            let _ = rx.await;
                        }
                        .boxed(),
                    }
                } else {
                    Self { fut: future::ready(()).boxed() }
                }
            }
            _ => Self { fut: future::ready(()).boxed() },
        }
    }
}

impl Future for Closed {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        self.fut.as_mut().poll(cx)
    }
}

/// Sends byte data over a channel.
pub struct Sender {
    local_port: u32,
    remote_port: u32,
    chunk_size: usize,
    max_data_size: usize,
    tx: mpsc::Sender<PortEvt>,
    credits: MixedCreditUser,
    hangup_recved: Weak<AtomicBool>,
    hangup_notify: Weak<std::sync::Mutex<Option<Vec<oneshot::Sender<()>>>>>,
    port_allocator: PortAllocator,
    storage: AnyStorage,
    _drop_tx: oneshot::Sender<()>,
}

impl fmt::Debug for Sender {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Sender")
            .field("local_port", &self.local_port)
            .field("remote_port", &self.remote_port)
            .field("chunk_size", &self.chunk_size)
            .field("max_data_size", &self.max_data_size)
            .field("is_closed", &self.is_closed())
            .finish()
    }
}

impl Sender {
    /// Create a new sender.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        local_port: u32, remote_port: u32, chunk_size: usize, max_data_size: usize, tx: mpsc::Sender<PortEvt>,
        credits: MixedCreditUser, hangup_recved: Weak<AtomicBool>,
        hangup_notify: Weak<std::sync::Mutex<Option<Vec<oneshot::Sender<()>>>>>, port_allocator: PortAllocator,
        storage: AnyStorage,
    ) -> Self {
        let (_drop_tx, drop_rx) = oneshot::channel();
        let tx_drop = tx.clone();
        exec::spawn(async move {
            let _ = drop_rx.await;
            let _ = tx_drop.send(PortEvt::SenderDropped { local_port }).await;
        });

        Self {
            local_port,
            remote_port,
            chunk_size,
            max_data_size,
            tx,
            credits,
            hangup_recved,
            hangup_notify,
            port_allocator,
            storage,
            _drop_tx,
        }
    }

    /// The local port number.
    pub fn local_port(&self) -> u32 {
        self.local_port
    }

    /// The remote port number.
    pub fn remote_port(&self) -> u32 {
        self.remote_port
    }

    /// Maximum chunk size that can be sent.
    ///
    /// This is set by the remote endpoint.
    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// Configured maximum data size of receiver.
    ///
    /// This is not a limit for the sender and only provided here for
    /// advisory purposes.
    pub fn max_data_size(&self) -> usize {
        self.max_data_size
    }

    /// Sends data over the channel.
    ///
    /// Waits until send space becomes available.
    /// Data is transmitted in chunks if it exceeds the maximum chunk size.
    ///
    /// # Cancel safety
    /// If this function is cancelled before completion, the remote endpoint will receive no data.
    pub async fn send(&mut self, mut data: Bytes) -> Result<(), SendError> {
        if data.is_empty() {
            let mut credits = self.credits.request(1, 1).await?;

            let msg = PortEvt::SendData {
                remote_port: self.remote_port,
                data,
                first: true,
                last: true,
                credits: credits.take(1),
            };
            self.tx.send(msg).await?;
        } else {
            let mut first = true;
            let mut credits = MixedAssignedCredits::default();

            while !data.is_empty() {
                if credits.is_empty() {
                    credits = self.credits.request(data.len().try_into().unwrap_or(u32::MAX), 1).await?;
                }

                let at = data.len().min(self.chunk_size).min(credits.available() as usize);
                let chunk = data.split_to(at);

                let msg = PortEvt::SendData {
                    remote_port: self.remote_port,
                    credits: credits.take(chunk.len() as u32),
                    data: chunk,
                    first,
                    last: data.is_empty(),
                };
                self.tx.send(msg).await?;

                first = false;
            }
        }

        Ok(())
    }

    /// Streams a message by sending individual chunks.
    pub fn send_chunks(&mut self) -> ChunkSender<'_> {
        ChunkSender { sender: self, credits: MixedAssignedCredits::default(), first: true }
    }

    /// Tries to send data over the channel.
    ///
    /// Does not wait until send space becomes available.
    /// The maximum size of data sendable by this function is limited by
    /// the total receive buffer size.
    pub fn try_send(&mut self, data: &Bytes) -> Result<(), TrySendError> {
        let mut data = data.clone();

        if data.is_empty() {
            match self.credits.try_request(1, 1)? {
                Some(mut credits) => {
                    let msg = PortEvt::SendData {
                        remote_port: self.remote_port,
                        data,
                        first: true,
                        last: true,
                        credits: credits.take(1),
                    };
                    self.tx.try_send(msg)?;
                    Ok(())
                }
                None => Err(TrySendError::Full),
            }
        } else {
            let req = data.len().try_into().unwrap_or(u32::MAX);
            match self.credits.try_request(req, req)? {
                Some(mut credits) => {
                    let mut first = true;
                    while !data.is_empty() {
                        let at = data.len().min(self.chunk_size);
                        let chunk = data.split_to(at);

                        let msg = PortEvt::SendData {
                            remote_port: self.remote_port,
                            credits: credits.take(chunk.len() as u32),
                            data: chunk,
                            first,
                            last: data.is_empty(),
                        };
                        self.tx.try_send(msg)?;

                        first = false;
                    }
                    Ok(())
                }
                None => Err(TrySendError::Full),
            }
        }
    }

    /// Sends port open requests over this port and returns the connect requests.
    ///
    /// The receiver limits the number of ports sendable per call, see
    /// [Receiver::max_ports](super::Receiver::max_ports).
    pub async fn connect(&mut self, ports: Vec<PortReq>, wait: bool) -> Result<Vec<Connect>, SendError> {
        let mut ports_response = Vec::new();
        let mut sent_txs = Vec::new();
        let mut connects = Vec::new();

        for port in ports {
            let (response_tx, response_rx) = oneshot::channel();
            ports_response.push((port, response_tx));

            let response = exec::spawn(async move {
                match response_rx.await {
                    Ok(ConnectResponse::Accepted(sender, receiver)) => Ok((sender, receiver)),
                    Ok(ConnectResponse::Rejected { no_ports }) => {
                        if no_ports {
                            Err(ConnectError::RemotePortsExhausted)
                        } else {
                            Err(ConnectError::Rejected)
                        }
                    }
                    Err(_) => Err(ConnectError::ChMux),
                }
            });

            let (sent_tx, sent_rx) = mpsc::channel(1);
            sent_txs.push(sent_tx);

            connects.push(Connect { sent_rx, response });
        }

        let mut first = true;
        let mut credits = AssignedCredits::empty(CreditPool::Port);

        while !ports_response.is_empty() {
            if credits.is_empty() {
                let data_len = ports_response.len() * size_of::<u32>();
                credits = self
                    .credits
                    .port
                    .request(data_len.min(u32::MAX as usize) as u32, size_of::<u32>() as u32)
                    .await?;
            }

            let max_ports = self.chunk_size.min(credits.available() as usize) / size_of::<u32>();
            let next =
                if ports_response.len() > max_ports { ports_response.split_off(max_ports) } else { Vec::new() };

            credits.take((ports_response.len() * size_of::<u32>()) as u32);

            let msg = PortEvt::SendPorts {
                remote_port: self.remote_port,
                first,
                last: next.is_empty(),
                wait,
                ports: ports_response,
            };
            self.tx.send(msg).await?;

            ports_response = next;
            first = false;
        }

        Ok(connects)
    }

    /// True, once the remote endpoint has closed its receiver.
    pub fn is_closed(&self) -> bool {
        self.hangup_recved.upgrade().map(|hr| hr.load(Ordering::Relaxed)).unwrap_or_default()
    }

    /// Returns a future that will resolve when the remote endpoint closes its receiver.
    pub fn closed(&self) -> Closed {
        Closed::new(&self.hangup_notify)
    }

    /// Returns whether data can be sent anyway, even if remote endpoint closed the channel gracefully.
    ///
    /// Sending always fails if remote endpoint closed the channel non-gracefully, for example
    /// by dropping the receiver.
    ///
    /// By default this is false.
    pub fn is_graceful_close_overridden(&self) -> bool {
        self.credits.port.override_graceful_close
    }

    /// Sets whether data should be sent anyway, even if remote endpoint closed the channel gracefully.
    ///
    /// Sending always fails if remote endpoint closed the channel non-gracefully, for example
    /// by dropping the receiver.
    pub fn set_override_graceful_close(&mut self, override_graceful_close: bool) {
        self.credits.port.override_graceful_close = override_graceful_close;
    }

    /// Convert this into a sink.
    pub fn into_sink(self) -> SenderSink {
        SenderSink::new(self)
    }

    /// Returns the port allocator of the channel multiplexer.
    pub fn port_allocator(&self) -> PortAllocator {
        self.port_allocator.clone()
    }

    /// Returns the arbitrary data storage of the channel multiplexer.
    pub fn storage(&self) -> AnyStorage {
        self.storage.clone()
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        // required for correct drop order
    }
}

/// Sends chunks of a message to the remote endpoint.
///
/// You must call [finish](Self::finish) to finalize the sending of the message.
/// Drop the chunk sender to cancel the message.
pub struct ChunkSender<'a> {
    sender: &'a mut Sender,
    credits: MixedAssignedCredits,
    first: bool,
}

impl<'a> ChunkSender<'a> {
    async fn send_int(&mut self, mut data: Bytes, finish: bool) -> Result<(), SendError> {
        if data.is_empty() {
            if self.credits.is_empty() {
                self.credits = self.sender.credits.request(1, 1).await?;
            }

            let msg = PortEvt::SendData {
                remote_port: self.sender.remote_port,
                data,
                first: self.first,
                last: finish,
                credits: self.credits.take(1),
            };
            self.sender.tx.send(msg).await?;

            self.first = false;
        } else {
            while !data.is_empty() {
                if self.credits.is_empty() {
                    self.credits =
                        self.sender.credits.request(data.len().try_into().unwrap_or(u32::MAX), 1).await?;
                }

                let at = data.len().min(self.sender.chunk_size).min(self.credits.available() as usize);
                let chunk = data.split_to(at);

                let msg = PortEvt::SendData {
                    remote_port: self.sender.remote_port,
                    credits: self.credits.take(chunk.len() as u32),
                    data: chunk,
                    first: self.first,
                    last: data.is_empty() && finish,
                };
                self.sender.tx.send(msg).await?;

                self.first = false;
            }
        }

        Ok(())
    }

    /// Sends a non-final chunk of a message.
    ///
    /// The boundaries of chunks within a message may change during transmission,
    /// thus there is no guarantee that [Receiver::recv_chunk](super::Receiver::recv_chunk)
    /// will return the same chunks as sent.
    pub async fn send(mut self, chunk: Bytes) -> Result<ChunkSender<'a>, SendError> {
        self.send_int(chunk, false).await?;
        Ok(self)
    }

    /// Send the final chunk of a message.
    ///
    /// This saves one multiplexer message compared to calling [send](Self::send)
    /// followed by [finish](Self::finish).
    pub async fn send_final(mut self, chunk: Bytes) -> Result<(), SendError> {
        self.send_int(chunk, true).await
    }

    /// Finishes the message.
    pub async fn finish(mut self) -> Result<(), SendError> {
        self.send_int(Bytes::new(), true).await
    }
}

/// A sink sending byte data over a channel.
pub struct SenderSink {
    sender: Option<Arc<Mutex<Sender>>>,
    send_fut: Option<ReusableBoxFuture<'static, Result<(), SendError>>>,
}

impl SenderSink {
    fn new(sender: Sender) -> Self {
        Self { sender: Some(Arc::new(Mutex::new(sender))), send_fut: None }
    }

    async fn send(sender: Arc<Mutex<Sender>>, data: Bytes) -> Result<(), SendError> {
        let mut sender = sender.lock().await;
        sender.send(data).await
    }

    fn start_send(&mut self, data: Bytes) -> Result<(), SendError> {
        if self.send_fut.is_some() {
            panic!("sink is not ready for sending");
        }

        match self.sender.clone() {
            Some(sender) => {
                self.send_fut = Some(ReusableBoxFuture::new(Self::send(sender, data)));
                Ok(())
            }
            None => panic!("start_send after sink has been closed"),
        }
    }

    fn poll_send(&mut self, cx: &mut Context) -> Poll<Result<(), SendError>> {
        match &mut self.send_fut {
            Some(fut) => {
                let res = ready!(fut.poll(cx));
                self.send_fut = None;
                Poll::Ready(res)
            }
            None => Poll::Ready(Ok(())),
        }
    }

    fn close(&mut self) {
        self.sender = None;
    }
}

impl Sink<Bytes> for SenderSink {
    type Error = SendError;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Pin::into_inner(self).poll_send(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: Bytes) -> Result<(), Self::Error> {
        Pin::into_inner(self).start_send(item)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Pin::into_inner(self).poll_send(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        ready!(Pin::into_inner(self.as_mut()).poll_send(cx))?;
        Pin::into_inner(self).close();
        Poll::Ready(Ok(()))
    }
}
