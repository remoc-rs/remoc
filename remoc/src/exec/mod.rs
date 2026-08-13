//! Async executive for futures.
//!
//! On native platforms this uses Tokio.
//! On JavaScript this executes Futures as Promises.
//!

cfg_select! {
    all(target_family = "wasm", feature = "js") => {
        mod js;
        pub use js::*;
    }
    _ => {
        mod native;
        pub use native::*;
    }
}

#[doc(no_inline)]
pub use task::spawn;

/// Tests whether threads are available and working on this platform
/// by spawning a test thread.
pub async fn has_threads() -> bool {
    use tokio::sync::{OnceCell, oneshot};

    static AVAILABLE: OnceCell<bool> = OnceCell::const_new();
    *AVAILABLE
        .get_or_init(|| async move {
            tracing::trace!("spawning test thread");

            let (tx, rx) = oneshot::channel();
            let res = std::thread::Builder::new().name("threads available".into()).spawn(move || {
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
                            tracing::warn!("test thread failed");
                            false
                        }
                    }
                }
                Err(os_error) => {
                    tracing::warn!(%os_error, "threads not available");
                    false
                }
            }
        })
        .await
}
