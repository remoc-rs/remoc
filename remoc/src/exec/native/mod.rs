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
}

pub mod time {
    pub use tokio::time::{Instant, Sleep, Timeout, sleep, timeout};

    pub mod error {
        pub use tokio::time::error::Elapsed;
    }
}
