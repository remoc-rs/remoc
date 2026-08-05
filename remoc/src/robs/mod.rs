//! Remotely observable collections.
//!
//! This module provides collections that emit an event for each change.
//! This event stream can be sent to a local or remote endpoint,
//! where it can be either processed event-wise or a mirrored collection can
//! be built from it.
//!
//! # Basic use
//!
//! Create an observable collection, for example an
//! [observable hash map](hash_map::ObservableHashMap), and obtain a subscription to it
//! by calling `subscribe` on it.
//! The subscription can be sent to a remote endpoint over any [remote channel](crate::rch).
//! There, call `mirror` on it to obtain a live mirror of the collection, or call `recv`
//! repeatedly to process each [change event](hash_map::HashMapEvent) individually.
//!
//! Call `done` on the observed collection when no further changes will be made.
//! Subscribers are notified of this and can distinguish it from a lost connection.
//!
//! # Example
//!
//! In the following example the client observes a hash map held by the server.
//!
//! ```
//! use remoc::prelude::*;
//! use remoc::robs::hash_map::{HashMapSubscription, ObservableHashMap};
//!
//! // This would be run on the server.
//! async fn server(mut tx: rch::base::Sender<HashMapSubscription<u32, String>>) {
//!     let mut map: ObservableHashMap<u32, String> = ObservableHashMap::new();
//!     map.insert(1, "one".to_string());
//!
//!     // The subscription conveys the current contents of the map,
//!     // followed by every subsequent change to it.
//!     tx.send(map.subscribe(1024)).await.unwrap();
//!
//!     map.insert(2, "two".to_string());
//!
//!     // Tells subscribers that no further changes will follow.
//!     map.done();
//! }
//!
//! // This would be run on the client.
//! async fn client(mut rx: rch::base::Receiver<HashMapSubscription<u32, String>>) {
//!     let sub = rx.recv().await.unwrap().unwrap();
//!
//!     // The mirror is kept up-to-date with the observed hash map.
//!     let mut mirror = sub.mirror(1000);
//!
//!     loop {
//!         // Wait for the mirror to change, then inspect it.
//!         mirror.changed().await;
//!         let map = mirror.borrow_and_update().await.unwrap();
//!
//!         if map.is_done() {
//!             assert_eq!(map.get(&1), Some(&"one".to_string()));
//!             assert_eq!(map.get(&2), Some(&"two".to_string()));
//!             break;
//!         }
//!     }
//! }
//! # tokio_test::block_on(remoc::doctest::client_server(server, client));
//! ```
//!

pub mod hash_map;
pub mod hash_set;
pub mod list;
pub mod vec;
pub mod vec_deque;

use serde::{Deserialize, Serialize};
use std::{error::Error, fmt};
use tokio::sync::watch;

use crate::prelude::*;

/// An error occurred during sending an event for an observable collection.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SendError {
    /// Sending to a remote endpoint failed.
    RemoteSend(rch::base::SendErrorKind),
    /// Connecting a sent channel failed.
    RemoteConnect(chmux::ConnectError),
    /// Listening to a received channel failed.
    RemoteListen(chmux::ListenerError),
    /// Forwarding at a remote endpoint to another remote endpoint failed.
    RemoteForward,
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::RemoteSend(err) => write!(f, "send error: {err}"),
            Self::RemoteConnect(err) => write!(f, "connect error: {err}"),
            Self::RemoteListen(err) => write!(f, "listen error: {err}"),
            Self::RemoteForward => write!(f, "forwarding error"),
        }
    }
}

impl Error for SendError {}

impl<T> TryFrom<rch::broadcast::SendError<T>> for SendError {
    type Error = rch::broadcast::SendError<T>;

    fn try_from(err: rch::broadcast::SendError<T>) -> Result<Self, Self::Error> {
        match err {
            rch::broadcast::SendError::RemoteSend(err) => Ok(Self::RemoteSend(err)),
            rch::broadcast::SendError::RemoteConnect(err) => Ok(Self::RemoteConnect(err)),
            rch::broadcast::SendError::RemoteListen(err) => Ok(Self::RemoteListen(err)),
            rch::broadcast::SendError::RemoteForward => Ok(Self::RemoteForward),
            other @ rch::broadcast::SendError::Closed(_) => Err(other),
        }
    }
}

impl<T> TryFrom<rch::mpsc::SendError<T>> for SendError {
    type Error = rch::mpsc::SendError<T>;

    fn try_from(err: rch::mpsc::SendError<T>) -> Result<Self, Self::Error> {
        match err {
            rch::mpsc::SendError::RemoteSend(err) => Ok(Self::RemoteSend(err)),
            rch::mpsc::SendError::RemoteConnect(err) => Ok(Self::RemoteConnect(err)),
            rch::mpsc::SendError::RemoteListen(err) => Ok(Self::RemoteListen(err)),
            rch::mpsc::SendError::RemoteForward => Ok(Self::RemoteForward),
            other @ rch::mpsc::SendError::Closed(_) => Err(other),
        }
    }
}

/// An error occurred during receiving an event or initial value of an observed collection.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RecvError {
    /// The observed collection was dropped before `done` was called on it.
    Closed,
    /// The receiver lagged behind, so that the the send buffer size has reached its limit.
    ///
    /// Try increasing the send buffer specified when calling `subscribe` on the
    /// observed collection.
    Lagged,
    /// The maximum size of the mirrored collection has been reached.
    MaxSizeExceeded(usize),
    /// Receiving from a remote endpoint failed.
    RemoteReceive(rch::base::RecvError),
    /// Connecting a sent channel failed.
    RemoteConnect(chmux::ConnectError),
    /// Listening for a connection from a received channel failed.
    RemoteListen(chmux::ListenerError),
    /// Invalid index.
    InvalidIndex(usize),
}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "observed collection was dropped"),
            Self::Lagged => write!(f, "observation lagged behind"),
            Self::MaxSizeExceeded(size) => write!(f, "mirrored collection reached it maximum size of {size}"),
            Self::RemoteReceive(err) => write!(f, "receive error: {err}"),
            Self::RemoteConnect(err) => write!(f, "connect error: {err}"),
            Self::RemoteListen(err) => write!(f, "listen error: {err}"),
            Self::InvalidIndex(idx) => write!(f, "index {idx} is invalid"),
        }
    }
}

impl Error for RecvError {}

impl From<rch::broadcast::RecvError> for RecvError {
    fn from(err: rch::broadcast::RecvError) -> Self {
        match err {
            rch::broadcast::RecvError::Closed => Self::Closed,
            rch::broadcast::RecvError::Lagged => Self::Lagged,
            rch::broadcast::RecvError::RemoteReceive(err) => Self::RemoteReceive(err),
            rch::broadcast::RecvError::RemoteConnect(err) => Self::RemoteConnect(err),
            rch::broadcast::RecvError::RemoteListen(err) => Self::RemoteListen(err),
        }
    }
}

impl From<rch::mpsc::RecvError> for RecvError {
    fn from(err: rch::mpsc::RecvError) -> Self {
        match err {
            rch::mpsc::RecvError::RemoteReceive(err) => Self::RemoteReceive(err),
            rch::mpsc::RecvError::RemoteConnect(err) => Self::RemoteConnect(err),
            rch::mpsc::RecvError::RemoteListen(err) => Self::RemoteListen(err),
        }
    }
}

/// Sends an event.
pub(crate) fn send_event<E, Codec>(tx: &rch::broadcast::Sender<E, Codec>, on_err: &dyn Fn(SendError), event: E)
where
    Codec: crate::codec::Codec,
    E: RemoteSend + Clone,
{
    match tx.send(event) {
        Ok(_) => (),
        Err(err) if err.is_disconnected() => (),
        Err(err) => match err.try_into() {
            Ok(err) => (on_err)(err),
            Err(_) => unreachable!(),
        },
    }
}

/// Default handler for sending errors.
pub(crate) fn default_on_err(err: SendError) {
    tracing::warn!(%err, "sending failed");
}

/// The observed object has been dropped.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DroppedError;

impl fmt::Display for DroppedError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "dropped")
    }
}

impl Error for DroppedError {}

/// Sends change notifications.
pub(crate) struct ChangeSender {
    tx: watch::Sender<()>,
    rx: watch::Receiver<()>,
}

impl ChangeSender {
    /// Create a new instance.
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(());
        Self { tx, rx }
    }

    /// Return a subscribed [ChangeNotifier].
    pub fn subscribe(&self) -> ChangeNotifier {
        ChangeNotifier(self.rx.clone())
    }

    /// Notify all subscribed [ChangeNotifier]s.
    pub fn notify(&self) {
        self.tx.send_replace(());
    }
}

/// Notifies a local observer of changes to an observable collection.
///
/// This can be cloned, but not sent to remote endpoints.
#[derive(Clone)]
pub struct ChangeNotifier(watch::Receiver<()>);

impl fmt::Debug for ChangeNotifier {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("ChangeNotifier").finish()
    }
}

impl ChangeNotifier {
    /// Returns when the collection has been changed and marks the
    /// newest value as seen.
    pub async fn changed(&mut self) -> Result<(), DroppedError> {
        self.0.changed().await.map_err(|_| DroppedError)
    }

    /// Marks the current value as seen, so that [changed](Self::changed)
    /// will not return immediately.
    pub fn update(&mut self) {
        self.0.borrow_and_update();
    }
}
