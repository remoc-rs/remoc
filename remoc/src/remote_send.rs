use serde::{Serialize, de::DeserializeOwned};

/// An object that is sendable to a remote endpoint.
///
/// This trait is automatically implemented for objects that are
/// serializable, deserializable and sendable between threads.
pub trait RemoteSend: Send + Serialize + DeserializeOwned + 'static {}

impl<T> RemoteSend for T where T: Send + Serialize + DeserializeOwned + 'static {}
