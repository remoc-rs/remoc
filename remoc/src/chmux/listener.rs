use futures::{
    ready,
    stream::Stream,
    task::{Context, Poll},
};
use std::{error::Error, fmt, pin::Pin, sync::Arc};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::ReusableBoxFuture;

use super::{
    any_storage::AnyStorage,
    mux::PortEvt,
    port_allocator::{
        AllocatedSidePort, PortAllocator, PortsExhausted, RemotePortAlreadyAllocated, ReservedPort,
    },
    receiver::Receiver,
    sender::Sender,
};
use wokio::runtime;

/// An error returned when a multiplexed channel listener cannot be created.
///
/// Most applications encounter this type through a higher-level channel error.
#[derive(Debug, Clone)]
pub enum ListenerError {
    /// No local port is currently available for the listener.
    LocalPortsExhausted,
    /// The requested remote port is already assigned to another channel.
    RemotePortAlreadyAllocated(u32),
    /// The underlying Remoc connection terminated.
    ChMux,
}

#[cfg(feature = "serde")]
crate::versioned::compact::impl_enum! {
    ListenerError,
    variants {
        LocalPortsExhausted => "_0",
        RemotePortAlreadyAllocated(port: u32) => "_1",
        ChMux => "_2",
    }
}

impl fmt::Display for ListenerError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::LocalPortsExhausted => write!(f, "all local ports are in use"),
            Self::RemotePortAlreadyAllocated(port) => write!(f, "remote port {port} is already allocated"),
            Self::ChMux => write!(f, "Remoc connection terminated"),
        }
    }
}

impl Error for ListenerError {}

impl From<RemotePortAlreadyAllocated> for ListenerError {
    fn from(err: RemotePortAlreadyAllocated) -> Self {
        Self::RemotePortAlreadyAllocated(err.0)
    }
}

impl From<PortsExhausted> for ListenerError {
    fn from(_: PortsExhausted) -> Self {
        Self::LocalPortsExhausted
    }
}

impl From<ListenerError> for std::io::Error {
    fn from(err: ListenerError) -> Self {
        use std::io::ErrorKind;
        match err {
            ListenerError::LocalPortsExhausted => Self::new(ErrorKind::AddrInUse, err),
            ListenerError::RemotePortAlreadyAllocated(_) => Self::new(ErrorKind::AddrInUse, err),
            ListenerError::ChMux => Self::new(ErrorKind::ConnectionReset, err),
        }
    }
}

/// An error occurred while tentatively accepting a connection request.
#[derive(Debug, Clone)]
pub enum TentativeAcceptError {
    /// The request was not pre-connected by the remote endpoint.
    ///
    /// Only a pre-connected request can be accepted tentatively, because only its
    /// requester is prepared to be told about a rejection after the channel has
    /// been established.
    NotPreConnected,
    /// Accepting the request failed.
    Listener(ListenerError),
}

impl fmt::Display for TentativeAcceptError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::NotPreConnected => write!(f, "request is not pre-connected"),
            Self::Listener(err) => write!(f, "{err}"),
        }
    }
}

impl Error for TentativeAcceptError {}

impl From<ListenerError> for TentativeAcceptError {
    fn from(err: ListenerError) -> Self {
        Self::Listener(err)
    }
}

impl From<TentativeAcceptError> for std::io::Error {
    fn from(err: TentativeAcceptError) -> Self {
        match err {
            TentativeAcceptError::NotPreConnected => Self::new(std::io::ErrorKind::InvalidInput, err),
            TentativeAcceptError::Listener(err) => err.into(),
        }
    }
}

/// Allows a tentatively accepted connection to be rejected afterwards.
///
/// Dropping this guard confirms the connection.
pub struct AcceptGuard {
    remote_port: u32,
    tx: mpsc::Sender<PortEvt>,
}

impl fmt::Debug for AcceptGuard {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("AcceptGuard").field("remote_port", &self.remote_port).finish()
    }
}

impl AcceptGuard {
    /// Confirms the connection that was tentatively accepted.
    ///
    /// This is equivalent to dropping the guard.
    pub fn accept(self) {}

    /// Rejects the connection that was tentatively accepted.
    ///
    /// Setting `no_ports` to true indicates to the remote endpoint that the request
    /// was rejected because no local port could be allocated.
    ///
    /// The requester observes this like the rejection of a pre-connected port, i.e. its
    /// [sender](super::Sender) and [receiver](super::Receiver) fail with
    /// [`SendError::Rejected`](super::SendError::Rejected) and
    /// [`RecvError::Rejected`](super::RecvError::Rejected). Data that was already
    /// exchanged over the port is discarded.
    ///
    /// The sender and receiver of the tentatively accepted port should be dropped, since
    /// the requester is only notified when the port is closed.
    pub async fn reject(self, no_ports: bool) {
        let _ = self.tx.send(PortEvt::RejectedAfterAccept { remote_port: self.remote_port, no_ports }).await;
    }
}

/// A connection request by the remote endpoint.
///
/// Dropping the request rejects it.
pub struct Request {
    remote_port: u32,
    id: u32,
    wait: bool,
    allocator: PortAllocator,
    tx: mpsc::Sender<PortEvt>,
    pre_connected: Option<std::sync::Mutex<(Sender, Receiver)>>,
    done: bool,
    handle: runtime::Handle,
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Request")
            .field("remote_port", &self.remote_port)
            .field("id", &self.id)
            .field("wait", &self.wait)
            .field("pre_connected", &self.is_pre_connected())
            .finish()
    }
}

impl Request {
    pub(super) fn new(
        remote_port: u32, id: u32, wait: bool, allocator: PortAllocator, tx: mpsc::Sender<PortEvt>,
        pre_connected: Option<(Sender, Receiver)>,
    ) -> Self {
        Self {
            remote_port,
            id,
            wait,
            allocator,
            tx,
            pre_connected: pre_connected.map(std::sync::Mutex::new),
            done: false,
            handle: runtime::Handle::current(),
        }
    }

    /// The remote port number.
    pub fn remote_port(&self) -> u32 {
        self.remote_port
    }

    /// The remotely provided id.
    ///
    /// If no id was provided, this returns the [`remote port`](Self::remote_port).
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Indicates whether the handler of the request should wait for a local
    /// port to become available, if all are currently in use.
    pub fn is_wait(&self) -> bool {
        self.wait
    }

    /// Returns whether the remote endpoint provisionally opened this channel.
    ///
    /// A pre-connected channel may already carry data even though this request has not
    /// yet been accepted. Rejecting the request also rejects that provisional channel.
    pub fn is_pre_connected(&self) -> bool {
        self.pre_connected.is_some()
    }

    /// If this is a pre-connected connect request, accept it.
    ///
    /// Otherwise returns `None`.
    async fn accept_pre_connected(&mut self) -> Result<Option<(Sender, Receiver)>, ListenerError> {
        if self.pre_connected.is_none() {
            return Ok(None);
        }

        let reserved_port = self.allocator.reserve().await?;
        let permit = self.tx.reserve().await.map_err(|_| ListenerError::ChMux)?;

        let mutex = self.pre_connected.take().unwrap();
        let (sender, receiver) = mutex.into_inner().unwrap();

        permit.send(PortEvt::AcceptedAfterPreConnect { remote_port: self.remote_port, reserved_port });
        self.done = true;

        Ok(Some((sender, receiver)))
    }

    /// Tentatively accepts the request, so that it can still be rejected afterwards.
    ///
    /// This consumes the request and returns the sender and receiver for the new
    /// bidirectional port together with a guard that allows rejecting the connection
    /// afterwards. Dropping the guard confirms the connection.
    ///
    /// Use this when the decision whether to serve the channel depends on a result that
    /// is not available yet, for example the response of another endpoint the channel is
    /// forwarded to. Data can be exchanged over the port while the decision is pending.
    ///
    /// This is only possible for a [pre-connected](Self::is_pre_connected) request, since
    /// only its requester is prepared to be told about a rejection after the channel has
    /// been established. Otherwise [`TentativeAcceptError::NotPreConnected`] is returned
    /// and the request is rejected.
    pub async fn accept_tentatively(mut self) -> Result<(Sender, Receiver, AcceptGuard), TentativeAcceptError> {
        if !self.is_pre_connected() {
            return Err(TentativeAcceptError::NotPreConnected);
        }

        let remote_port = self.remote_port;
        let tx = self.tx.clone();

        let (sender, receiver) = self.accept_pre_connected().await?.unwrap();

        Ok((sender, receiver, AcceptGuard { remote_port, tx }))
    }

    /// Accepts the request.
    ///
    /// This consumes the request and returns the sender and receiver for the new
    /// bidirectional port. It may wait for a local port number when the requester
    /// permits waiting.
    pub async fn accept(mut self) -> Result<(Sender, Receiver), ListenerError> {
        if let Some(tx_rx) = self.accept_pre_connected().await? {
            return Ok(tx_rx);
        }

        let reserved = if self.wait {
            self.allocator.reserve().await?
        } else {
            match self.allocator.try_reserve() {
                Some(reserved) => reserved,
                None => {
                    self.reject(true).await;
                    return Err(ListenerError::LocalPortsExhausted);
                }
            }
        };

        self.accept_reserved(reserved).await
    }

    /// Accepts the request using the already reserved port.
    async fn accept_reserved(mut self, reserved_port: ReservedPort) -> Result<(Sender, Receiver), ListenerError> {
        if let Some(tx_rx) = self.accept_pre_connected().await? {
            return Ok(tx_rx);
        }

        let local_port: AllocatedSidePort = if self.allocator.is_port_side_supported() {
            reserved_port.into_remote(self.remote_port)?.into()
        } else {
            reserved_port.into_local().into()
        };

        let (port_tx, port_rx) = oneshot::channel();
        let _ = self.tx.send(PortEvt::Accepted { local_port, remote_port: self.remote_port, port_tx }).await;
        self.done = true;

        port_rx.await.map_err(|_| ListenerError::ChMux)
    }

    /// Rejects the connect request.
    ///
    /// Setting `no_ports` to true indicates to the remote endpoint that the request
    /// was rejected because no local port could be allocated.
    pub async fn reject(mut self, no_ports: bool) {
        let reserved_port =
            if self.is_pre_connected() { Some(self.allocator.wait_reserve().await) } else { None };
        let _ = self.tx.send(PortEvt::Rejected { remote_port: self.remote_port, no_ports, reserved_port }).await;
        self.done = true;
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        if self.done {
            return;
        }

        let remote_port = self.remote_port;
        let is_pre_connected = self.pre_connected.is_some();
        let allocator = self.allocator.clone();
        let drop_tx = self.tx.clone();

        self.handle.spawn(async move {
            let reserved_port = if is_pre_connected { Some(allocator.wait_reserve().await) } else { None };
            let _ = drop_tx.send(PortEvt::Rejected { remote_port, no_ports: false, reserved_port }).await;
        });
    }
}

/// Remote connect message.
#[allow(clippy::large_enum_variant)]
pub(crate) enum RemoteConnectMsg {
    /// Remote connect request.
    Request(Request),
    /// Client of remote endpoint has been dropped.
    ClientDropped,
}

/// Multiplexer listener.
pub struct Listener {
    wait_rx: mpsc::Receiver<RemoteConnectMsg>,
    no_wait_rx: mpsc::Receiver<RemoteConnectMsg>,
    port_allocator: PortAllocator,
    terminate_tx: mpsc::UnboundedSender<()>,
    wait_closed: bool,
    no_wait_closed: bool,
    storage: AnyStorage,
}

impl fmt::Debug for Listener {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Listener").field("port_allocator", &self.port_allocator).finish()
    }
}

impl Listener {
    pub(crate) fn new(
        wait_rx: mpsc::Receiver<RemoteConnectMsg>, no_wait_rx: mpsc::Receiver<RemoteConnectMsg>,
        port_allocator: PortAllocator, terminate_tx: mpsc::UnboundedSender<()>, storage: AnyStorage,
    ) -> Self {
        Self {
            wait_rx,
            no_wait_rx,
            port_allocator,
            terminate_tx,
            wait_closed: false,
            no_wait_closed: false,
            storage,
        }
    }

    /// Obtains the port allocator.
    pub fn port_allocator(&self) -> PortAllocator {
        self.port_allocator.clone()
    }

    /// Returns the arbitrary data storage of the channel multiplexer.
    pub fn storage(&self) -> AnyStorage {
        self.storage.clone()
    }

    /// Accept a connection returning the sender and receiver for the opened port.
    ///
    /// Returns [None] when the client of the remote endpoint has been dropped and
    /// no more connection requests can be made.
    pub async fn accept(&mut self) -> Result<Option<(Sender, Receiver)>, ListenerError> {
        loop {
            tokio::select! {
                biased;

                reserved_port = self.port_allocator.wait_reserve(), if !self.wait_closed || !self.no_wait_closed => {
                    match self.inspect().await? {
                        Some(req) => break Ok(Some(req.accept_reserved(reserved_port).await?)),
                        None => break Ok(None),
                    }
                },

                no_wait_req_opt = self.no_wait_rx.recv(), if !self.no_wait_closed => {
                    match no_wait_req_opt {
                        Some(RemoteConnectMsg::Request(no_wait_req)) => {
                            match self.port_allocator.try_reserve() {
                                Some(reserved_port) => break Ok(Some(no_wait_req.accept_reserved(reserved_port).await?)),
                                None => no_wait_req.reject(true).await,
                            }
                        },
                        Some(RemoteConnectMsg::ClientDropped) => {
                            self.no_wait_closed = true;
                            continue;
                        },
                        None => break Err(ListenerError::ChMux),
                    }
                },

                else => break Ok(None),
            }
        }
    }

    /// Obtains the next connection request from the remote endpoint.
    ///
    /// Connection requests can be stored and accepted or rejected at a later time.
    /// The maximum number of unanswered connection requests is specified in the
    /// configuration. If this number is reached, the remote endpoint will
    /// not send any more connection requests.
    ///
    /// Returns [None] when the client of the remote endpoint has been dropped and
    /// no more connection requests can be made.
    pub async fn inspect(&mut self) -> Result<Option<Request>, ListenerError> {
        loop {
            let (wait, req_opt) = tokio::select! {
                req_opt = self.wait_rx.recv(), if !self.wait_closed => (true, req_opt),
                req_opt = self.no_wait_rx.recv(), if !self.no_wait_closed => (false, req_opt),
                else => return Ok(None),
            };

            match req_opt {
                Some(RemoteConnectMsg::Request(req)) => break Ok(Some(req)),
                Some(RemoteConnectMsg::ClientDropped) => {
                    if wait {
                        self.wait_closed = true;
                    } else {
                        self.no_wait_closed = true;
                    }
                    continue;
                }
                None => break Err(ListenerError::ChMux),
            }
        }
    }

    /// Converts this listener into a stream of incoming connection requests.
    ///
    /// The stream ends when the remote client is dropped and no more requests
    /// can arrive. Each yielded [`Request`] must be accepted or rejected.
    pub fn into_stream(self) -> ListenerStream {
        ListenerStream::new(self)
    }

    /// Terminates the multiplexer, forcibly closing all open ports.
    ///
    /// This also prevents new ports from being opened. Existing senders,
    /// receivers, and clients subsequently observe termination.
    pub fn terminate(&self) {
        let _ = self.terminate_tx.send(());
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // required for correct drop order
    }
}

/// A stream accepting connections and returning senders and receivers.
///
/// Ends when the client is dropped at the remote endpoint.
pub struct ListenerStream {
    server: Arc<Mutex<Listener>>,
    #[allow(clippy::type_complexity)]
    accept_fut: Option<ReusableBoxFuture<'static, Option<Result<(Sender, Receiver), ListenerError>>>>,
}

impl ListenerStream {
    fn new(server: Listener) -> Self {
        Self { server: Arc::new(Mutex::new(server)), accept_fut: None }
    }

    async fn accept(server: Arc<Mutex<Listener>>) -> Option<Result<(Sender, Receiver), ListenerError>> {
        let mut server = server.lock().await;
        server.accept().await.transpose()
    }

    fn poll_next(&mut self, cx: &mut Context) -> Poll<Option<Result<(Sender, Receiver), ListenerError>>> {
        if self.accept_fut.is_none() {
            self.accept_fut = Some(ReusableBoxFuture::new(Self::accept(self.server.clone())));
        }

        let accept_fut = self.accept_fut.as_mut().unwrap();
        let res = ready!(accept_fut.poll(cx));

        self.accept_fut = None;
        Poll::Ready(res)
    }
}

impl Stream for ListenerStream {
    type Item = Result<(Sender, Receiver), ListenerError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        Pin::into_inner(self).poll_next(cx)
    }
}
