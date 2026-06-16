use futures::FutureExt;
use serde::{Deserialize, Serialize};
use std::{
    convert::{TryFrom, TryInto},
    error::Error,
    fmt,
    future::Future,
    mem,
    pin::Pin,
    sync::{Arc, Mutex, Weak},
    task::{Context, Poll, ready},
};

use super::{
    super::{SendErrorExt, Sending as BaseSending, SendingError, base, mpsc},
    BroadcastMsg, Receiver,
};
use crate::{RemoteSend, chmux, codec, exec};

/// An error occurred during sending over a broadcast channel.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SendError<T> {
    /// All receivers have been dropped.
    Closed(T),
    /// Sending to a remote endpoint failed.
    RemoteSend(base::SendErrorKind),
    /// Connecting a sent channel failed.
    RemoteConnect(chmux::ConnectError),
    /// Listening to a received channel failed.
    RemoteListen(chmux::ListenerError),
    /// Forwarding at a remote endpoint to another remote endpoint failed.
    RemoteForward,
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Closed(_) => write!(f, "no subscribers"),
            Self::RemoteSend(err) => write!(f, "send error: {err}"),
            Self::RemoteConnect(err) => write!(f, "connect error: {err}"),
            Self::RemoteListen(err) => write!(f, "listen error: {err}"),
            Self::RemoteForward => write!(f, "forwarding error"),
        }
    }
}

impl<T> Error for SendError<T> where T: fmt::Debug {}

impl<T, R> TryFrom<mpsc::TrySendError<T>> for SendError<R> {
    type Error = mpsc::TrySendError<T>;

    fn try_from(err: mpsc::TrySendError<T>) -> Result<Self, Self::Error> {
        match err {
            mpsc::TrySendError::RemoteSend(err) => Ok(Self::RemoteSend(err)),
            mpsc::TrySendError::RemoteConnect(err) => Ok(Self::RemoteConnect(err)),
            mpsc::TrySendError::RemoteListen(err) => Ok(Self::RemoteListen(err)),
            mpsc::TrySendError::RemoteForward => Ok(Self::RemoteForward),
            other => Err(other),
        }
    }
}

impl<T> SendError<T> {
    /// True, if the remote endpoint closed the channel.
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Closed(_))
    }

    /// True, if the remote endpoint closed the channel, was dropped or the connection failed.
    pub fn is_disconnected(&self) -> bool {
        !matches!(self, Self::RemoteSend(base::SendErrorKind::Serialize(_)))
    }

    /// Returns whether the error is final, i.e. no further send operation can succeed.
    pub fn is_final(&self) -> bool {
        match self {
            Self::RemoteSend(err) => err.is_final(),
            Self::Closed(_) | Self::RemoteConnect(_) | Self::RemoteListen(_) | Self::RemoteForward => true,
        }
    }

    /// Whether the error is caused by the item to be sent.
    pub fn is_item_specific(&self) -> bool {
        matches!(self, Self::RemoteSend(err) if err.is_item_specific())
    }

    /// Returns the error without the contained item.
    pub fn without_item(self) -> SendError<()> {
        match self {
            Self::Closed(_) => SendError::Closed(()),
            Self::RemoteSend(err) => SendError::RemoteSend(err),
            Self::RemoteConnect(err) => SendError::RemoteConnect(err),
            Self::RemoteListen(err) => SendError::RemoteListen(err),
            Self::RemoteForward => SendError::RemoteForward,
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

    fn is_final(&self) -> bool {
        self.is_final()
    }

    fn is_item_specific(&self) -> bool {
        self.is_item_specific()
    }
}

/// Sending-half of the broadcast channel.
///
/// Cannot be sent over a remote channel.
/// Use [feeder](Self::feeder) to obtain an mpsc sender that feeds this
/// broadcast sender and can be sent over a remote channel.
#[derive(Clone)]
pub struct Sender<T, Codec = codec::Default> {
    inner: Arc<Mutex<SenderInner<T, Codec>>>,
}

struct SenderInner<T, Codec> {
    subs: Vec<mpsc::Sender<BroadcastMsg<T>, Codec, 1>>,
    ready_tx: tokio::sync::mpsc::UnboundedSender<mpsc::Sender<BroadcastMsg<T>, Codec, 1>>,
    ready_rx: tokio::sync::mpsc::UnboundedReceiver<mpsc::Sender<BroadcastMsg<T>, Codec, 1>>,
    not_ready: usize,
}

impl<T, Codec> fmt::Debug for Sender<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Sender").finish()
    }
}

impl<T, Codec> Default for Sender<T, Codec>
where
    T: RemoteSend + Clone,
    Codec: codec::Codec,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<T, Codec> Sender<T, Codec>
where
    T: RemoteSend + Clone,
    Codec: codec::Codec,
{
    /// Creates the sending-half of the broadcast channel.
    pub fn new() -> Self {
        let (ready_tx, ready_rx) = tokio::sync::mpsc::unbounded_channel();
        let inner = SenderInner { subs: Vec::new(), ready_tx, ready_rx, not_ready: 0 };
        Self { inner: Arc::new(Mutex::new(inner)) }
    }

    /// Attempts to send a value to all active receivers.
    ///
    /// No back-pressure is provided.
    pub fn send(&self, value: T) -> Result<Broadcasting<T>, SendError<T>> {
        let mut inner = self.inner.lock().unwrap();

        // Fetch subscribers that have become ready again.
        while let Ok(sub) = inner.ready_rx.try_recv() {
            inner.subs.push(sub);
            inner.not_ready -= 1;
        }

        let mut keep = Vec::new();
        let mut last_err = None;

        // Broadcast value to all subscribers that are ready.
        let subs = mem::take(&mut inner.subs);
        let mut broadcasted = Vec::with_capacity(subs.len());
        for sub in subs {
            match sub.try_send(BroadcastMsg::Value(value.clone())) {
                Ok(sent) => {
                    broadcasted.push(Sending(sent));
                    keep.push(sub);
                }
                Err(mpsc::TrySendError::Full(BroadcastMsg::Value(_))) => {
                    // Spawn task that waits for subscriber to become ready again,
                    // then add it back to subscriber list.
                    let ready_tx = inner.ready_tx.clone();
                    exec::spawn(async move {
                        let _ = sub.send(BroadcastMsg::Lagged).await;
                        // Make sure subscriber has space for next message.
                        let _permit = sub.reserve().await;
                        let _ = ready_tx.send(sub);
                    });
                    inner.not_ready += 1;
                }
                Err(mpsc::TrySendError::Closed(_)) => (),
                Err(err) => last_err = Some(err),
            }
        }
        inner.subs = keep;

        // Return detailed error if last subscriber was disconnected because of error.
        if !(inner.subs.is_empty() && inner.not_ready == 0) {
            Ok(Broadcasting(broadcasted))
        } else {
            match last_err {
                Some(err) => match err.try_into() {
                    Ok(err) => Err(err),
                    Err(_) => unreachable!("error must be convertible"),
                },
                None => Err(SendError::Closed(value)),
            }
        }
    }

    /// Creates a new receiver that will receive values sent after this call to subscribe.
    pub fn subscribe<const RECEIVE_BUFFER: usize>(
        &self, send_buffer: usize,
    ) -> Receiver<T, Codec, RECEIVE_BUFFER> {
        let mut inner = self.inner.lock().unwrap();

        let (tx, rx) = mpsc::channel(send_buffer);
        let tx = tx.set_buffer();
        let rx = rx.set_buffer();
        inner.subs.push(tx);
        Receiver::new(rx)
    }

    /// Creates a new receiver with a custom maximum item size.
    pub fn subscribe_with_max_item_size<const RECEIVE_BUFFER: usize, const MAX_ITEM_SIZE: usize>(
        &self, send_buffer: usize,
    ) -> Receiver<T, Codec, RECEIVE_BUFFER, MAX_ITEM_SIZE> {
        let mut inner = self.inner.lock().unwrap();

        let (tx, rx) = mpsc::channel(send_buffer);
        let mut tx = tx.set_buffer();
        tx.set_max_item_size(MAX_ITEM_SIZE);
        let rx = rx.set_buffer().set_max_item_size();
        inner.subs.push(tx);
        Receiver::new(rx)
    }

    /// Creates an mpsc sender that feeds values to this broadcast sender.
    ///
    /// The mpsc sender can be sent over a remote channel.
    /// All feeders are disconnected once all receivers are disconnected.
    pub fn feeder<const SEND_BUFFER: usize>(&self) -> mpsc::Sender<T, Codec, SEND_BUFFER> {
        let (tx, rx) = mpsc::channel(1);
        let tx = tx.set_buffer();
        let mut rx = rx.set_buffer::<1>();
        let this = self.clone();

        exec::spawn(async move {
            while let Ok(Some(value)) = rx.recv().await {
                if this.send(value).is_err() {
                    break;
                }
            }
        });

        tx
    }

    /// Returns the number of active receivers.
    pub fn receiver_count(&self) -> usize {
        let inner = self.inner.lock().unwrap();

        inner.subs.len() + inner.not_ready
    }

    /// Converts the Sender to a [WeakSender].
    ///
    /// If all [Sender]s were dropped and only [WeakSender] instances remain, the channel is closed.    
    pub fn downgrade(&self) -> WeakSender<T, Codec> {
        WeakSender { inner: Arc::downgrade(&self.inner) }
    }

    /// Returns the number of [Sender] handles.
    pub fn strong_count(&self) -> usize {
        Arc::strong_count(&self.inner)
    }

    /// Returns the number of [WeakSender] handles.
    pub fn weak_count(&self) -> usize {
        Arc::weak_count(&self.inner)
    }
}

impl<T, Codec> Drop for Sender<T, Codec> {
    fn drop(&mut self) {
        // empty
    }
}

/// Handle to obtain the result of a queued send operation that is
/// part of a broadcast.
///
/// Await this handle to obtain the result of the sending operation.
/// This is optional and only necessary if you want to explicitly handle errors
/// that can occur during sending; for example serialization errors or exceedence
/// of maximum item size.
///
/// You *should not* delay sending other items by awaiting this handle.
/// This would massively impact the throughput of the channel.
///
/// Dropping the handle *does not* abort sending the value.
pub struct Sending<T>(BaseSending<BroadcastMsg<T>>);

impl<T> fmt::Debug for Sending<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("Sending").finish()
    }
}

impl<T> Sending<T> {
    fn map_result(res: Result<(), SendingError<BroadcastMsg<T>>>) -> Result<(), SendingError<T>> {
        match res {
            Ok(()) => Ok(()),
            Err(SendingError::Dropped) => Err(SendingError::Dropped),
            Err(SendingError::Send(base::SendError { kind, item })) => Err(SendingError::Send(base::SendError {
                kind,
                item: match item {
                    BroadcastMsg::Value(value) => value,
                    BroadcastMsg::Lagged => unreachable!("result of sending lagged is ignored"),
                },
            })),
        }
    }

    /// Tries to obtain the result of the sending operation.
    ///
    /// If the value is still queued for sending `None` is returned.
    pub fn try_result(&mut self) -> Option<Result<(), SendingError<T>>> {
        self.0.try_result().map(Self::map_result)
    }
}

impl<T> Future for Sending<T> {
    type Output = Result<(), SendingError<T>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        Poll::Ready(Self::map_result(ready!(self.0.poll_unpin(cx))))
    }
}

/// Handle to obtain the result of a queued broadcast operation.
///
/// You *should not* delay sending other items by awaiting this handle.
/// This would massively impact the throughput of the channel.
///
/// Dropping the handle *does not* abort the broadcast.
pub struct Broadcasting<T>(Vec<Sending<T>>);

impl<T> fmt::Debug for Broadcasting<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Broadcasting").field("sendings", &self.0.len()).finish()
    }
}

impl<T> Broadcasting<T> {
    /// Returns the handles to the queued send operations that make
    /// up this broadcast operation.
    pub fn into_sendings(self) -> Vec<Sending<T>> {
        self.0
    }
}

/// A broadcast sender that does not prevent the channel from being closed.
///
/// If all [Sender] instances of a channel were dropped and only [WeakSender] instances remain,
/// the channel is closed.
//
/// In order to send messages, the [WeakSender] needs to be upgraded using [WeakSender::upgrade].
///
/// Cannot be sent over a remote channel.
#[derive(Clone)]
pub struct WeakSender<T, Codec = codec::Default> {
    inner: Weak<Mutex<SenderInner<T, Codec>>>,
}

impl<T, Codec> fmt::Debug for WeakSender<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("WeakSender").finish()
    }
}

impl<T, Codec> Default for WeakSender<T, Codec> {
    /// Creates a [WeakSender] that can never be converted to a [Sender].
    fn default() -> Self {
        Self { inner: Weak::new() }
    }
}

impl<T, Codec> WeakSender<T, Codec> {
    /// Tries to convert a WeakSender into a [Sender].
    ///
    /// This will return `Some` if there are other [Sender] instances alive and the channel wasn’t previously dropped.
    /// Otherwise `None` is returned.    
    pub fn upgrade(&self) -> Option<Sender<T, Codec>> {
        self.inner.upgrade().map(|inner| Sender { inner })
    }

    /// Returns the number of [Sender] handles.
    pub fn strong_count(&self) -> usize {
        self.inner.strong_count()
    }

    /// Returns the number of [WeakSender] handles.
    pub fn weak_count(&self) -> usize {
        self.inner.weak_count()
    }
}
