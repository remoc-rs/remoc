use futures::{Stream, future, ready};
use serde::{Deserialize, Serialize};
use std::{
    convert::TryFrom,
    error::Error,
    fmt,
    marker::PhantomData,
    pin::Pin,
    sync::Mutex,
    task::{Context, Poll},
};

use super::{
    super::{
        ClosedReason, DEFAULT_BUFFER, DEFAULT_MAX_ITEM_SIZE, RemoteSendError,
        base::{self, PortDeserializer, PortSerializer},
    },
    Distributor, SendReq,
};
use crate::{
    RemoteSend, chmux,
    codec::{self, ErasedDeserializer, ErasedSerializer},
    versioned::result::Result as CompactResult,
};
/// An error returned while receiving from an MPSC channel.
///
/// Channel closure is represented by `Ok(None)` from [`Receiver::recv`], not by
/// this type. These variants describe failures while connecting or transferring
/// values.
#[derive(Clone, Debug)]
pub enum RecvError {
    /// Receiving or decoding a value failed; see [`base::RecvError`].
    Receive(base::RecvError),
    /// Opening a channel contained in a received value failed; see [`chmux::ConnectError`].
    Connect(chmux::ConnectError),
    /// Preparing a channel contained in a received value failed; see [`chmux::ListenerError`].
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

impl TryFrom<TryRecvError> for RecvError {
    type Error = TryRecvError;

    fn try_from(err: TryRecvError) -> Result<Self, Self::Error> {
        match err {
            TryRecvError::Closed => Err(TryRecvError::Closed),
            TryRecvError::Empty => Err(TryRecvError::Empty),
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
            Self::Connect(_) | Self::Listen(_) => true,
            Self::Remote(Some(err)) => err.is_disconnected(),
            Self::Remote(None) => false,
        }
    }
}

/// An error returned when attempting to receive without waiting.
#[derive(Clone, Debug)]
pub enum TryRecvError {
    /// All channel senders have been dropped.
    Closed,
    /// Currently no value is ready to receive, but values may still arrive
    /// in the future.
    Empty,
    /// Receiving or decoding a value failed; see [`base::RecvError`].
    Receive(base::RecvError),
    /// Opening a channel contained in a received value failed; see [`chmux::ConnectError`].
    Connect(chmux::ConnectError),
    /// Preparing a channel contained in a received value failed; see [`chmux::ListenerError`].
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
        Closed => "_0",
        Empty => "_1",
        Receive(err: base::RecvError) => "_2",
        Connect(err: chmux::ConnectError) => "_3",
        Listen(err: chmux::ListenerError) => "_4",
        Remote(err: Option<Box<RecvError>>) => "_50",
    }
}

impl fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "channel is closed"),
            Self::Empty => write!(f, "channel is empty"),
            Self::Receive(err) => write!(f, "receive error: {err}"),
            Self::Connect(err) => write!(f, "connect error: {err}"),
            Self::Listen(err) => write!(f, "listen error: {err}"),
            Self::Remote(Some(err)) => write!(f, "remote {err}"),
            Self::Remote(None) => write!(f, "unknown remote error"),
        }
    }
}

impl From<RecvError> for TryRecvError {
    fn from(err: RecvError) -> Self {
        match err {
            RecvError::Receive(err) => Self::Receive(err),
            RecvError::Connect(err) => Self::Connect(err),
            RecvError::Listen(err) => Self::Listen(err),
            RecvError::Remote(err) => Self::Remote(err),
        }
    }
}

impl Error for TryRecvError {}

impl TryRecvError {
    /// Returns whether the connection was rejected or failed.
    pub fn is_disconnected(&self) -> bool {
        match self {
            Self::Empty => false,
            Self::Receive(err) => err.is_disconnected(),
            Self::Closed | Self::Connect(_) | Self::Listen(_) => true,
            Self::Remote(Some(err)) => err.is_disconnected(),
            Self::Remote(None) => false,
        }
    }
}

/// The receiving half of a bounded MPSC channel.
///
/// Receivers are created by [`channel`](super::channel) and can be transferred
/// to another endpoint. There is only one receiver, but any number of cloned or
/// transferred [`Sender`](super::Sender)s may feed it.
///
/// Dropping the receiver closes the channel. Call [`close`](Self::close) instead
/// when queued values should still be drained.
pub struct Receiver<
    T,
    Codec = codec::Default,
    const BUFFER: usize = DEFAULT_BUFFER,
    const MAX_ITEM_SIZE: usize = DEFAULT_MAX_ITEM_SIZE,
> {
    inner: Option<ReceiverInner<T>>,
    #[allow(clippy::type_complexity)]
    successor_tx: Mutex<Option<tokio::sync::oneshot::Sender<ReceiverInner<T>>>>,
    final_err: Option<RecvError>,
    remote_max_item_size: Option<usize>,
    parallel: Option<usize>,
    _codec: PhantomData<Codec>,
}

impl<T, Codec, const BUFFER: usize, const MAX_ITEM_SIZE: usize> fmt::Debug
    for Receiver<T, Codec, BUFFER, MAX_ITEM_SIZE>
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Receiver").finish()
    }
}

pub(crate) struct ReceiverInner<T> {
    rx: tokio::sync::mpsc::Receiver<SendReq<T>>,
    closed_tx: tokio::sync::watch::Sender<Option<ClosedReason>>,
    remote_send_err_tx: tokio::sync::watch::Sender<Option<RemoteSendError>>,
    closed: bool,
}

/// Mpsc receiver in transport.
struct TransportedReceiver {
    /// chmux port number.
    port: u32,
    /// Receiver has been closed.
    closed: bool,
    /// Maximum item size.
    max_item_size: u64,
    /// Additional chmux port numbers for multi base channel operation.
    parallel: Vec<u32>,
}

crate::versioned::compact::impl_struct! {
    TransportedReceiver,
    fields {
        port: u32 => "_0",
        #[serde(default)]
        data: PhantomData<()> = PhantomData,
        #[serde(default)]
        codec: PhantomData<()> = PhantomData,
        #[serde(default)]
        closed: bool => "_1",
        #[serde(default)]
        max_item_size: u64 => "_2",
        #[serde(default)]
        parallel: Vec<u32> => "_3",
    }
}

impl<T, Codec> Receiver<T, Codec>
where
    T: RemoteSend,
    Codec: codec::Codec,
{
    /// Creates a receiver that forwards values from the given local mpsc receiver.
    ///
    /// The returned receiver may be sent to remote endpoints via channels.
    ///
    /// Any send errors that occur during forwarding are silently dropped;
    /// use [`forward`](super::forward) if you need to observe them.
    pub fn forwarded(local_rx: tokio::sync::mpsc::Receiver<T>) -> Self {
        let (_fwd, rx) = super::forward(local_rx);
        rx
    }
}

impl<T, Codec, const BUFFER: usize, const MAX_ITEM_SIZE: usize> Receiver<T, Codec, BUFFER, MAX_ITEM_SIZE> {
    pub(crate) fn new(
        rx: tokio::sync::mpsc::Receiver<SendReq<T>>, closed_tx: tokio::sync::watch::Sender<Option<ClosedReason>>,
        closed: bool, remote_send_err_tx: tokio::sync::watch::Sender<Option<RemoteSendError>>,
        remote_max_item_size: Option<usize>, parallel: Option<usize>,
    ) -> Self {
        Self {
            inner: Some(ReceiverInner { rx, closed_tx, remote_send_err_tx, closed }),
            successor_tx: Mutex::new(None),
            final_err: None,
            remote_max_item_size,
            parallel,
            _codec: PhantomData,
        }
    }

    /// Receives the next value for this receiver.
    ///
    /// Returns `Ok(None)` after all senders have been dropped and all queued values
    /// have been received.
    ///
    /// When a receive error occurs due to a connection failure and other senders are still
    /// present, it is held back and returned after all other senders have been dropped or failed.
    /// Use [error](Self::error) to check if such an error is present.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel safe.
    /// If it is cancelled, it is guaranteed that no messages were received on this channel.
    pub async fn recv(&mut self) -> Result<Option<T>, RecvError> {
        loop {
            match self.inner.as_mut().unwrap().rx.recv().await {
                Some(send_req) => match send_req.ack() {
                    Ok(value_opt) => return Ok(Some(value_opt)),
                    Err(err) => {
                        if err.is_disconnected() {
                            if self.final_err.is_none() {
                                self.final_err = Some(err);
                            }
                            continue;
                        } else {
                            return Err(err);
                        }
                    }
                },
                None => match self.take_error() {
                    Some(err) => return Err(err),
                    None => return Ok(None),
                },
            }
        }
    }

    /// Polls to receive the next message on this channel.
    ///
    /// This function returns `Poll::Ready(Ok(None))` when all channel senders have been dropped.
    ///
    /// When a receive error occurs due to a connection failure and other senders are still
    /// present, it is held back and returned after all other senders have been dropped or failed.
    /// Use [error](Self::error) to check if such an error is present.
    pub fn poll_recv(&mut self, cx: &mut Context) -> Poll<Result<Option<T>, RecvError>> {
        loop {
            match ready!(self.inner.as_mut().unwrap().rx.poll_recv(cx)) {
                Some(send_req) => match send_req.ack() {
                    Ok(value_opt) => return Poll::Ready(Ok(Some(value_opt))),
                    Err(err) => {
                        if err.is_disconnected() {
                            if self.final_err.is_none() {
                                self.final_err = Some(err);
                            }
                            continue;
                        } else {
                            return Poll::Ready(Err(err));
                        }
                    }
                },
                None => match self.take_error() {
                    Some(err) => return Poll::Ready(Err(err)),
                    None => return Poll::Ready(Ok(None)),
                },
            }
        }
    }

    /// Tries to receive the next value without waiting.
    ///
    /// Returns [`TryRecvError::Closed`] when all senders have been dropped and the
    /// queue is empty, or [`TryRecvError::Empty`] if a value may still arrive.
    ///
    /// When a receive error occurs due to a connection failure and other senders are still
    /// present, it is held back and returned after all other senders have been dropped or failed.
    /// Use [error](Self::error) to check if such an error is present.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        loop {
            match self.inner.as_mut().unwrap().rx.try_recv() {
                Ok(send_req) => match send_req.ack() {
                    Ok(value_opt) => return Ok(value_opt),
                    Err(err) => {
                        if err.is_disconnected() {
                            if self.final_err.is_none() {
                                self.final_err = Some(err);
                            }
                            continue;
                        } else {
                            return Err(err.into());
                        }
                    }
                },
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => match self.take_error() {
                    Some(err) => return Err(err.into()),
                    None => return Err(TryRecvError::Closed),
                },
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return Err(TryRecvError::Empty),
            }
        }
    }

    /// Receives a value while blocking the current thread.
    ///
    /// Returns `Ok(None)` after all senders have been dropped and all queued values
    /// have been received.
    ///
    /// # Panics
    ///
    /// Panics if called from an asynchronous execution context.
    pub fn blocking_recv(&mut self) -> Result<Option<T>, RecvError> {
        wokio::task::block_on(self.recv())
    }

    /// Receives the next values for this receiver and extends `buffer`.
    ///
    /// This method extends `buffer` by no more than a fixed number of values as specified by `limit`.
    /// If `limit` is zero, the function immediately returns 0.
    ///
    /// The return value is the number of values added to buffer.
    /// The method returns `Ok(0)` when the channel has been closed.
    ///
    /// The number of values added to the buffer can never exceed the generic parameter `BUFFER`
    /// of this receiver.
    ///
    /// For `limit > 0`, if there are no messages in the channel’s queue, but the channel has not
    /// yet been closed, this method will sleep until a message is sent or the channel is closed.
    ///
    /// If a non-final receive error occurs (for example due to a message being not
    /// deserializable), the error is reported but already buffered messages from the
    /// same batch are lost.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel safe.
    /// If it is cancelled, it is guaranteed that no messages were received on this channel.
    pub async fn recv_many(&mut self, buffer: &mut Vec<T>, limit: usize) -> Result<usize, RecvError> {
        if limit == 0 {
            return Ok(0);
        }

        let mut send_req_buf = Vec::with_capacity(limit);
        let n = self.inner.as_mut().unwrap().rx.recv_many(&mut send_req_buf, limit).await;

        if n == 0 {
            match self.take_error() {
                Some(err) => return Err(err),
                None => return Ok(0),
            }
        }

        let mut p = 0;
        for send_req in send_req_buf {
            match send_req.ack() {
                Ok(value_opt) => {
                    buffer.push(value_opt);
                    p += 1;
                }
                Err(err) => {
                    if err.is_disconnected() {
                        if self.final_err.is_none() {
                            self.final_err = Some(err);
                        }
                    } else {
                        return Err(err);
                    }
                }
            }
        }

        Ok(p)
    }

    /// Returns the number of values available for receiving.
    ///
    /// This might be overestimated if a receive error occurred.
    /// However, in this case the receive functions will return an error.
    pub fn len(&self) -> usize {
        self.inner.as_ref().unwrap().rx.len()
    }

    /// Returns whether no values are available for receiving.
    pub fn is_empty(&self) -> bool {
        self.inner.as_ref().unwrap().rx.is_empty()
    }

    /// Closes the receiving half of a channel without dropping it.
    ///
    /// Outstanding queued values can still be received afterwards, but further
    /// send attempts fail and senders observe the channel as closed. This is
    /// useful when shutdown should stop new work while draining accepted items.
    pub fn close(&mut self) {
        let inner = self.inner.as_mut().unwrap();
        let _ = inner.closed_tx.send(Some(ClosedReason::Closed));
        inner.closed = true;
    }

    /// Returns the first error that occurred during receiving due to a connection failure,
    /// but is being held back because other senders are still connected to this receiver.
    ///
    /// Use [take_error](Self::take_error) to clear it.
    pub fn error(&self) -> &Option<RecvError> {
        &self.final_err
    }

    /// Returns the held back error and clears it.
    ///
    /// See [recv](Self::recv) and [error](Self::error) for details.
    pub fn take_error(&mut self) -> Option<RecvError> {
        self.final_err.take()
    }

    /// Sets the codec that will be used when sending this receiver to a remote endpoint.
    pub fn set_codec<NewCodec>(mut self) -> Receiver<T, NewCodec, BUFFER, MAX_ITEM_SIZE> {
        Receiver {
            inner: self.inner.take(),
            successor_tx: Mutex::new(None),
            final_err: self.final_err.clone(),
            remote_max_item_size: self.remote_max_item_size,
            parallel: self.parallel,
            _codec: PhantomData,
        }
    }

    /// Sets the buffer size that will be used when sending this receiver to a remote endpoint.
    pub fn set_buffer<const NEW_BUFFER: usize>(mut self) -> Receiver<T, Codec, NEW_BUFFER, MAX_ITEM_SIZE> {
        assert!(NEW_BUFFER > 0, "buffer size must not be zero");
        Receiver {
            inner: self.inner.take(),
            successor_tx: Mutex::new(None),
            final_err: self.final_err.clone(),
            remote_max_item_size: self.remote_max_item_size,
            parallel: self.parallel,
            _codec: PhantomData,
        }
    }

    /// The maximum item size in bytes.
    pub fn max_item_size(&self) -> usize {
        MAX_ITEM_SIZE
    }

    /// Sets the maximum item size in bytes.
    pub fn set_max_item_size<const NEW_MAX_ITEM_SIZE: usize>(
        mut self,
    ) -> Receiver<T, Codec, BUFFER, NEW_MAX_ITEM_SIZE> {
        Receiver {
            inner: self.inner.take(),
            successor_tx: Mutex::new(None),
            final_err: self.final_err.clone(),
            remote_max_item_size: self.remote_max_item_size,
            parallel: self.parallel,
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

    /// Number of additional parallel transfer channels.
    ///
    /// If `None` the default value from [`Cfg::mpsc_parallel`](crate::Cfg::mpsc_parallel) is used.
    pub fn parallel(&self) -> Option<usize> {
        self.parallel
    }

    /// Sets the number of additional parallel transfer channels.
    ///
    /// This must be set before sending the receiver to a remote endpoint.
    ///
    /// A value of 1 is not recommended; see [`Cfg::mpsc_parallel`](crate::Cfg::mpsc_parallel)
    /// for what to choose.
    ///
    /// If `None` the default value from [`Cfg::mpsc_parallel`](crate::Cfg::mpsc_parallel) is used.
    pub fn set_parallel(&mut self, parallel: Option<usize>) {
        self.parallel = parallel;
    }
}

impl<T, Codec, const BUFFER: usize, const MAX_ITEM_SIZE: usize> Receiver<T, Codec, BUFFER, MAX_ITEM_SIZE>
where
    T: RemoteSend + Clone,
    Codec: codec::Codec,
{
    /// Distributes received items over multiple subscribed receivers.
    ///
    /// Each value is delivered to exactly one subscribed receiver. If
    /// `wait_on_empty` is `true`, the distributor stays alive while no
    /// subscribers are present and resumes delivery once a new subscriber is
    /// added; otherwise it terminates as soon as the last subscriber goes away.
    ///
    /// # Example
    ///
    /// In the following example the server distributes a work queue over two
    /// workers running on the client.
    ///
    /// ```
    /// use remoc::prelude::*;
    ///
    /// // This would be run on the server.
    /// async fn server(mut tx: rch::base::Sender<rch::mpsc::Receiver<u32>>) {
    ///     let (work_tx, work_rx) = rch::mpsc::channel(1);
    ///     let distributor = work_rx.distribute(false);
    ///
    ///     // Each subscribed receiver can be sent to another endpoint.
    ///     for _ in 0..2 {
    ///         let (worker_rx, _handle) = distributor.subscribe().await.unwrap();
    ///         tx.send(worker_rx).await.unwrap();
    ///     }
    ///
    ///     for i in 1..=10 {
    ///         work_tx.send(i).await.unwrap();
    ///     }
    ///
    ///     // Keep the distributor alive until all work has been handed out.
    ///     drop(work_tx);
    ///     distributor.closed().await;
    /// }
    ///
    /// // This would be run on the client.
    /// async fn client(mut rx: rch::base::Receiver<rch::mpsc::Receiver<u32>>) {
    ///     let worker = |mut work_rx: rch::mpsc::Receiver<u32>| async move {
    ///         let mut sum = 0;
    ///         while let Some(work) = work_rx.recv().await.unwrap() {
    ///             sum += work;
    ///         }
    ///         sum
    ///     };
    ///
    ///     let first = rx.recv().await.unwrap().unwrap();
    ///     let second = rx.recv().await.unwrap().unwrap();
    ///
    ///     // Each value is processed by exactly one worker.
    ///     let (a, b) = tokio::join!(worker(first), worker(second));
    ///     assert_eq!(a + b, 55);
    /// }
    /// # tokio_test::block_on(remoc::doctest::client_server(server, client));
    /// ```
    pub fn distribute(self, wait_on_empty: bool) -> Distributor<T, Codec, BUFFER, MAX_ITEM_SIZE> {
        Distributor::new(self, wait_on_empty)
    }
}

impl<T, Codec, const BUFFER: usize, const MAX_ITEM_SIZE: usize> Drop
    for Receiver<T, Codec, BUFFER, MAX_ITEM_SIZE>
{
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            let mut successor_tx = self.successor_tx.lock().unwrap();
            match successor_tx.take() {
                Some(successor_tx) => {
                    let _ = successor_tx.send(inner);
                }
                _ => {
                    if !inner.closed {
                        let _ = inner.closed_tx.send(Some(ClosedReason::Dropped));
                    }
                }
            }
        }
    }
}

impl<T, Codec, const BUFFER: usize, const MAX_ITEM_SIZE: usize> Serialize
    for Receiver<T, Codec, BUFFER, MAX_ITEM_SIZE>
where
    T: RemoteSend,
    Codec: codec::Codec,
{
    /// Serializes this receiver for sending over a chmux channel.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Register successor of this receiver.
        let (successor_tx, successor_rx) = tokio::sync::oneshot::channel();
        *self.successor_tx.lock().unwrap() = Some(successor_tx);

        // Request additional paralllel chmux channels.
        let mut parallel = Vec::new();
        let mut parallel_rxs = Vec::new();
        let mpsc_parallel = PortSerializer::with_storage(|storage| storage.cfg().mpsc_parallel)?;
        for _ in 0..self.parallel.unwrap_or(mpsc_parallel) {
            let (parallel_tx, parallel_rx) = tokio::sync::oneshot::channel();
            let Ok(parallel_port) = PortSerializer::connect_port::<S::Error, _, _>(async move |connect| {
                if let Ok((raw_tx, _raw_rx)) = connect.await {
                    let _ = parallel_tx.send(raw_tx);
                }
            }) else {
                continue;
            };
            parallel.push(parallel_port);
            parallel_rxs.push(parallel_rx);
        }

        let port = PortSerializer::connect_port(async move |connect| {
            // Receiver has been dropped after sending, so we receive its channels.
            let ReceiverInner { rx, closed_tx, remote_send_err_tx, closed: _ } = match successor_rx.await {
                Ok(inner) => inner,
                Err(_) => return,
            };

            // Establish chmux channel.
            let (raw_tx, raw_rx) = match connect.await {
                Ok(tx_rx) => tx_rx,
                Err(err) => {
                    let _ = remote_send_err_tx.send(Some(RemoteSendError::Connect(err)));
                    return;
                }
            };

            // Establish additional parallel chmux channels.
            let mut raw_txs = vec![raw_tx];
            for raw_tx in future::join_all(parallel_rxs).await.into_iter().flatten() {
                raw_txs.push(raw_tx);
            }

            super::send_impl(
                ErasedSerializer::new::<CompactResult<T, RecvError>, Codec>(),
                Box::new(rx),
                raw_txs,
                raw_rx,
                remote_send_err_tx,
                closed_tx,
                MAX_ITEM_SIZE,
            )
            .await;
        })?;

        // Encode chmux port number in transport type and serialize it.
        TransportedReceiver {
            port,
            closed: self.inner.as_ref().unwrap().closed,
            max_item_size: self.max_item_size().try_into().unwrap_or(u64::MAX),
            parallel,
        }
        .serialize(serializer)
    }
}

impl<'de, T, Codec, const BUFFER: usize, const MAX_ITEM_SIZE: usize> Deserialize<'de>
    for Receiver<T, Codec, BUFFER, MAX_ITEM_SIZE>
where
    T: RemoteSend,
    Codec: codec::Codec,
{
    /// Deserializes the receiver after it has been received over a chmux channel.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        assert!(BUFFER > 0, "BUFFER must not be zero");

        // Get chmux port number from deserialized transport type.
        let TransportedReceiver { port, closed, max_item_size, parallel } =
            TransportedReceiver::deserialize(deserializer)?;

        let max_item_size = usize::try_from(max_item_size).unwrap_or(usize::MAX);
        if max_item_size > MAX_ITEM_SIZE {
            tracing::debug!(
                "MPSC receiver maximum item size is {MAX_ITEM_SIZE} bytes, \
                 but remote endpoint expects at least {max_item_size} bytes"
            );
        }

        // Create internal communication channels.
        let (tx, rx) = tokio::sync::mpsc::channel(BUFFER);
        let (closed_tx, closed_rx) = tokio::sync::watch::channel(None);
        let (remote_send_err_tx, remote_send_err_rx) = tokio::sync::watch::channel(None);

        // Accept additional paralllel chmux channels.
        let mut parallel_txs = Vec::with_capacity(parallel.len());
        for parallel_port in parallel {
            let (parallel_tx, parallel_rx) = tokio::sync::oneshot::channel();
            if PortDeserializer::accept::<D::Error, _, _>(parallel_port, async move |request| {
                if let Ok((_raw_tx, raw_rx)) = request.accept().await {
                    let _ = parallel_tx.send(raw_rx);
                }
            })
            .is_ok()
            {
                parallel_txs.push(parallel_rx);
            }
        }
        let parallel = parallel_txs.len();

        PortDeserializer::accept(port, async move |request| {
            // Accept chmux connection request.
            let (raw_tx, raw_rx) = match request.accept().await {
                Ok(tx_rx) => tx_rx,
                Err(err) => {
                    let _ = tx.send(SendReq::new(Err(RecvError::Listen(err)))).await;
                    return;
                }
            };

            // Establish additional parallel chmux channels.
            let mut raw_rxs = vec![raw_rx];
            for raw_rx in future::join_all(parallel_txs).await.into_iter().flatten() {
                raw_rxs.push(raw_rx);
            }

            super::recv_impl(
                ErasedDeserializer::new::<CompactResult<T, RecvError>, Codec>(),
                &tx,
                raw_tx,
                raw_rxs,
                remote_send_err_rx,
                closed_rx,
                MAX_ITEM_SIZE,
            )
            .await;
        })?;

        Ok(Self::new(rx, closed_tx, closed, remote_send_err_tx, Some(max_item_size), Some(parallel)))
    }
}

impl<T, Codec, const BUFFER: usize, const MAX_ITEM_SIZE: usize> Stream
    for Receiver<T, Codec, BUFFER, MAX_ITEM_SIZE>
{
    type Item = Result<T, RecvError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let res = ready!(Pin::into_inner(self).poll_recv(cx));
        Poll::Ready(res.transpose())
    }
}

impl<T, Codec, const BUFFER: usize, const MAX_ITEM_SIZE: usize> Unpin
    for Receiver<T, Codec, BUFFER, MAX_ITEM_SIZE>
{
}
