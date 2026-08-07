use futures::FutureExt;
use serde::{Deserialize, Serialize};
use std::{error::Error, fmt, marker::PhantomData, sync::Mutex, time::Duration};

use super::{
    super::{
        RemoteSendError, SendErrorExt,
        base::{self, PortDeserializer, PortSerializer},
    },
    RateLimitReceiver, RateLimitSender, Receiver, Ref, TransferStrategy, default_max_item_size,
    default_rate_limit, rate_limit_channel,
    receiver::RecvError,
};
use crate::{
    RemoteSend, chmux,
    codec::{self, ErasedDeserializer, ErasedSerializer},
};

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
    pub(super) inner: Option<SenderInner<T, Codec>>,
    successor_tx: Mutex<Option<tokio::sync::oneshot::Sender<SenderInner<T, Codec>>>>,
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
    pub(super) sender_rate_limit_tx: tokio::sync::watch::Sender<Duration>,
    sender_rate_limit_rx: tokio::sync::watch::Receiver<Duration>,
    receiver_rate_limit_tx: RateLimitSender,
    receiver_rate_limit_rx: RateLimitReceiver,
    pub(super) transfer_strategy: TransferStrategy,
    _codec: PhantomData<Codec>,
}

/// Watch sender in transport.
#[derive(Serialize, Deserialize)]
pub(crate) struct TransportedSender<T> {
    /// chmux port number.
    port: u32,
    /// Current data value.
    data: Result<T, RecvError>,
    /// Data codec.
    #[serde(default)]
    codec: PhantomData<()>,
    /// Maximum item size in bytes.
    #[serde(default = "default_max_item_size")]
    max_item_size: u64,
    /// Minimum delay between sending value updates.
    #[serde(default = "default_rate_limit")]
    sender_rate_limit: Duration,
    /// Minimum delay between receiving value updates.
    #[serde(default = "default_rate_limit")]
    receiver_rate_limit: Duration,
    /// Transfer strategy.
    #[serde(default)]
    transfer_strategy: TransferStrategy,
}

impl<T, Codec> Sender<T, Codec>
where
    T: Send + 'static,
{
    /// Creates a new sender.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        tx: tokio::sync::watch::Sender<Result<T, RecvError>>,
        remote_send_err_tx: tokio::sync::mpsc::UnboundedSender<RemoteSendError>,
        remote_send_err_rx: tokio::sync::mpsc::UnboundedReceiver<RemoteSendError>, max_item_size: usize,
        sender_rate_limit_tx: tokio::sync::watch::Sender<Duration>,
        sender_rate_limit_rx: tokio::sync::watch::Receiver<Duration>, receiver_rate_limit_tx: RateLimitSender,
        receiver_rate_limit_rx: RateLimitReceiver, transfer_strategy: TransferStrategy,
    ) -> Self {
        let inner = SenderInner {
            tx,
            remote_send_err_tx,
            remote_send_err_rx: Mutex::new(remote_send_err_rx),
            current_err: Mutex::new(None),
            max_item_size,
            sender_rate_limit_tx,
            sender_rate_limit_rx,
            receiver_rate_limit_tx,
            receiver_rate_limit_rx,
            transfer_strategy,
            _codec: PhantomData,
        };
        Self { inner: Some(inner), successor_tx: Mutex::new(None) }
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
        match self.inner.as_ref().unwrap().tx.send(Ok(value)) {
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
        self.inner.as_ref().unwrap().tx.send_modify(move |v| func(v.as_mut().unwrap()))
    }

    /// Modifies the watched value conditionally in-place, notifying all receivers
    /// only if the closure returns `true`.
    ///
    /// This method never fails, even if all receivers have been dropped or become
    /// disconnected.
    ///
    /// # Local vs. remote consistency
    /// If `func` mutates the value but returns `false`, the mutation is applied
    /// locally (and visible via [`borrow`](Self::borrow)) but is not forwarded to
    /// remote receivers. Local and remote observations of the value can therefore
    /// become inconsistent until the next notifying send.
    ///
    /// # Panics
    /// This method panics if calling `func` results in a panic.
    pub fn send_if_modified<F>(&self, func: F) -> bool
    where
        F: FnOnce(&mut T) -> bool,
    {
        self.inner.as_ref().unwrap().tx.send_if_modified(move |v| func(v.as_mut().unwrap()))
    }

    /// Replaces the watched value and notifies all receivers only if the new value
    /// differs from the current value.
    ///
    /// Returns `true` if the value was updated, i.e. `value` was not equal to the
    /// previously watched value.
    ///
    /// This method never fails, even if all receivers have been dropped or become
    /// disconnected.
    pub fn send_if_different(&self, value: T) -> bool
    where
        T: PartialEq,
    {
        self.send_if_modified(move |v| {
            if *v == value {
                false
            } else {
                *v = value;
                true
            }
        })
    }

    /// Sends a new value via the channel, notifying all receivers and returning the
    /// previous value in the channel.
    ///
    /// This method never fails, even if all receivers have been dropped or become
    /// disconnected.
    pub fn send_replace(&self, value: T) -> T {
        self.inner.as_ref().unwrap().tx.send_replace(Ok(value)).unwrap()
    }

    /// Returns a reference to the most recently sent value.
    pub fn borrow(&self) -> Ref<'_, T> {
        Ref(self.inner.as_ref().unwrap().tx.borrow())
    }

    /// Completes when all receivers have been dropped or the connection failed.
    pub async fn closed(&self) {
        self.inner.as_ref().unwrap().tx.closed().await
    }

    /// Returns whether all receivers have been dropped or the connection failed.
    pub fn is_closed(&self) -> bool {
        self.inner.as_ref().unwrap().tx.is_closed()
    }

    /// Creates a new receiver subscribed to this sender.
    pub fn subscribe(&self) -> Receiver<T, Codec> {
        let inner = self.inner.as_ref().unwrap();
        Receiver::new(
            inner.tx.subscribe(),
            inner.remote_send_err_tx.clone(),
            None,
            inner.sender_rate_limit_rx.clone(),
            inner.receiver_rate_limit_tx.clone(),
            inner.transfer_strategy.clone(),
        )
    }

    fn update_error(&self) {
        let inner = self.inner.as_ref().unwrap();
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

        let inner = self.inner.as_ref().unwrap();
        let current_err = inner.current_err.lock().unwrap();
        current_err.clone().map(|err| err.into())
    }

    /// Clears the error that occurred during sending to a remote endpoint.
    pub fn clear_error(&mut self) {
        self.update_error();

        let inner = self.inner.as_ref().unwrap();
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
        self.inner.as_ref().unwrap().max_item_size
    }

    /// Sets the maximum allowed item size in bytes.
    pub fn set_max_item_size(&mut self, max_item_size: usize) {
        self.inner.as_mut().unwrap().max_item_size = max_item_size;
    }

    /// Minimum delay between sending value updates.
    ///
    /// By default this is [`Duration::ZERO`], thus rate limiting is disabled.
    ///
    /// See the [module-level documentation](super#rate-limiting) for how this
    /// combines with a rate limit requested by the receiver.
    pub fn rate_limit(&self) -> Duration {
        *self.inner.as_ref().unwrap().sender_rate_limit_tx.borrow()
    }

    /// Sets the minimum delay between sending value updates.
    ///
    /// Transmission of value updates to remote endpoints is throttled accordingly,
    /// coalescing intermediate values. It is guaranteed that the latest value will
    /// eventually be transmitted; the final value is transmitted immediately when
    /// the sender is dropped.
    ///
    /// If the receiver also configures a rate limit, the effective minimum delay
    /// is the maximum of both values. See the
    /// [module-level documentation](super#rate-limiting) for details.
    pub fn set_rate_limit(&mut self, rate_limit: Duration) {
        self.inner.as_ref().unwrap().sender_rate_limit_tx.send_replace(rate_limit);
    }
}

impl<T, Codec> Drop for Sender<T, Codec> {
    fn drop(&mut self) {
        if let Some(successor_tx) = self.successor_tx.lock().unwrap().take() {
            let _ = successor_tx.send(self.inner.take().unwrap());
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
        let sender_rate_limit = self.rate_limit();
        let receiver_rate_limit = self.inner.as_ref().unwrap().receiver_rate_limit_rx.get();
        let transfer_strategy = self.inner.as_ref().unwrap().transfer_strategy.clone();

        // Prepare channel for takeover.
        let (successor_tx, successor_rx) = tokio::sync::oneshot::channel();
        *self.successor_tx.lock().unwrap() = Some(successor_tx);

        let port = PortSerializer::connect(move |connect| {
            async move {
                // Sender has been dropped after sending, so we receive its channels.
                let SenderInner { tx, remote_send_err_rx, current_err, receiver_rate_limit_rx, .. } =
                    match successor_rx.await {
                        Ok(inner) => inner,
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

                super::recv_impl(
                    ErasedDeserializer::new::<Result<T, RecvError>, Codec>(),
                    Box::new(tx),
                    raw_tx,
                    raw_rx,
                    remote_send_err_rx,
                    current_err,
                    max_item_size,
                    receiver_rate_limit_rx,
                )
                .await;
            }
            .boxed()
        })?;

        // Encode chmux port number in transport type and serialize it.
        let data = self.inner.as_ref().unwrap().tx.borrow().clone();
        let transported = TransportedSender::<T> {
            port,
            data,
            codec: PhantomData,
            max_item_size: max_item_size.try_into().unwrap_or(u64::MAX),
            sender_rate_limit,
            receiver_rate_limit,
            transfer_strategy,
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
        let TransportedSender {
            port,
            data,
            max_item_size,
            sender_rate_limit,
            receiver_rate_limit,
            transfer_strategy,
            ..
        } = TransportedSender::<T>::deserialize(deserializer)?;
        let max_item_size = usize::try_from(max_item_size).unwrap_or(usize::MAX);
        let (sender_rate_limit_tx, sender_rate_limit_rx) = tokio::sync::watch::channel(sender_rate_limit);
        if data.is_err() {
            return Err(serde::de::Error::custom("received watch data with error"));
        }

        // Create internal communication channels.
        let (tx, rx) = tokio::sync::watch::channel(data);
        let (remote_send_err_tx, remote_send_err_rx) = tokio::sync::mpsc::unbounded_channel();
        let remote_send_err_tx2 = remote_send_err_tx.clone();
        let sender_rate_limit_rx2 = sender_rate_limit_rx.clone();
        let (receiver_rate_limit_tx, receiver_rate_limit_rx) = rate_limit_channel(receiver_rate_limit);
        let receiver_rate_limit_tx2 = receiver_rate_limit_tx.clone();
        let transfer_strategy2 = transfer_strategy.clone();

        // Accept chmux port request.
        PortDeserializer::accept(port, move |request| {
            async move {
                // Accept chmux connection request.
                let (raw_tx, raw_rx) = match request.accept().await {
                    Ok(tx_rx) => tx_rx,
                    Err(err) => {
                        let _ = remote_send_err_tx.send(RemoteSendError::Listen(err));
                        return;
                    }
                };

                super::send_impl(
                    ErasedSerializer::new::<Result<T, RecvError>, Codec>(),
                    Box::new(rx),
                    raw_tx,
                    raw_rx,
                    remote_send_err_tx,
                    max_item_size,
                    sender_rate_limit_rx,
                    receiver_rate_limit_tx,
                    transfer_strategy,
                )
                .await;
            }
            .boxed()
        })?;

        Ok(Self::new(
            tx,
            remote_send_err_tx2,
            remote_send_err_rx,
            max_item_size,
            sender_rate_limit_tx,
            sender_rate_limit_rx2,
            receiver_rate_limit_tx2,
            receiver_rate_limit_rx,
            transfer_strategy2,
        ))
    }
}
