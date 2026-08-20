use futures::{FutureExt, Sink, future};
use serde::{Deserialize, Serialize};
use std::{
    any::Any,
    convert::TryFrom,
    error::Error,
    fmt,
    marker::PhantomData,
    pin::Pin,
    sync::{Arc, Weak},
    task::{Context, Poll, ready},
};
use tokio_util::sync::ReusableBoxFuture;

use super::{
    super::{
        ClosedReason, DEFAULT_BUFFER, DEFAULT_MAX_ITEM_SIZE, RemoteSendError, SendErrorExt, Sending,
        SendingError,
        base::{self, PortDeserializer, PortSerializer},
    },
    SendReq,
    receiver::RecvError,
    send_req,
};
use crate::{
    RemoteSend, chmux,
    codec::{self, ErasedDeserializer, ErasedSerializer},
    versioned::result::Result as CompactResult,
};
/// An error returned when a value cannot be queued for sending.
///
/// The [`Closed`](Self::Closed) variant contains the value that was not queued.
/// Other variants can describe failures reported by an earlier asynchronous
/// transfer; use [`is_item_specific`](Self::is_item_specific) to determine
/// whether a failure applies to the value passed to the current operation.
#[derive(Clone, custom_debug::Debug)]
pub enum SendError<T> {
    /// The receiver closed the channel before this value could be queued.
    Closed(#[debug(skip)] T),
    /// Encoding or transferring a value failed; see [`base::SendErrorKind`].
    Send(base::SendErrorKind),
    /// Opening a channel carried by a value failed; see [`chmux::ConnectError`].
    Connect(chmux::ConnectError),
    /// Preparing to receive a channel carried by a value failed; see [`chmux::ListenerError`].
    Listen(chmux::ListenerError),
    /// An endpoint forwarding this channel could not complete the transfer.
    Forward,
}

crate::versioned::compact::impl_enum! {
    SendError<T>,
    variants {
        Closed(item: T) => "_0",
        Send(err: base::SendErrorKind) => "_1",
        Connect(err: chmux::ConnectError) => "_2",
        Listen(err: chmux::ListenerError) => "_3",
        Forward => "_4",
    }
    where T: RemoteSend
}

impl<T> SendError<T> {
    /// Returns `true` if the receiving endpoint went away.
    ///
    /// This is the case when the receiver was closed or dropped, but not when the
    /// connection to it failed; see [`closed_reason`](Self::closed_reason).
    pub fn is_closed(&self) -> bool {
        matches!(self.closed_reason(), Some(ClosedReason::Closed | ClosedReason::Dropped))
    }

    /// Returns the reason the channel was disconnected.
    ///
    /// Returns [None] if the error is not due to the channel being disconnected,
    /// i.e. if it is specific to the item that was sent.
    pub fn closed_reason(&self) -> Option<ClosedReason> {
        match self {
            Self::Closed(_) => Some(ClosedReason::Closed),
            Self::Send(err) => ClosedReason::from_send_error_kind(err),
            Self::Connect(err) => Some(ClosedReason::from_connect_error(err)),
            Self::Listen(_) | Self::Forward => Some(ClosedReason::Failed),
        }
    }

    /// Returns `true` if the channel can no longer transfer values.
    pub fn is_disconnected(&self) -> bool {
        match self {
            Self::Send(err) => err.is_disconnected(),
            Self::Closed(_) | Self::Connect(_) | Self::Listen(_) | Self::Forward => true,
        }
    }

    /// Returns whether the error was caused by the value being sent.
    ///
    /// Serialization errors and size-limit errors are item-specific. Connection
    /// failures and channel closure are not.
    pub fn is_item_specific(&self) -> bool {
        matches!(self, Self::Send(err) if err.is_item_specific())
    }

    /// Discards the unsent value and returns the same error with `()` in its place.
    pub fn without_item(self) -> SendError<()> {
        match self {
            Self::Closed(_) => SendError::Closed(()),
            Self::Send(err) => SendError::Send(err),
            Self::Connect(err) => SendError::Connect(err),
            Self::Listen(err) => SendError::Listen(err),
            Self::Forward => SendError::Forward,
        }
    }
}

impl<T> SendErrorExt for SendError<T> {
    fn is_closed(&self) -> bool {
        self.is_closed()
    }

    fn is_disconnected(&self) -> bool {
        self.is_disconnected()
    }

    fn is_item_specific(&self) -> bool {
        self.is_item_specific()
    }
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Closed(_) => write!(f, "channel is closed"),
            Self::Send(err) => write!(f, "send error: {err}"),
            Self::Connect(err) => write!(f, "connect error: {err}"),
            Self::Listen(err) => write!(f, "listen error: {err}"),
            Self::Forward => write!(f, "forwarding error"),
        }
    }
}

impl<T> Error for SendError<T> where T: fmt::Debug {}

impl<T> SendError<T> {
    fn from_remote_send_error(err: RemoteSendError, value: T) -> Self {
        match err {
            RemoteSendError::Send(err) => Self::Send(err),
            RemoteSendError::Connect(err) => Self::Connect(err),
            RemoteSendError::Listen(err) => Self::Listen(err),
            RemoteSendError::Forward => Self::Forward,
            RemoteSendError::Closed => Self::Closed(value),
        }
    }
}

/// An error returned by [`Sender::try_send`].
///
/// [`Full`](Self::Full) is temporary and indicates that the channel currently
/// has no capacity. The value is returned in both [`Full`](Self::Full) and
/// [`Closed`](Self::Closed), allowing the caller to retry or recover it.
#[derive(Clone, custom_debug::Debug)]
pub enum TrySendError<T> {
    /// The receiver closed the channel before this value could be queued.
    Closed(#[debug(skip)] T),
    /// The data could not be sent on the channel because the channel
    /// is currently full and sending would require blocking.
    Full(#[debug(skip)] T),
    /// Encoding or transferring a value failed; see [`base::SendErrorKind`].
    Send(base::SendErrorKind),
    /// Opening a channel carried by a value failed; see [`chmux::ConnectError`].
    Connect(chmux::ConnectError),
    /// Preparing to receive a channel carried by a value failed; see [`chmux::ListenerError`].
    Listen(chmux::ListenerError),
    /// An endpoint forwarding this channel could not complete the transfer.
    Forward,
}

crate::versioned::compact::impl_enum! {
    TrySendError<T>,
    variants {
        Closed(item: T) => "_0",
        Full(item: T) => "_1",
        Send(err: base::SendErrorKind) => "_2",
        Connect(err: chmux::ConnectError) => "_3",
        Listen(err: chmux::ListenerError) => "_4",
        Forward => "_5",
    }
    where T: RemoteSend
}

impl<T> TrySendError<T> {
    /// Returns `true` if the receiving endpoint went away.
    ///
    /// This is the case when the receiver was closed or dropped, but not when the
    /// connection to it failed; see [`closed_reason`](Self::closed_reason).
    pub fn is_closed(&self) -> bool {
        matches!(self.closed_reason(), Some(ClosedReason::Closed | ClosedReason::Dropped))
    }

    /// Returns the reason the channel was disconnected.
    ///
    /// Returns [None] if the error is not due to the channel being disconnected,
    /// i.e. if the channel was full or the error is specific to the item that was sent.
    pub fn closed_reason(&self) -> Option<ClosedReason> {
        match self {
            Self::Full(_) => None,
            Self::Closed(_) => Some(ClosedReason::Closed),
            Self::Send(err) => ClosedReason::from_send_error_kind(err),
            Self::Connect(err) => Some(ClosedReason::from_connect_error(err)),
            Self::Listen(_) | Self::Forward => Some(ClosedReason::Failed),
        }
    }

    /// Returns `true` if the channel can no longer transfer values.
    ///
    /// This returns `false` for [`Full`](Self::Full).
    pub fn is_disconnected(&self) -> bool {
        match self {
            Self::Send(err) => err.is_disconnected(),
            Self::Closed(_) | Self::Connect(_) | Self::Listen(_) | Self::Forward => true,
            Self::Full(_) => false,
        }
    }

    /// Returns whether the error was caused by the value being sent.
    pub fn is_item_specific(&self) -> bool {
        matches!(self, Self::Send(err) if err.is_item_specific())
    }
}

impl<T> SendErrorExt for TrySendError<T> {
    fn is_closed(&self) -> bool {
        self.is_closed()
    }

    fn is_disconnected(&self) -> bool {
        self.is_disconnected()
    }

    fn is_item_specific(&self) -> bool {
        self.is_item_specific()
    }
}

impl<T> fmt::Display for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Closed(_) => write!(f, "channel is closed"),
            Self::Full(_) => write!(f, "channel is full"),
            Self::Send(err) => write!(f, "send error: {err}"),
            Self::Connect(err) => write!(f, "connect error: {err}"),
            Self::Listen(err) => write!(f, "listen error: {err}"),
            Self::Forward => write!(f, "forwarding error"),
        }
    }
}

impl<T> TrySendError<T> {
    fn from_remote_send_error(err: RemoteSendError, value: T) -> Self {
        match err {
            RemoteSendError::Send(err) => Self::Send(err),
            RemoteSendError::Connect(err) => Self::Connect(err),
            RemoteSendError::Listen(err) => Self::Listen(err),
            RemoteSendError::Forward => Self::Forward,
            RemoteSendError::Closed => Self::Closed(value),
        }
    }
}

impl<T> From<SendError<T>> for TrySendError<T> {
    fn from(err: SendError<T>) -> Self {
        match err {
            SendError::Closed(v) => Self::Closed(v),
            SendError::Send(err) => Self::Send(err),
            SendError::Connect(err) => Self::Connect(err),
            SendError::Listen(err) => Self::Listen(err),
            SendError::Forward => Self::Forward,
        }
    }
}

impl<T> TryFrom<TrySendError<T>> for SendError<T> {
    type Error = TrySendError<T>;

    fn try_from(err: TrySendError<T>) -> Result<Self, Self::Error> {
        match err {
            TrySendError::Closed(v) => Ok(Self::Closed(v)),
            TrySendError::Send(err) => Ok(Self::Send(err)),
            TrySendError::Connect(err) => Ok(Self::Connect(err)),
            TrySendError::Forward => Ok(Self::Forward),
            other => Err(other),
        }
    }
}

impl<T> Error for TrySendError<T> where T: fmt::Debug {}

/// The sending half of a bounded MPSC channel.
///
/// Senders are created by [`channel`](super::channel) and may be cloned or
/// transferred to other endpoints. The channel remains open until every sender
/// has been dropped or the receiver closes it.
///
/// Use [`send`](Self::send) to wait for buffer capacity or
/// [`try_send`](Self::try_send) when waiting is not acceptable. Both methods
/// queue a value for transfer; await the returned [`Sending`] handle to observe
/// the outcome of that transfer.
///
/// A sender can be adapted to [`Sink`] with [`SenderSink`].
pub struct Sender<T, Codec = codec::Default, const BUFFER: usize = DEFAULT_BUFFER> {
    tx: Weak<tokio::sync::mpsc::Sender<SendReq<T>>>,
    closed_rx: tokio::sync::watch::Receiver<Option<ClosedReason>>,
    remote_send_err_rx: tokio::sync::watch::Receiver<Option<RemoteSendError>>,
    dropped_tx: tokio::sync::mpsc::Sender<()>,
    max_item_size: usize,
    parallel: Option<usize>,
    _codec: PhantomData<Codec>,
}

impl<T, Codec, const BUFFER: usize> fmt::Debug for Sender<T, Codec, BUFFER> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Sender").finish()
    }
}

impl<T, Codec, const BUFFER: usize> Clone for Sender<T, Codec, BUFFER> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            closed_rx: self.closed_rx.clone(),
            remote_send_err_rx: self.remote_send_err_rx.clone(),
            dropped_tx: self.dropped_tx.clone(),
            max_item_size: self.max_item_size,
            parallel: self.parallel,
            _codec: PhantomData,
        }
    }
}

/// Mpsc sender in transport.
struct TransportedSender {
    /// chmux port number. `None` if closed.
    port: Option<u32>,
    /// Maximum item size in bytes.
    max_item_size: u64,
    /// Additional chmux port numbers for multi base channel operation.
    parallel: Vec<u32>,
}

const fn default_max_item_size() -> u64 {
    u64::MAX
}

crate::versioned::compact::impl_struct! {
    TransportedSender,
    fields {
        port: Option<u32> => "_0",
        #[serde(default)]
        data: PhantomData<()> = PhantomData,
        #[serde(default)]
        codec: PhantomData<()> = PhantomData,
        #[serde(default = "default_max_item_size")]
        max_item_size: u64 => "_1",
        #[serde(default)]
        parallel: Vec<u32> => "_2",
    }
}

/// Drops the strong reference to `target` when channel is closed or dropped.
fn drop_when_closed(
    target: Arc<dyn Any + Send + Sync>, mut dropped_rx: tokio::sync::mpsc::Receiver<()>,
    mut closed_rx: tokio::sync::watch::Receiver<Option<ClosedReason>>,
) {
    wokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = dropped_rx.recv() => break,
                res = closed_rx.changed() => {
                    match res {
                        Ok(()) if closed_rx.borrow().is_some() => break,
                        Ok(()) => (),
                        Err(_) => break,
                    }
                },
            }
        }

        drop(target);
    });
}

impl<T, Codec, const BUFFER: usize> Sender<T, Codec, BUFFER>
where
    T: Send + 'static,
{
    /// Creates a new sender.
    pub(crate) fn new(
        tx: tokio::sync::mpsc::Sender<SendReq<T>>, closed_rx: tokio::sync::watch::Receiver<Option<ClosedReason>>,
        remote_send_err_rx: tokio::sync::watch::Receiver<Option<RemoteSendError>>, parallel: Option<usize>,
    ) -> Self {
        let tx = Arc::new(tx);
        let (dropped_tx, dropped_rx) = tokio::sync::mpsc::channel(1);

        let this = Self {
            tx: Arc::downgrade(&tx),
            closed_rx: closed_rx.clone(),
            remote_send_err_rx,
            dropped_tx,
            max_item_size: DEFAULT_MAX_ITEM_SIZE,
            parallel,
            _codec: PhantomData,
        };

        // Drop strong reference to sender when channel is closed.
        drop_when_closed(tx, dropped_rx, closed_rx);

        this
    }

    /// Creates a new sender that is closed.
    pub(crate) fn new_closed() -> Self {
        Self {
            tx: Weak::new(),
            closed_rx: tokio::sync::watch::channel(Some(ClosedReason::Closed)).1,
            remote_send_err_rx: tokio::sync::watch::channel(None).1,
            dropped_tx: tokio::sync::mpsc::channel(1).0,
            max_item_size: DEFAULT_MAX_ITEM_SIZE,
            parallel: None,
            _codec: PhantomData,
        }
    }

    /// Sends a value over this channel.
    ///
    /// This method waits until the channel has capacity. `Ok` means that the value
    /// was queued for transfer; await the returned [`Sending`] handle to learn
    /// whether the transfer completed.
    ///
    /// # Error reporting
    ///
    /// Because transfer happens asynchronously, this method can report an error
    /// caused by an earlier value. Use [`SendError::is_item_specific`] when the
    /// distinction matters.
    ///
    /// # Cancel safety
    ///
    /// If this method is used in [`tokio::select!`] and another branch completes
    /// first, the value is not queued. Because `send` owns the value, canceling the
    /// future drops it. Use [`reserve`](Self::reserve) when the value must be
    /// retained until capacity has been secured.
    pub async fn send(&self, value: T) -> Result<Sending<T>, SendError<T>> {
        if let Some(err) = self.remote_send_err_rx.borrow().as_ref() {
            return Err(SendError::from_remote_send_error(err.clone(), value));
        }

        match self.tx.upgrade() {
            Some(tx) => {
                let (req, sent) = send_req(Ok(value));
                match tx.send(req).await {
                    Ok(()) => Ok(sent),
                    Err(err) => Err(SendError::Closed(err.0.value.expect("unreachable"))),
                }
            }
            None => Err(SendError::Closed(value)),
        }
    }

    /// Attempts to immediately send a message over this channel.
    ///
    /// This method never waits for capacity. If the channel is full, it returns
    /// [`TrySendError::Full`] containing the value.
    ///
    /// `Ok` means that the value was queued for transfer; await the returned
    /// [`Sending`] handle to learn whether the transfer completed.
    ///
    /// # Error reporting
    /// An error may be delayed and therefore caused by a previous invocation.
    pub fn try_send(&self, value: T) -> Result<Sending<T>, TrySendError<T>> {
        if let Some(err) = self.remote_send_err_rx.borrow().as_ref() {
            return Err(TrySendError::from_remote_send_error(err.clone(), value));
        }

        match self.tx.upgrade() {
            Some(tx) => {
                let (req, sent) = send_req(Ok(value));
                match tx.try_send(req) {
                    Ok(()) => Ok(sent),
                    Err(tokio::sync::mpsc::error::TrySendError::Full(err)) => {
                        Err(TrySendError::Full(err.value.expect("unreachable")))
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(err)) => {
                        Err(TrySendError::Closed(err.value.expect("unreachable")))
                    }
                }
            }
            None => Err(TrySendError::Closed(value)),
        }
    }

    /// Sends a value while blocking the current thread until capacity is available.
    ///
    /// `Ok` means that the value was queued for transfer; the returned
    /// [`Sending`] handle reports whether the transfer completed.
    ///
    /// # Error reporting
    /// An error may be delayed and thus be caused by a previous invocation.
    ///
    /// # Panics
    ///
    /// Panics if called from an asynchronous execution context.
    pub fn blocking_send(&self, value: T) -> Result<Sending<T>, SendError<T>> {
        wokio::task::block_on(self.send(value))
    }

    /// Reserves capacity to send one value.
    ///
    /// Once this method returns, the reserved slot belongs to the returned
    /// [`Permit`]. Dropping the permit without sending releases the capacity.
    ///
    /// # Error reporting
    /// Sending and error reporting are done asynchronously.
    /// Thus, the reporting of an error may be delayed and this function may
    /// return errors caused by previous invocations.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel safe. If canceled before it returns, no capacity is
    /// reserved.
    pub async fn reserve(&self) -> Result<Permit<T>, SendError<()>> {
        if let Some(err) = self.remote_send_err_rx.borrow().as_ref() {
            return Err(SendError::from_remote_send_error(err.clone(), ()));
        }

        match self.tx.upgrade() {
            Some(tx) => {
                let tx = (*tx).clone();
                match tx.reserve_owned().await {
                    Ok(permit) => Ok(Permit(permit)),
                    Err(_) => Err(SendError::Closed(())),
                }
            }
            _ => Err(SendError::Closed(())),
        }
    }

    /// Tries to reserve capacity for one value without waiting.
    ///
    /// If a slot is available, it becomes owned by the returned [`Permit`].
    /// Dropping that permit without sending releases the capacity again.
    ///
    /// # Error reporting
    /// Sending and error reporting are done asynchronously.
    /// Thus, the reporting of an error may be delayed and this function may
    /// return errors caused by previous invocations.
    pub fn try_reserve(&self) -> Result<Permit<T>, TrySendError<()>> {
        if let Some(err) = self.remote_send_err_rx.borrow().as_ref() {
            return Err(TrySendError::from_remote_send_error(err.clone(), ()));
        }

        match self.tx.upgrade() {
            Some(tx) => {
                let tx = (*tx).clone();
                match tx.try_reserve_owned() {
                    Ok(permit) => Ok(Permit(permit)),
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Err(TrySendError::Full(())),
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err(TrySendError::Closed(())),
                }
            }
            _ => Err(TrySendError::Closed(())),
        }
    }

    /// Returns the current capacity of the channel.
    ///
    /// Zero is returned when the channel has been closed or an error has occurred.
    pub fn capacity(&self) -> usize {
        match self.tx.upgrade() {
            Some(tx) => tx.capacity(),
            None => 0,
        }
    }

    /// Completes when the receiver has been closed, dropped or the connection failed.
    ///
    /// Use [closed_reason](Self::closed_reason) to obtain the cause for closure.
    pub async fn closed(&self) {
        let mut closed = self.closed_rx.clone();
        while closed.borrow().is_none() {
            if closed.changed().await.is_err() {
                break;
            }
        }
    }

    /// Returns the reason the channel was closed.
    ///
    /// Returns [None] if the channel is not closed.
    pub fn closed_reason(&self) -> Option<ClosedReason> {
        match (self.closed_rx.borrow().clone(), self.remote_send_err_rx.borrow().as_ref()) {
            (Some(reason), _) => Some(reason),
            (None, Some(err)) => Some(match err {
                RemoteSendError::Closed => ClosedReason::Closed,
                RemoteSendError::Send(err) => {
                    ClosedReason::from_send_error_kind(err).unwrap_or(ClosedReason::Failed)
                }
                RemoteSendError::Connect(err) => ClosedReason::from_connect_error(err),
                RemoteSendError::Listen(_) | RemoteSendError::Forward => ClosedReason::Failed,
            }),
            (None, None) => None,
        }
    }

    /// Returns whether the receiver has been closed, dropped or the connection failed.
    ///
    /// Use [closed_reason](Self::closed_reason) to obtain the cause for closure.
    pub fn is_closed(&self) -> bool {
        self.closed_reason().is_some()
    }

    /// Sets the codec that will be used when sending this sender to a remote endpoint.
    pub fn set_codec<NewCodec>(self) -> Sender<T, NewCodec, BUFFER> {
        Sender {
            tx: self.tx.clone(),
            closed_rx: self.closed_rx.clone(),
            remote_send_err_rx: self.remote_send_err_rx.clone(),
            dropped_tx: self.dropped_tx.clone(),
            max_item_size: self.max_item_size,
            parallel: self.parallel,
            _codec: PhantomData,
        }
    }

    /// Sets the buffer size that will be used when sending this sender to a remote endpoint.
    pub fn set_buffer<const NEW_BUFFER: usize>(self) -> Sender<T, Codec, NEW_BUFFER> {
        assert!(NEW_BUFFER > 0, "buffer size must not be zero");
        Sender {
            tx: self.tx.clone(),
            closed_rx: self.closed_rx.clone(),
            remote_send_err_rx: self.remote_send_err_rx.clone(),
            dropped_tx: self.dropped_tx.clone(),
            max_item_size: self.max_item_size,
            parallel: self.parallel,
            _codec: PhantomData,
        }
    }

    /// The maximum allowed item size in bytes.
    pub fn max_item_size(&self) -> usize {
        self.max_item_size
    }

    /// Sets the maximum allowed item size in bytes.
    pub fn set_max_item_size(&mut self, max_item_size: usize) {
        self.max_item_size = max_item_size;
    }

    /// Number of additional parallel transfer channels.
    ///
    /// If `None` the default value from [`Cfg::mpsc_parallel`](crate::Cfg::mpsc_parallel) is used.
    pub fn parallel(&self) -> Option<usize> {
        self.parallel
    }

    /// Sets the number of additional parallel transfer channels.
    ///
    /// This must be set before sending the sender to a remote endpoint.
    ///
    /// A value of 1 is not recommended; see [`Cfg::mpsc_parallel`](crate::Cfg::mpsc_parallel)
    /// for what to choose.
    ///
    /// If `None` the default value from [`Cfg::mpsc_parallel`](crate::Cfg::mpsc_parallel) is used.
    pub fn set_parallel(&mut self, parallel: Option<usize>) {
        self.parallel = parallel;
    }
}

/// Reserved capacity to send one value over an MPSC channel.
///
/// Permits are created by [`Sender::reserve`] and [`Sender::try_reserve`].
/// Dropping a permit without calling [`send`](Self::send) releases its capacity.
pub struct Permit<T>(tokio::sync::mpsc::OwnedPermit<SendReq<T>>);

impl<T> Permit<T>
where
    T: Send,
{
    /// Queues a value using the reserved capacity.
    ///
    /// This consumes the permit and hands the value to the channel immediately,
    /// but transfer still continues asynchronously. The returned [`Sending`]
    /// handle reports whether that transfer completed.
    pub fn send(self, value: T) -> Sending<T> {
        let (req, sent) = send_req(Ok(value));
        self.0.send(req);
        sent
    }
}

impl<T, Codec, const BUFFER: usize> Drop for Sender<T, Codec, BUFFER> {
    fn drop(&mut self) {
        // empty
    }
}

impl<T, Codec, const BUFFER: usize> Serialize for Sender<T, Codec, BUFFER>
where
    T: RemoteSend,
    Codec: codec::Codec,
{
    /// Serializes this sender for sending over a chmux channel.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut parallel = Vec::new();

        let port = match self.tx.upgrade() {
            // Channel is open.
            Some(tx) => {
                // Prepare channel for takeover.
                let closed_rx = self.closed_rx.clone();
                let remote_send_err_rx = self.remote_send_err_rx.clone();
                let max_item_size = self.max_item_size;

                // Request additional paralllel chmux channels.
                let mut parallel_rxs = Vec::new();
                let mpsc_parallel = PortSerializer::with_storage(|storage| storage.cfg().mpsc_parallel)?;
                for _ in 0..self.parallel.unwrap_or(mpsc_parallel) {
                    let (parallel_tx, parallel_rx) = tokio::sync::oneshot::channel();
                    let Ok(parallel_port) =
                        PortSerializer::connect_port::<S::Error, _, _>(async move |connect| {
                            if let Ok((_raw_tx, raw_rx)) = connect.await {
                                let _ = parallel_tx.send(raw_rx);
                            }
                        })
                    else {
                        continue;
                    };
                    parallel.push(parallel_port);
                    parallel_rxs.push(parallel_rx);
                }

                Some(PortSerializer::connect_port(async move |connect| {
                    // Establish base chmux channel.
                    let (raw_tx, raw_rx) = match connect.await {
                        Ok(tx_rx) => tx_rx,
                        Err(err) => {
                            let _ = tx.send(SendReq::new(Err(RecvError::Connect(err)))).await;
                            return;
                        }
                    };

                    // Establish additional parallel chmux channels.
                    let mut raw_rxs = vec![raw_rx];
                    for raw_rx in future::join_all(parallel_rxs).await.into_iter().flatten() {
                        raw_rxs.push(raw_rx);
                    }

                    super::recv_impl(
                        ErasedDeserializer::new::<CompactResult<T, RecvError>, Codec>(),
                        &*tx,
                        raw_tx,
                        raw_rxs,
                        remote_send_err_rx,
                        closed_rx,
                        max_item_size,
                    )
                    .await;
                })?)
            }
            None => {
                // Channel is closed.
                None
            }
        };

        // Encode chmux port number in transport type and serialize it.
        TransportedSender { port, max_item_size: self.max_item_size.try_into().unwrap_or(u64::MAX), parallel }
            .serialize(serializer)
    }
}

impl<'de, T, Codec, const BUFFER: usize> Deserialize<'de> for Sender<T, Codec, BUFFER>
where
    T: RemoteSend,
    Codec: codec::Codec,
{
    /// Deserializes this sender after it has been received over a chmux channel.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        assert!(BUFFER > 0, "BUFFER must not be zero");

        // Get chmux port number from deserialized transport type.
        let TransportedSender { port, max_item_size, parallel } = TransportedSender::deserialize(deserializer)?;
        let max_item_size = usize::try_from(max_item_size).unwrap_or(usize::MAX);

        let Some(port) = port else {
            // Received closed channel.
            return Ok(Self::new_closed());
        };

        // Create internal communication channels.
        let (tx, rx) = tokio::sync::mpsc::channel(BUFFER);
        let (closed_tx, closed_rx) = tokio::sync::watch::channel(None);
        let (remote_send_err_tx, remote_send_err_rx) = tokio::sync::watch::channel(None);

        // Accept additional paralllel chmux channels.
        let mut parallel_txs = Vec::with_capacity(parallel.len());
        for parallel_port in parallel {
            let (parallel_tx, parallel_rx) = tokio::sync::oneshot::channel();
            if PortDeserializer::accept::<D::Error, _, _>(parallel_port, async move |request| {
                if let Ok((raw_tx, _raw_rx)) = request.accept().await {
                    let _ = parallel_tx.send(raw_tx);
                }
            })
            .is_ok()
            {
                parallel_txs.push(parallel_rx);
            }
        }
        let parallel = parallel_txs.len();

        // Accept chmux port request.
        PortDeserializer::accept(port, async move |request| {
            // Accept chmux connection request.
            let (raw_tx, raw_rx) = match request.accept().await {
                Ok(tx_rx) => tx_rx,
                Err(err) => {
                    let _ = remote_send_err_tx.send(Some(RemoteSendError::Listen(err)));
                    return;
                }
            };

            // Establish additional parallel chmux channels.
            let mut raw_txs = vec![raw_tx];
            for raw_tx in future::join_all(parallel_txs).await.into_iter().flatten() {
                raw_txs.push(raw_tx);
            }

            super::send_impl(
                ErasedSerializer::new::<CompactResult<T, RecvError>, Codec>(),
                Box::new(rx),
                raw_txs,
                raw_rx,
                remote_send_err_tx,
                closed_tx,
                max_item_size,
            )
            .await;
        })?;

        Ok(Self::new(tx, closed_rx, remote_send_err_rx, Some(parallel)))
    }
}

type ReserveRet<T, Codec, const BUFFER: usize> = (Result<Permit<T>, SendError<()>>, Sender<T, Codec, BUFFER>);

/// A wrapper around an mpsc [Sender] that implements [Sink].
pub struct SenderSink<T, Codec = codec::Default, const BUFFER: usize = DEFAULT_BUFFER> {
    tx: Option<Sender<T, Codec, BUFFER>>,
    permit: Option<Permit<T>>,
    reserve: Option<ReusableBoxFuture<'static, ReserveRet<T, Codec, BUFFER>>>,
    sending: Option<Sending<T>>,
}

impl<T, Codec, const BUFFER: usize> fmt::Debug for SenderSink<T, Codec, BUFFER> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("SenderSink").field("ready", &self.permit.is_some()).finish()
    }
}

impl<T, Codec, const BUFFER: usize> SenderSink<T, Codec, BUFFER>
where
    T: Send + 'static,
    Codec: codec::Codec,
{
    /// Wraps a [Sender] to provide a [Sink].
    pub fn new(tx: Sender<T, Codec, BUFFER>) -> Self {
        Self {
            tx: Some(tx.clone()),
            permit: None,
            reserve: Some(ReusableBoxFuture::new(Self::make_reserve(tx))),
            sending: None,
        }
    }

    fn new_closed() -> Self {
        Self { tx: None, permit: None, reserve: None, sending: None }
    }

    /// Gets a reference to the [Sender] of the underlying channel.
    ///
    /// `None` is returned if the sink has been closed.
    pub fn get_ref(&self) -> Option<&Sender<T, Codec, BUFFER>> {
        self.tx.as_ref()
    }

    async fn make_reserve(tx: Sender<T, Codec, BUFFER>) -> ReserveRet<T, Codec, BUFFER> {
        let result = tx.reserve().await;
        (result, tx)
    }
}

impl<T, Codec, const BUFFER: usize> Clone for SenderSink<T, Codec, BUFFER>
where
    T: Send + 'static,
    Codec: codec::Codec,
{
    fn clone(&self) -> Self {
        match self.tx.clone() {
            Some(tx) => Self::new(tx),
            None => Self::new_closed(),
        }
    }
}

impl<T, Codec, const BUFFER: usize> Sink<T> for SenderSink<T, Codec, BUFFER>
where
    T: Send + 'static,
    Codec: codec::Codec,
{
    type Error = SendError<()>;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        if self.permit.is_some() {
            return Poll::Ready(Ok(()));
        }

        let Some(reserve) = self.reserve.as_mut() else { return Poll::Ready(Err(SendError::Closed(()))) };
        let (permit, tx) = ready!(reserve.poll(cx));
        reserve.set(Self::make_reserve(tx));

        self.permit = Some(permit?);

        Poll::Ready(Ok(()))
    }

    fn start_send(mut self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        let permit = self.permit.take().expect("SenderSink is not ready for sending");
        self.sending = Some(permit.send(item));
        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let Some(sending) = self.sending.as_mut() else { return Poll::Ready(Ok(())) };

        let res = ready!(sending.poll_unpin(cx));
        self.sending = None;

        Poll::Ready(res.map_err(|err| match err {
            SendingError::Send(base) => SendError::Send(base.kind),
            SendingError::Dropped => SendError::Closed(()),
        }))
    }

    fn poll_close(mut self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        self.tx = None;
        self.permit = None;
        self.reserve = None;
        Poll::Ready(Ok(()))
    }
}

impl<T, Codec, const BUFFER: usize> From<Sender<T, Codec, BUFFER>> for SenderSink<T, Codec, BUFFER>
where
    T: Send + 'static,
    Codec: codec::Codec,
{
    fn from(tx: Sender<T, Codec, BUFFER>) -> Self {
        Self::new(tx)
    }
}
