//! Async executive for futures.
//!
//! On native platforms this uses Tokio.
//! On JavaScript this executes Futures as Promises.

#[cfg(not(feature = "js"))]
mod native;

#[cfg(not(feature = "js"))]
pub use native::*;

#[cfg(feature = "js")]
mod js;

#[cfg(feature = "js")]
pub use js::*;

pub use task::spawn;

/// The [Send] bound required to [spawn] a task on this platform.
///
/// On native platforms tasks may be moved between threads, thus this requires [Send].
///
/// In a JavaScript runtime environment (crate feature `js`) tasks are spawned onto the
/// current thread, thus this imposes no requirement and is implemented for every type.
/// This allows working with values that are not [Send], such as JavaScript objects.
#[cfg(not(feature = "js"))]
pub trait MaybeSend: Send {}

#[cfg(not(feature = "js"))]
impl<T: Send + ?Sized> MaybeSend for T {}

/// The [Send] bound required to [spawn] a task on this platform.
///
/// On native platforms tasks may be moved between threads, thus this requires [Send].
///
/// In a JavaScript runtime environment (crate feature `js`) tasks are spawned onto the
/// current thread, thus this imposes no requirement and is implemented for every type.
/// This allows working with values that are not [Send], such as JavaScript objects.
#[cfg(feature = "js")]
pub trait MaybeSend {}

#[cfg(feature = "js")]
impl<T: ?Sized> MaybeSend for T {}

/// Whether threads are available and working on this platform.
pub async fn are_threads_available() -> bool {
    use tokio::sync::{OnceCell, oneshot};

    static AVAILABLE: OnceCell<bool> = OnceCell::const_new();
    *AVAILABLE
        .get_or_init(|| async move {
            tracing::trace!("spawning test thread");

            let (tx, rx) = oneshot::channel();
            let res = std::thread::Builder::new().name("remoc thread test".into()).spawn(move || {
                tracing::trace!("test thread started");
                let _ = tx.send(());
            });

            match res {
                Ok(_) => {
                    tracing::trace!("waiting for test thread");
                    match rx.await {
                        Ok(()) => {
                            tracing::trace!("threads are available");
                            true
                        }
                        Err(_) => {
                            tracing::warn!("test thread failed, streaming (de)serialization disabled");
                            false
                        }
                    }
                }
                Err(os_error) => {
                    tracing::warn!(%os_error, "threads not available, streaming (de)serialization disabled");
                    false
                }
            }
        })
        .await
}
