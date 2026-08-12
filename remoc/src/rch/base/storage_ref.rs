//! Reference to the data storage of a connection.

use serde::{Deserialize, Serialize};
use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::Mutex,
    task::{Context, Poll, ready},
};

use super::{PortDeserializer, PortSerializer};
use crate::chmux::AnyStorage;

/// Creates a reference to the [data storage](AnyStorage) of a connection and
/// its associated handle.
///
/// The storage reference must be sent to a remote endpoint over a connection,
/// while the handle is kept locally.
/// Both endpoints then obtain the storage of the connection that was used for
/// the transfer:
///
///   * the sending endpoint by awaiting the [handle](StorageRefHandle),
///   * the receiving endpoint by calling [StorageRef::get].
///
/// Use [StorageRef::new] if only the receiving endpoint requires the storage.
///
/// A connection is only established when a value is sent to a remote endpoint,
/// thus the storage of a connection cannot be known in advance.
/// This is useful for a library that is passed a channel by an application and
/// thus has no access to the connection itself.
///
/// # Storages are not synchronized
///
/// Each endpoint of a connection has its own storage.
/// Storing a value on one endpoint does not make it available on the other
/// endpoint; the data is never transferred.
/// Use a channel if data must be exchanged between endpoints.
///
/// # Example
///
/// ```
/// use remoc::prelude::*;
/// use remoc::rch::base::{StorageRef, storage_ref};
///
/// #[derive(Debug, Clone, PartialEq)]
/// struct MyVersion(u32);
///
/// #[derive(serde::Serialize, serde::Deserialize)]
/// struct Msg {
///     storage_ref: StorageRef,
/// }
///
/// // This would be run on the client.
/// async fn client(mut tx: rch::base::Sender<Msg>) {
///     let (storage_ref, handle) = storage_ref();
///     tx.send(Msg { storage_ref }).await.unwrap();
///
///     // The storage used for sending becomes available once the value has been sent.
///     let storage = handle.await.unwrap();
///     storage.insert(MyVersion(1));
/// }
///
/// // This would be run on the server.
/// async fn server(mut rx: rch::base::Receiver<Msg>) {
///     let msg = rx.recv().await.unwrap().unwrap();
///
///     // The storage used for receiving is available immediately.
///     // It is distinct from the storage of the client.
///     let storage = msg.storage_ref.get().unwrap();
///     storage.insert(MyVersion(1));
/// }
/// # tokio_test::block_on(remoc::doctest::client_server(client, server));
/// ```
pub fn storage_ref() -> (StorageRef, StorageRefHandle) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    (StorageRef { tx: Mutex::new(Some(tx)), storage: None }, StorageRefHandle(rx))
}

/// Provides access to the [data storage](AnyStorage) of the connection it is
/// sent over.
///
/// Obtain this using [StorageRef::new], or using [storage_ref] if the storage
/// of the sending endpoint is also required.
///
/// The storage of the receiving endpoint is available using [get](Self::get).
pub struct StorageRef {
    /// Reports the storage used for serialization to the associated handle.
    tx: Mutex<Option<tokio::sync::oneshot::Sender<AnyStorage>>>,
    /// Storage used for deserialization.
    storage: Option<AnyStorage>,
}

impl fmt::Debug for StorageRef {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("StorageRef").field("storage", &self.storage).finish()
    }
}

impl StorageRef {
    /// Creates a new storage reference without an associated handle.
    ///
    /// Use [storage_ref] instead, if the storage of the sending endpoint is
    /// also required.
    pub fn new() -> Self {
        Self { tx: Mutex::new(None), storage: None }
    }

    /// The data storage of the connection this was received over.
    ///
    /// This is the storage of the local endpoint; it is not synchronized with
    /// the storage of the sending endpoint.
    ///
    /// `None` is returned if this has not been received from a remote endpoint.
    pub fn get(&self) -> Option<&AnyStorage> {
        self.storage.as_ref()
    }

    /// Consumes this and returns the data storage of the connection it was
    /// received over.
    ///
    /// `None` is returned if this has not been received from a remote endpoint.
    pub fn into_inner(self) -> Option<AnyStorage> {
        self.storage
    }
}

impl Default for StorageRef {
    fn default() -> Self {
        Self::new()
    }
}

impl Serialize for StorageRef {
    /// Serializes this for sending over a chmux channel.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let storage = PortSerializer::storage::<S::Error>()?;

        // Report the storage to the handle, if this is sent for the first time.
        if let Some(tx) = self.tx.lock().unwrap().take() {
            let _ = tx.send(storage);
        }

        ().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for StorageRef {
    /// Deserializes this after it has been received over a chmux channel.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        <()>::deserialize(deserializer)?;
        let storage = PortDeserializer::storage::<D::Error>()?;

        Ok(Self { tx: Mutex::new(None), storage: Some(storage) })
    }
}

/// Provides the [data storage](AnyStorage) of the connection the associated
/// [storage reference](StorageRef) was sent over.
///
/// Await this to obtain the storage.
/// This is the storage of the local endpoint; it is not synchronized with the
/// storage of the receiving endpoint.
/// It resolves to `None` if the associated storage reference is dropped
/// without being sent to a remote endpoint.
pub struct StorageRefHandle(tokio::sync::oneshot::Receiver<AnyStorage>);

impl fmt::Debug for StorageRefHandle {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("StorageRefHandle").finish()
    }
}

impl Future for StorageRefHandle {
    type Output = Option<AnyStorage>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        Poll::Ready(ready!(Pin::new(&mut self.0).poll(cx)).ok())
    }
}
