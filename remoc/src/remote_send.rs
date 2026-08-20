use serde::{Serialize, de::DeserializeOwned};

/// An object that is sendable to a remote endpoint.
///
/// This is the common bound used by Remoc channels and remote objects. It is
/// automatically implemented for every owned, thread-safe type that implements
/// [`Serialize`] and [`DeserializeOwned`].
///
/// Implement Serde's traits for your type rather than implementing `RemoteSend`
/// directly.
pub trait RemoteSend: Send + Serialize + DeserializeOwned + 'static {}

impl<T> RemoteSend for T where T: Send + Serialize + DeserializeOwned + 'static {}
