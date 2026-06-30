use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    fmt,
    ops::{Deref, DerefMut},
    sync::Arc,
};
use tracing::Instrument;

use super::msg::{ReadRequest, Value, WriteRequest};
use crate::{
    RemoteSend, chmux, codec, exec,
    rch::{base, mpsc, oneshot},
};

/// An error occurred during locking of an RwLock value for reading or writing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum LockError {
    /// The [owner](super::Owner) has been dropped.
    Dropped,
    /// Receiving from a remote endpoint failed.
    RemoteReceive(base::RecvError),
    /// Connecting a sent channel failed.
    RemoteConnect(chmux::ConnectError),
    /// Listening for a connection from a received channel failed.
    RemoteListen(chmux::ListenerError),
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Dropped => write!(f, "owner dropped"),
            Self::RemoteReceive(err) => write!(f, "receive error: {err}"),
            Self::RemoteConnect(err) => write!(f, "connect error: {err}"),
            Self::RemoteListen(err) => write!(f, "listen error: {err}"),
        }
    }
}

impl From<oneshot::RecvError> for LockError {
    fn from(err: oneshot::RecvError) -> Self {
        match err {
            oneshot::RecvError::Closed => Self::Dropped,
            oneshot::RecvError::RemoteReceive(err) => Self::RemoteReceive(err),
            oneshot::RecvError::RemoteConnect(err) => Self::RemoteConnect(err),
            oneshot::RecvError::RemoteListen(err) => Self::RemoteListen(err),
        }
    }
}

impl Error for LockError {}

/// An error occurred during committing an RwLock value.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CommitError {
    /// The [owner](super::Owner) has been dropped.
    Dropped,
    /// Commit failed.
    Failed,
}

impl fmt::Display for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Dropped => write!(f, "owner dropped"),
            Self::Failed => write!(f, "commit failed"),
        }
    }
}

impl<T> From<oneshot::SendError<T>> for CommitError {
    fn from(err: oneshot::SendError<T>) -> Self {
        match err {
            oneshot::SendError::Closed(_) => Self::Dropped,
            oneshot::SendError::Failed => Self::Failed,
        }
    }
}

impl From<oneshot::RecvError> for CommitError {
    fn from(_: oneshot::RecvError) -> Self {
        Self::Failed
    }
}

impl Error for CommitError {}

/// A lock that allows reading of a shared value, possibly stored on a remote endpoint.
///
/// This can be cloned and sent to remote endpoints.
///
/// See [module-level documentation](super) for details.
#[derive(Serialize, Deserialize)]
#[serde(bound(serialize = "T: RemoteSend, Codec: codec::Codec"))]
#[serde(bound(deserialize = "T: RemoteSend, Codec: codec::Codec"))]
pub struct ReadLock<T, Codec = codec::Default> {
    req_tx: mpsc::Sender<ReadRequest<T, Codec>, Codec, 1>,
    #[serde(skip)]
    #[serde(default = "empty_cache")]
    cache: Arc<tokio::sync::RwLock<Option<Value<T, Codec>>>>,
}

fn empty_cache<T, Codec>() -> Arc<tokio::sync::RwLock<Option<Value<T, Codec>>>> {
    Arc::new(tokio::sync::RwLock::new(None))
}

impl<T, Codec> Clone for ReadLock<T, Codec> {
    fn clone(&self) -> Self {
        Self { req_tx: self.req_tx.clone(), cache: self.cache.clone() }
    }
}

impl<T, Codec> fmt::Debug for ReadLock<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ReadLock").finish()
    }
}

impl<T, Codec> ReadLock<T, Codec>
where
    T: RemoteSend + Sync,
    Codec: codec::Codec,
{
    pub(crate) fn new(read_req_tx: mpsc::Sender<ReadRequest<T, Codec>, Codec, 1>) -> Self {
        Self { req_tx: read_req_tx, cache: empty_cache() }
    }

    /// Fetches the current shared value, possibly from the local cache.
    async fn fetch(&self) -> Result<tokio::sync::RwLockReadGuard<'_, Value<T, Codec>>, LockError> {
        // Return cached value if it is valid.
        {
            let cache_opt = self.cache.read().await;
            match &*cache_opt {
                Some(cache) if cache.is_valid() => {
                    return Ok(tokio::sync::RwLockReadGuard::map(cache_opt, |co| co.as_ref().unwrap()));
                }
                _ => (),
            }
        }

        // Wait for write lock before requesting current value.
        // This is necessary because there may be outstanding read locks
        // for the invalidated value.
        let mut cache_opt = self.cache.write().await;

        // Request and receive current value.
        let (value_tx, value_rx) = oneshot::channel();
        let _ = self.req_tx.send(ReadRequest { value_tx }).await;
        let value = value_rx.await?;

        // Start task that monitors cache validity and releases cache
        // when it becomes invalid.
        let mut invalid_rx = value.invalid_rx.clone();
        let cache_lock = self.cache.clone();
        exec::spawn(
            async move {
                // Wait for cache invalidation.
                loop {
                    match invalid_rx.borrow_and_update() {
                        Ok(invalid) if !*invalid => (),
                        _ => break,
                    }

                    if invalid_rx.changed().await.is_err() {
                        break;
                    }
                }

                // Remove cache, if it is invalid.
                // This will wait until all read locks are released.
                // The validity check is necessary, because a new (valid) cached value may
                // have been written while we were waiting to acquire the write lock.
                let mut cache_opt = cache_lock.write().await;
                match &*cache_opt {
                    Some(cache) if !cache.is_valid() => *cache_opt = None,
                    _ => (),
                }
            }
            .in_current_span(),
        );

        // Store value in cache.
        *cache_opt = Some(value);

        Ok(tokio::sync::RwLockReadGuard::map(tokio::sync::RwLockWriteGuard::downgrade(cache_opt), |co| {
            co.as_ref().unwrap()
        }))
    }

    /// Locks the current shared value for reading and returns a reference to it.
    ///
    /// At first invocation the value is fetched from the [owner](super::Owner) and cached locally.
    /// Thus subsequent invocations are cheap until the value is invalidated.
    pub async fn read(&self) -> Result<ReadGuard<'_, T, Codec>, LockError> {
        let cache = self.fetch().await?;
        Ok(ReadGuard(cache))
    }
}

/// RAII structure used to release the shared read access of a lock when dropped.
///
/// As long as this is held, no write access to the lock can occur.
/// It is therefore recommend to either hold the guard for only short periods of time
/// or call [invalidated](Self::invalidated) to be notified when write access is requested.
pub struct ReadGuard<'a, T, Codec = codec::Default>(tokio::sync::RwLockReadGuard<'a, Value<T, Codec>>);

impl<T, Codec> ReadGuard<'_, T, Codec>
where
    Codec: codec::Codec,
{
    /// Waits until the shared value is invalidated because a write request is made.
    ///
    /// In this case the holder should drop this guard and reissue the read request
    /// to obtain the new value.
    /// As long as the guard is held the shared value will not be changed.
    ///
    /// This also returns when the owner is dropped or a connection error occurs.
    pub async fn invalidated(&self) {
        let mut invalid_rx = self.0.invalid_rx.clone();
        while !invalid_rx.borrow_and_update().map(|v| *v).unwrap_or_default() {
            if invalid_rx.changed().await.is_err() {
                break;
            }
        }
    }

    /// Returns true, if the shared value has been invalidated.
    pub fn is_invalidated(&self) -> bool {
        self.0.invalid_rx.borrow().map(|v| *v).unwrap_or(true)
    }
}

impl<T, Codec> Deref for ReadGuard<'_, T, Codec> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0.value
    }
}

impl<T, Codec> fmt::Debug for ReadGuard<'_, T, Codec>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", **self)
    }
}

impl<T, Codec> Drop for ReadGuard<'_, T, Codec> {
    fn drop(&mut self) {
        // empty
    }
}

/// A lock that allows reading and writing of a shared value, possibly stored on a remote endpoint.
///
/// This can be cloned and sent to remote endpoints.
///
/// See [module-level documentation](super) for details.
#[derive(Serialize, Deserialize)]
#[serde(bound(serialize = "T: RemoteSend, Codec: codec::Codec"))]
#[serde(bound(deserialize = "T: RemoteSend, Codec: codec::Codec"))]
pub struct RwLock<T, Codec = codec::Default> {
    read: ReadLock<T, Codec>,
    req_tx: mpsc::Sender<WriteRequest<T, Codec>, Codec, 1>,
}

impl<T, Codec> Clone for RwLock<T, Codec> {
    fn clone(&self) -> Self {
        Self { read: self.read.clone(), req_tx: self.req_tx.clone() }
    }
}

impl<T, Codec> fmt::Debug for RwLock<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("RwLock").finish()
    }
}

impl<T, Codec> RwLock<T, Codec>
where
    T: RemoteSend + Sync,
    Codec: codec::Codec,
{
    pub(crate) fn new(
        read_lock: ReadLock<T, Codec>, write_req_tx: mpsc::Sender<WriteRequest<T, Codec>, Codec, 1>,
    ) -> Self {
        Self { read: read_lock, req_tx: write_req_tx }
    }

    /// Locks the current shared value for reading and returns a reference to it.
    ///
    /// At first invocation the value is fetched from the [owner](super::Owner) and cached locally.
    /// Thus subsequent invocations are cheap until the value is invalidated.
    pub async fn read(&self) -> Result<ReadGuard<'_, T, Codec>, LockError> {
        self.read.read().await
    }

    /// Locks the current shared value for reading and writing and returns a mutable reference to it.
    ///
    /// To commit the new value [WriteGuard::commit] must be called, otherwise the
    /// changes will be lost.
    ///
    /// When called the following things occur:
    ///
    /// 1. A message is sent to the [owner](super::Owner), indicating that write access is requested.
    /// 2. The owner stops processing read requests and messages all [read guards](ReadGuard) that
    ///    they are invalidated.
    /// 3. The owner waits from confirmation from all read guards that they have been dropped.
    /// 4. The owner sends the current shared value to this RwLock, which creates a [WriteGuard]
    ///    to allow write access.
    /// 5. Once [WriteGuard::commit] has been called, the updated value is sent back to the owner.
    /// 6. The owner starts processing other read and write requests again.
    pub async fn write(&self) -> Result<WriteGuard<T, Codec>, LockError> {
        let (value_tx, value_rx) = oneshot::channel();
        let (new_value_tx, new_value_rx) = oneshot::channel();
        let (confirm_tx, confirm_rx) = oneshot::channel();

        let _ = self.req_tx.send(WriteRequest { value_tx, new_value_rx, confirm_tx }).await;
        let value = value_rx.await?;

        Ok(WriteGuard { value: Some(value), new_value_tx: Some(new_value_tx), confirm_rx: Some(confirm_rx) })
    }

    /// Returns a read lock to the shared value.
    pub fn read_lock(&self) -> ReadLock<T, Codec> {
        self.read.clone()
    }
}

/// RAII structure used to release the exclusive write access of a lock when dropped.
///
/// To commit changes [commit](Self::commit) must be called.
/// Dropping the guard will result in the changes to be not applied to the shared value.
pub struct WriteGuard<T, Codec = codec::Default> {
    value: Option<T>,
    new_value_tx: Option<oneshot::Sender<T, Codec>>,
    confirm_rx: Option<oneshot::Receiver<(), Codec>>,
}

impl<T, Codec> WriteGuard<T, Codec>
where
    T: RemoteSend,
    Codec: codec::Codec,
{
    /// Consumes the guard and commits the changes to the shared value.
    pub async fn commit(mut self) -> Result<(), CommitError> {
        let new_value = self.value.take().unwrap();

        let new_value_tx = self.new_value_tx.take().unwrap();
        new_value_tx.send(new_value)?;

        let confirm_rx = self.confirm_rx.take().unwrap();
        confirm_rx.await?;

        Ok(())
    }
}

impl<T, Codec> Deref for WriteGuard<T, Codec> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.value.as_ref().unwrap()
    }
}

impl<T, Codec> DerefMut for WriteGuard<T, Codec> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.value.as_mut().unwrap()
    }
}

impl<T, Codec> fmt::Debug for WriteGuard<T, Codec>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", **self)
    }
}

impl<T, Codec> Drop for WriteGuard<T, Codec> {
    fn drop(&mut self) {
        // empty
    }
}
