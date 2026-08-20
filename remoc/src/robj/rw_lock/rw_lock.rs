use std::{
    error::Error,
    fmt,
    ops::{Deref, DerefMut},
    sync::Arc,
};
use tracing::Instrument;

use super::msg::{ReadRequest, Value, WriteRequest};
use crate::{
    RemoteSend, chmux, codec,
    rch::{
        base::{self},
        mpsc, oneshot,
    },
};

/// An error occurred during locking of an RwLock value for reading or writing.
#[derive(Clone, Debug)]
pub enum LockError {
    /// The [owner](super::Owner) has been dropped.
    Dropped,
    /// Receiving or decoding the lock response failed; see [`base::RecvError`].
    Receive(base::RecvError),
    /// Opening a channel contained in the response failed; see [`chmux::ConnectError`].
    Connect(chmux::ConnectError),
    /// Preparing a channel contained in the response failed; see [`chmux::ListenerError`].
    Listen(chmux::ListenerError),
    /// A failure was reported by an endpoint forwarding the lock request or response.
    ///
    /// The nested error is [`None`] when that endpoint reported a newer error
    /// variant that this version of Remoc does not recognize.
    Remote(Option<Box<LockError>>),
}

crate::versioned::compact::impl_enum! {
    LockError,
    recover = LockError::Remote(None),
    variants {
        Dropped => "_0",
        Receive(err: base::RecvError) => "_1",
        Connect(err: chmux::ConnectError) => "_2",
        Listen(err: chmux::ListenerError) => "_3",
        Remote(err: Option<Box<LockError>>) => "_50",
    }
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Dropped => write!(f, "owner dropped"),
            Self::Receive(err) => write!(f, "receive error: {err}"),
            Self::Connect(err) => write!(f, "connect error: {err}"),
            Self::Listen(err) => write!(f, "listen error: {err}"),
            Self::Remote(Some(err)) => write!(f, "remote {err}"),
            Self::Remote(None) => write!(f, "unknown remote error"),
        }
    }
}

impl From<oneshot::RecvError> for LockError {
    fn from(err: oneshot::RecvError) -> Self {
        match err {
            oneshot::RecvError::Closed => Self::Dropped,
            oneshot::RecvError::Receive(err) => Self::Receive(err),
            oneshot::RecvError::Connect(err) => Self::Connect(err),
            oneshot::RecvError::Listen(err) => Self::Listen(err),
            oneshot::RecvError::Remote(err) => Self::Remote(err.map(|err| Box::new(Self::from(*err)))),
        }
    }
}

impl Error for LockError {}

/// An error occurred during committing an RwLock value.
#[derive(Clone, Debug)]
pub enum CommitError {
    /// The [owner](super::Owner) has been dropped.
    Dropped,
    /// The updated value could not be returned to the owner.
    Failed,
}

crate::versioned::compact::impl_enum! {
    CommitError,
    variants {
        Dropped => "_0",
        Failed => "_1",
    }
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
            oneshot::SendError::Closed(_) | oneshot::SendError::Dropped => Self::Dropped,
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
pub struct ReadLock<T, Codec = codec::Default> {
    req_tx: mpsc::Sender<ReadRequest<T, Codec>, Codec, 1>,
    cache: Arc<tokio::sync::RwLock<Option<Value<T, Codec>>>>,
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

crate::versioned::compact::impl_struct! {
    ReadLock<T, Codec>,
    fields {
        req_tx: mpsc::Sender<ReadRequest<T, Codec>, Codec, 1> => "_0",
    }
    default { cache }
    where T: RemoteSend, Codec: codec::Codec
}

impl<T, Codec> ReadLock<T, Codec>
where
    T: RemoteSend + Sync,
    Codec: codec::Codec,
{
    pub(super) fn new(read_req_tx: mpsc::Sender<ReadRequest<T, Codec>, Codec, 1>) -> Self {
        Self { req_tx: read_req_tx, cache: Default::default() }
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
        wokio::spawn(
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
    ///
    /// # Example
    ///
    /// The following function keeps a locally cached view of the shared value
    /// without blocking other endpoints from writing to it.
    ///
    /// ```
    /// # use remoc::robj::rw_lock::RwLock;
    /// # async fn observe(rw_lock: &RwLock<u32>) {
    /// loop {
    ///     let read = rw_lock.read().await.unwrap();
    ///     println!("value is {}", *read);
    ///
    ///     // Once a write is requested the guard is dropped by the end of the
    ///     // loop body and the next iteration reads the updated value.
    ///     read.invalidated().await;
    /// }
    /// # }
    /// ```
    pub async fn invalidated(&self) {
        let mut invalid_rx = self.0.invalid_rx.clone();
        while !invalid_rx.borrow_and_update().map(|v| *v).unwrap_or_default() {
            if invalid_rx.changed().await.is_err() {
                break;
            }
        }
    }

    /// Returns `true` if the shared value has been invalidated.
    ///
    /// This also returns `true` if the owner was dropped or the invalidation
    /// state can no longer be received.
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
pub struct RwLock<T, Codec = codec::Default> {
    read: ReadLock<T, Codec>,
    req_tx: mpsc::Sender<WriteRequest<T, Codec>, Codec, 1>,
}

crate::versioned::compact::impl_struct! {
    RwLock<T, Codec>,
    fields {
        read: ReadLock<T, Codec> => "_0",
        req_tx: mpsc::Sender<WriteRequest<T, Codec>, Codec, 1> => "_1",
    }
    where T: RemoteSend, Codec: codec::Codec
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
    pub(super) fn new(
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
    ///
    /// This waits until the owner has accepted the updated value. If the owner
    /// was dropped or the value could not be transferred, the changes are not
    /// known to have been committed.
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
