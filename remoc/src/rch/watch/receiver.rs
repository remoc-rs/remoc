use futures::{FutureExt, Stream, ready};
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    fmt,
    marker::PhantomData,
    mem,
    pin::Pin,
    task::{Context, Poll},
};
use tokio_util::sync::ReusableBoxFuture;

use super::{
    super::{
        DEFAULT_MAX_ITEM_SIZE, RemoteSendError,
        base::{self, PortDeserializer, PortSerializer},
    },
    Ref,
};
use crate::{RemoteSend, chmux, codec};

/// An error occurred during receiving over a watch channel.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RecvError {
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
            Self::RemoteReceive(err) => write!(f, "receive error: {err}"),
            Self::RemoteConnect(err) => write!(f, "connect error: {err}"),
            Self::RemoteListen(err) => write!(f, "listen error: {err}"),
        }
    }
}

impl Error for RecvError {}

impl RecvError {
    /// Returns whether the error is final, i.e. no further receive operation can succeed.
    pub fn is_final(&self) -> bool {
        match self {
            Self::RemoteReceive(err) => err.is_final(),
            Self::RemoteConnect(_) | Self::RemoteListen(_) => true,
        }
    }
}

/// An error occurred during waiting for a change on a watch channel.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ChangedError {
    /// The sender has been dropped or the connection has been lost.
    Closed,
}

impl ChangedError {
    /// True, if remote endpoint has closed the channel.
    #[deprecated = "a remoc::rch::watch::ChangedError is always due to closure"]
    pub fn is_closed(&self) -> bool {
        true
    }
}

impl fmt::Display for ChangedError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "closed"),
        }
    }
}

impl Error for ChangedError {}

/// An error occurred during wait for a value that satisfies a condition on a channel
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WaitForError {
    /// Receiving failed.
    Recv(RecvError),
    /// The sender has been dropped or the connection has been lost.
    Closed,
}

impl From<RecvError> for WaitForError {
    fn from(err: RecvError) -> Self {
        Self::Recv(err)
    }
}

impl From<ChangedError> for WaitForError {
    fn from(err: ChangedError) -> Self {
        match err {
            ChangedError::Closed => Self::Closed,
        }
    }
}

impl WaitForError {
    /// True, if remote endpoint has closed the channel.
    pub fn is_closed(&self) -> bool {
        true
    }
}

impl fmt::Display for WaitForError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Recv(err) => write!(f, "{err}"),
            Self::Closed => write!(f, "closed"),
        }
    }
}

impl Error for WaitForError {}

/// Receive values from the associated [Sender](super::Sender),
/// which may be located on a remote endpoint.
///
/// Instances are created by the [channel](super::channel) function.
///
/// This can be converted into a [Stream](futures::Stream) of values by wrapping it into
/// a [ReceiverStream].
#[derive(Clone)]
pub struct Receiver<T, Codec = codec::Default, const MAX_ITEM_SIZE: usize = DEFAULT_MAX_ITEM_SIZE> {
    rx: tokio::sync::watch::Receiver<Result<T, RecvError>>,
    remote_send_err_tx: tokio::sync::mpsc::UnboundedSender<RemoteSendError>,
    remote_max_item_size: Option<usize>,
    _codec: PhantomData<Codec>,
}

impl<T, Codec, const MAX_ITEM_SIZE: usize> fmt::Debug for Receiver<T, Codec, MAX_ITEM_SIZE> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Receiver").finish()
    }
}

/// Watch receiver in transport.
#[derive(Serialize, Deserialize)]
pub(crate) struct TransportedReceiver<T, Codec> {
    /// chmux port number.
    port: u32,
    /// Current data value.
    data: Result<T, RecvError>,
    /// Data codec.
    codec: PhantomData<Codec>,
    /// Maximum item size.
    #[serde(default)]
    max_item_size: u64,
}

impl<T, Codec> Receiver<T, Codec>
where
    T: RemoteSend + Sync + Clone,
    Codec: codec::Codec,
{
    /// Creates a receiver that forwards values from the given local watch receiver.
    ///
    /// The returned receiver may be sent to remote endpoints via channels.
    /// Any send errors that occur during forwarding are silently dropped; 
    /// use [`forward`](super::forward) if you need to observe them.
    pub fn forwarded(local_rx: tokio::sync::watch::Receiver<T>) -> Self {
        let (_fwd, rx) = super::forward(local_rx);
        rx
    }
}

impl<T, Codec, const MAX_ITEM_SIZE: usize> Receiver<T, Codec, MAX_ITEM_SIZE> {
    pub(crate) fn new(
        rx: tokio::sync::watch::Receiver<Result<T, RecvError>>,
        remote_send_err_tx: tokio::sync::mpsc::UnboundedSender<RemoteSendError>,
        remote_max_item_size: Option<usize>,
    ) -> Self {
        Self { rx, remote_send_err_tx, remote_max_item_size, _codec: PhantomData }
    }

    /// Returns a reference to the most recently received value.
    pub fn borrow(&self) -> Result<Ref<'_, T>, RecvError> {
        let ref_res = self.rx.borrow();
        match &*ref_res {
            Ok(_) => Ok(Ref(ref_res)),
            Err(err) => Err(err.clone()),
        }
    }

    /// Returns a reference to the most recently received value and mark that value as seen.
    pub fn borrow_and_update(&mut self) -> Result<Ref<'_, T>, RecvError> {
        let ref_res = self.rx.borrow_and_update();
        match &*ref_res {
            Ok(_) => Ok(Ref(ref_res)),
            Err(err) => Err(err.clone()),
        }
    }

    /// Checks if this channel contains a message that this receiver has not yet seen.
    /// The current value will not be marked as seen.
    pub fn has_changed(&self) -> Result<bool, ChangedError> {
        self.rx.has_changed().map_err(|_| ChangedError::Closed)
    }

    /// Wait for a change notification, then mark the newest value as seen.
    pub async fn changed(&mut self) -> Result<(), ChangedError> {
        self.rx.changed().await.map_err(|_| ChangedError::Closed)
    }

    /// Marks the state as changed.
    pub fn mark_changed(&mut self) {
        self.rx.mark_changed();
    }

    /// Marks the state as unchanged.
    pub fn mark_unchanged(&mut self) {
        self.rx.mark_unchanged();
    }

    /// Waits for a value that satisfies the provided condition.
    pub async fn wait_for(&mut self, mut f: impl FnMut(&T) -> bool) -> Result<Ref<'_, T>, WaitForError> {
        let res = self
            .rx
            .wait_for(move |res| match res {
                Ok(value) => f(value),
                Err(_) => true,
            })
            .await;

        match res {
            Ok(ref_res) => match &*ref_res {
                Ok(_) => Ok(Ref(ref_res)),
                Err(err) => Err(err.clone().into()),
            },
            Err(_) => Err(WaitForError::Closed),
        }
    }

    /// Maximum allowed item size in bytes when receiving items.
    pub fn max_item_size(&self) -> usize {
        MAX_ITEM_SIZE
    }

    /// Sets the maximum allowed item size in bytes when receiving items.
    pub fn set_max_item_size<const NEW_MAX_ITEM_SIZE: usize>(mut self) -> Receiver<T, Codec, NEW_MAX_ITEM_SIZE> {
        Receiver {
            rx: mem::replace(
                &mut self.rx,
                tokio::sync::watch::channel(Err(RecvError::RemoteConnect(chmux::ConnectError::ChMux))).1,
            ),
            remote_send_err_tx: self.remote_send_err_tx.clone(),
            remote_max_item_size: self.remote_max_item_size,
            _codec: PhantomData,
        }
    }

    /// The maximum item size of the remote sender.
    ///
    /// If this is larger than [max_item_size](Self::max_item_size) sending of oversized
    /// items will succeed but receiving will fail with a
    /// [MaxItemSizeExceeded error](base::RecvError::MaxItemSizeExceeded).
    pub fn remote_max_item_size(&self) -> Option<usize> {
        self.remote_max_item_size
    }
}

impl<T, Codec, const MAX_ITEM_SIZE: usize> Drop for Receiver<T, Codec, MAX_ITEM_SIZE> {
    fn drop(&mut self) {
        // empty
    }
}

impl<T, Codec, const MAX_ITEM_SIZE: usize> Serialize for Receiver<T, Codec, MAX_ITEM_SIZE>
where
    T: RemoteSend + Sync + Clone,
    Codec: codec::Codec,
{
    /// Serializes this receiver for sending over a chmux channel.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Prepare channel for takeover.
        let mut rx = self.rx.clone();
        let data = rx.borrow_and_update().clone();
        let remote_send_err_tx = self.remote_send_err_tx.clone();

        let port = PortSerializer::connect(|connect| {
            async move {
                // Establish chmux channel.
                let (raw_tx, raw_rx) = match connect.await {
                    Ok(tx_rx) => tx_rx,
                    Err(err) => {
                        let _ = remote_send_err_tx.send(RemoteSendError::Connect(err));
                        return;
                    }
                };

                super::send_impl::<T, Codec>(rx, raw_tx, raw_rx, remote_send_err_tx, MAX_ITEM_SIZE).await;
            }
            .boxed()
        })?;

        // Encode chmux port number in transport type and serialize it.
        let transported = TransportedReceiver::<T, Codec> {
            port,
            data,
            max_item_size: self.max_item_size().try_into().unwrap_or(u64::MAX),
            codec: PhantomData,
        };
        transported.serialize(serializer)
    }
}

impl<'de, T, Codec, const MAX_ITEM_SIZE: usize> Deserialize<'de> for Receiver<T, Codec, MAX_ITEM_SIZE>
where
    T: RemoteSend + Sync,
    Codec: codec::Codec,
{
    /// Deserializes the receiver after it has been received over a chmux channel.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Get chmux port number from deserialized transport type.
        let TransportedReceiver { port, data, max_item_size, .. } =
            TransportedReceiver::<T, Codec>::deserialize(deserializer)?;

        let max_item_size = usize::try_from(max_item_size).unwrap_or(usize::MAX);
        if max_item_size > MAX_ITEM_SIZE {
            tracing::debug!(
                "Watch receiver maximum item size is {MAX_ITEM_SIZE} bytes, \
                 but remote endpoint expects at least {max_item_size} bytes"
            );
        }

        // Create channels.
        let (tx, rx) = tokio::sync::watch::channel(data);
        let (remote_send_err_tx, remote_send_err_rx) = tokio::sync::mpsc::unbounded_channel();

        PortDeserializer::accept(port, |local_port, request| {
            async move {
                // Accept chmux connection request.
                let (raw_tx, raw_rx) = match request.accept_from(local_port).await {
                    Ok(tx_rx) => tx_rx,
                    Err(err) => {
                        let _ = tx.send(Err(RecvError::RemoteListen(err)));
                        return;
                    }
                };

                super::recv_impl::<T, Codec>(tx, raw_tx, raw_rx, remote_send_err_rx, None, MAX_ITEM_SIZE).await;
            }
            .boxed()
        })?;

        Ok(Self::new(rx, remote_send_err_tx, Some(max_item_size)))
    }
}

/// A wrapper around a watch [Receiver] that implements [Stream](futures::Stream).
///
/// This stream will always start by yielding the current value when it is polled,
/// regardless of whether it was the initial value or sent afterwards.
///
/// Note that intermediate values may be missed due to the nature of watch channels.
pub struct ReceiverStream<T, Codec = codec::Default, const MAX_ITEM_SIZE: usize = DEFAULT_MAX_ITEM_SIZE> {
    inner: ReusableBoxFuture<'static, (Result<(), ChangedError>, Receiver<T, Codec, MAX_ITEM_SIZE>)>,
}

impl<T, Codec, const MAX_ITEM_SIZE: usize> fmt::Debug for ReceiverStream<T, Codec, MAX_ITEM_SIZE> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ReceiverStream").finish()
    }
}

impl<T, Codec, const MAX_ITEM_SIZE: usize> ReceiverStream<T, Codec, MAX_ITEM_SIZE>
where
    T: RemoteSend + Sync,
    Codec: Send + 'static,
{
    /// Creates a new `ReceiverStream`.
    pub fn new(rx: Receiver<T, Codec, MAX_ITEM_SIZE>) -> Self {
        Self { inner: ReusableBoxFuture::new(async move { (Ok(()), rx) }) }
    }

    async fn make_future(
        mut rx: Receiver<T, Codec, MAX_ITEM_SIZE>,
    ) -> (Result<(), ChangedError>, Receiver<T, Codec, MAX_ITEM_SIZE>) {
        let result = rx.changed().await;
        (result, rx)
    }
}

impl<T, Codec, const MAX_ITEM_SIZE: usize> Stream for ReceiverStream<T, Codec, MAX_ITEM_SIZE>
where
    T: Clone + RemoteSend + Sync,
    Codec: Send + 'static,
{
    type Item = Result<T, RecvError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let (result, mut rx) = ready!(self.inner.poll(cx));
        match result {
            Ok(()) => {
                let received = rx.borrow_and_update().map(|v| v.clone());
                self.inner.set(Self::make_future(rx));
                Poll::Ready(Some(received))
            }
            Err(_) => {
                self.inner.set(Self::make_future(rx));
                Poll::Ready(None)
            }
        }
    }
}

impl<T, Codec, const MAX_ITEM_SIZE: usize> Unpin for ReceiverStream<T, Codec, MAX_ITEM_SIZE> {}

impl<T, Codec, const MAX_ITEM_SIZE: usize> From<Receiver<T, Codec, MAX_ITEM_SIZE>>
    for ReceiverStream<T, Codec, MAX_ITEM_SIZE>
where
    T: RemoteSend + Sync,
    Codec: Send + 'static,
{
    fn from(recv: Receiver<T, Codec, MAX_ITEM_SIZE>) -> Self {
        Self::new(recv)
    }
}
