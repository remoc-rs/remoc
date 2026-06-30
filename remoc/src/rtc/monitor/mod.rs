//! Remote trait calling (RTC) monitors.

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
