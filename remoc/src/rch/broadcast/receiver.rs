use futures::{Stream, ready};
use std::{
    convert::TryFrom,
    error::Error,
    fmt,
    pin::Pin,
    task::{Context, Poll},
};
use tokio_util::sync::ReusableBoxFuture;

use super::{
    super::{DEFAULT_BUFFER, DEFAULT_MAX_ITEM_SIZE, base, mpsc},
    BroadcastMsg,
};
use crate::{RemoteSend, chmux, codec};

/// An error occurred during receiving over a broadcast channel.
#[derive(Clone, Debug)]
pub enum RecvError {
    /// There are no more active senders implying no further messages will ever be sent.
    Closed,
    /// The receiver lagged too far behind.
    ///
    /// Attempting to receive again will return the oldest message still retained by the channel.
    Lagged,
    /// Receiving from a remote endpoint failed.
    Receive(base::RecvError),
    /// Connecting a sent channel failed.
    Connect(chmux::ConnectError),
    /// Listening for a connection from a received channel failed.
    Listen(chmux::ListenerError),
    /// Remote error.
    ///
    /// The error occurred at the endpoint the value was received from.
    /// [`None`] if that endpoint reported an error this one does not know.
    Remote(Option<Box<RecvError>>),
}

crate::versioned::compact::impl_enum! {
    RecvError,
    recover = RecvError::Remote(None),
    variants {
        Closed => "_0",
        Lagged => "_1",
        Receive(err: base::RecvError) => "_2",
        Connect(err: chmux::ConnectError) => "_3",
        Listen(err: chmux::ListenerError) => "_4",
        Remote(err: Option<Box<RecvError>>) => "_50",
    }
}

impl RecvError {
    /// True, if all senders have been dropped.
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Closed)
    }

    /// True, if the receiver has lagged behind and messages have been lost.
    pub fn is_lagged(&self) -> bool {
        matches!(self, Self::Lagged)
    }

    /// Returns whether the connection was closed or failed.
    pub fn is_disconnected(&self) -> bool {
        match self {
            Self::Receive(err) => err.is_disconnected(),
            Self::Closed | Self::Connect(_) | Self::Listen(_) => true,
            Self::Lagged => false,
            Self::Remote(Some(err)) => err.is_disconnected(),
            Self::Remote(None) => false,
        }
    }
}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "channel closed"),
            Self::Lagged => write!(f, "receiver lagged behind"),
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
            TryRecvError::Lagged => Ok(Self::Lagged),
            TryRecvError::Receive(err) => Ok(Self::Receive(err)),
            TryRecvError::Connect(err) => Ok(Self::Connect(err)),
            TryRecvError::Listen(err) => Ok(Self::Listen(err)),
            TryRecvError::Remote(err) => Ok(Self::Remote(err)),
        }
    }
}

impl Error for RecvError {}

/// An error occurred during trying to receive over a broadcast channel.
#[derive(Clone, Debug)]
pub enum TryRecvError {
    /// The channel is currently empty. There are still active sender, so data may yet become available.
    Empty,
    /// There are no more active senders implying no further messages will ever be sent.
    Closed,
    /// The receiver lagged too far behind.
    ///
    /// Attempting to receive again will return the oldest message still retained by the channel.
    Lagged,
    /// Receiving from a remote endpoint failed.
    Receive(base::RecvError),
    /// Connecting a sent channel failed.
    Connect(chmux::ConnectError),
    /// Listening for a connection from a received channel failed.
    Listen(chmux::ListenerError),
    /// Remote error.
    ///
    /// The error occurred at the endpoint the value was received from.
    /// [`None`] if that endpoint reported an error this one does not know.
    Remote(Option<Box<RecvError>>),
}

crate::versioned::compact::impl_enum! {
    TryRecvError,
    recover = TryRecvError::Remote(None),
    variants {
        Empty => "_0",
        Closed => "_1",
        Lagged => "_2",
        Receive(err: base::RecvError) => "_3",
        Connect(err: chmux::ConnectError) => "_4",
        Listen(err: chmux::ListenerError) => "_5",
        Remote(err: Option<Box<RecvError>>) => "_50",
    }
}

impl TryRecvError {
    /// True, if no value is currently present.
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    /// True, if all senders have been dropped.
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Closed)
    }

    /// True, if the receiver has lagged behind and messages have been lost.
    pub fn is_lagged(&self) -> bool {
        matches!(self, Self::Lagged)
    }

    /// Returns whether the connection was closed or failed.
    pub fn is_disconnected(&self) -> bool {
        match self {
            Self::Receive(err) => err.is_disconnected(),
            Self::Closed | Self::Connect(_) | Self::Listen(_) => true,
            Self::Empty | Self::Lagged => false,
            Self::Remote(Some(err)) => err.is_disconnected(),
            Self::Remote(None) => false,
        }
    }
}

impl fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "channel empty"),
            Self::Closed => write!(f, "channel closed"),
            Self::Lagged => write!(f, "receiver lagged behind"),
            Self::Receive(err) => write!(f, "receive error: {err}"),
            Self::Connect(err) => write!(f, "connect error: {err}"),
            Self::Listen(err) => write!(f, "listen error: {err}"),
            Self::Remote(Some(err)) => write!(f, "remote {err}"),
            Self::Remote(None) => write!(f, "unknown remote error"),
        }
    }
}

impl From<mpsc::RecvError> for TryRecvError {
    fn from(err: mpsc::RecvError) -> Self {
        match err {
            mpsc::RecvError::Receive(err) => Self::Receive(err),
            mpsc::RecvError::Connect(err) => Self::Connect(err),
            mpsc::RecvError::Listen(err) => Self::Listen(err),
            mpsc::RecvError::Remote(err) => Self::Remote(err.map(|err| Box::new(RecvError::from(*err)))),
        }
    }
}

impl From<mpsc::TryRecvError> for TryRecvError {
    fn from(err: mpsc::TryRecvError) -> Self {
        match err {
            mpsc::TryRecvError::Receive(err) => Self::Receive(err),
            mpsc::TryRecvError::Connect(err) => Self::Connect(err),
            mpsc::TryRecvError::Listen(err) => Self::Listen(err),
            mpsc::TryRecvError::Closed => Self::Closed,
            mpsc::TryRecvError::Empty => Self::Empty,
            mpsc::TryRecvError::Remote(err) => Self::Remote(err.map(|err| Box::new(RecvError::from(*err)))),
        }
    }
}

impl From<RecvError> for TryRecvError {
    fn from(err: RecvError) -> Self {
        match err {
            RecvError::Closed => Self::Closed,
            RecvError::Lagged => Self::Lagged,
            RecvError::Receive(err) => Self::Receive(err),
            RecvError::Connect(err) => Self::Connect(err),
            RecvError::Listen(err) => Self::Listen(err),
            RecvError::Remote(err) => Self::Remote(err),
        }
    }
}

impl Error for TryRecvError {}

/// Receiving-half of the broadcast channel.
///
/// Can be sent over a remote channel.
///
/// This can be converted into a [Stream](futures::Stream) of values by wrapping it into
/// a [ReceiverStream].
pub struct Receiver<
    T,
    Codec = codec::Default,
    const BUFFER: usize = DEFAULT_BUFFER,
    const MAX_ITEM_SIZE: usize = DEFAULT_MAX_ITEM_SIZE,
> {
    rx: mpsc::Receiver<BroadcastMsg<T>, Codec, BUFFER, MAX_ITEM_SIZE>,
}

crate::versioned::compact::impl_struct! {
    Receiver<T, Codec; const BUFFER: usize, const MAX_ITEM_SIZE: usize>,
    fields {
        rx: mpsc::Receiver<BroadcastMsg<T>, Codec, BUFFER, MAX_ITEM_SIZE> => "_0",
    }
    where T: RemoteSend, Codec: codec::Codec
}

impl<T, Codec, const BUFFER: usize, const MAX_ITEM_SIZE: usize> fmt::Debug
    for Receiver<T, Codec, BUFFER, MAX_ITEM_SIZE>
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Receiver").finish()
    }
}

impl<T, Codec, const BUFFER: usize, const MAX_ITEM_SIZE: usize> Receiver<T, Codec, BUFFER, MAX_ITEM_SIZE>
where
    T: RemoteSend,
    Codec: codec::Codec,
{
    pub(crate) fn new(rx: mpsc::Receiver<BroadcastMsg<T>, Codec, BUFFER, MAX_ITEM_SIZE>) -> Self {
        Self { rx }
    }

    /// Receives the next value for this receiver.
    pub async fn recv(&mut self) -> Result<T, RecvError> {
        match self.rx.recv().await {
            Ok(Some(BroadcastMsg::Value(value))) => Ok(value),
            Ok(Some(BroadcastMsg::Lagged)) => Err(RecvError::Lagged),
            Ok(None) => Err(RecvError::Closed),
            Err(err) => Err(err.into()),
        }
    }

    /// Attempts to return a pending value on this receiver without awaiting.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        match self.rx.try_recv() {
            Ok(BroadcastMsg::Value(value)) => Ok(value),
            Ok(BroadcastMsg::Lagged) => Err(TryRecvError::Lagged),
            Err(err) => Err(err.into()),
        }
    }

    /// The maximum item size in bytes.
    pub fn max_item_size(&self) -> usize {
        self.rx.max_item_size()
    }

    /// Sets the maximum item size in bytes.
    pub fn set_max_item_size<const NEW_MAX_ITEM_SIZE: usize>(
        self,
    ) -> Receiver<T, Codec, BUFFER, NEW_MAX_ITEM_SIZE> {
        Receiver { rx: self.rx.set_max_item_size() }
    }

    /// The maximum item size of the remote sender.
    ///
    /// If this is larger than [max_item_size](Self::max_item_size) sending of oversized
    /// items will succeed but receiving will fail with a
    /// [MaxItemSizeExceeded error](base::RecvError::MaxItemSizeExceeded).
    pub fn remote_max_item_size(&self) -> Option<usize> {
        self.rx.remote_max_item_size()
    }
}

/// An error occurred during receiving over a broadcast channel receiver stream.
#[derive(Clone, Debug)]
pub enum StreamError {
    /// The receiver stream lagged too far behind.
    ///
    /// The next value will be the oldest message still retained by the channel.
    Lagged,
    /// Receiving from a remote endpoint failed.
    Receive(base::RecvError),
    /// Connecting a sent channel failed.
    Connect(chmux::ConnectError),
    /// Listening for a connection from a received channel failed.
    Listen(chmux::ListenerError),
    /// Remote error.
    ///
    /// The error occurred at the endpoint the value was received from.
    /// [`None`] if that endpoint reported an error this one does not know.
    Remote(Option<Box<RecvError>>),
}

crate::versioned::compact::impl_enum! {
    StreamError,
    recover = StreamError::Remote(None),
    variants {
        Lagged => "_0",
        Receive(err: base::RecvError) => "_1",
        Connect(err: chmux::ConnectError) => "_2",
        Listen(err: chmux::ListenerError) => "_3",
        Remote(err: Option<Box<RecvError>>) => "_50",
    }
}

impl StreamError {
    /// True, if the receiver has lagged behind and messages have been lost.
    pub fn is_lagged(&self) -> bool {
        matches!(self, Self::Lagged)
    }

    /// Returns whether the connection failed.
    pub fn is_disconnected(&self) -> bool {
        match self {
            Self::Receive(err) => err.is_disconnected(),
            Self::Connect(_) | Self::Listen(_) => true,
            Self::Lagged => false,
            Self::Remote(Some(err)) => err.is_disconnected(),
            Self::Remote(None) => false,
        }
    }
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Lagged => write!(f, "receiver lagged behind"),
            Self::Receive(err) => write!(f, "receive error: {err}"),
            Self::Connect(err) => write!(f, "connect error: {err}"),
            Self::Listen(err) => write!(f, "listen error: {err}"),
            Self::Remote(Some(err)) => write!(f, "remote {err}"),
            Self::Remote(None) => write!(f, "unknown remote error"),
        }
    }
}

impl TryFrom<RecvError> for StreamError {
    type Error = RecvError;
    fn try_from(err: RecvError) -> Result<Self, Self::Error> {
        match err {
            RecvError::Lagged => Ok(Self::Lagged),
            RecvError::Receive(err) => Ok(Self::Receive(err)),
            RecvError::Connect(err) => Ok(Self::Connect(err)),
            RecvError::Listen(err) => Ok(Self::Listen(err)),
            RecvError::Remote(err) => Ok(Self::Remote(err)),
            other => Err(other),
        }
    }
}

impl Error for StreamError {}

/// A wrapper around a broadcast [Receiver] that implements [Stream](futures::Stream).
pub struct ReceiverStream<T, Codec = codec::Default, const BUFFER: usize = DEFAULT_BUFFER> {
    #[allow(clippy::type_complexity)]
    inner: ReusableBoxFuture<'static, (Result<T, RecvError>, Receiver<T, Codec, BUFFER>)>,
}

impl<T, Codec> fmt::Debug for ReceiverStream<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ReceiverStream").finish()
    }
}

impl<T, Codec, const BUFFER: usize> ReceiverStream<T, Codec, BUFFER>
where
    T: RemoteSend + Sync,
    Codec: codec::Codec,
{
    /// Creates a new `ReceiverStream`.
    pub fn new(rx: Receiver<T, Codec, BUFFER>) -> Self {
        Self { inner: ReusableBoxFuture::new(Self::make_future(rx)) }
    }

    async fn make_future(
        mut rx: Receiver<T, Codec, BUFFER>,
    ) -> (Result<T, RecvError>, Receiver<T, Codec, BUFFER>) {
        let result = rx.recv().await;
        (result, rx)
    }
}

impl<T: Clone, Codec, const BUFFER: usize> Stream for ReceiverStream<T, Codec, BUFFER>
where
    T: RemoteSend + Sync,
    Codec: codec::Codec,
{
    type Item = Result<T, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let (result, rx) = ready!(self.inner.poll(cx));
        self.inner.set(Self::make_future(rx));
        match result {
            Ok(value) => Poll::Ready(Some(Ok(value))),
            Err(RecvError::Closed) => Poll::Ready(None),
            Err(err) => Poll::Ready(Some(Err(StreamError::try_from(err).unwrap()))),
        }
    }
}

impl<T, Codec, const BUFFER: usize> Unpin for ReceiverStream<T, Codec, BUFFER> {}

impl<T, Codec, const BUFFER: usize> From<Receiver<T, Codec, BUFFER>> for ReceiverStream<T, Codec, BUFFER>
where
    T: RemoteSend + Sync,
    Codec: codec::Codec,
{
    fn from(recv: Receiver<T, Codec, BUFFER>) -> Self {
        Self::new(recv)
    }
}
