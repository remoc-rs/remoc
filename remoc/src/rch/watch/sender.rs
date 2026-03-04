use futures::FutureExt;
use serde::{Deserialize, Serialize};
use std::{any::Any, error::Error, fmt, marker::PhantomData, sync::Mutex};

use super::{
    super::{
        RemoteSendError, SendErrorExt,
        base::{self, PortDeserializer, PortSerializer},
    },
    Receiver, Ref,
    receiver::RecvError,
};
use crate::{RemoteSend, chmux, codec};

/// An error occurred during sending over an mpsc channel.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SendError {
    /// The receiver was dropped or the connection failed.
    Closed,
    /// Sending to a remote endpoint failed.
    RemoteSend(base::SendErrorKind),
    /// Connecting a sent channel failed.
    RemoteConnect(chmux::ConnectError),
    /// Listening to a received channel failed.
    RemoteListen(chmux::ListenerError),
    /// Forwarding at a remote endpoint to another remote endpoint failed.
    RemoteForward,
}

impl SendError {
    /// True, if the remote endpoint closed the channel.
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Closed)
    }

    /// True, if the remote endpoint was dropped or the connection failed.
    pub fn is_disconnected(&self) -> bool {
        !matches!(self, Self::RemoteSend(base::SendErrorKind::Serialize(_)))
    }

    /// Returns whether the error is final, i.e. no further send operation can succeed.
    pub fn is_final(&self) -> bool {
        match self {
            Self::RemoteSend(err) => err.is_final(),
            Self::Closed | Self::RemoteConnect(_) | Self::RemoteListen(_) | Self::RemoteForward => true,
        }
    }

    /// Whether the error is caused by the item to be sent.
    pub fn is_item_specific(&self) -> bool {
        matches!(self, Self::RemoteSend(err) if err.is_item_specific())
    }
}

impl SendErrorExt for SendError {
    fn is_closed(&self) -> bool {
        self.is_closed()
    }

    fn is_disconnected(&self) -> bool {
        self.is_disconnected()
    }

    fn is_final(&self) -> bool {
        self.is_final()
    }

    fn is_item_specific(&self) -> bool {
        self.is_item_specific()
    }
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "channel is closed"),
            Self::RemoteSend(err) => write!(f, "send error: {err}"),
            Self::RemoteConnect(err) => write!(f, "connect error: {err}"),
            Self::RemoteListen(err) => write!(f, "listen error: {err}"),
            Self::RemoteForward => write!(f, "forwarding error"),
        }
    }
}

impl Error for SendError {}

impl From<RemoteSendError> for SendError {
    fn from(err: RemoteSendError) -> Self {
        match err {
            RemoteSendError::Send(err) => Self::RemoteSend(err),
            RemoteSendError::Connect(err) => Self::RemoteConnect(err),
            RemoteSendError::Listen(err) => Self::RemoteListen(err),
            RemoteSendError::Forward => Self::RemoteForward,
            RemoteSendError::Closed => Self::Closed,
        }
    }
}

/// Send values to the associated [Receiver](super::Receiver), which may be located on a remote endpoint.
///
/// Instances are created by the [channel](super::channel) function.
pub struct Sender<T, Codec = codec::Default> {
    /// Type-erased inner to avoid monomorphizing Drop per (T, Codec).
    /// Always contains `Box<SenderInner<T, Codec>>`.
    inner: Option<Box<dyn Any + Send + Sync>>,
    #[allow(clippy::type_complexity)]
    successor_tx: Mutex<Option<tokio::sync::oneshot::Sender<Box<dyn Any + Send + Sync>>>>,
    _phantom: PhantomData<(T, Codec)>,
}

impl<T, Codec> fmt::Debug for Sender<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Sender").finish()
    }
}

pub(crate) struct SenderInner<T, Codec> {
    tx: tokio::sync::watch::Sender<Result<T, RecvError>>,
    remote_send_err_tx: tokio::sync::mpsc::UnboundedSender<RemoteSendError>,
    remote_send_err_rx: Mutex<tokio::sync::mpsc::UnboundedReceiver<RemoteSendError>>,
    current_err: Mutex<Option<RemoteSendError>>,
    max_item_size: usize,
    _codec: PhantomData<Codec>,
}

/// Watch sender in transport.
#[derive(Serialize, Deserialize)]
pub(crate) struct TransportedSender<T, Codec> {
    /// chmux port number.
    port: u32,
    /// Current data value.
    data: Result<T, RecvError>,
    /// Data codec.
    codec: PhantomData<Codec>,
    /// Maximum item size in bytes.
    #[serde(default = "default_max_item_size")]
    max_item_size: u64,
}

const fn default_max_item_size() -> u64 {
    u64::MAX
}

impl<T, Codec> Sender<T, Codec>
where
    T: Send + Sync + 'static,
    Codec: Send + Sync + 'static,
{
    /// Downcast helper: immutable reference to the inner.
    fn inner_ref(&self) -> &SenderInner<T, Codec> {
        self.inner.as_ref().unwrap().downcast_ref::<SenderInner<T, Codec>>().unwrap()
    }

    /// Downcast helper: mutable reference to the inner.
    fn inner_mut(&mut self) -> &mut SenderInner<T, Codec> {
        self.inner.as_mut().unwrap().downcast_mut::<SenderInner<T, Codec>>().unwrap()
    }

    /// Creates a new sender.
    pub(crate) fn new(
        tx: tokio::sync::watch::Sender<Result<T, RecvError>>,
        remote_send_err_tx: tokio::sync::mpsc::UnboundedSender<RemoteSendError>,
        remote_send_err_rx: tokio::sync::mpsc::UnboundedReceiver<RemoteSendError>, max_item_size: usize,
    ) -> Self {
        let inner = SenderInner {
            tx,
            remote_send_err_tx,
            remote_send_err_rx: Mutex::new(remote_send_err_rx),
            current_err: Mutex::new(None),
            max_item_size,
            _codec: PhantomData::<Codec>,
        };
        Self { inner: Some(Box::new(inner)), successor_tx: Mutex::new(None), _phantom: PhantomData::<(T, Codec)> }
    }

    /// Sends a value over this channel, notifying all receivers.
    ///
    /// This method fails if all receivers have been dropped or become disconnected.
    ///
    /// # Error reporting
    /// Sending and error reporting are done asynchronously.
    /// Thus, the reporting of an error may be delayed and this function may
    /// return errors caused by previous invocations.
    pub fn send(&self, value: T) -> Result<(), SendError> {
        match self.inner_ref().tx.send(Ok(value)) {
            Ok(()) => Ok(()),
            Err(_) => match self.error() {
                Some(err) => Err(err),
                None => Err(SendError::Closed),
            },
        }
    }

    /// Modifies the watched value and notifies all receivers.
    ///
    /// This method never fails, even if all receivers have been dropped or become
    /// disconnected.
    ///
    /// # Panics
    /// This method panics if calling `func` results in a panic.
    pub fn send_modify<F>(&self, func: F)
    where
        F: FnOnce(&mut T),
    {
        self.inner_ref().tx.send_modify(move |v| func(v.as_mut().unwrap()))
    }

    /// Sends a new value via the channel, notifying all receivers and returning the
    /// previous value in the channel.
    ///
    /// This method never fails, even if all receivers have been dropped or become
    /// disconnected.
    pub fn send_replace(&self, value: T) -> T {
        self.inner_ref().tx.send_replace(Ok(value)).unwrap()
    }

    /// Returns a reference to the most recently sent value.
    pub fn borrow(&self) -> Ref<'_, T> {
        Ref(self.inner_ref().tx.borrow())
    }

    /// Completes when all receivers have been dropped or the connection failed.
    pub async fn closed(&self) {
        self.inner_ref().tx.closed().await
    }

    /// Returns whether all receivers have been dropped or the connection failed.
    pub fn is_closed(&self) -> bool {
        self.inner_ref().tx.is_closed()
    }

    /// Creates a new receiver subscribed to this sender.
    pub fn subscribe(&self) -> Receiver<T, Codec> {
        let inner = self.inner_ref();
        Receiver::new(inner.tx.subscribe(), inner.remote_send_err_tx.clone(), None)
    }

    fn update_error(&self) {
        let inner = self.inner_ref();
        let mut current_err = inner.current_err.lock().unwrap();
        if current_err.is_some() {
            return;
        }

        let mut remote_send_err_rx = inner.remote_send_err_rx.lock().unwrap();
        if let Ok(err) = remote_send_err_rx.try_recv() {
            *current_err = Some(err);
        }
    }

    /// Returns the error that occurred during sending to a remote endpoint, if any.
    ///
    /// # Error reporting
    /// Sending and error reporting are done asynchronously.
    /// Thus, the reporting of an error may be delayed.
    pub fn error(&self) -> Option<SendError> {
        self.update_error();

        let inner = self.inner_ref();
        let current_err = inner.current_err.lock().unwrap();
        current_err.clone().map(|err| err.into())
    }

    /// Clears the error that occurred during sending to a remote endpoint.
    pub fn clear_error(&mut self) {
        self.update_error();

        let inner = self.inner_ref();
        let mut current_err = inner.current_err.lock().unwrap();
        *current_err = None;
    }

    /// Checks that no item-specific send error has occurred.
    ///
    /// This method clears non-item-specific errors present on the channel.
    ///
    /// # Error reporting
    /// Sending and error reporting are done asynchronously.
    /// Thus, the reporting of an error may be delayed.
    ///
    /// To verify that no item-specific send error has occurred during the lifetime of
    /// the channel, call this method after the channel is closed, i.e.
    /// [`closed`](Self::closed) has returned or [`is_closed`](Self::is_closed) is
    /// `true`.
    pub fn check(&mut self) -> Result<(), SendError> {
        while let Some(err) = self.error() {
            if err.is_item_specific() {
                return Err(err);
            }
            self.clear_error();
        }
        Ok(())
    }

    /// Maximum allowed item size in bytes.
    pub fn max_item_size(&self) -> usize {
        self.inner_ref().max_item_size
    }

    /// Sets the maximum allowed item size in bytes.
    pub fn set_max_item_size(&mut self, max_item_size: usize) {
        self.inner_mut().max_item_size = max_item_size;
    }
}

impl<T, Codec> Drop for Sender<T, Codec> {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            if let Some(successor_tx) = self.successor_tx.lock().unwrap().take() {
                let _ = successor_tx.send(inner);
            }
        }
    }
}

impl<T, Codec> Serialize for Sender<T, Codec>
where
    T: RemoteSend + Sync + Clone,
    Codec: codec::Codec,
{
    /// Serializes this sender for sending over a chmux channel.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let max_item_size = self.max_item_size();

        // Prepare channel for takeover using type-erased oneshot.
        let (successor_tx, successor_rx) = tokio::sync::oneshot::channel::<Box<dyn Any + Send + Sync>>();
        *self.successor_tx.lock().unwrap() = Some(successor_tx);

        let port = PortSerializer::connect(move |connect| {
            async move {
                // Sender has been dropped after sending, so we receive its channels.
                let SenderInner { tx, remote_send_err_rx, current_err, .. } = match successor_rx.await {
                    Ok(inner) => *inner
                        .downcast::<SenderInner<T, Codec>>()
                        .expect("type mismatch in watch Sender successor"),
                    Err(_) => return,
                };
                let remote_send_err_rx = remote_send_err_rx.into_inner().unwrap();
                let current_err = current_err.into_inner().unwrap();

                // Establish chmux channel.
                let (raw_tx, raw_rx) = match connect.await {
                    Ok(tx_rx) => tx_rx,
                    Err(err) => {
                        let _ = tx.send(Err(RecvError::RemoteConnect(err)));
                        return;
                    }
                };

                super::recv_impl::<T, Codec>(tx, raw_tx, raw_rx, remote_send_err_rx, current_err, max_item_size)
                    .await;
            }
            .boxed()
        })?;

        // Encode chmux port number in transport type and serialize it.
        let data = self.inner_ref().tx.borrow().clone();
        let transported = TransportedSender::<T, Codec> {
            port,
            data,
            max_item_size: max_item_size.try_into().unwrap_or(u64::MAX),
            codec: PhantomData,
        };
        transported.serialize(serializer)
    }
}

impl<'de, T, Codec> Deserialize<'de> for Sender<T, Codec>
where
    T: RemoteSend + Sync + Clone,
    Codec: codec::Codec,
{
    /// Deserializes this sender after it has been received over a chmux channel.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Get chmux port number from deserialized transport type.
        let TransportedSender { port, data, max_item_size, .. } =
            TransportedSender::<T, Codec>::deserialize(deserializer)?;
        let max_item_size = usize::try_from(max_item_size).unwrap_or(usize::MAX);
        if data.is_err() {
            return Err(serde::de::Error::custom("received watch data with error"));
        }

        // Create internal communication channels.
        let (tx, rx) = tokio::sync::watch::channel(data);
        let (remote_send_err_tx, remote_send_err_rx) = tokio::sync::mpsc::unbounded_channel();
        let remote_send_err_tx2 = remote_send_err_tx.clone();

        // Accept chmux port request.
        PortDeserializer::accept(port, move |local_port, request| {
            async move {
                // Accept chmux connection request.
                let (raw_tx, raw_rx) = match request.accept_from(local_port).await {
                    Ok(tx_rx) => tx_rx,
                    Err(err) => {
                        let _ = remote_send_err_tx.send(RemoteSendError::Listen(err));
                        return;
                    }
                };

                super::send_impl::<T, Codec>(rx, raw_tx, raw_rx, remote_send_err_tx, max_item_size).await;
            }
            .boxed()
        })?;

        Ok(Self::new(tx, remote_send_err_tx2, remote_send_err_rx, max_item_size))
    }
}
