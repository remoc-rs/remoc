//! Remote trait calling (RTC) monitors.
//!
//! A monitor is installed on a [client](super::MonitorableClient::set_monitor),
//! [server](super::MonitorableServer::set_monitor) or
//! [request receiver](super::MonitorableReqReceiver::set_monitor) before it is
//! sent to a remote endpoint or starts serving.
//! Use [`ChainedMonitor`](super::ChainedMonitor) to install more than one.
//!
//! # Example
//!
//! In the following example the server accepts at most 10 requests per second.
//! Further requests are delayed rather than rejected.
//!
//! ```
//! use std::{num::NonZeroUsize, sync::Arc, time::Duration};
//! use remoc::prelude::*;
//! use remoc::rtc::{CallError, monitor::RateLimitMonitor};
//!
//! #[rtc::remote]
//! pub trait Counter {
//!     async fn value(&self) -> Result<u32, CallError>;
//! }
//!
//! pub struct CounterObj;
//!
//! impl Counter for CounterObj {
//!     async fn value(&self) -> Result<u32, CallError> {
//!         Ok(42)
//!     }
//! }
//!
//! // This would be run on the server.
//! async fn server(mut tx: rch::base::Sender<CounterClient>) {
//!     let (mut server, client) = CounterServerShared::new(Arc::new(CounterObj), 1);
//!
//!     // The monitor must be set before serving starts.
//!     let limit = RateLimitMonitor::new(NonZeroUsize::new(10).unwrap(), Duration::from_secs(1));
//!     server.set_monitor(limit);
//!
//!     tx.send(client).await.unwrap();
//!     server.serve(true).await.unwrap();
//! }
//!
//! // This would be run on the client.
//! async fn client(mut rx: rch::base::Receiver<CounterClient>) {
//!     let counter = rx.recv().await.unwrap().unwrap();
//!     assert_eq!(counter.value().await.unwrap(), 42);
//! }
//! # tokio_test::block_on(remoc::doctest::client_server(server, client));
//! ```

/// Emits a [`tracing`] event at the level given by an `Option<Level>`.
macro_rules! log_at {
    ($level:expr, $($arg:tt)*) => {
        match $level {
            Some(::tracing::Level::ERROR) => ::tracing::error!($($arg)*),
            Some(::tracing::Level::WARN) => ::tracing::warn!($($arg)*),
            Some(::tracing::Level::INFO) => ::tracing::info!($($arg)*),
            Some(::tracing::Level::DEBUG) => ::tracing::debug!($($arg)*),
            Some(::tracing::Level::TRACE) => ::tracing::trace!($($arg)*),
            None => (),
        }
    };
}

mod concurrent;
mod incompatible_client;
mod incompatible_server;
mod rate_limit;

pub use concurrent::ConcurrentLimitMonitor;
pub use incompatible_client::{IncompatibleClientLimitExceeded, IncompatibleClientMonitor};
pub use incompatible_server::IncompatibleServerMonitor;
pub use rate_limit::RateLimitMonitor;
