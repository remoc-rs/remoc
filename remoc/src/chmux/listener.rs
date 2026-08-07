use futures::{
    ready,
    stream::Stream,
    task::{Context, Poll},
};
use std::{error::Error, fmt, pin::Pin, sync::Arc};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::ReusableBoxFuture;

use super::{
    mux::PortEvt,
    port_allocator::{AllocatedSidePort, PortAllocator, RemotePortAlreadyAllocated},
    receiver::Receiver,
    sender::Sender,
};
use crate::{chmux::port_allocator::ReservedPort, exec};

/// An multiplexer listener error.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ListenerError {
    /// All local ports are in use.
    LocalPortsExhausted,
    /// Used mismatched remote port number for accepting.
    MismatchedRemotePort {
        /// Port number of remote endpoint in connection request.
        requesting_remote_port: u32,
        /// Port number used to accept request.
        accepted_remote_port: u32,
    },
    /// The requested remote port number has alredy been allocated.
    RemotePortAlreadyAllocated(u32),
    /// A multiplexer error has occurred or it has been terminated.
    MultiplexerError,
}

impl fmt::Display for ListenerError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::LocalPortsExhausted => write!(f, "all local ports are in use"),
            Self::MismatchedRemotePort { requesting_remote_port, accepted_remote_port } => write!(
                f,
                "cannot accept request from remote port {requesting_remote_port} using remote port {accepted_remote_port}"
            ),
            Self::RemotePortAlreadyAllocated(port) => write!(f, "remote port {port} is already allocated"),
            Self::MultiplexerError => write!(f, "multiplexer error"),
        }
    }
}

impl Error for ListenerError {}

impl From<RemotePortAlreadyAllocated> for ListenerError {
    fn from(err: RemotePortAlreadyAllocated) -> Self {
        Self::RemotePortAlreadyAllocated(err.0)
    }
}

impl From<ListenerError> for std::io::Error {
    fn from(err: ListenerError) -> Self {
        use std::io::ErrorKind;
        match err {
            ListenerError::LocalPortsExhausted => Self::new(ErrorKind::AddrInUse, err),
            ListenerError::MismatchedRemotePort { .. } => Self::new(ErrorKind::InvalidInput, err),
            ListenerError::RemotePortAlreadyAllocated(_) => Self::new(ErrorKind::AddrInUse, err),
            ListenerError::MultiplexerError => Self::new(ErrorKind::ConnectionReset, err),
        }
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
    done_tx: Option<oneshot::Sender<()>>,
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Request")
            .field("remote_port", &self.remote_port)
            .field("id", &self.id)
            .field("wait", &self.wait)
            .finish()
    }
}

impl Request {
    pub(crate) fn new(
        remote_port: u32, id: u32, wait: bool, allocator: PortAllocator, tx: mpsc::Sender<PortEvt>,
    ) -> Self {
        let (done_tx, done_rx) = oneshot::channel();
        let drop_tx = tx.clone();
        exec::spawn(async move {
            if done_rx.await.is_err() {
                let _ = drop_tx.send(PortEvt::Rejected { remote_port, no_ports: false }).await;
            }
        });

        Self { remote_port, id, wait, allocator, tx, done_tx: Some(done_tx) }
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

    /// Accepts the request.
    pub async fn accept(self) -> Result<(Sender, Receiver), ListenerError> {
        let reserved = if self.wait {
            self.allocator.reserve().await
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
    pub async fn accept_reserved(self, reserved_port: ReservedPort) -> Result<(Sender, Receiver), ListenerError> {
        let local_port: AllocatedSidePort = if self.allocator.is_port_side_supported() {
            reserved_port.into_remote(self.remote_port)?.into()
        } else {
            reserved_port.into_local().into()
        };

        self.accept_from(local_port).await
    }

    /// Accepts the request using the specified local port.
    pub async fn accept_from(
        mut self, local_port: impl Into<AllocatedSidePort>,
    ) -> Result<(Sender, Receiver), ListenerError> {
        let local_port = local_port.into();
        if let AllocatedSidePort::Remote(remote_port) = &local_port
            && **remote_port != self.remote_port
        {
            return Err(ListenerError::MismatchedRemotePort {
                requesting_remote_port: **remote_port,
                accepted_remote_port: self.remote_port,
            });
        }

        let (port_tx, port_rx) = oneshot::channel();
        let _ = self.tx.send(PortEvt::Accepted { local_port, remote_port: self.remote_port, port_tx }).await;
        let _ = self.done_tx.take().unwrap().send(());

        port_rx.await.map_err(|_| ListenerError::MultiplexerError)
    }

    /// Rejects the connect request.
    ///
    /// Setting `no_ports` to true indicates to the remote endpoint that the request
    /// was rejected because no local port could be allocated.
    pub async fn reject(mut self, no_ports: bool) {
        let _ = self.tx.send(PortEvt::Rejected { remote_port: self.remote_port, no_ports }).await;
        let _ = self.done_tx.take().unwrap().send(());
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        // required for correct drop order
    }
}

/// Remote connect message.
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
    closed: bool,
}

impl fmt::Debug for Listener {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Listener").field("port_allocator", &self.port_allocator).finish()
    }
}

impl Listener {
    pub(crate) fn new(
        wait_rx: mpsc::Receiver<RemoteConnectMsg>, no_wait_rx: mpsc::Receiver<RemoteConnectMsg>,
        port_allocator: PortAllocator, terminate_tx: mpsc::UnboundedSender<()>,
    ) -> Self {
        Self { wait_rx, no_wait_rx, port_allocator, terminate_tx, closed: false }
    }

    /// Obtains the port allocator.
    pub fn port_allocator(&self) -> PortAllocator {
        self.port_allocator.clone()
    }

    /// Accept a connection returning the sender and receiver for the opened port.
    ///
    /// Returns [None] when the client of the remote endpoint has been dropped and
    /// no more connection requests can be made.
    pub async fn accept(&mut self) -> Result<Option<(Sender, Receiver)>, ListenerError> {
        if self.closed {
            return Ok(None);
        }

        loop {
            tokio::select! {
                biased;

                reserved_port = self.port_allocator.reserve() => {
                    match self.inspect().await? {
                        Some(req) => break Ok(Some(req.accept_reserved(reserved_port).await?)),
                        None => break Ok(None),
                    }
                },

                no_wait_req_opt = self.no_wait_rx.recv() => {
                    match no_wait_req_opt {
                        Some(RemoteConnectMsg::Request(no_wait_req)) => {
                            match self.port_allocator.try_reserve() {
                                Some(reserved_port) => break Ok(Some(no_wait_req.accept_reserved(reserved_port).await?)),
                                None => no_wait_req.reject(true).await,
                            }
                        },
                        Some(RemoteConnectMsg::ClientDropped) => {
                            self.closed = true;
                            break Ok(None);
                        },
                        None => break Err(ListenerError::MultiplexerError),
                    }
                },
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
        if self.closed {
            return Ok(None);
        }

        let req_opt = tokio::select! {
            req_opt = self.wait_rx.recv() => req_opt,
            req_opt = self.no_wait_rx.recv() => req_opt,
        };

        match req_opt {
            Some(RemoteConnectMsg::Request(req)) => Ok(Some(req)),
            Some(RemoteConnectMsg::ClientDropped) => {
                self.closed = true;
                Ok(None)
            }
            None => Err(ListenerError::MultiplexerError),
        }
    }

    /// Convert this into a listener stream.
    pub fn into_stream(self) -> ListenerStream {
        ListenerStream::new(self)
    }

    /// Terminates the multiplexer, forcibly closing all open ports.
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
