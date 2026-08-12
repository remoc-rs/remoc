use bytes::Bytes;
use futures::{
    Future, FutureExt,
    future::{self, BoxFuture},
    ready,
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
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio_util::sync::ReusableBoxFuture;

use super::{
    AnyStorage, Connect, ConnectError, ConnectReq, PortAllocator,
    client::{ConnectRsp, PreConnectState},
    credit::{AssignedCredits, CreditPool, MixedAssignedCredits, MixedCreditUser},
    mux::PortEvt,
    port_allocator::{PortsExhausted, SidePort},
};
use crate::exec::{self, runtime};

/// An error occurred during sending of a message.
#[derive(Debug, Clone)]
pub enum SendError {
    /// The multiplexer terminated.
    ChMux,
    /// Other side closed receiving end of channel.
    Closed {
        /// True, if remote endpoint still processes messages that were already sent.
        gracefully: bool,
    },
    /// Pre-connected port was rejected.
    Rejected {
        /// Remote endpoint had not ports available.
        no_ports: bool,
    },
    /// All local ports are in use.
    LocalPortsExhausted,
    /// Too many pre-connection requests are pending.
    TooManyPendingPreConnectReqs,
}

#[cfg(feature = "serde")]
crate::versioned::compact::impl_enum! {
    SendError,
    variants {
        ChMux => "_0",
        Closed { gracefully: bool } => "_1",
        Rejected { no_ports: bool } => "_2",
        LocalPortsExhausted => "_3",
        TooManyPendingPreConnectReqs => "_4",
    }
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
            Self::LocalPortsExhausted => write!(f, "all local ports are in use"),
            Self::Rejected { .. } => write!(f, "pre-connected port was rejected"),
            Self::TooManyPendingPreConnectReqs => {
                write!(f, "too many pre-connection requests are pending")
            }
        }
    }
}

impl Error for SendError {}

impl<T> From<mpsc::error::SendError<T>> for SendError {
    fn from(_err: mpsc::error::SendError<T>) -> Self {
        Self::ChMux
    }
}

impl From<PortsExhausted> for SendError {
    fn from(_: PortsExhausted) -> Self {
        SendError::LocalPortsExhausted
    }
}

impl From<SendError> for std::io::Error {
    fn from(err: SendError) -> Self {
        use std::io::ErrorKind;
        match err {
            SendError::ChMux => Self::new(ErrorKind::ConnectionReset, err),
            SendError::Closed { gracefully: false } => Self::new(ErrorKind::ConnectionReset, err),
            SendError::Closed { gracefully: true } => Self::new(ErrorKind::ConnectionAborted, err),
            SendError::Rejected { .. } => Self::new(ErrorKind::ConnectionRefused, err),
            SendError::LocalPortsExhausted => Self::new(ErrorKind::AddrInUse, err),
            SendError::TooManyPendingPreConnectReqs => Self::new(ErrorKind::AddrInUse, err),
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
#[must_use = "the receiver is only closed once this has been awaited"]
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

/// This future resolves when the remote endpoint has received all data sent on the channel
/// up to calling [`Sender::all_received`].
///
/// It also resolves when the channel is closed or fails.
///
/// There is no way to determine whether the data was received or the channel failed.
#[must_use = "remote endpoint has received data only once this is awaited"]
pub struct AllReceived(BoxFuture<'static, ()>);

impl fmt::Debug for AllReceived {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("AllReceived").finish()
    }
}

impl Future for AllReceived {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        ready!(self.0.poll_unpin(cx));
        Poll::Ready(())
    }
}

/// This future resolves when flush initiated by calling [`Sender::flush`] has completed.
///
/// Dropping this cancels the flush, if possible.
#[must_use = "the flush is only complete once this has been awaited"]
pub struct Flushed(BoxFuture<'static, ()>);

impl fmt::Debug for Flushed {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("Flushed").finish()
    }
}

impl Future for Flushed {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        ready!(self.0.poll_unpin(cx));
        Poll::Ready(())
    }
}

/// Sends byte data over a channel.
pub struct Sender {
    local_port: SidePort,
    remote_port: SidePort,
    chunk_size: usize,
    max_data_size: usize,
    tx: mpsc::Sender<PortEvt>,
    credits: MixedCreditUser,
    hangup_recved: Weak<AtomicBool>,
    hangup_notify: Weak<std::sync::Mutex<Option<Vec<oneshot::Sender<()>>>>>,
    port_allocator: PortAllocator,
    storage: AnyStorage,
    all_received_supported: bool,
    pre_connected_rx: Option<watch::Receiver<PreConnectState>>,
    handle: runtime::Handle,
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
    pub(super) fn new(
        local_port: SidePort, remote_port: SidePort, chunk_size: usize, max_data_size: usize,
        tx: mpsc::Sender<PortEvt>, credits: MixedCreditUser, hangup_recved: Weak<AtomicBool>,
        hangup_notify: Weak<std::sync::Mutex<Option<Vec<oneshot::Sender<()>>>>>, port_allocator: PortAllocator,
        storage: AnyStorage, all_received_supported: bool,
        pre_connected_rx: Option<watch::Receiver<PreConnectState>>,
    ) -> Self {
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
            all_received_supported,
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

    /// Checks whether a possible error is due to the rejection of a pre-connected
    /// error and maps the returned error appropriately.
    async fn handle_pre_connect_result<T>(&mut self, res: Result<T, SendError>) -> Result<T, SendError> {
        match res {
            Ok(res) => Ok(res),
            Err(err) => match &mut self.pre_connected_rx {
                None => Err(err),
                Some(pre_connected_rx) => {
                    let Ok(res) = pre_connected_rx.wait_for(|res| res.is_decided()).await else {
                        return Err(err);
                    };
                    match &*res {
                        PreConnectState::PreConnected => unreachable!(),
                        PreConnectState::Accepted => Err(err),
                        PreConnectState::Rejected { no_ports } => {
                            Err(SendError::Rejected { no_ports: *no_ports })
                        }
                    }
                }
            },
        }
    }

    /// Sends data over the channel.
    ///
    /// Waits until send space becomes available.
    /// Data is transmitted in chunks if it exceeds the maximum chunk size.
    ///
    /// # Cancel safety
    /// If this function is cancelled before completion, the remote endpoint will receive no data.
    pub async fn send(&mut self, data: Bytes) -> Result<(), SendError> {
        let res = self.do_send(data).await;
        self.handle_pre_connect_result(res).await
    }

    async fn do_send(&mut self, mut data: Bytes) -> Result<(), SendError> {
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
        match self.do_try_send(data) {
            Ok(()) => Ok(()),
            Err(err) => match &mut self.pre_connected_rx {
                None => Err(err),
                Some(pre_connected_rx) => match &*pre_connected_rx.borrow_and_update() {
                    PreConnectState::PreConnected | PreConnectState::Accepted => Err(err),
                    PreConnectState::Rejected { no_ports } => {
                        Err(TrySendError::Send(SendError::Rejected { no_ports: *no_ports }))
                    }
                },
            },
        }
    }

    fn do_try_send(&mut self, data: &Bytes) -> Result<(), TrySendError> {
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

    /// Flushes the global send queue and underlying sink of the channel multiplexer.
    ///
    /// Explicit flushing is usually not required, since the channel multiplexer
    /// will automatically flush the underlying sink when there is currently no data
    /// to send. Indeed, if this method is called too often, it may prevent the channel
    /// multiplexer from batching small messages for sending, thus reducing send throughput.
    ///
    /// The returned future completes once flushing is completed.
    /// Dropping the returned future cancels the flush, if possible.
    pub fn flush(&mut self) -> Flushed {
        let tx = self.tx.clone();
        let (flushed_tx, flushed_rx) = oneshot::channel();

        let task = async move {
            let _ = tx.send(PortEvt::Flush { flushed_tx }).await;
            let _ = flushed_rx.await;
        };
        Flushed(task.boxed())
    }

    /// Allocates a port connection request.
    pub fn connect_req(&self) -> Result<ConnectReq, PortsExhausted> {
        self.port_allocator.connect_req()
    }

    /// Sends port open requests over this port and returns the connect requests.
    ///
    /// The receiver limits the number of ports sendable per call, see
    /// [Receiver::max_ports](super::Receiver::max_ports).
    pub async fn connect(&mut self, connects: Vec<ConnectReq>) -> Result<Vec<Connect>, SendError> {
        let res = self.do_connect(connects).await;
        self.handle_pre_connect_result(res).await
    }

    async fn do_connect(&mut self, connect_reqs: Vec<ConnectReq>) -> Result<Vec<Connect>, SendError> {
        // Assemble port requests.
        let mut ports_response = Vec::new();
        let mut sent_txs = Vec::new();
        let mut connects = Vec::new();

        for connect_req in connect_reqs {
            let port_req = connect_req.into_port_req().await?;

            let (sent_tx, sent_rx) = oneshot::channel();
            sent_txs.push(sent_tx);

            let (response_tx, response_rx) = oneshot::channel();
            ports_response.push((port_req, response_tx));

            let response = exec::spawn(async move {
                match response_rx.await {
                    Ok(ConnectRsp::Accepted(sender, receiver)) => Ok((sender, receiver)),
                    Ok(ConnectRsp::Rejected { no_ports }) => {
                        if no_ports {
                            Err(ConnectError::RemotePortsExhausted)
                        } else {
                            Err(ConnectError::Rejected)
                        }
                    }
                    Err(_) => Err(ConnectError::ChMux),
                }
            });

            connects.push(Connect { sent_rx: Some(sent_rx), response });
        }

        // Send port data.
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

    /// Returns a future that will resolve when the remote endpoints has received all
    /// data sent on this channel up to now.
    ///
    /// Data is considered received once it has been returned by [`Receiver::recv`],
    /// [`Receiver::recv_chunk`] or [`Receiver::recv_any`] on the remote endpoint.
    ///
    /// This does not imply that the data has actually been processed, since the remote
    /// endpoint may have failed directly after receiving the data.
    ///
    /// If the remote endpoint does not support this method, the returned future will
    /// resolve immediately.
    ///
    /// [`Receiver::recv`]: super::Receiver::recv
    /// [`Receiver::recv_chunk`]: super::Receiver::recv_chunk
    /// [`Receiver::recv_any`]: super::Receiver::recv_any
    pub fn all_received(&self) -> AllReceived {
        let tx = self.tx.clone();
        let all_received_supported = self.all_received_supported;
        let is_sendable = self.credits.port.check_sendable().is_ok();
        let (processed_tx, processed_rx) = oneshot::channel();
        let msg = PortEvt::RequestReceivedReport { local_port: self.local_port(), processed_tx };

        let task = async move {
            if !all_received_supported || !is_sendable {
                return;
            }

            let _ = tx.send(msg).await;
            let _ = processed_rx.await;
        };

        AllReceived(task.boxed())
    }

    /// Returns whehter the remote endpoint supports calling [all_received](Self::all_received).
    pub fn is_all_received_supported(&self) -> bool {
        self.all_received_supported
    }

    /// Returns whether this channel may use global credits for sending data.
    ///
    /// By default this is enabled.
    pub fn are_global_credits_used(&self) -> bool {
        self.credits.global_enabled
    }

    /// Sets whether this channel may use global credits for sending data.
    ///
    /// When this is disabled the channel may only have a limited amount of data
    /// in-flight between the local and remote endpoint, thus limiting the maximum
    /// achievable bandwidth. Normally this should be kept enabled and only disabled
    /// if you want to avoid that a channel with high expected backpressure takes up
    /// a lot of buffer space.
    ///
    /// By default this is enabled.
    pub fn set_global_credits_use(&mut self, use_global_credits: bool) {
        self.credits.global_enabled = use_global_credits;
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
        let tx = self.tx.clone();
        let local_port = self.local_port;
        self.handle.spawn(async move {
            let _ = tx.send(PortEvt::SenderDropped { local_port }).await;
        });
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
    async fn send_int(&mut self, data: Bytes, finish: bool) -> Result<(), SendError> {
        let res = self.do_send_int(data, finish).await;
        self.sender.handle_pre_connect_result(res).await
    }

    async fn do_send_int(&mut self, mut data: Bytes, finish: bool) -> Result<(), SendError> {
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
    sending: bool,
    send_fut: ReusableBoxFuture<'static, Result<(), SendError>>,
    flushable: bool,
    flushing: bool,
    flush_fut: ReusableBoxFuture<'static, ()>,
}

impl SenderSink {
    fn new(sender: Sender) -> Self {
        Self {
            sender: Some(Arc::new(Mutex::new(sender))),
            sending: false,
            send_fut: ReusableBoxFuture::new(async { Ok(()) }),
            flushable: false,
            flushing: false,
            flush_fut: ReusableBoxFuture::new(async {}),
        }
    }

    /// Whether flushing this sink [flushes the global send queue](Sender::flush).
    ///
    /// By default this is false.
    pub fn flushable(&self) -> bool {
        self.flushable
    }

    /// Sets whether flushing this sink [flushes the global send queue](Sender::flush).
    ///
    /// By default this is false.    
    pub fn set_flushable(&mut self, flushable: bool) {
        self.flushable = flushable;
    }

    async fn send(sender: Arc<Mutex<Sender>>, data: Bytes) -> Result<(), SendError> {
        let mut sender = sender.lock().await;
        sender.send(data).await
    }

    async fn flush(sender: Arc<Mutex<Sender>>) {
        let mut sender = sender.lock().await;
        sender.flush().await
    }

    fn start_send(&mut self, data: Bytes) -> Result<(), SendError> {
        if self.sending || self.flushing {
            panic!("sink is not ready for sending");
        }

        let Some(sender) = self.sender.clone() else { panic!("start_send after sink has been closed") };
        self.sending = true;
        self.send_fut.set(Self::send(sender, data));
        Ok(())
    }

    fn poll_send(&mut self, cx: &mut Context) -> Poll<Result<(), SendError>> {
        if !self.sending {
            return Poll::Ready(Ok(()));
        }

        let res = ready!(self.send_fut.poll(cx));
        self.sending = false;
        Poll::Ready(res)
    }

    fn poll_flush(&mut self, cx: &mut Context) -> Poll<Result<(), SendError>> {
        if !self.flushable && !self.flushing {
            return Poll::Ready(Ok(()));
        }

        let Some(sender) = self.sender.clone() else { panic!("poll_flush after sink has been closed") };

        if !self.flushing {
            self.flushing = true;
            self.flush_fut.set(Self::flush(sender));
        }

        ready!(self.flush_fut.poll(cx));
        self.flushing = false;
        Poll::Ready(Ok(()))
    }

    fn close(&mut self) {
        self.sender = None;
    }
}

impl Sink<Bytes> for SenderSink {
    type Error = SendError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        if self.flushing {
            ready!(Pin::into_inner(self.as_mut()).poll_flush(cx))?;
        }
        Pin::into_inner(self).poll_send(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: Bytes) -> Result<(), Self::Error> {
        Pin::into_inner(self).start_send(item)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        if self.sending {
            ready!(Pin::into_inner(self.as_mut()).poll_send(cx))?;
        }
        Pin::into_inner(self).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        ready!(Pin::into_inner(self.as_mut()).poll_send(cx))?;
        ready!(Pin::into_inner(self.as_mut()).poll_flush(cx))?;
        Pin::into_inner(self).close();
        Poll::Ready(Ok(()))
    }
}

impl Unpin for SenderSink {}

impl From<Sender> for SenderSink {
    fn from(sender: Sender) -> Self {
        Self::new(sender)
    }
}
