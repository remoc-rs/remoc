use futures::{Stream, ready};
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    fmt,
    marker::PhantomData,
    mem,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio_util::sync::ReusableBoxFuture;

use super::{
    super::{
        DEFAULT_MAX_ITEM_SIZE, RemoteSendError,
        base::{self, PortDeserializer, PortSerializer},
    },
    RateLimitSender, Ref, TransferStrategy, rate_limit_channel,
};
use crate::{
    RemoteSend, chmux,
    codec::{self, ErasedDeserializer, ErasedSerializer},
    versioned::result::Result as CompactResult,
};

/// An error returned while receiving updates from a watch channel.
#[derive(Clone, Debug)]
pub enum RecvError {
    /// Receiving or decoding an update failed; see [`base::RecvError`].
    Receive(base::RecvError),
    /// Opening a channel contained in an update failed; see [`chmux::ConnectError`].
    Connect(chmux::ConnectError),
    /// Preparing a channel contained in an update failed; see [`chmux::ListenerError`].
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
        Receive(err: base::RecvError) => "_0",
        Connect(err: chmux::ConnectError) => "_1",
        Listen(err: chmux::ListenerError) => "_2",
        Remote(err: Option<Box<RecvError>>) => "_50",
    }
}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Receive(err) => write!(f, "receive error: {err}"),
            Self::Connect(err) => write!(f, "connect error: {err}"),
            Self::Listen(err) => write!(f, "listen error: {err}"),
            Self::Remote(Some(err)) => write!(f, "remote {err}"),
            Self::Remote(None) => write!(f, "unknown remote error"),
        }
    }
}

impl Error for RecvError {}

impl RecvError {
    /// Returns whether the connection was rejected or failed.
    pub fn is_disconnected(&self) -> bool {
        match self {
            Self::Receive(err) => err.is_disconnected(),
            Self::Connect(_) | Self::Listen(_) => true,
            Self::Remote(Some(err)) => err.is_disconnected(),
            Self::Remote(None) => false,
        }
    }
}

/// An error returned by [`Receiver::changed`] or [`Receiver::has_changed`].
#[derive(Clone, Debug)]
pub enum ChangedError {
    /// The sender has been dropped.
    Closed,
    /// A final receive error occurred, meaning that no further values can be received.
    Recv(RecvError),
}

crate::versioned::compact::impl_enum! {
    ChangedError,
    variants {
        Closed => "_0",
        Recv(err: RecvError) => "_1",
    }
}

impl ChangedError {
    /// Returns `true` if the sender has been dropped.
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Closed)
    }
}

impl From<RecvError> for ChangedError {
    fn from(err: RecvError) -> Self {
        Self::Recv(err)
    }
}

impl fmt::Display for ChangedError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "closed"),
            Self::Recv(err) => write!(f, "{err}"),
        }
    }
}

impl Error for ChangedError {}

/// The receiving half of a watch channel.
///
/// A receiver retains the newest value. It can be cloned or transferred to other
/// endpoints; each clone independently tracks whether it has seen the current
/// value.
///
/// Use [`borrow`](Self::borrow) to inspect the current value without marking it
/// seen, or [`borrow_and_update`](Self::borrow_and_update) to mark it seen.
/// [`changed`](Self::changed) waits until an unseen value is available.
///
/// A receiver can be adapted to [`Stream`](futures::Stream) with [`ReceiverStream`].
#[derive(Clone)]
pub struct Receiver<T, Codec = codec::Default, const MAX_ITEM_SIZE: usize = DEFAULT_MAX_ITEM_SIZE> {
    rx: tokio::sync::watch::Receiver<Result<T, RecvError>>,
    remote_send_err_tx: tokio::sync::mpsc::UnboundedSender<RemoteSendError>,
    remote_max_item_size: Option<usize>,
    sender_rate_limit_rx: tokio::sync::watch::Receiver<Duration>,
    receiver_rate_limit_tx: RateLimitSender,
    pub(super) transfer_strategy: TransferStrategy,
    _codec: PhantomData<Codec>,
}

impl<T, Codec, const MAX_ITEM_SIZE: usize> fmt::Debug for Receiver<T, Codec, MAX_ITEM_SIZE> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Receiver").finish()
    }
}

/// Watch receiver in transport.
struct TransportedReceiver<T> {
    /// chmux port number.
    port: u32,
    /// Current data value.
    data: Result<T, RecvError>,
    /// Maximum item size.
    max_item_size: u64,
    /// Minimum delay between sending value updates.
    rate_limit: Duration,
    /// Transfer strategy.
    transfer_strategy: TransferStrategy,
}

crate::versioned::compact::impl_struct! {
    TransportedReceiver<T>,
    fields {
        port: u32 => "_0",
        #[compact]
        data: Result<T, RecvError> => "_1",
        #[serde(default)]
        codec: PhantomData<()> = PhantomData,
        #[serde(default = "crate::rch::default_max_item_size")]
        #[serde(skip_serializing_if = "crate::rch::is_default_max_item_size")]
        max_item_size: u64 => "_2",
        #[compact]
        #[serde(default)]
        #[serde(skip_serializing_if = "crate::codec::skip::if_default_ref")]
        rate_limit: Duration => "_3",
        #[serde(default)]
        #[serde(skip_serializing_if = "crate::codec::skip::if_default_ref")]
        transfer_strategy: TransferStrategy => "_4",
    }
    where T: RemoteSend + Clone
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
        remote_max_item_size: Option<usize>, sender_rate_limit_rx: tokio::sync::watch::Receiver<Duration>,
        receiver_rate_limit_tx: RateLimitSender, transfer_strategy: TransferStrategy,
    ) -> Self {
        Self {
            rx,
            remote_send_err_tx,
            remote_max_item_size,
            sender_rate_limit_rx,
            receiver_rate_limit_tx,
            transfer_strategy,
            _codec: PhantomData,
        }
    }

    /// Borrows the most recently received value.
    ///
    /// This does not mark the value as seen. A subsequent call to
    /// [`changed`](Self::changed) may therefore return immediately.
    ///
    /// The returned [`Ref`] holds a read lock. Keep it short-lived and do not hold
    /// it across an `.await`.
    pub fn borrow(&self) -> Result<Ref<'_, T>, RecvError> {
        let ref_res = self.rx.borrow();
        match &*ref_res {
            Ok(_) => Ok(Ref(ref_res)),
            Err(err) => Err(err.clone()),
        }
    }

    /// Borrows the most recently received value and marks it as seen.
    ///
    /// The returned [`Ref`] holds a read lock. Keep it short-lived and do not hold
    /// it across an `.await`.
    pub fn borrow_and_update(&mut self) -> Result<Ref<'_, T>, RecvError> {
        let ref_res = self.rx.borrow_and_update();
        match &*ref_res {
            Ok(_) => Ok(Ref(ref_res)),
            Err(err) => Err(err.clone()),
        }
    }

    /// Returns whether this receiver has an unseen value.
    ///
    /// This does not mark the current value as seen. If the sender has been
    /// dropped, the method can still return `Ok(true)` while an unseen final value
    /// remains.
    pub fn has_changed(&self) -> Result<bool, ChangedError> {
        if let Err(err) = &*self.rx.borrow()
            && err.is_disconnected()
        {
            return Err(ChangedError::Recv(err.clone()));
        }
        self.rx.has_changed().map_err(|_| ChangedError::Closed)
    }

    /// Waits for an unseen value and marks the newest value as seen.
    ///
    /// Several updates may be coalesced into one notification. After this method
    /// returns, use [`borrow`](Self::borrow) to access the newest value.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel safe. If canceled before returning, the current value
    /// is not marked as seen.
    ///
    /// # Example
    ///
    /// ```
    /// use remoc::rch;
    ///
    /// # tokio_test::block_on(async {
    /// let (tx, mut rx): (_, rch::watch::Receiver<u32>) = rch::watch::channel(1);
    ///
    /// // The initial value has not been marked as seen yet.
    /// rx.borrow_and_update().unwrap();
    /// assert!(!rx.has_changed().unwrap());
    ///
    /// tx.send(2).unwrap();
    /// rx.changed().await.unwrap();
    /// assert_eq!(*rx.borrow().unwrap(), 2);
    /// # });
    /// ```
    pub async fn changed(&mut self) -> Result<(), ChangedError> {
        if let Err(err) = &*self.rx.borrow()
            && err.is_disconnected()
        {
            return Err(ChangedError::Recv(err.clone()));
        }
        self.rx.changed().await.map_err(|_| ChangedError::Closed)?;
        if let Err(err) = &*self.rx.borrow()
            && err.is_disconnected()
        {
            return Err(ChangedError::Recv(err.clone()));
        }
        Ok(())
    }

    /// Marks the current value as unseen.
    ///
    /// After calling this, [`has_changed`](Self::has_changed) returns `Ok(true)`
    /// and [`changed`](Self::changed) can complete immediately even if no sender
    /// published a new value.
    pub fn mark_changed(&mut self) {
        self.rx.mark_changed();
    }

    /// Marks the current value as seen.
    ///
    /// This suppresses an immediate wake-up from [`changed`](Self::changed)
    /// until a later update arrives or [`mark_changed`](Self::mark_changed) is
    /// called.
    pub fn mark_unchanged(&mut self) {
        self.rx.mark_unchanged();
    }

    /// Waits for a value that satisfies the provided condition.
    ///
    /// If the current value already matches, this returns immediately with a
    /// borrow of that value. Values examined while waiting are marked as seen, so
    /// a later call to [`changed`](Self::changed) waits for a newer update.
    pub async fn wait_for(&mut self, mut f: impl FnMut(&T) -> bool) -> Result<Ref<'_, T>, ChangedError> {
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
            Err(_) => Err(ChangedError::Closed),
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
                tokio::sync::watch::channel(Err(RecvError::Connect(chmux::ConnectError::ChMux))).1,
            ),
            remote_send_err_tx: self.remote_send_err_tx.clone(),
            remote_max_item_size: self.remote_max_item_size,
            sender_rate_limit_rx: self.sender_rate_limit_rx.clone(),
            receiver_rate_limit_tx: self.receiver_rate_limit_tx.clone(),
            transfer_strategy: self.transfer_strategy.clone(),
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

    /// Minimum delay between receiving value updates.
    ///
    /// By default this is [`Duration::ZERO`], thus rate limiting is disabled.
    ///
    /// See the [module-level documentation](super#rate-limiting) for how this
    /// combines with a rate limit requested by the sender or by other receivers
    /// sharing the same channel.
    pub fn rate_limit(&self) -> Duration {
        self.receiver_rate_limit_tx.get()
    }

    /// Sets the minimum delay between receiving value updates.
    ///
    /// The request is transmitted back to the sending endpoint, where value
    /// updates are throttled accordingly, coalescing intermediate values. It is
    /// guaranteed that the latest value will eventually be received; the final
    /// value is received immediately when the sender is dropped.
    ///
    /// If the sender also configures a rate limit, the effective minimum delay is
    /// the maximum of both values. When multiple receivers share the same channel,
    /// the effective delay is the minimum of their requested rate limits. See the
    /// [module-level documentation](super#rate-limiting) for details.
    ///
    /// Cloning a receiver copies the configured rate limit to the new clone.
    pub fn set_rate_limit(&mut self, rate_limit: Duration) {
        self.receiver_rate_limit_tx.set(rate_limit);
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
        let sender_rate_limit_rx = self.sender_rate_limit_rx.clone();
        let receiver_rate_limit_tx = self.receiver_rate_limit_tx.clone();
        let receiver_rate_limit = receiver_rate_limit_tx.get();
        let transfer_strategy = self.transfer_strategy.clone();

        let port = PortSerializer::connect_port(async move |connect| {
            // Establish chmux channel.
            let (raw_tx, raw_rx) = match connect.await {
                Ok(tx_rx) => tx_rx,
                Err(err) => {
                    let _ = remote_send_err_tx.send(RemoteSendError::Connect(err));
                    return;
                }
            };

            super::send_impl(
                ErasedSerializer::new::<CompactResult<T, RecvError>, Codec>(),
                Box::new(rx),
                raw_tx,
                raw_rx,
                remote_send_err_tx,
                MAX_ITEM_SIZE,
                sender_rate_limit_rx,
                receiver_rate_limit_tx,
                transfer_strategy,
            )
            .await;
        })?;

        // Encode chmux port number in transport type and serialize it.
        TransportedReceiver::<T> {
            port,
            data,
            max_item_size: self.max_item_size().try_into().unwrap_or(u64::MAX),
            rate_limit: receiver_rate_limit,
            transfer_strategy: self.transfer_strategy.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de, T, Codec, const MAX_ITEM_SIZE: usize> Deserialize<'de> for Receiver<T, Codec, MAX_ITEM_SIZE>
where
    T: RemoteSend + Clone + Sync,
    Codec: codec::Codec,
{
    /// Deserializes the receiver after it has been received over a chmux channel.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Get chmux port number from deserialized transport type.
        let TransportedReceiver { port, data, max_item_size, rate_limit: receiver_rate_limit, transfer_strategy } =
            TransportedReceiver::deserialize(deserializer)?;

        let max_item_size = usize::try_from(max_item_size).unwrap_or(usize::MAX);
        if max_item_size > MAX_ITEM_SIZE {
            tracing::debug!(
                "Watch receiver maximum item size is {MAX_ITEM_SIZE} bytes, \
                 but remote endpoint expects at least {max_item_size} bytes"
            );
        }

        // Create channels.
        let data = data.map_err(|err| RecvError::Remote(Some(Box::new(err))));
        let (tx, rx) = tokio::sync::watch::channel(data);
        let (remote_send_err_tx, remote_send_err_rx) = tokio::sync::mpsc::unbounded_channel();
        let (receiver_rate_limit_tx, receiver_rate_limit_rx) = rate_limit_channel(receiver_rate_limit);

        PortDeserializer::accept(port, async move |request| {
            // Accept chmux connection request.
            let (raw_tx, raw_rx) = match request.accept().await {
                Ok(tx_rx) => tx_rx,
                Err(err) => {
                    let _ = tx.send(Err(RecvError::Listen(err)));
                    return;
                }
            };

            super::recv_impl(
                ErasedDeserializer::new::<CompactResult<T, RecvError>, Codec>(),
                Box::new(tx),
                raw_tx,
                raw_rx,
                remote_send_err_rx,
                None,
                MAX_ITEM_SIZE,
                receiver_rate_limit_rx,
            )
            .await;
        })?;

        // A once transported receiver will only send values when forwarding,
        // for which it would be redundant to apply rate limiting.
        let sender_rate_limit_rx = tokio::sync::watch::channel(Duration::ZERO).1;

        Ok(Self::new(
            rx,
            remote_send_err_tx,
            Some(max_item_size),
            sender_rate_limit_rx,
            receiver_rate_limit_tx,
            transfer_strategy,
        ))
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
