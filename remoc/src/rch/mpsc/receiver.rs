use futures::{FutureExt, Stream, ready};
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
use crate::{RemoteSend, chmux, codec, exec};

/// An error occurred during receiving over an mpsc channel.
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

impl TryFrom<TryRecvError> for RecvError {
    type Error = TryRecvError;

    fn try_from(err: TryRecvError) -> Result<Self, Self::Error> {
        match err {
            TryRecvError::Closed => Err(TryRecvError::Closed),
            TryRecvError::Empty => Err(TryRecvError::Empty),
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
            Self::RemoteConnect(_) | Self::RemoteListen(_) => true,
        }
    }
}

/// An error occurred during trying to receive over an mpsc channel.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TryRecvError {
    /// All channel senders have been dropped.
    Closed,
    /// Currently no value is ready to receive, but values may still arrive
    /// in the future.
    Empty,
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
            Self::Closed => write!(f, "channel is closed"),
            Self::Empty => write!(f, "channel is empty"),
            Self::RemoteReceive(err) => write!(f, "receive error: {err}"),
            Self::RemoteConnect(err) => write!(f, "connect error: {err}"),
            Self::RemoteListen(err) => write!(f, "listen error: {err}"),
        }
    }
}

impl From<RecvError> for TryRecvError {
    fn from(err: RecvError) -> Self {
        match err {
            RecvError::RemoteReceive(err) => Self::RemoteReceive(err),
            RecvError::RemoteConnect(err) => Self::RemoteConnect(err),
            RecvError::RemoteListen(err) => Self::RemoteListen(err),
        }
    }
}

impl Error for TryRecvError {}

impl TryRecvError {
    /// Returns whether the error is final, i.e. no further receive operation can succeed.
    pub fn is_final(&self) -> bool {
        match self {
            Self::Empty => false,
            Self::RemoteReceive(err) => err.is_final(),
            Self::Closed | Self::RemoteConnect(_) | Self::RemoteListen(_) => true,
        }
    }
}

/// Receive values from the associated [Sender](super::Sender),
/// which may be located on a remote endpoint.
///
/// Instances are created by the [channel](super::channel) function.
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
#[derive(Serialize, Deserialize)]
pub(crate) struct TransportedReceiver<T, Codec> {
    /// chmux port number.
    port: u32,
    /// Data type.
    data: PhantomData<T>,
    /// Data codec.
    codec: PhantomData<Codec>,
    /// Receiver has been closed.
    #[serde(default)]
    closed: bool,
    /// Maximum item size.
    #[serde(default)]
    max_item_size: u64,
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
        remote_max_item_size: Option<usize>,
    ) -> Self {
        Self {
            inner: Some(ReceiverInner { rx, closed_tx, remote_send_err_tx, closed }),
            successor_tx: Mutex::new(None),
            final_err: None,
            remote_max_item_size,
            _codec: PhantomData,
        }
    }

    /// Receives the next value for this receiver.
    ///
    /// This function returns `Ok(None)` when all channel senders have been dropped.
    ///
    /// When a receive error occurs due to a connection failure and other senders are still
    /// present, it is held back and returned after all other senders have been dropped or failed.
    /// Use [error](Self::error) to check if such an error is present.
    ///
    /// ### Cancel safety
    /// This method is cancel safe.
    /// If it is cancelled, it is guaranteed that no messages were received on this channel.
    pub async fn recv(&mut self) -> Result<Option<T>, RecvError> {
        loop {
            match self.inner.as_mut().unwrap().rx.recv().await {
                Some(send_req) => match send_req.ack() {
                    Ok(value_opt) => return Ok(Some(value_opt)),
                    Err(err) => {
                        if err.is_final() {
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
                        if err.is_final() {
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

    /// Tries to receive the next message on this channel, if one is immediately available.
    ///
    /// This function returns `Err(RecvError::Closed)` when all channel senders have been dropped
    /// and `Err(RecvError::Empty)` if no value to receive is currently available.
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
                        if err.is_final() {
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

    /// Blocking receive to call outside of asynchronous contexts.
    ///
    /// This function returns `Ok(None)` when the channel sender has been dropped.
    ///
    /// # Panics
    /// This function panics if called within an asynchronous execution context.
    pub fn blocking_recv(&mut self) -> Result<Option<T>, RecvError> {
        exec::task::block_on(self.recv())
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
    /// deserializable), the error is reported but alreadyed buffered messages from the
    /// same batch are lost.
    ///
    /// ### Cancel safety
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
                    if err.is_final() {
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
    /// This might be over-estimated in case a receive error occured.
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
    /// This allows to process outstanding values while stopping the sender from
    /// sending new values.
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

impl<T, Codec, const BUFFER: usize, const MAX_ITEM_SIZE: usize> Receiver<T, Codec, BUFFER, MAX_ITEM_SIZE>
where
    T: RemoteSend + Clone,
    Codec: codec::Codec,
{
    /// Distribute received items over multiple receivers.
    ///
    /// Each value is received by one of the receivers.
    ///
    /// If `wait_on_empty` is true, the distributor waits if all subscribers are closed.
    /// Otherwise it terminates.
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

        let port = PortSerializer::connect(|connect| {
            async move {
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

                super::send_impl::<T, Codec>(rx, raw_tx, raw_rx, remote_send_err_tx, closed_tx, MAX_ITEM_SIZE)
                    .await;
            }
            .boxed()
        })?;

        // Encode chmux port number in transport type and serialize it.
        let transported = TransportedReceiver::<T, Codec> {
            port,
            data: PhantomData,
            codec: PhantomData,
            closed: self.inner.as_ref().unwrap().closed,
            max_item_size: self.max_item_size().try_into().unwrap_or(u64::MAX),
        };
        transported.serialize(serializer)
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
        let TransportedReceiver { port, closed, max_item_size, .. } =
            TransportedReceiver::<T, Codec>::deserialize(deserializer)?;

        let max_item_size = usize::try_from(max_item_size).unwrap_or(usize::MAX);
        if max_item_size > MAX_ITEM_SIZE {
            tracing::debug!(
                "MPSC receiver maximum item size is {MAX_ITEM_SIZE} bytes, \
                 but remote endpoint expects at least {max_item_size} bytes"
            );
        }

        // Create channels.
        let (tx, rx) = tokio::sync::mpsc::channel(BUFFER);
        let (closed_tx, closed_rx) = tokio::sync::watch::channel(None);
        let (remote_send_err_tx, remote_send_err_rx) = tokio::sync::watch::channel(None);

        PortDeserializer::accept(port, |local_port, request| {
            async move {
                // Accept chmux connection request.
                let (raw_tx, raw_rx) = match request.accept_from(local_port).await {
                    Ok(tx_rx) => tx_rx,
                    Err(err) => {
                        let _ = tx.send(SendReq::new(Err(RecvError::RemoteListen(err)))).await;
                        return;
                    }
                };

                super::recv_impl::<T, Codec>(&tx, raw_tx, raw_rx, remote_send_err_rx, closed_rx, MAX_ITEM_SIZE)
                    .await;
            }
            .boxed()
        })?;

        Ok(Self::new(rx, closed_tx, closed, remote_send_err_tx, Some(max_item_size)))
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
