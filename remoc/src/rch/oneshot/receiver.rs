use futures::{Future, ready};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    convert::TryFrom,
    error::Error,
    fmt,
    pin::Pin,
    task::{Context, Poll},
};

use super::super::{DEFAULT_MAX_ITEM_SIZE, base, mpsc};
use crate::{RemoteSend, chmux, codec};

/// An error occurred during receiving over an oneshot channel.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RecvError {
    /// Sender dropped without sending a value.
    Closed,
    /// Receiving from a remote endpoint failed.
    RemoteReceive(base::RecvError),
    /// Connecting a sent channel failed.
    RemoteConnect(chmux::ConnectError),
    /// Listening for a connection from a received channel failed.
    RemoteListen(chmux::ListenerError),
}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "channel is closed"),
            Self::RemoteReceive(err) => write!(f, "receive error: {err}"),
            Self::RemoteConnect(err) => write!(f, "connect error: {err}"),
            Self::RemoteListen(err) => write!(f, "listen error: {err}"),
        }
    }
}

impl From<mpsc::RecvError> for RecvError {
    fn from(err: mpsc::RecvError) -> Self {
        match err {
            mpsc::RecvError::RemoteReceive(err) => Self::RemoteReceive(err),
            mpsc::RecvError::RemoteConnect(err) => Self::RemoteConnect(err),
            mpsc::RecvError::RemoteListen(err) => Self::RemoteListen(err),
        }
    }
}

impl TryFrom<TryRecvError> for RecvError {
    type Error = TryRecvError;

    fn try_from(err: TryRecvError) -> Result<Self, Self::Error> {
        match err {
            TryRecvError::Empty => Err(TryRecvError::Empty),
            TryRecvError::Closed => Ok(Self::Closed),
            TryRecvError::RemoteReceive(err) => Ok(Self::RemoteReceive(err)),
            TryRecvError::RemoteConnect(err) => Ok(Self::RemoteConnect(err)),
            TryRecvError::RemoteListen(err) => Ok(Self::RemoteListen(err)),
        }
    }
}

impl Error for RecvError {}

impl RecvError {
    /// Returns whether the error is final, i.e. no further receive operation can succeed.
    pub fn is_final(&self) -> bool {
        match self {
            Self::RemoteReceive(err) => err.is_final(),
            Self::Closed | Self::RemoteConnect(_) | Self::RemoteListen(_) => true,
        }
    }
}

/// An error occurred during trying to receive over an oneshot channel.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TryRecvError {
    /// No value has been received yet.
    Empty,
    /// Sender dropped without sending a value.
    Closed,
    /// Receiving from a remote endpoint failed.
    RemoteReceive(base::RecvError),
    /// Connecting a sent channel failed.
    RemoteConnect(chmux::ConnectError),
    /// Listening for a connection from a received channel failed.
    RemoteListen(chmux::ListenerError),
}

impl fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "channel is empty"),
            Self::Closed => write!(f, "channel is closed"),
            Self::RemoteReceive(err) => write!(f, "receive error: {err}"),
            Self::RemoteConnect(err) => write!(f, "connect error: {err}"),
            Self::RemoteListen(err) => write!(f, "listen error: {err}"),
        }
    }
}

impl From<mpsc::TryRecvError> for TryRecvError {
    fn from(err: mpsc::TryRecvError) -> Self {
        match err {
            mpsc::TryRecvError::Empty => Self::Empty,
            mpsc::TryRecvError::Closed => Self::Closed,
            mpsc::TryRecvError::RemoteReceive(err) => Self::RemoteReceive(err),
            mpsc::TryRecvError::RemoteConnect(err) => Self::RemoteConnect(err),
            mpsc::TryRecvError::RemoteListen(err) => Self::RemoteListen(err),
        }
    }
}

impl From<RecvError> for TryRecvError {
    fn from(err: RecvError) -> Self {
        match err {
            RecvError::Closed => Self::Closed,
            RecvError::RemoteReceive(err) => Self::RemoteReceive(err),
            RecvError::RemoteConnect(err) => Self::RemoteConnect(err),
            RecvError::RemoteListen(err) => Self::RemoteListen(err),
        }
    }
}

impl Error for TryRecvError {}

/// Receive a value from the associated sender.
///
/// Await this future to receive the value.
#[derive(Serialize, Deserialize)]
#[serde(bound(serialize = "T: RemoteSend, Codec: codec::Codec"))]
#[serde(bound(deserialize = "T: RemoteSend, Codec: codec::Codec"))]
pub struct Receiver<T, Codec = codec::Default, const MAX_ITEM_SIZE: usize = DEFAULT_MAX_ITEM_SIZE>(
    pub(crate) mpsc::Receiver<T, Codec, 1, MAX_ITEM_SIZE>,
);

impl<T, Codec, const MAX_ITEM_SIZE: usize> fmt::Debug for Receiver<T, Codec, MAX_ITEM_SIZE> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Receiver").finish()
    }
}

impl<T, Codec> Receiver<T, Codec>
where
    T: RemoteSend,
    Codec: codec::Codec,
{
    /// Creates a receiver that forwards the value from the given local oneshot receiver.
    ///
    /// The returned receiver may be sent to remote endpoints via channels.
    ///
    /// Any send errors that occur during forwarding are silently dropped;
    /// use [`forward`](super::forward) if you need to observe them.
    pub fn forwarded(local_rx: tokio::sync::oneshot::Receiver<T>) -> Self {
        let (_fwd, rx) = super::forward(local_rx);
        rx
    }
}

impl<T, Codec, const MAX_ITEM_SIZE: usize> Receiver<T, Codec, MAX_ITEM_SIZE>
where
    T: DeserializeOwned + Send + 'static,
    Codec: codec::Codec,
{
    /// Prevents the associated sender from sending a value.
    pub fn close(&mut self) {
        self.0.close()
    }

    /// Attempts to receive a value transmitted by the sender.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        Ok(self.0.try_recv()?)
    }

    /// The maximum item size in bytes.
    pub fn max_item_size(&self) -> usize {
        self.0.max_item_size()
    }

    /// Sets the maximum item size in bytes.
    pub fn set_max_item_size<const NEW_MAX_ITEM_SIZE: usize>(self) -> Receiver<T, Codec, NEW_MAX_ITEM_SIZE> {
        Receiver(self.0.set_max_item_size())
    }

    /// The maximum item size of the remote sender.
    ///
    /// If this is larger than [max_item_size](Self::max_item_size) sending of oversized
    /// items will succeed but receiving will fail with a
    /// [MaxItemSizeExceeded error](base::RecvError::MaxItemSizeExceeded).
    pub fn remote_max_item_size(&self) -> Option<usize> {
        self.0.remote_max_item_size()
    }
}

impl<T, Codec, const MAX_ITEM_SIZE: usize> Future for Receiver<T, Codec, MAX_ITEM_SIZE>
where
    T: DeserializeOwned + Send + 'static,
    Codec: codec::Codec,
{
    type Output = Result<T, RecvError>;

    /// Receives the value transmitted by the sender.
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        match ready!(Pin::into_inner(self).0.poll_recv(cx)) {
            Ok(Some(v)) => Poll::Ready(Ok(v)),
            Ok(None) => Poll::Ready(Err(RecvError::Closed)),
            Err(err) => Poll::Ready(Err(err.into())),
        }
    }
}
