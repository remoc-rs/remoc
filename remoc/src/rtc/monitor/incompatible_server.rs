//! Client monitor that logs and throttles calls to methods that fail on the server.

use futures::{FutureExt, future::BoxFuture};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};
use tracing::Level;

use crate::{
    exec::time::{Instant, sleep},
    rch::oneshot,
    rtc::{CallDecision, CallGuard, ClientMonitor, Req, ReqEnum},
};

/// Shared state for [`IncompatibleServerMonitor`].
#[derive(Debug, Default)]
struct State {
    /// Methods whose most recent call failed to be received by the server.
    failed_methods: HashSet<&'static str>,
    /// Start of the current failure-counting window.
    window_start: Option<Instant>,
    /// Number of receive failures within the current window.
    count: usize,
}

impl State {
    /// Records a receive failure within the current
    /// [window](IncompatibleServerMonitor::window), resetting the window if it
    /// has elapsed.
    fn record_failure(&mut self, window: Duration) {
        match self.window_start {
            Some(start) if start.elapsed() < window => self.count += 1,
            _ => {
                self.window_start = Some(Instant::now());
                self.count = 1;
            }
        }
    }

    /// Returns whether more than `limit` receive failures have occurred within
    /// the current [window](IncompatibleServerMonitor::window).
    fn over_limit(&self, limit: usize, window: Duration) -> bool {
        match self.window_start {
            Some(start) if start.elapsed() < window => self.count > limit,
            _ => false,
        }
    }
}

/// A [client monitor](ClientMonitor) that logs and throttles calls to methods
/// that fail to be received on the server.
///
/// The server is incompatible when it cannot decode the client's requests, for
/// example because the client was built against a different version of the
/// service trait. Such calls fail to be received by the server.
///
/// Repeatedly calling a method that keeps failing to be received generates a
/// steady stream of decode failures on the server and may cause the server's
/// [`IncompatibleClientMonitor`](super::IncompatibleClientMonitor) to terminate
/// the connection. This monitor prevents that by tracking which methods fail and
/// throttling further calls to them:
///
///   * Each method whose call fails to be received is remembered. A method is
///     forgotten again once a call to it is received by the server, whether it
///     returns successfully or with an application error.
///   * The total number of receive failures within the configured
///     [window](Self::window) is counted.
///   * When a remembered method is called while more than [`limit`](Self::limit)
///     failures have occurred within the current window, the call is delayed by
///     one whole window period before being sent. This stops the client from
///     busy-looping on a method that keeps failing.
///
/// If [logging](Self::log_level) is enabled, both each receive failure and each
/// throttling delay are logged at the configured [tracing](tracing) level,
/// including the trait and method name.
///
/// The [default limit](Self::DEFAULT_LIMIT) is chosen below the server's
/// [default](super::IncompatibleClientMonitor::DEFAULT_LIMIT) so that a
/// misbehaving client throttles itself before the server stops serving.
///
/// # Defaults
///
/// By default logging is enabled at the [`WARN`](Level::WARN) level and calls to
/// failing methods are throttled once more than [`10`](Self::DEFAULT_LIMIT)
/// failures occur within [`10 seconds`](Self::DEFAULT_WINDOW).
///
/// Install it on a client via
/// [`set_monitor`](crate::rtc::MonitorableClient::set_monitor).
#[derive(Debug)]
pub struct IncompatibleServerMonitor {
    log_level: Option<Level>,
    limit: Option<usize>,
    window: Duration,
    state: Arc<Mutex<State>>,
}

impl IncompatibleServerMonitor {
    /// The default logging level.
    pub const DEFAULT_LOG_LEVEL: Option<Level> = super::IncompatibleClientMonitor::DEFAULT_LOG_LEVEL;

    /// The default number of tolerated request failures within the
    /// [default window](Self::DEFAULT_WINDOW).
    ///
    /// This is chosen below the server's
    /// [default limit](super::IncompatibleClientMonitor::DEFAULT_LIMIT) so that
    /// the client throttles itself before the server stops serving due to
    /// too many invalid requests.
    pub const DEFAULT_LIMIT: Option<usize> = Some(super::IncompatibleClientMonitor::DEFAULT_LIMIT.unwrap() / 2);

    /// The default timeframe the [default limit](Self::DEFAULT_LIMIT) applies to.
    pub const DEFAULT_WINDOW: Duration = super::IncompatibleClientMonitor::DEFAULT_WINDOW;

    /// Creates a new monitor with default settings.
    ///
    /// See the [type-level documentation](Self#defaults) for the defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the [tracing](tracing) level at which request failures are logged.
    ///
    /// Pass `None` to disable logging.
    #[must_use]
    pub fn log_level(mut self, level: Option<Level>) -> Self {
        self.log_level = level;
        self
    }

    /// Sets the maximum number of receive failures tolerated within the
    /// [window](Self::window) before further calls to failing methods are
    /// throttled.
    ///
    /// Pass `None` to disable limiting, i.e. never throttle calls.
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
}

impl Default for IncompatibleServerMonitor {
    fn default() -> Self {
        Self {
            log_level: Self::DEFAULT_LOG_LEVEL,
            limit: Self::DEFAULT_LIMIT,
            window: Self::DEFAULT_WINDOW,
            state: Arc::new(Mutex::new(State::default())),
        }
    }
}

impl<Value, Ref, RefMut> ClientMonitor<Value, Ref, RefMut> for IncompatibleServerMonitor
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_call<'a>(&'a self, req: &'a Req<Value, Ref, RefMut>) -> BoxFuture<'a, CallDecision> {
        let trait_name = Req::<Value, Ref, RefMut>::trait_name();
        let method_name = req.method_name();

        // Throttle the call if the method is known to fail to be received by the
        // server and the failure limit is currently exceeded.
        let throttle = match self.limit {
            Some(limit) => {
                let state = self.state.lock().unwrap();
                state.failed_methods.contains(method_name) && state.over_limit(limit, self.window)
            }
            None => false,
        };

        let guard = IncompatibleServerGuard {
            window: self.window,
            state: self.state.clone(),
            log_level: self.log_level,
            trait_name,
            method_name,
            reply_failed: false,
        };

        async move {
            if throttle {
                let target = format!("{trait_name}::{method_name}");
                log_at!(self.log_level, %target, "delaying previously failed call");
                sleep(self.window).await;
            }

            CallDecision::Guard(Box::new(guard))
        }
        .boxed()
    }
}

/// Guard attached to each call by [`IncompatibleServerMonitor`].
struct IncompatibleServerGuard {
    window: Duration,
    state: Arc<Mutex<State>>,
    log_level: Option<Level>,
    trait_name: &'static str,
    method_name: &'static str,
    reply_failed: bool,
}

impl CallGuard for IncompatibleServerGuard {
    fn reply_failed(&mut self, error: &oneshot::RecvError) {
        self.reply_failed = true;

        let (trait_name, method_name) = (self.trait_name, self.method_name);
        let target = format!("{trait_name}::{method_name}");
        log_at!(self.log_level, %target, %error, "failed to call");
    }
}

impl Drop for IncompatibleServerGuard {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        if self.reply_failed {
            // The method failed to be received: remember it and count the failure.
            state.failed_methods.insert(self.method_name);
            state.record_failure(self.window);
        } else {
            // The method was received, so it is no longer expected to fail.
            state.failed_methods.remove(self.method_name);
        }
    }
}
