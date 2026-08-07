use futures::{Future, FutureExt, ready};
use std::{
    clone::Clone,
    error::Error,
    fmt,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use super::{
    PortReq,
    port_allocator::{AllocatedLocalPort, PortAllocator},
    receiver::Receiver,
    sender::Sender,
};
use crate::{exec, exec::task::JoinHandle};

/// An error occurred during connecting to a remote service.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ConnectError {
    /// All local ports are in use.
    LocalPortsExhausted,
    /// All remote ports are in use.
    RemotePortsExhausted,
    /// Too many connection requests are pending.
    TooManyPendingConnectionRequests,
    /// Connection has been rejected by server.
    Rejected,
    /// A multiplexer error has occurred or it has been terminated.
    ChMux,
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::LocalPortsExhausted => write!(f, "all local ports are in use"),
            Self::RemotePortsExhausted => write!(f, "all remote ports are in use"),
            Self::TooManyPendingConnectionRequests => write!(f, "too many connection requests are pending"),
            Self::Rejected => write!(f, "connection has been rejected by server"),
            Self::ChMux => write!(f, "multiplexer error"),
        }
    }
}

impl Error for ConnectError {}

impl From<ConnectError> for std::io::Error {
    fn from(err: ConnectError) -> Self {
        use std::io::ErrorKind;
        match err {
            ConnectError::LocalPortsExhausted => Self::new(ErrorKind::AddrInUse, err),
            ConnectError::RemotePortsExhausted => Self::new(ErrorKind::AddrInUse, err),
            ConnectError::TooManyPendingConnectionRequests => Self::new(ErrorKind::AddrInUse, err),
            ConnectError::Rejected => Self::new(ErrorKind::ConnectionRefused, err),
            ConnectError::ChMux => Self::new(ErrorKind::ConnectionReset, err),
        }
    }
}

/// Accounts connection request credits.
#[derive(Debug, Clone)]
struct ConnectRequestCrediter(Arc<Semaphore>);

impl ConnectRequestCrediter {
    /// Creates a new connection request crediter.
    pub fn new(limit: u16) -> Self {
        Self(Arc::new(Semaphore::new(limit.into())))
    }

    /// Obtains a connection request credit.
    ///
    /// Waits for the credit to become available.
    pub async fn request(&self) -> ConnectRequestCredit {
        ConnectRequestCredit(self.0.clone().acquire_owned().await.unwrap())
    }

    /// Tries to obtain a connection request credit.
    ///
    /// Does not wait for the credit to become available.
    pub fn try_request(&self) -> Option<ConnectRequestCredit> {
        self.0.clone().try_acquire_owned().ok().map(ConnectRequestCredit)
    }
}

/// A credit for requesting a connection.
pub(crate) struct ConnectRequestCredit(OwnedSemaphorePermit);

impl Drop for ConnectRequestCredit {
    fn drop(&mut self) {
        let _ = self.0;
    }
}

/// Connection to remote service request to local multiplexer.
#[derive(Debug)]
pub(crate) struct ConnectRequest {
    /// Local port.
    pub local_port: AllocatedLocalPort,
    /// Port id.
    pub id: u32,
    /// Notification that request has been queued for sending.
    pub sent_tx: oneshot::Sender<()>,
    /// Response channel sender.
    pub response_tx: oneshot::Sender<ConnectResponse>,
    /// Wait for port to become available.
    pub wait: bool,
}

/// Connection to remote service response from local multiplexer.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum ConnectResponse {
    /// Connection accepted and channel opened.
    Accepted(Sender, Receiver),
    /// Connection was rejected.
    Rejected {
        /// Remote endpoint had not ports available.
        no_ports: bool,
    },
}

/// An outstanding connection request.
///
/// Await it to obtain the result of the connection request.
pub struct Connect {
    pub(crate) sent_rx: Option<oneshot::Receiver<()>>,
    pub(crate) response: JoinHandle<Result<(Sender, Receiver), ConnectError>>,
}

impl Connect {
    /// Returns once the connect request has been sent.
    ///
    /// It is guaranteed that the connect request will be made available via
    /// the [Listener](super::Listener) at the remote endpoint before messages
    /// sent on any port after this function returns will arrive.
    ///
    /// This will also return when the multiplexer has been terminated.
    pub async fn sent(&mut self) {
        if let Some(sent_rx) = &mut self.sent_rx {
            let _ = sent_rx.await;
            self.sent_rx = None;
        }
    }
}

impl Future for Connect {
    type Output = Result<(Sender, Receiver), ConnectError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let result = ready!(Pin::into_inner(self).response.poll_unpin(cx));
        Poll::Ready(result.map_err(|_| ConnectError::ChMux)?)
    }
}

/// Multiplexer client.
///
/// Use to request a new port for sending and receiving.
/// This can be cloned to make simultaneous requests.
#[derive(Clone)]
pub struct Client {
    tx: mpsc::UnboundedSender<ConnectRequest>,
    crediter: ConnectRequestCrediter,
    port_allocator: PortAllocator,
    listener_dropped: Arc<AtomicBool>,
    terminate_tx: mpsc::UnboundedSender<()>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Client").field("port_allocator", &self.port_allocator).finish()
    }
}

impl Client {
    pub(crate) fn new(
        tx: mpsc::UnboundedSender<ConnectRequest>, limit: u16, port_allocator: PortAllocator,
        listener_dropped: Arc<AtomicBool>, terminate_tx: mpsc::UnboundedSender<()>,
    ) -> Client {
        Client {
            tx,
            crediter: ConnectRequestCrediter::new(limit),
            port_allocator,
            listener_dropped,
            terminate_tx,
        }
    }

    /// Obtains the port allocator.
    pub fn port_allocator(&self) -> PortAllocator {
        self.port_allocator.clone()
    }

    /// Connects to a newly allocated remote port from a newly allocated local port.
    ///
    /// This function waits until a local and remote port become available.
    pub async fn connect(&self) -> Result<(Sender, Receiver), ConnectError> {
        self.connect_ext(None, true).await?.await
    }

    /// Start opening a new port to the remote endpoint with extended options.
    ///
    /// If `local_port` is [None] a new local port number is allocated.
    /// Otherwise the specified port is used.
    ///
    /// If `wait` is true, this function waits until a local and remote port become available.
    /// Otherwise it returns the appropriate [ConnectError] if no ports are available.
    /// If `wait` is false, it still waits until the listener on the remote endpoint accepts
    /// or rejects the connection.
    ///
    /// This returns a [Connect] that must be awaited to obtain the result.
    pub async fn connect_ext(&self, local_port: Option<PortReq>, wait: bool) -> Result<Connect, ConnectError> {
        // Obtain local port.
        let local_port = match local_port {
            Some(local_port) => local_port,
            None => {
                if wait {
                    self.port_allocator.allocate_local().await.into()
                } else {
                    self.port_allocator.try_allocate_local().ok_or(ConnectError::LocalPortsExhausted)?.into()
                }
            }
        };

        // Obtain credit for connection request.
        let credit = if wait {
            self.crediter.request().await
        } else {
            match self.crediter.try_request() {
                Some(credit) => credit,
                None => return Err(ConnectError::TooManyPendingConnectionRequests),
            }
        };

        // Build and send request.
        let (sent_tx, sent_rx) = oneshot::channel();
        let (response_tx, response_rx) = oneshot::channel();
        let PortReq { port: local_port, id } = local_port;
        let req = ConnectRequest { local_port, id, sent_tx, response_tx, wait };
        let _ = self.tx.send(req);

        let listener_dropped = self.listener_dropped.clone();
        let response = exec::spawn(async move {
            // Credit must be kept until response is received.
            let _credit = credit;

            // Process response.
            match response_rx.await {
                Ok(ConnectResponse::Accepted(sender, receiver)) => Ok((sender, receiver)),
                Ok(ConnectResponse::Rejected { no_ports }) => {
                    if no_ports {
                        Err(ConnectError::RemotePortsExhausted)
                    } else {
                        Err(ConnectError::Rejected)
                    }
                }
                Err(_) => {
                    if listener_dropped.load(Ordering::Relaxed) {
                        Err(ConnectError::Rejected)
                    } else {
                        Err(ConnectError::ChMux)
                    }
                }
            }
        });

        Ok(Connect { sent_rx: Some(sent_rx), response })
    }

    /// Terminates the multiplexer, forcibly closing all open ports.
    pub fn terminate(&self) {
        let _ = self.terminate_tx.send(());
    }
}
