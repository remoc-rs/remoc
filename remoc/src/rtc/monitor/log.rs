//! Logging monitor.

use futures::{FutureExt, future::BoxFuture};
use std::collections::HashMap;
use tracing::Level;
use wokio::time::Instant;

use crate::{
    rch,
    rtc::{
        Req, ReqEnum,
        monitor::{
            CallDecision, CallGuard, ClientMonitor, DispatchDecision, DispatchGuard, RecvDecision,
            ReqReceiverMonitor, ServerMonitor,
        },
    },
};

/// A monitor that logs every request and its outcome.
///
/// This monitor can be installed on a
/// [client](super::MonitorableClient::set_monitor), a
/// [server](super::MonitorableServer::set_monitor) and a
/// [request receiver](super::MonitorableReqReceiver::set_monitor).
///
/// Two events are logged per request: one when it is made, dispatched or received, and
/// one when it has been processed, carrying how long it took and whether it failed.
///
/// The level is chosen per method, so that a chatty method can be logged less
/// prominently than the rest, or not at all. Requests that fail are logged at
/// [`failure_level`](Self::failure_level) instead.
///
/// # Example
///
/// ```
/// use remoc::rtc::monitor::LogMonitor;
/// use tracing::Level;
///
/// // Log every call at debug level, but the chatty `value` at trace level and
/// // `heartbeat` not at all.
/// let monitor = LogMonitor::new(Some(Level::DEBUG))
///     .method("value", Some(Level::TRACE))
///     .method("heartbeat", None);
/// ```
#[derive(Debug, Clone)]
pub struct LogMonitor {
    level: Option<Level>,
    failure_level: Option<Level>,
    methods: HashMap<&'static str, Option<Level>>,
}

impl LogMonitor {
    /// The level requests are logged at unless overridden, which is [`Level::DEBUG`].
    pub const DEFAULT_LEVEL: Option<Level> = Some(Level::DEBUG);

    /// The level failed requests are logged at, which is [`Level::WARN`].
    pub const DEFAULT_FAILURE_LEVEL: Option<Level> = Some(Level::WARN);

    /// Creates a monitor logging every request at the specified level.
    ///
    /// [`None`] logs no request, which is only useful together with a
    /// [per-method level](Self::method).
    pub fn new(level: Option<Level>) -> Self {
        Self { level, failure_level: Self::DEFAULT_FAILURE_LEVEL, methods: HashMap::new() }
    }

    /// Sets the level requests to the specified method are logged at.
    ///
    /// [`None`] logs no request to that method. The name is the method name as written
    /// in the trait, without the trait name.
    #[must_use]
    pub fn method(mut self, method: &'static str, level: Option<Level>) -> Self {
        self.methods.insert(method, level);
        self
    }

    /// Sets the level a request that failed is logged at, regardless of the level of
    /// its method.
    #[must_use]
    pub fn failure_level(mut self, level: Option<Level>) -> Self {
        self.failure_level = level;
        self
    }

    /// The level the specified method is logged at.
    fn level_of(&self, method: Option<&'static str>) -> Option<Level> {
        match method.and_then(|method| self.methods.get(method)) {
            Some(level) => *level,
            None => self.level,
        }
    }

    /// The name of the request and the level it is logged at.
    fn target_of<Value, Ref, RefMut>(
        &self, req: &Result<Option<Req<Value, Ref, RefMut>>, rch::mpsc::RecvError>,
    ) -> (String, Option<&'static str>, Option<Level>)
    where
        Value: ReqEnum,
        Ref: ReqEnum,
        RefMut: ReqEnum,
    {
        let trait_name = Req::<Value, Ref, RefMut>::trait_name();
        let method = if let Ok(Some(req)) = req { Some(req.method_name()) } else { None };
        let target = match method {
            Some(method) => format!("{trait_name}::{method}"),
            None => trait_name.to_string(),
        };
        (target, method, self.level_of(method))
    }

    /// Logs that a request is starting and returns the guard logging its outcome.
    fn started(&self, target: String, level: Option<Level>, what: &'static str) -> LogGuard {
        log_at!(level, target = %target, "{what}");
        LogGuard { target, level, failure_level: self.failure_level, started: Instant::now(), failed: false }
    }
}

impl Default for LogMonitor {
    fn default() -> Self {
        Self::new(Self::DEFAULT_LEVEL)
    }
}

/// Logs the outcome of a request when it has been processed.
#[derive(Debug)]
struct LogGuard {
    target: String,
    level: Option<Level>,
    failure_level: Option<Level>,
    started: Instant,
    failed: bool,
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        if self.failed {
            log_at!(self.failure_level, target = %self.target, ?elapsed, "failed");
        } else {
            log_at!(self.level, target = %self.target, ?elapsed, "done");
        }
    }
}

impl CallGuard for LogGuard {
    fn failed(&mut self) {
        self.failed = true;
    }

    fn response_failed(&mut self, err: &rch::oneshot::RecvError) {
        log_at!(self.failure_level, target = %self.target, %err, "receiving the response failed");
        self.failed = true;
    }
}

impl DispatchGuard for LogGuard {
    fn failed(&mut self) {
        self.failed = true;
    }
}

impl<Value, Ref, RefMut> ClientMonitor<Value, Ref, RefMut> for LogMonitor
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_call<'a>(&'a self, req: &'a Req<Value, Ref, RefMut>) -> BoxFuture<'a, CallDecision> {
        let method = req.method_name();
        let target = format!("{}::{}", Req::<Value, Ref, RefMut>::trait_name(), method);
        let level = self.level_of(Some(method));
        async move { CallDecision::Guard(Box::new(self.started(target, level, "calling"))) }.boxed()
    }
}

impl<Value, Ref, RefMut> ServerMonitor<Value, Ref, RefMut> for LogMonitor
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_dispatch<'a>(
        &'a mut self, req: &'a Result<Option<Req<Value, Ref, RefMut>>, rch::mpsc::RecvError>,
    ) -> BoxFuture<'a, DispatchDecision> {
        let (target, _, level) = self.target_of(req);
        async move { DispatchDecision::Guard(Box::new(self.started(target, level, "dispatching"))) }.boxed()
    }
}

impl<Value, Ref, RefMut> ReqReceiverMonitor<Value, Ref, RefMut> for LogMonitor
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_recv<'a>(
        &'a mut self, req: &'a Result<Option<Req<Value, Ref, RefMut>>, rch::mpsc::RecvError>,
    ) -> BoxFuture<'a, RecvDecision> {
        let (target, _, level) = self.target_of(req);
        log_at!(level, target = %target, "received");
        async move { RecvDecision::Pass }.boxed()
    }
}
