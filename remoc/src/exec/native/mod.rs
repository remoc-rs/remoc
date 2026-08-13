//! Native async executive using Tokio.

pub mod runtime {
    pub use tokio::runtime::Handle;
}

pub mod task {
    use std::future::Future;

    pub use tokio::task::{JoinError, JoinHandle, spawn, spawn_blocking};

    /// Runs a future to completion.
    #[track_caller]
    pub fn block_on<F: Future>(future: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(future)
    }

    /// The [Send] bound required to [spawn] a task on this platform.
    ///
    /// On native platforms tasks may be moved between threads, thus this requires [Send].
    pub trait MaybeSend: Send {}
    impl<T: Send + ?Sized> MaybeSend for T {}
}

pub mod time {
    pub use tokio::time::{Instant, Sleep, Timeout, sleep, sleep_until, timeout};

    pub mod error {
        pub use tokio::time::error::Elapsed;
    }
}
