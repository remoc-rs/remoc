//! Server monitor that handles incompatible clients.

use futures::{FutureExt, future::BoxFuture};
use std::{error::Error, fmt, future, time::Duration};
use tracing::Level;

use crate::{
    exec::time::Instant,
    rch::mpsc,
    rtc::{DispatchDecision, Req, ReqEnum, ServerMonitor},
};

/// Error returned by [`IncompatibleClientMonitor`] when too many requests fail
/// to decode within the configured timeframe.
///
/// The server stops serving with [`ServeError::Monitor`](crate::rtc::ServeError::Monitor)
/// holding this error.
#[derive(Debug, Clone)]
pub struct IncompatibleClientLimitExceeded {
    /// The configured limit that was exceeded.
    pub limit: usize,
    /// The timeframe the limit applies to.
    pub window: Duration,
}

impl fmt::Display for IncompatibleClientLimitExceeded {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "more than {} requests failed to receive within {} s", self.limit, self.window.as_secs_f32())
    }
}

impl Error for IncompatibleClientLimitExceeded {}

/// A [server monitor](ServerMonitor) that logs and rate-limits requests from
/// incompatible clients.
///
/// A client is incompatible when the server cannot decode its requests, for
/// example because the client was built against a different version of the
/// service trait. Such requests are observed by the server as non-final receive
/// failures.
///
/// This monitor handles each such failure as follows:
///
///   * if [logging](Self::log_level) is enabled, the error is logged at the
///     configured [tracing](tracing) level, and
///   * if [limiting](Self::limit) is enabled, the failure is counted and the
///     server is stopped with [`IncompatibleClientLimitExceeded`] once more than
///     `limit` requests fail to receive within the configured [window](Self::window).
///
/// Otherwise the offending request is dropped and serving continues.
///
/// # Defaults
///
/// By default logging is enabled at the [`WARN`](Level::WARN) level and the
/// server fails once more than [`20`](Self::DEFAULT_LIMIT) requests fail to
/// receive within [`10 seconds`](Self::DEFAULT_WINDOW).
///
/// Install it on a server via
/// [`set_monitor`](crate::rtc::MonitorableServer::set_monitor).
#[derive(Debug)]
pub struct IncompatibleClientMonitor {
    log_level: Option<Level>,
    limit: Option<usize>,
    window: Duration,
    window_start: Option<Instant>,
    count: usize,
}

impl IncompatibleClientMonitor {
    /// The default logging level.
    pub const DEFAULT_LOG_LEVEL: Option<Level> = Some(Level::WARN);

    /// The default number of tolerated decode failures within the
    /// [default window](Self::DEFAULT_WINDOW).
    pub const DEFAULT_LIMIT: Option<usize> = Some(20);

    /// The default timeframe the [default limit](Self::DEFAULT_LIMIT) applies to.
    pub const DEFAULT_WINDOW: Duration = Duration::from_secs(10);

    /// Creates a new monitor with default settings.
    ///
    /// See the [type-level documentation](Self#defaults) for the defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the [tracing](tracing) level at which failures are logged.
    ///
    /// Pass `None` to disable logging.
    #[must_use]
    pub fn log_level(mut self, level: Option<Level>) -> Self {
        self.log_level = level;
        self
    }

    /// Sets the maximum number of failures tolerated within the
    /// [window](Self::window).
    ///
    /// Pass `None` to disable limiting, i.e. tolerate an unlimited number of
    /// failures.
    #[must_use]
    pub fn limit(mut self, limit: Option<usize>) -> Self {
        self.limit = limit;
        self
    }

    /// Sets the timeframe the [limit](Self::limit) applies to.
    #[must_use]
    pub fn window(mut self, window: Duration) -> Self {
        self.window = window;
        self
    }

    /// Counts a decode failure and returns whether the [limit](Self::limit) has
    /// been exceeded.
    ///
    /// The failure count is reset whenever the current [window](Self::window)
    /// has elapsed.
    fn count_failure(&mut self, limit: usize) -> bool {
        match self.window_start {
            Some(start) if start.elapsed() < self.window => self.count += 1,
            _ => {
                self.window_start = Some(Instant::now());
                self.count = 1;
            }
        }
        self.count > limit
    }

    /// Handles a non-final request decode failure.
    fn on_decode_error(&mut self, error: &mpsc::RecvError) -> DispatchDecision {
        log_at!(self.log_level, %error, "failed to receive request");

        let Some(limit) = self.limit else {
            return DispatchDecision::Drop;
        };

        if self.count_failure(limit) {
            DispatchDecision::Error(Box::new(IncompatibleClientLimitExceeded { limit, window: self.window }))
        } else {
            DispatchDecision::Drop
        }
    }
}

impl Default for IncompatibleClientMonitor {
    fn default() -> Self {
        Self {
            log_level: Self::DEFAULT_LOG_LEVEL,
            limit: Self::DEFAULT_LIMIT,
            window: Self::DEFAULT_WINDOW,
            window_start: None,
            count: 0,
        }
    }
}

impl<Value, Ref, RefMut> ServerMonitor<Value, Ref, RefMut> for IncompatibleClientMonitor
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_dispatch<'a>(
        &mut self, req: &'a Result<Option<Req<Value, Ref, RefMut>>, mpsc::RecvError>,
    ) -> BoxFuture<'a, DispatchDecision> {
        let decision = match req {
            Err(err) if !err.is_final() => self.on_decode_error(err),
            _ => DispatchDecision::Pass,
        };
        future::ready(decision).boxed()
    }
}
