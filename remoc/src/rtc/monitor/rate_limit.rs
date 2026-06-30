//! Rate limiting server monitor.

use futures::{FutureExt, future::BoxFuture};
use std::{collections::VecDeque, num::NonZeroUsize, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tracing::Level;

use crate::{
    exec::time::{Instant, sleep},
    rch,
    rtc::{DispatchDecision, Req, ReqEnum, ServerMonitor},
};

/// A [server monitor](ServerMonitor) that rate-limits requests from clients.
///
/// At most `requests` requests are dispatched within any sliding time `window`.
/// Requests exceeding this limit are not rejected but delayed (queued) until
/// dispatching them would no longer exceed the limit. Requests are released in
/// the order they arrive.
///
/// Construct a monitor using [`new`](Self::new) and install it on a server with
/// [`set_monitor`](super::super::MonitorableServer::set_monitor).
///
/// # Clone
/// Cloning the monitor produces a handle that shares the same rate limit, i.e.
/// requests processed by all clones count against one common limit. The log
/// level, however, is configured independently per clone.
#[derive(Debug, Clone)]
pub struct RateLimitMonitor {
    requests: NonZeroUsize,
    window: Duration,
    history: Arc<Mutex<VecDeque<Instant>>>,
    log_level: Option<Level>,
}

impl RateLimitMonitor {
    /// The default logging level used when rate limiting delays a request.
    pub const DEFAULT_LOG_LEVEL: Option<Level> = Some(Level::TRACE);

    /// Creates a new monitor that allows at most `requests` requests within any
    /// sliding time `window`.
    ///
    /// Logging of rate-limiting events is enabled at the
    /// [default level](Self::DEFAULT_LOG_LEVEL) and can be changed using
    /// [`log_level`](Self::log_level).
    pub fn new(requests: NonZeroUsize, window: Duration) -> Self {
        Self {
            requests,
            window,
            history: Arc::new(Mutex::new(VecDeque::new())),
            log_level: Self::DEFAULT_LOG_LEVEL,
        }
    }

    /// Sets the [tracing](tracing) level at which a [tracing](tracing) event is
    /// emitted whenever a request is delayed due to rate limiting.
    ///
    /// Pass `None` to disable logging entirely.
    #[must_use]
    pub fn log_level(mut self, level: Option<Level>) -> Self {
        self.log_level = level;
        self
    }
}

impl<Value, Ref, RefMut> ServerMonitor<Value, Ref, RefMut> for RateLimitMonitor
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_dispatch<'a>(
        &'a mut self, req: &'a Result<Option<Req<Value, Ref, RefMut>>, rch::mpsc::RecvError>,
    ) -> BoxFuture<'a, DispatchDecision> {
        let trait_name = Req::<Value, Ref, RefMut>::trait_name();
        let method_name = if let Ok(Some(req)) = req { Some(req.method_name()) } else { None };

        async move {
            // The lock is intentionally held across the `sleep` below. The rate limit is global
            // (shared by all clones via `history`), so whenever the window is saturated every other
            // call would have to wait for the same oldest entry to expire anyway. Holding the lock
            // therefore queues subsequent calls in arrival order (tokio's `Mutex` is FIFO-fair)
            // without reducing throughput, while keeping dispatch strictly in order.
            let mut history = self.history.lock().await;

            loop {
                while let Some(front) = history.front()
                    && front.elapsed() >= self.window
                {
                    history.pop_front();
                }

                if history.len() < self.requests.get() {
                    break;
                }

                let target = match method_name {
                    Some(method_name) => format!("{trait_name}::{method_name}"),
                    None => trait_name.to_string(),
                };
                log_at!(self.log_level, %target, "delaying this and possibly subsequent calls due to rate limiting");

                let front = history.front().unwrap();
                sleep(self.window - front.elapsed()).await;
            }

            history.push_back(Instant::now());
            DispatchDecision::Pass
        }
        .boxed()
    }
}
