//! Asynchronous green-threads

use futures::future::FutureExt;
use std::{
    any::Any,
    error::Error,
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll, ready},
};
use tokio::sync::oneshot;

use super::{runtime::Handle, sync_wrapper::SyncWrapper};

/// Task failed to execute to completion.
pub struct JoinError(pub(super) JoinErrorRepr);

pub(super) enum JoinErrorRepr {
    /// Aborted.
    Aborted,
    /// Panicked.
    Panicked(SyncWrapper<Box<dyn Any + Send + 'static>>),
    /// Thread failed.
    Failed,
    /// Spawning worker thread failed.
    Spawn(std::io::Error),
}

fn panic_payload_as_str(payload: &SyncWrapper<Box<dyn Any + Send>>) -> Option<&str> {
    if let Some(s) = payload.downcast_ref_sync::<String>() {
        return Some(s);
    }

    if let Some(s) = payload.downcast_ref_sync::<&'static str>() {
        return Some(s);
    }

    None
}

impl fmt::Debug for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.0 {
            JoinErrorRepr::Aborted => f.debug_tuple("Aborted").finish(),
            JoinErrorRepr::Panicked(p) => {
                f.debug_tuple("Panicked").field(&panic_payload_as_str(p).unwrap_or("...")).finish()
            }
            JoinErrorRepr::Failed => f.debug_tuple("Failed").finish(),
            JoinErrorRepr::Spawn(err) => f.debug_tuple("Spawn").field(&err).finish(),
        }
    }
}

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.0 {
            JoinErrorRepr::Aborted => write!(f, "task was cancelled"),
            JoinErrorRepr::Panicked(p) => match panic_payload_as_str(p) {
                Some(msg) => write!(f, "task panicked with message {msg}"),
                None => write!(f, "task panicked"),
            },
            JoinErrorRepr::Failed => write!(f, "task failed"),
            JoinErrorRepr::Spawn(err) => write!(f, "spawning worker failed: {err}"),
        }
    }
}

impl From<JoinError> for std::io::Error {
    fn from(err: JoinError) -> Self {
        std::io::Error::other(err)
    }
}

impl Error for JoinError {}

impl JoinError {
    /// Returns true if the error was caused by the task being cancelled.
    pub fn is_cancelled(&self) -> bool {
        matches!(&self.0, JoinErrorRepr::Aborted)
    }

    /// Returns true if the error was caused by thread failure.
    pub fn is_failed(&self) -> bool {
        matches!(&self.0, JoinErrorRepr::Failed)
    }

    /// Returns true if the error was caused by the task panicking.
    pub fn is_panic(&self) -> bool {
        matches!(&self.0, JoinErrorRepr::Panicked(_))
    }

    /// Returns true if the error was caused by worker thread spawning failure.
    pub fn is_spawn(&self) -> bool {
        matches!(&self.0, JoinErrorRepr::Spawn(_))
    }

    /// Consumes the join error, returning the object with which the task panicked.
    #[track_caller]
    pub fn into_panic(self) -> Box<dyn Any + Send + 'static> {
        self.try_into_panic().expect("`JoinError` reason is not a panic.")
    }

    /// Consumes the join error, returning the object with which the task
    /// panicked if the task terminated due to a panic. Otherwise, `self` is
    /// returned.
    pub fn try_into_panic(self) -> Result<Box<dyn Any + Send + 'static>, JoinError> {
        match self.0 {
            JoinErrorRepr::Panicked(p) => Ok(p.into_inner()),
            _ => Err(self),
        }
    }
}

/// An owned permission to join on a task.
pub struct JoinHandle<T> {
    pub(super) result_rx: Pin<Box<oneshot::Receiver<Result<T, JoinError>>>>,
    pub(super) abort_tx: std::sync::Mutex<Option<oneshot::Sender<()>>>,
}

impl<T> fmt::Debug for JoinHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_tuple("JoinHandle").finish()
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let mut result_rx = self.result_rx.as_mut();
        let result = ready!(result_rx.poll_unpin(cx)).unwrap_or(Err(JoinError(JoinErrorRepr::Failed)));
        Poll::Ready(result)
    }
}

impl<T> JoinHandle<T> {
    /// Abort the task associated with the handle.
    pub fn abort(&self) {
        let mut abort_tx = self.abort_tx.lock().unwrap();
        if let Some(abort_tx) = abort_tx.take() {
            let _ = abort_tx.send(());
        }
    }
}

/// Spawns a future onto the browser.
#[track_caller]
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    Handle::current().spawn(future)
}

/// Spawns a task providing a name for diagnostic purposes.
#[track_caller]
pub fn spawn_named<Fut>(name: &str, future: Fut) -> JoinHandle<Fut::Output>
where
    Fut: Future + 'static,
    Fut::Output: 'static,
{
    let _ = name;
    spawn(future)
}

/// Runs the provided function on an thread pool dedicated to blocking operations.
#[track_caller]
pub fn spawn_blocking<F, R>(f: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    Handle::current().spawn_blocking(f)
}

/// Runs a future to completion.
#[track_caller]
pub fn block_on<F: Future>(future: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    rt.block_on(future)
}

/// The [Send] bound required to [spawn] a task on this platform.
///
/// In a JavaScript runtime environment tasks are spawned onto the current thread,
/// thus this imposes no requirement and is implemented for every type.
pub trait MaybeSend {}
impl<T: ?Sized> MaybeSend for T {}
