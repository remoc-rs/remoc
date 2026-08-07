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
use tokio::sync::{mpsc, oneshot};

use super::{
    port_allocator::{ConnectReq, ConnectReqsExhausted, PortAllocator, PortReq},
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
    TooManyPendingConnectReqs,
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
            Self::TooManyPendingConnectReqs => write!(f, "too many connection requests are pending"),
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
            ConnectError::TooManyPendingConnectReqs => Self::new(ErrorKind::AddrInUse, err),
            ConnectError::Rejected => Self::new(ErrorKind::ConnectionRefused, err),
            ConnectError::ChMux => Self::new(ErrorKind::ConnectionReset, err),
        }
    }
}

/// Connection to remote service request to local multiplexer.
#[derive(Debug)]
pub(super) struct ConnectRequest {
    /// Port request.
    pub port_req: PortReq,
    /// Notification that request has been queued for sending.
    pub sent_tx: oneshot::Sender<()>,
    /// Response channel sender.
    pub response_tx: oneshot::Sender<ConnectResponse>,
}

/// Connection to remote service response from local multiplexer.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub(super) enum ConnectResponse {
    /// Connection accepted or pre-connected and channel opened.
    Accepted(Sender, Receiver),
    /// Connection was rejected.
    Rejected {
        /// Remote endpoint had not ports available.
        no_ports: bool,
    },
}

/// Pre-connection state.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub(super) enum PreConnectState {
    /// Port is pre-connected and no accept/reject response has yet been
    /// received from the remote endpoint.
    PreConnected,
    /// Connection accepted.
    Accepted,
    /// Connection was rejected.
    Rejected {
        /// Remote endpoint had not ports available.
        no_ports: bool,
    },
}

impl PreConnectState {
    /// Whether the accept/reject response has been received.
    pub fn is_decided(&self) -> bool {
        !matches!(self, Self::PreConnected)
    }
}

/// An outstanding connection request.
///
/// Await it to obtain the result of the connection request.
pub struct Connect {
    pub(crate) sent_rx: Option<oneshot::Receiver<()>>,
    pub(crate) response: JoinHandle<Result<(Sender, Receiver), ConnectError>>,
}

impl fmt::Debug for Connect {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Connect").finish()
    }
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
    pub(super) fn new(
        tx: mpsc::UnboundedSender<ConnectRequest>, port_allocator: PortAllocator,
        listener_dropped: Arc<AtomicBool>, terminate_tx: mpsc::UnboundedSender<()>,
    ) -> Client {
        Client { tx, port_allocator, listener_dropped, terminate_tx }
    }

    /// Obtains the port allocator.
    pub fn port_allocator(&self) -> PortAllocator {
        self.port_allocator.clone()
    }

    /// Allocates a port connection request.
    pub fn connect_req(&self) -> Result<ConnectReq, ConnectReqsExhausted> {
        self.port_allocator.connect_req()
    }

    /// Connects the specified port connection request to the remote endpoint.
    ///
    /// This returns a [Connect] that must be awaited to obtain the result.
    pub fn connect(&self, connect_req: ConnectReq) -> Connect {
        let port_allocator = self.port_allocator.clone();
        let tx = self.tx.clone();
        let (sent_tx, sent_rx) = oneshot::channel();
        let listener_dropped = self.listener_dropped.clone();

        let response = exec::spawn(async move {
            if listener_dropped.load(Ordering::Relaxed) {
                return Err(ConnectError::Rejected);
            }

            let port_req = connect_req.into_port_req().await.ok_or(ConnectError::LocalPortsExhausted)?;

            // Obtain credit for connection request.
            // Not necessary when port will be pre-connected, because then request already
            // carries a connection request credit.
            let _connect_credit = if port_req.opts.pre_connect_credit.is_some() {
                None
            } else {
                Some(if port_req.opts.wait {
                    port_allocator.connect_req_credit().await
                } else {
                    port_allocator.try_connect_req_credit().ok_or(ConnectError::TooManyPendingConnectReqs)?
                })
            };

            // Build and send request.
            let (response_tx, response_rx) = oneshot::channel();
            let req = ConnectRequest { port_req, sent_tx, response_tx };
            let _ = tx.send(req);

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

        Connect { sent_rx: Some(sent_rx), response }
    }

    /// Connects to a newly allocated remote port from a newly allocated local port.
    ///
    /// This function waits until a local and remote port become available and tries
    /// to pre-connect the port if possible.
    pub async fn connect_port(&self) -> Result<(Sender, Receiver), ConnectError> {
        let req = self.connect_req().map_err(|_| ConnectError::LocalPortsExhausted)?;
        let req = req.wait().try_pre_connect();
        self.connect(req).await
    }

    /// Terminates the multiplexer, forcibly closing all open ports.
    pub fn terminate(&self) {
        let _ = self.terminate_tx.send(());
    }
}
