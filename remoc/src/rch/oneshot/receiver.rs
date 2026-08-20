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

/// An error returned while receiving from a one-shot channel.
#[derive(Clone, Debug)]
pub enum RecvError {
    /// The sender was dropped without sending a value.
    Closed,
    /// Receiving or decoding the value failed; see [`base::RecvError`].
    Receive(base::RecvError),
    /// Opening a channel contained in the value failed; see [`chmux::ConnectError`].
    Connect(chmux::ConnectError),
    /// Preparing a channel contained in the value failed; see [`chmux::ListenerError`].
    Listen(chmux::ListenerError),
    /// A failure was reported by an endpoint forwarding this channel.
    ///
    /// The nested error is [`None`] when that endpoint reported a newer error
    /// variant that this version of Remoc does not recognize.
    Remote(Option<Box<RecvError>>),
}

crate::versioned::compact::impl_enum! {
    RecvError,
    recover = RecvError::Remote(None),
    variants {
        Closed => "_0",
        Receive(err: base::RecvError) => "_1",
        Connect(err: chmux::ConnectError) => "_2",
        Listen(err: chmux::ListenerError) => "_3",
        Remote(err: Option<Box<RecvError>>) => "_50",
    }
}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "channel is closed"),
            Self::Receive(err) => write!(f, "receive error: {err}"),
            Self::Connect(err) => write!(f, "connect error: {err}"),
            Self::Listen(err) => write!(f, "listen error: {err}"),
            Self::Remote(Some(err)) => write!(f, "remote {err}"),
            Self::Remote(None) => write!(f, "unknown remote error"),
        }
    }
}

impl From<mpsc::RecvError> for RecvError {
    fn from(err: mpsc::RecvError) -> Self {
        match err {
            mpsc::RecvError::Receive(err) => Self::Receive(err),
            mpsc::RecvError::Connect(err) => Self::Connect(err),
            mpsc::RecvError::Listen(err) => Self::Listen(err),
            mpsc::RecvError::Remote(err) => Self::Remote(err.map(|err| Box::new(Self::from(*err)))),
        }
    }
}

impl TryFrom<TryRecvError> for RecvError {
    type Error = TryRecvError;

    fn try_from(err: TryRecvError) -> Result<Self, Self::Error> {
        match err {
            TryRecvError::Empty => Err(TryRecvError::Empty),
            TryRecvError::Closed => Ok(Self::Closed),
            TryRecvError::Receive(err) => Ok(Self::Receive(err)),
            TryRecvError::Connect(err) => Ok(Self::Connect(err)),
            TryRecvError::Listen(err) => Ok(Self::Listen(err)),
            TryRecvError::Remote(err) => Ok(Self::Remote(err)),
        }
    }
}

impl Error for RecvError {}

impl RecvError {
    /// Returns whether the connection was rejected or failed.
    pub fn is_disconnected(&self) -> bool {
        match self {
            Self::Receive(err) => err.is_disconnected(),
            Self::Closed | Self::Connect(_) | Self::Listen(_) => true,
            Self::Remote(Some(err)) => err.is_disconnected(),
            Self::Remote(None) => false,
        }
    }
}

/// An error returned by [`Receiver::try_recv`].
#[derive(Clone, Debug)]
pub enum TryRecvError {
    /// No value has been received yet.
    Empty,
    /// The sender was dropped without sending a value.
    Closed,
    /// Receiving or decoding the value failed; see [`base::RecvError`].
    Receive(base::RecvError),
    /// Opening a channel contained in the value failed; see [`chmux::ConnectError`].
    Connect(chmux::ConnectError),
    /// Preparing a channel contained in the value failed; see [`chmux::ListenerError`].
    Listen(chmux::ListenerError),
    /// A failure was reported by an endpoint forwarding this channel.
    ///
    /// The nested error is [`None`] when that endpoint reported a newer error
    /// variant that this version of Remoc does not recognize.
    Remote(Option<Box<RecvError>>),
}

crate::versioned::compact::impl_enum! {
    TryRecvError,
    recover = TryRecvError::Remote(None),
    variants {
        Empty => "_0",
        Closed => "_1",
        Receive(err: base::RecvError) => "_2",
        Connect(err: chmux::ConnectError) => "_3",
        Listen(err: chmux::ListenerError) => "_4",
        Remote(err: Option<Box<RecvError>>) => "_50",
    }
}

impl fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "channel is empty"),
            Self::Closed => write!(f, "channel is closed"),
            Self::Receive(err) => write!(f, "receive error: {err}"),
            Self::Connect(err) => write!(f, "connect error: {err}"),
            Self::Listen(err) => write!(f, "listen error: {err}"),
            Self::Remote(Some(err)) => write!(f, "remote {err}"),
            Self::Remote(None) => write!(f, "unknown remote error"),
        }
    }
}

impl From<mpsc::TryRecvError> for TryRecvError {
    fn from(err: mpsc::TryRecvError) -> Self {
        match err {
            mpsc::TryRecvError::Empty => Self::Empty,
            mpsc::TryRecvError::Closed => Self::Closed,
            mpsc::TryRecvError::Receive(err) => Self::Receive(err),
            mpsc::TryRecvError::Connect(err) => Self::Connect(err),
            mpsc::TryRecvError::Listen(err) => Self::Listen(err),
            mpsc::TryRecvError::Remote(err) => Self::Remote(err.map(|err| Box::new(RecvError::from(*err)))),
        }
    }
}

impl From<RecvError> for TryRecvError {
    fn from(err: RecvError) -> Self {
        match err {
            RecvError::Closed => Self::Closed,
            RecvError::Receive(err) => Self::Receive(err),
            RecvError::Connect(err) => Self::Connect(err),
            RecvError::Listen(err) => Self::Listen(err),
            RecvError::Remote(err) => Self::Remote(err),
        }
    }
}

impl Error for TryRecvError {}

/// The receiving half of a one-shot channel.
///
/// Await this future to receive the value. Dropping it prevents the sender from
/// successfully transferring a value.
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
    /// Closes the channel without receiving a value.
    ///
    /// Any subsequent send fails. A value that has already been received can still
    /// be obtained with [`try_recv`](Self::try_recv) or by awaiting this receiver.
    pub fn close(&mut self) {
        self.0.close()
    }

    /// Attempts to receive the value without waiting.
    ///
    /// Returns [`TryRecvError::Empty`] if the sender has not sent a value yet.
    /// Once this returns `Ok(_)`, the channel is consumed; if the sender closed or
    /// transfer failed before a value arrived, an error is returned instead.
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
