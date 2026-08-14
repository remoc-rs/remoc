//! A single-producer, multi-consumer remote channel that only retains the last sent value.
//!
//! The sender and receiver can both be sent to remote endpoints.
//! The channel also works if both halves are local.
//! Forwarding over multiple connections is supported.
//!
//! This has similar functionality as [tokio::sync::watch] with the additional
//! ability to work over remote connections.
//!
//! ### Rate limiting
//!
//! To limit the used bandwidth a minimum delay between subsequent value
//! transmissions to the remote endpoint can be configured. Intermediate values
//! are coalesced, i.e. only the most recent value is transmitted once the delay
//! has elapsed, and the latest value is always eventually delivered. The final
//! value is transmitted immediately when the sender is dropped.
//!
//! Rate limiting can be requested from both ends of the channel:
//!
//!   * the sending side via [`Sender::set_rate_limit`] (or
//!     [`Forwarding::set_rate_limit`]), and
//!   * the receiving side via [`Receiver::set_rate_limit`].
//!
//! The receiver-requested rate limit is transmitted back to the sending endpoint,
//! where the actual throttling takes place. When both sides configure a rate
//! limit, the effective minimum delay is the **maximum** of the two values, so
//! that the bandwidth limits requested by both endpoints are honored.
//!
//! When multiple receivers share the same channel (for example clones of a
//! received [`Receiver`]), their individually requested rate limits are combined
//! by taking the **minimum**; a receiver that has not configured a rate limit
//! contributes [`Duration::ZERO`] and thus disables throttling for that channel.
//!
//! By default rate limiting is disabled and all value updates are sent immediately.
//!
//! # Alternatives
//!
//! If your endpoints need the ability to change the value and synchronize the changes
//! with other endpoints, consider using an [read/write lock](crate::robj::rw_lock)
//! instead.
//!
//! # Example
//!
//! In the following example the client sends a number and a watch channel sender to the server.
//! The server counts to the number and sends each value to the client over the watch channel.
//!
//! ```
//! use remoc::prelude::*;
//!
//! #[derive(Debug, serde::Serialize, serde::Deserialize)]
//! struct CountReq {
//!     up_to: u32,
//!     watch_tx: rch::watch::Sender<u32>,
//! }
//!
//! // This would be run on the client.
//! async fn client(mut tx: rch::base::Sender<CountReq>) {
//!     let (watch_tx, mut watch_rx) = rch::watch::channel(0);
//!     tx.send(CountReq { up_to: 4, watch_tx }).await.unwrap();
//!
//!     // Intermediate values may be missed.
//!     while *watch_rx.borrow_and_update().unwrap() != 3 {
//!         watch_rx.changed().await;
//!     }
//! }
//!
//! // This would be run on the server.
//! async fn server(mut rx: rch::base::Receiver<CountReq>) {
//!     while let Some(CountReq { up_to, watch_tx }) = rx.recv().await.unwrap() {
//!         for i in 0..up_to {
//!             watch_tx.send(i).unwrap();
//!         }
//!     }
//! }
//! # tokio_test::block_on(remoc::doctest::client_server(client, server));
//! ```
//!

use bytes::Buf;
use futures::{FutureExt, future::BoxFuture};
use std::{
    fmt,
    future::{self, Future},
    ops::Deref,
    pin::Pin,
    sync::{Arc, Weak},
    task::{Context, Poll, ready},
    time::Duration,
};
use wokio::{
    self,
    time::{Instant, sleep},
};

use super::{
    DEFAULT_MAX_ITEM_SIZE, RemoteSendError,
    base::{self},
};
use crate::{
    RemoteSend, chmux,
    codec::{self, AnySend, ErasedDeserializer, ErasedSerializer},
    rch::{BACKCHANNEL_MSG_ERROR, BACKCHANNEL_MSG_RATE_LIMIT},
};

mod receiver;
mod sender;

pub use receiver::{ChangedError, Receiver, ReceiverStream, RecvError};
pub use sender::{SendError, Sender};

/// Returns a reference to the inner value.
pub struct Ref<'a, T>(tokio::sync::watch::Ref<'a, Result<T, RecvError>>);

impl<T> Deref for Ref<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().unwrap()
    }
}

impl<T> fmt::Debug for Ref<'_, T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", **self)
    }
}

/// Creates a new watch channel, returning the sender and receiver.
///
/// The sender and receiver may be sent to remote endpoints via channels.
pub fn channel<T, Codec>(init: T) -> (Sender<T, Codec>, Receiver<T, Codec>)
where
    T: RemoteSend,
{
    let (tx, rx) = tokio::sync::watch::channel(Ok(init));
    let (remote_send_err_tx, remote_send_err_rx) = tokio::sync::mpsc::unbounded_channel();
    let (sender_rate_limit_tx, sender_rate_limit_rx) = tokio::sync::watch::channel(default_rate_limit());
    let (receiver_rate_limit_tx, receiver_rate_limit_rx) = rate_limit_channel(default_rate_limit());

    let sender = Sender::new(
        tx,
        remote_send_err_tx.clone(),
        remote_send_err_rx,
        DEFAULT_MAX_ITEM_SIZE,
        sender_rate_limit_tx,
        sender_rate_limit_rx.clone(),
        receiver_rate_limit_tx.clone(),
        receiver_rate_limit_rx,
        TransferStrategy::default(),
    );
    let receiver = Receiver::new(
        rx,
        remote_send_err_tx,
        None,
        sender_rate_limit_rx,
        receiver_rate_limit_tx,
        TransferStrategy::default(),
    );
    (sender, receiver)
}

/// Makes a local watch receiver forwardable to remote endpoints.
///
/// The returned [`Forwarding`] future resolves once forwarding has completed or an error occurs.
/// The returned receiver may be sent to remote endpoints via channels.
pub fn forward<T, Codec>(mut local_rx: tokio::sync::watch::Receiver<T>) -> (Forwarding, Receiver<T, Codec>)
where
    T: RemoteSend + Sync + Clone,
    Codec: codec::Codec,
{
    let init = local_rx.borrow_and_update().clone();
    let (mut tx, rx) = channel(init);
    let sender_rate_limit_tx = tx.inner.as_ref().unwrap().sender_rate_limit_tx.clone();

    let hnd = wokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                () = tx.closed() => break,
                res = local_rx.changed() => {
                    match res {
                        Ok(()) => {
                            let value = local_rx.borrow_and_update().clone();
                            match tx.send(value) {
                                Ok(()) => (),
                                Err(err) if err.is_closed() => break,
                                Err(err) => return Err(err),
                            }
                        }
                        Err(_) => break,
                    }
                }

            }
        }

        tx.check()
    });

    (Forwarding { hnd, sender_rate_limit_tx }, rx)
}

/// Handle to obtain the result of forwarding a local receiver remotely by [`forward`].
///
/// Await this to obtain the result of the forwarding operation.
/// The operation is assumed to have finished successfully if either the local or remote
/// channel is closed or dropped.
///
/// Dropping this *does not* stop forwarding.
pub struct Forwarding {
    hnd: wokio::task::JoinHandle<Result<(), SendError>>,
    sender_rate_limit_tx: tokio::sync::watch::Sender<Duration>,
}

impl fmt::Debug for Forwarding {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Forwarding").finish()
    }
}

impl Future for Forwarding {
    type Output = Result<(), SendError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        match ready!(self.hnd.poll_unpin(cx)) {
            Ok(res) => Poll::Ready(res),
            Err(_) => Poll::Ready(Err(SendError::Closed)),
        }
    }
}

impl Forwarding {
    /// Stops forwarding.
    ///
    /// The remote sending half and local receiving half of the watch channels are dropped.
    pub fn stop(self) {
        self.hnd.abort();
    }

    /// Minimum delay between sending value updates.
    ///
    /// By default this is [`Duration::ZERO`], thus rate limiting is disabled.
    pub fn rate_limit(&self) -> Duration {
        *self.sender_rate_limit_tx.borrow()
    }

    /// Sets the minimum delay between sending value updates.
    ///
    /// Transmission of value updates to remote endpoints is throttled accordingly,
    /// coalescing intermediate values. It is guaranteed that the latest value will
    /// eventually be transmitted; the final value is transmitted immediately when
    /// the forwarding is stopped or the sender is dropped.
    ///
    /// If the receiver also configures a rate limit, the effective minimum delay
    /// is the maximum of both values. See the
    /// [module-level documentation](self#rate-limiting) for details.
    pub fn set_rate_limit(&mut self, rate_limit: Duration) {
        self.sender_rate_limit_tx.send_replace(rate_limit);
    }
}

/// Strategy for transfering values to the remote endpoint.
#[derive(Default, Clone, PartialEq, Eq)]
pub enum TransferStrategy {
    /// Only a single value may be in transfer.
    ///
    /// The next value is sent once the previous value has been received.
    /// This limits the achievable bandwidth of the channel and increases latency.
    Single,
    /// A global buffer is used, allowing multiple values to be in transfer.
    ///
    /// Global credits and thus buffer space is being used.
    /// This maximizes the achievable bandwidth of the channel.
    GlobalBuffered,
    /// A channel-specific buffer is used, allowing multiple values to be in transfer.
    ///
    /// No global credits and thus buffer space is being used.
    /// This limits the achievable bandwidth of the channel.
    #[default]
    ChannelBuffered,
}

crate::versioned::compact::impl_enum! {
    TransferStrategy,
    variants {
        Single => "_0",
        GlobalBuffered => "_1",
        #[serde(other)]
        ChannelBuffered => "_59",
    }
}

/// Extensions for watch channels.
pub trait WatchExt<T, Codec, const MAX_ITEM_SIZE: usize> {
    /// Sets the maximum item size for the channel.
    fn with_max_item_size<const NEW_MAX_ITEM_SIZE: usize>(
        self,
    ) -> (Sender<T, Codec>, Receiver<T, Codec, NEW_MAX_ITEM_SIZE>);

    /// Sets the [transfer strategy](TransferStrategy) for the channel.
    ///
    /// The transfer strategy balances throughput, global buffer use and latency.
    ///
    /// The default is [`TransferStrategy::ChannelBuffered`].
    ///
    /// This must be called before transfering any half of the channel to a
    /// remote endpoint.
    fn with_transfer_strategy(
        self, transfer_strategy: TransferStrategy,
    ) -> (Sender<T, Codec>, Receiver<T, Codec, MAX_ITEM_SIZE>);
}

impl<T, Codec, const MAX_ITEM_SIZE: usize> WatchExt<T, Codec, MAX_ITEM_SIZE>
    for (Sender<T, Codec>, Receiver<T, Codec, MAX_ITEM_SIZE>)
where
    T: Send + 'static,
{
    fn with_max_item_size<const NEW_MAX_ITEM_SIZE: usize>(
        self,
    ) -> (Sender<T, Codec>, Receiver<T, Codec, NEW_MAX_ITEM_SIZE>) {
        let (mut tx, rx) = self;
        tx.set_max_item_size(NEW_MAX_ITEM_SIZE);
        let rx = rx.set_max_item_size();
        (tx, rx)
    }

    fn with_transfer_strategy(
        self, transfer_strategy: TransferStrategy,
    ) -> (Sender<T, Codec>, Receiver<T, Codec, MAX_ITEM_SIZE>) {
        let (mut tx, mut rx) = self;
        if let Some(inner) = &mut tx.inner {
            inner.transfer_strategy = transfer_strategy.clone();
        }
        rx.transfer_strategy = transfer_strategy;
        (tx, rx)
    }
}

trait ErasedWatchRx {
    fn borrow_and_update_clone(&mut self) -> AnySend;
    fn changed<'a>(&'a mut self) -> BoxFuture<'a, Result<(), tokio::sync::watch::error::RecvError>>;
}

impl<T> ErasedWatchRx for tokio::sync::watch::Receiver<Result<T, RecvError>>
where
    T: Clone + Send + Sync + 'static,
{
    fn borrow_and_update_clone(&mut self) -> AnySend {
        Box::new(self.borrow_and_update().clone())
    }

    fn changed<'a>(&'a mut self) -> BoxFuture<'a, Result<(), tokio::sync::watch::error::RecvError>> {
        self.changed().boxed()
    }
}

/// Send implementation for deserializer of Sender and serializer of Receiver.
#[allow(clippy::too_many_arguments)]
async fn send_impl(
    erased_serializer: ErasedSerializer, mut rx: Box<dyn ErasedWatchRx + Send>, raw_tx: chmux::Sender,
    mut raw_rx: chmux::Receiver, remote_send_err_tx: tokio::sync::mpsc::UnboundedSender<RemoteSendError>,
    max_item_size: usize, mut sender_rate_limit_rx: tokio::sync::watch::Receiver<Duration>,
    mut receiver_rate_limit_tx: RateLimitSender, strategy: TransferStrategy,
) {
    // Encode data using remote sender for sending.
    let mut remote_tx = base::ErasedSender::new(erased_serializer, raw_tx);
    remote_tx.set_max_item_size(max_item_size);
    remote_tx.set_global_credits_allowed(strategy != TransferStrategy::ChannelBuffered);

    // Rate limiting state.
    let mut last_send: Option<Instant> = None;
    let mut send_pending = false;
    let mut closed = false;

    // Process events.
    while !closed {
        let rate_limit = sender_rate_limit_rx.borrow_and_update().max(receiver_rate_limit_tx.get());
        let pending_send_trigger = async {
            if send_pending {
                if let Some(last_send) = last_send
                    && rate_limit > Duration::ZERO
                {
                    let until = last_send + rate_limit;
                    let delay = until.duration_since(Instant::now());
                    if delay > Duration::ZERO {
                        sleep(delay).await;
                    }
                }
            } else {
                future::pending().await
            }
        };

        let send = tokio::select! {
            biased;

            // Back channel message from remote endpoint.
            backchannel_msg = raw_rx.recv() => {
                match backchannel_msg {
                    Ok(Some(mut msg)) => {
                        match msg.try_get_u8() {
                            Ok(BACKCHANNEL_MSG_ERROR) => {
                                let _ = remote_send_err_tx.send(RemoteSendError::Forward);
                            }
                            Ok(BACKCHANNEL_MSG_RATE_LIMIT) => {
                                if let Ok(ns) = msg.try_get_u128_le() {
                                    receiver_rate_limit_tx.set(Duration::from_nanos_u128(ns));
                                }
                            }
                            _ => (),
                        }
                    }
                    _ => closed = true,
                }
                false
            }

            // Rate-limit delay has passed.
            () = pending_send_trigger => true,

            // Sender rate-limit has changed.
            Ok(()) = sender_rate_limit_rx.changed() => false,

            // Value was updated and needs to be sent to remote endpoint.
            changed = rx.changed() => {
                match changed {
                    Ok(()) => send_pending = true,
                    Err(_) => closed = true,
                }
                false
            }
        };

        // Send updated value to remote endpoint.
        if send || (send_pending && closed) {
            let value = rx.borrow_and_update_clone();
            if let Err(err) = remote_tx.send_erased(value).await {
                let _ = remote_send_err_tx.send(RemoteSendError::Send(err.kind.clone()));
                if err.is_item_specific() {
                    tracing::warn!(%err, "sending over remote channel failed");
                    break;
                }
            }

            last_send = Some(Instant::now());
            send_pending = false;

            if strategy == TransferStrategy::Single {
                remote_tx.all_received().await;
            }
        }
    }
}

trait ErasedWatchTx {
    fn send(&self, value: AnySend) -> Result<(), ()>;
    fn send_err(&self, err: RecvError) -> Result<(), ()>;
    fn closed(&'_ self) -> BoxFuture<'_, ()>;
}

impl<T> ErasedWatchTx for tokio::sync::watch::Sender<Result<T, RecvError>>
where
    T: Clone + Send + Sync + 'static,
{
    fn send(&self, value: AnySend) -> Result<(), ()> {
        let value: Result<T, RecvError> = *value.downcast().expect("type mismatch in watch receiver");
        self.send(value).map_err(|_| ())
    }

    fn send_err(&self, err: RecvError) -> Result<(), ()> {
        let value: Result<T, RecvError> = Err(err);
        self.send(value).map_err(|_| ())
    }

    fn closed(&'_ self) -> BoxFuture<'_, ()> {
        self.closed().boxed()
    }
}

/// Receive implementation for serializer of Sender and deserializer of Receiver.
#[allow(clippy::too_many_arguments)]
async fn recv_impl(
    erased_deserializer: ErasedDeserializer, tx: Box<dyn ErasedWatchTx + Send>, mut raw_tx: chmux::Sender,
    raw_rx: chmux::Receiver, mut remote_send_err_rx: tokio::sync::mpsc::UnboundedReceiver<RemoteSendError>,
    mut current_err: Option<RemoteSendError>, max_item_size: usize, mut rate_limit_rx: RateLimitReceiver,
) {
    // Decode raw received data using remote receiver.
    let mut remote_rx = base::ErasedReceiver::new(erased_deserializer, raw_rx);
    remote_rx.set_max_item_size(max_item_size);

    // Rate limiting signaling to sender.
    let mut rate_limit = None;

    // Process events.
    loop {
        tokio::select! {
            biased;

            // Channel closure requested locally.
            () = tx.closed() => break,

            // Notify remote endpoint of error.
            Some(_) = remote_send_err_rx.recv() => {
                let _ = raw_tx.send(vec![BACKCHANNEL_MSG_ERROR].into()).await;
            }
            () = futures::future::ready(()), if current_err.is_some() => {
                let _ = raw_tx.send(vec![BACKCHANNEL_MSG_ERROR].into()).await;
                current_err = None;
            }

            // Rate limit changed; update sender via back channel.
            Ok(()) = rate_limit_rx.changed() => {
                let new_rate_limit = rate_limit_rx.get_and_update();
                if rate_limit != Some(new_rate_limit) {
                    let mut msg = vec![BACKCHANNEL_MSG_RATE_LIMIT];
                    msg.extend(new_rate_limit.as_nanos().to_le_bytes());
                    let _ = raw_tx.send(msg.into()).await;
                    rate_limit = Some(new_rate_limit);
                }
            }

            // Data received from remote endpoint.
            res = remote_rx.recv_erased() => {
                match res {
                    Ok(Some(value)) => {
                        if tx.send(value).is_err() {
                            break;
                        }
                    },
                    Ok(None) => break,
                    Err(err) => {
                        let is_disconnected_err = err.is_disconnected();
                        if tx.send_err(RecvError::RemoteReceive(err)).is_err() {
                            break
                        }
                        if is_disconnected_err {
                            break;
                        }
                    },
                }
            }
        }
    }
}

/// Create channel for combining rate limits of multiple receivers.
pub(crate) fn rate_limit_channel(rate_limit: Duration) -> (RateLimitSender, RateLimitReceiver) {
    let current = Arc::new(rate_limit);
    let (tx, rx) = tokio::sync::watch::channel(vec![Arc::downgrade(&current)]);
    (RateLimitSender { tx, current }, RateLimitReceiver(rx))
}

/// Rate limit sender beloning to a watch channel receiver.
#[derive(Clone)]
pub(crate) struct RateLimitSender {
    tx: tokio::sync::watch::Sender<Vec<Weak<Duration>>>,
    current: Arc<Duration>,
}

impl RateLimitSender {
    /// Get the rate limit.
    pub fn get(&self) -> Duration {
        *self.current
    }

    /// Set the rate limit.
    pub fn set(&mut self, rate_limit: Duration) {
        self.current = Arc::new(Duration::ZERO);

        let rate_limit = Arc::new(rate_limit);
        self.tx.send_modify(|limits| {
            limits.retain(|weak| weak.strong_count() > 0);
            limits.push(Arc::downgrade(&rate_limit));
        });

        self.current = rate_limit
    }
}

impl Drop for RateLimitSender {
    fn drop(&mut self) {
        self.current = Arc::new(Duration::ZERO);
        self.tx.send_modify(|limits| limits.retain(|weak| weak.strong_count() > 0));
    }
}

/// Rate limit receiver, combining the requested rate limits.
pub(crate) struct RateLimitReceiver(tokio::sync::watch::Receiver<Vec<Weak<Duration>>>);

impl RateLimitReceiver {
    /// Wait for rate limit to change.
    pub async fn changed(&mut self) -> Result<(), tokio::sync::watch::error::RecvError> {
        self.0.changed().await
    }

    fn compute(weaks: &[Weak<Duration>]) -> Duration {
        weaks.iter().filter_map(|weak| weak.upgrade()).map(|limit| *limit).min().unwrap_or_default()
    }

    /// Get lowest rate limit of all senders.
    pub fn get(&self) -> Duration {
        Self::compute(&self.0.borrow())
    }

    /// Get lowest rate limit of all senders and mark as seen.
    pub fn get_and_update(&mut self) -> Duration {
        Self::compute(&self.0.borrow_and_update())
    }
}

const fn default_max_item_size() -> u64 {
    u64::MAX
}

const fn default_rate_limit() -> Duration {
    Duration::ZERO
}
