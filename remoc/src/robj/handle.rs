//! Remote handle to an object.
//!
//! Handles provide the ability to send a shared reference to a local object to
//! a remote endpoint.
//! The remote endpoint cannot access the referenced object remotely but it can send back
//! the handle (or a clone of it), which can then be dereferenced locally.
//!
//! Handles can be cloned and they are reference counted.
//! If all handles to the object are dropped, it is dropped automatically.
//!
//! # Usage
//!
//! [Create a handle](Handle::new) to an object and send it to a remote endpoint, for example
//! over a channel from the [rch](crate::rch) module.
//! When you receive a handle that was created locally from the remote endpoint, use [Handle::as_ref]
//! to access the object it references.
//! Calling [Handle::as_ref] on a handle that was not created locally will result in an error.
//!
//! # Security
//!
//! When sending a handle an UUID is generated, associated with the object and send to the
//! remote endpoint.
//! Since the object itself is never send, there is no risk that private data within the object
//! may be accessed remotely.
//!
//! The UUID is associated with the [channel multiplexer](crate::chmux) over which the
//! handle was sent to the remote endpoint and a received handle can only access the object
//! if it is received over the same channel multiplexer connection.
//! Thus, even if the UUID is eavesdropped during transmission, another remote endpoint connected
//! via a different channel multiplexer connection will not be able to access the object
//! by constructing and sending back a handle with the eavesdropped UUID.
//!
//! # Example
//!
//! In the following example the server creates a handle and sends it to the client.
//! The client tries to dereference a handle but receives an error because the handle
//! was not created locally.
//! The client then sends back the handle to the server.
//! The server can successfully dereference the received handle.
//!
//! ```
//! use remoc::prelude::*;
//! use remoc::robj::handle::{Handle, HandleError};
//!
//! // This would be run on the client.
//! async fn client(
//!     mut tx: rch::base::Sender<Handle<String>>,
//!     mut rx: rch::base::Receiver<Handle<String>>,
//! ) {
//!     let handle = rx.recv().await.unwrap().unwrap();
//!     assert!(matches!(handle.as_ref().await, Err(HandleError::Unknown)));
//!     tx.send(handle).await.unwrap();
//! }
//!
//! // This would be run on the server.
//! async fn server(
//!     mut tx: rch::base::Sender<Handle<String>>,
//!     mut rx: rch::base::Receiver<Handle<String>>,
//! ) {
//!     let data = "private data".to_string();
//!     let handle = Handle::new(data);
//!     assert_eq!(*handle.as_ref().await.unwrap(), "private data".to_string());
//!
//!     tx.send(handle).await.unwrap();
//!
//!     let handle = rx.recv().await.unwrap().unwrap();
//!     assert_eq!(*handle.as_ref().await.unwrap(), "private data".to_string());
//! }
//! # tokio_test::block_on(remoc::doctest::client_server_bidir(client, server));
//! ```

use serde::{Deserialize, Serialize};
use std::{
    fmt,
    marker::PhantomData,
    mem,
    ops::{Deref, DerefMut},
    sync::Arc,
};
use tokio::sync::{OwnedRwLockMappedWriteGuard, OwnedRwLockReadGuard, OwnedRwLockWriteGuard};
use tracing::Instrument;
use uuid::Uuid;

use crate::{
    chmux::{AnyBox, AnyEntry},
    codec, exec,
    rch::{
        base::{PortDeserializer, PortSerializer},
        mpsc,
    },
};

/// An error during getting the value of a handle.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum HandleError {
    /// The value of the handle is not stored locally or it has already
    /// been taken.
    Unknown,
    /// The values of the handle is of another type.
    MismatchedType(String),
}

impl fmt::Display for HandleError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            HandleError::Unknown => write!(f, "unknown, taken or non-local handle"),
            HandleError::MismatchedType(ty) => write!(f, "mismatched handle type: {ty}"),
        }
    }
}

impl std::error::Error for HandleError {}

/// Provider for a handle.
///
/// Dropping the provider causes all corresponding handles to become
/// invalid and the underlying object to be dropped.
pub struct Provider {
    keep_tx: Option<tokio::sync::watch::Sender<bool>>,
}

impl fmt::Debug for Provider {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Provider").finish()
    }
}

impl Provider {
    /// Keeps the provider alive until the handle is dropped.
    pub fn keep(mut self) {
        let _ = self.keep_tx.take().unwrap().send(true);
    }

    /// Waits until the handle provider can be safely dropped.
    ///
    /// This is the case when the handle is dropped.
    pub async fn done(&mut self) {
        self.keep_tx.as_mut().unwrap().closed().await
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        // empty
    }
}

/// Handle state.
#[derive(Clone, Default)]
enum State<Codec> {
    /// Empty (for dropping).
    #[default]
    Empty,
    /// Value has been created locally.
    LocalCreated {
        /// Reference to value.
        entry: AnyEntry,
        /// Keep notification.
        keep_rx: tokio::sync::watch::Receiver<bool>,
    },
    /// Value is stored locally and handle has been received from
    /// a remote endpoint.
    LocalReceived {
        /// Reference to value.
        entry: AnyEntry,
        /// Id in local handle storage.
        id: Uuid,
        /// Dropped notification.
        dropped_tx: mpsc::Sender<(), Codec, 1>,
    },
    /// Value is stored on a remote endpoint.
    Remote {
        /// Id in remote handle storage.
        id: Uuid,
        /// Dropped notification.
        dropped_tx: mpsc::Sender<(), Codec, 1>,
    },
}

impl<Codec> fmt::Debug for State<Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "Empty"),
            Self::LocalCreated { .. } => write!(f, "LocalCreated"),
            Self::LocalReceived { id, .. } => f.debug_struct("LocalReceived").field("id", id).finish(),
            Self::Remote { id, .. } => f.debug_struct("Remote").field("id", id).finish(),
        }
    }
}

/// A handle to a value that is possibly stored on a remote endpoint.
///
/// If the value is stored locally, the handle can be used to obtain the value
/// or a (mutable) reference to it.
///
/// See [module-level documentation](self) for details.
#[derive(Clone)]
pub struct Handle<T, Codec = codec::Default> {
    state: State<Codec>,
    _data: PhantomData<T>,
}

impl<T, Codec> fmt::Debug for Handle<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.state)
    }
}

impl<T, Codec> Handle<T, Codec>
where
    T: Send + Sync + 'static,
    Codec: codec::Codec,
{
    /// Creates a new handle for the value.
    ///
    /// There is *no* requirement on `T` to be [remote sendable](crate::RemoteSend), since
    /// it is never send to a remote endpoint.
    pub fn new(value: T) -> Self {
        let (handle, provider) = Self::provided(value);
        provider.keep();
        handle
    }

    /// Creates a new handle for the value and returns it together
    /// with its provider.
    ///
    /// This allows you to drop the object without relying upon all handles being
    /// dropped, possibly by a remote endpoint.
    /// This is especially useful when you connect to untrusted remote endpoints
    /// that could try to obtain and keep a large number of handles to
    /// perform a denial of service attack by exhausting your memory.
    pub fn provided(value: T) -> (Self, Provider) {
        let (keep_tx, keep_rx) = tokio::sync::watch::channel(false);

        let handle = Self {
            state: State::LocalCreated {
                entry: Arc::new(tokio::sync::RwLock::new(Some(Box::new(value)))),
                keep_rx,
            },
            _data: PhantomData,
        };
        let provider = Provider { keep_tx: Some(keep_tx) };

        (handle, provider)
    }

    /// Takes the value of the handle and returns it, if it is stored locally.
    ///
    /// The handle and all its clones become invalid.
    ///
    /// This blocks until all existing read and write reference have been released.
    pub async fn into_inner(mut self) -> Result<T, HandleError> {
        let entry = match mem::take(&mut self.state) {
            State::LocalCreated { entry, .. } => entry,
            State::LocalReceived { entry, .. } => entry,
            _ => return Err(HandleError::Unknown),
        };

        let mut entry = entry.write().await;
        match entry.take() {
            Some(any) => match any.downcast::<T>() {
                Ok(value) => Ok(*value),
                Err(any) => Err(HandleError::MismatchedType(format!("{:?}", (*any).type_id()))),
            },
            None => Err(HandleError::Unknown),
        }
    }

    /// Returns a reference to the value of the handle, if it is stored locally.
    ///
    /// This blocks until all existing write reference have been released.
    pub async fn as_ref(&self) -> Result<Ref<T>, HandleError> {
        let entry = match &self.state {
            State::LocalCreated { entry, .. } | State::LocalReceived { entry, .. } => entry.clone(),
            _ => return Err(HandleError::Unknown),
        };

        let entry = entry.read_owned().await;
        match &*entry {
            Some(any) => {
                if !any.is::<T>() {
                    return Err(HandleError::MismatchedType(format!("{:?}", (**any).type_id())));
                }
                let value_ref = OwnedRwLockReadGuard::map(entry, |entry| {
                    entry.as_ref().unwrap().downcast_ref::<T>().unwrap()
                });
                Ok(Ref(value_ref))
            }
            None => Err(HandleError::Unknown),
        }
    }

    /// Returns a mutable reference to the value of the handle, if it is stored locally.
    ///
    /// This blocks until all existing read and write reference have been released.
    pub async fn as_mut(&mut self) -> Result<RefMut<T>, HandleError> {
        let entry = match &self.state {
            State::LocalCreated { entry, .. } | State::LocalReceived { entry, .. } => entry.clone(),
            _ => return Err(HandleError::Unknown),
        };

        let entry = entry.write_owned().await;
        match &*entry {
            Some(any) => {
                if !any.is::<T>() {
                    return Err(HandleError::MismatchedType(format!("{:?}", (**any).type_id())));
                }
                let value_ref = OwnedRwLockWriteGuard::map(entry, |entry| {
                    entry.as_mut().unwrap().downcast_mut::<T>().unwrap()
                });
                Ok(RefMut(value_ref))
            }
            None => Err(HandleError::Unknown),
        }
    }

    /// Change the data type of the handle.
    ///
    /// Before the handle can be dereferenced the type must be changed back to the original
    /// type, otherwise a [HandleError::MismatchedType] error will occur.
    ///
    /// This is useful when you need to send a handle of a private type to a remote endpoint.
    /// You can do so by creating a public, empty proxy struct and sending handles of this
    /// type to remote endpoints.
    pub fn cast<TNew>(self) -> Handle<TNew, Codec> {
        Handle { state: self.state.clone(), _data: PhantomData }
    }
}

impl<T, Codec> Drop for Handle<T, Codec> {
    fn drop(&mut self) {
        // empty
    }
}

/// Handle in transport.
#[derive(Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "Codec: codec::Codec"))]
#[serde(bound(deserialize = "Codec: codec::Codec"))]
pub(crate) struct TransportedHandle<T, Codec> {
    /// Handle id.
    id: Uuid,
    /// Dropped notification.
    dropped_tx: mpsc::Sender<(), Codec, 1>,
    /// Data type.
    data: PhantomData<T>,
    /// Codec
    codec: PhantomData<Codec>,
}

impl<T, Codec> Serialize for Handle<T, Codec>
where
    Codec: codec::Codec,
{
    /// Serializes this handle for sending over a chmux channel.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let (id, dropped_tx) = match self.state.clone() {
            State::LocalCreated { entry, mut keep_rx, .. } => {
                let handle_storage = PortSerializer::storage()?;
                let id = handle_storage.insert(entry.clone());

                let (dropped_tx, dropped_rx) = mpsc::channel(1);
                let dropped_tx = dropped_tx.set_buffer::<1>();
                let mut dropped_rx = dropped_rx.set_buffer::<1>();

                exec::spawn(
                    async move {
                        loop {
                            if *keep_rx.borrow_and_update() {
                                let _ = dropped_rx.recv().await;
                                break;
                            } else {
                                tokio::select! {
                                    biased;
                                    res = keep_rx.changed() => {
                                        if !*keep_rx.borrow_and_update() && res.is_err() {
                                            break;
                                        }
                                    },
                                    _ = dropped_rx.recv() => break,
                                }
                            }
                        }

                        handle_storage.remove(id);
                    }
                    .in_current_span(),
                );

                (id, dropped_tx)
            }
            State::LocalReceived { id, dropped_tx, .. } | State::Remote { id, dropped_tx } => (id, dropped_tx),
            State::Empty => unreachable!("state is only empty when dropping"),
        };

        let transported = TransportedHandle::<T, Codec> { id, dropped_tx, data: PhantomData, codec: PhantomData };

        transported.serialize(serializer)
    }
}

impl<'de, T, Codec> Deserialize<'de> for Handle<T, Codec>
where
    Codec: codec::Codec,
{
    /// Deserializes this handle after it has been received over a chmux channel.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let TransportedHandle { id, dropped_tx, .. } = TransportedHandle::<T, Codec>::deserialize(deserializer)?;

        let handle_storage = PortDeserializer::storage()?;
        let state = match handle_storage.remove(id) {
            Some(entry) => State::LocalReceived { entry, id, dropped_tx },
            None => State::Remote { id, dropped_tx },
        };

        Ok(Self { state, _data: PhantomData })
    }
}

/// An owned reference to the value of a handle.
pub struct Ref<T>(OwnedRwLockReadGuard<Option<AnyBox>, T>);

impl<T> Deref for Ref<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> fmt::Debug for Ref<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", **self)
    }
}

/// An owned mutable reference to the value of a handle.
pub struct RefMut<T>(OwnedRwLockMappedWriteGuard<Option<AnyBox>, T>);

impl<T> Deref for RefMut<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for RefMut<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> fmt::Debug for RefMut<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", **self)
    }
}
