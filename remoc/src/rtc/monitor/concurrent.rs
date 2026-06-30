//! Concurrent request limiting server monitor.

use futures::{FutureExt, future::BoxFuture};
use std::{num::NonZeroUsize, sync::Arc};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::Level;

use crate::{
    rch,
    rtc::{DispatchDecision, DispatchGuard, Req, ReqEnum, ServerMonitor},
};

/// A [server monitor](ServerMonitor) that limits the number of concurrent requests from clients.
///
/// At most the configured number of requests are dispatched and processed at the
/// same time. Further requests are not rejected but delayed (queued) until an
/// in-flight request completes and frees up a slot. Queued requests are released
/// in the order they arrive.
///
/// Construct a monitor using [`new`](Self::new) and install it on a server with
/// [`set_monitor`](super::super::MonitorableServer::set_monitor).
///
/// # Clone
/// Cloning the monitor produces a handle that shares the same concurrency limit,
/// i.e. requests processed by all clones count against one common limit. The log
/// level, however, is configured independently per clone.
#[derive(Debug, Clone)]
pub struct ConcurrentLimitMonitor {
    semaphore: Arc<Semaphore>,
    log_level: Option<Level>,
}

impl ConcurrentLimitMonitor {
    /// The default logging level used when the concurrency limit delays a request.
    pub const DEFAULT_LOG_LEVEL: Option<Level> = Some(Level::TRACE);

    /// Creates a new monitor that allows at most `concurrent_requests` requests
    /// to be processed simultaneously.
    ///
    /// Logging of limiting events is enabled at the
    /// [default level](Self::DEFAULT_LOG_LEVEL) and can be changed using
    /// [`log_level`](Self::log_level).
    pub fn new(concurrent_requests: NonZeroUsize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(concurrent_requests.get())),
            log_level: Self::DEFAULT_LOG_LEVEL,
        }
    }

    /// Sets the [tracing](tracing) level at which a [tracing](tracing) event is
    /// emitted whenever a request is delayed due to the concurrency limit.
    ///
    /// Pass `None` to disable logging entirely.
    #[must_use]
    pub fn log_level(mut self, level: Option<Level>) -> Self {
        self.log_level = level;
        self
    }
}

#[allow(dead_code)]
struct ConcurrentGuard(OwnedSemaphorePermit);
impl DispatchGuard for ConcurrentGuard {}

impl<Value, Ref, RefMut> ServerMonitor<Value, Ref, RefMut> for ConcurrentLimitMonitor
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
            let permit = match self.semaphore.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    let target = match method_name {
                        Some(method_name) => format!("{trait_name}::{method_name}"),
                        None => trait_name.to_string(),
                    };
                    log_at!(self.log_level, %target, "queueing call due to concurrent request limit");
                    self.semaphore.clone().acquire_owned().await.unwrap()
                }
            };

            DispatchDecision::Guard(Box::new(ConcurrentGuard(permit)))
        }
        .boxed()
    }
}
