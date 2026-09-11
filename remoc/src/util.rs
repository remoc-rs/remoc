//! Utility functions.

use bytes::{Buf, Bytes};
use futures::{FutureExt, future::BoxFuture};
use std::{fmt, future::Future, sync::Arc};

use wokio::runtime;

/// Debug formatter for [Bytes].
pub fn dbg_bytes(bytes: &Bytes, f: &mut fmt::Formatter) -> fmt::Result {
    const LIMIT: usize = 16;

    if bytes.len() > LIMIT {
        let tmp = bytes.clone().copy_to_bytes(LIMIT);
        write!(f, "{tmp:?}...[{} bytes]", bytes.len())
    } else {
        write!(f, "{bytes:?}")
    }
}

/// Debug formatter for `Option<Bytes>`.
pub fn dbg_option_bytes(bytes: &Option<Bytes>, f: &mut fmt::Formatter) -> fmt::Result {
    match bytes {
        Some(bytes) => {
            write!(f, "Some(")?;
            dbg_bytes(bytes, f)?;
            write!(f, ")")?;
        }
        None => write!(f, "None")?,
    }
    Ok(())
}

/// Creates the span of a task that outlives the operation spawning it.
///
/// Level, name and fields are specified as for [`span!`](tracing::span).
macro_rules! task_span {
    ($level:expr, $name:literal $(, $($fields:tt)*)?) => {{
        let span = ::tracing::span!($level, $name $(, $($fields)*)?);
        span.follows_from(::tracing::Span::current());
        span
    }};
}
pub(crate) use task_span;

/// Spawns tasks onto the runtime that was current when the spawner was created.
#[derive(Clone)]
pub(crate) struct Spawner(Arc<dyn Fn(BoxFuture<'static, ()>) + Send + Sync>);

impl fmt::Debug for Spawner {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Spawner").finish()
    }
}

impl Spawner {
    /// Creates a spawner for the current runtime.
    ///
    /// # Panics
    /// Panics if called outside of a runtime context.
    #[track_caller]
    pub fn current() -> Self {
        let handle = runtime::Handle::current();
        Self(Arc::new(move |task| {
            handle.spawn(task);
        }))
    }

    /// Spawns a task.
    pub fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        (self.0)(task.boxed())
    }
}
