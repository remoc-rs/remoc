//! Remote trait calling (RTC) monitors.
//!
//! A monitor is installed on a [client](MonitorableClient::set_monitor),
//! [server](MonitorableServer::set_monitor) or
//! [request receiver](MonitorableReqReceiver::set_monitor) before it is
//! sent to a remote endpoint or starts serving.
//! Use [`ChainedMonitor`] to install more than one.
//!
//! # Default monitors
//!
//! A client, server and request receiver each start out with a monitor that keeps
//! them working when the remote endpoint was built against a different version of
//! the trait:
//!
//!   * a server and a request receiver use an [`IncompatibleClientMonitor`], which
//!     skips a request it cannot decode instead of stopping to serve, so that a
//!     client calling a method they do not know does not end the session,
//!   * a client uses an [`IncompatibleServerMonitor`], which throttles calls to a
//!     method that the server repeatedly fails to receive.
//!
//! Both give up once the failures exceed their limit, so a wholly incompatible
//! endpoint is still reported rather than retried forever.
//!
//! [`set_monitor`](MonitorableServer::set_monitor) replaces the default monitor;
//! use [`add_monitor`](MonitorableServer::add_monitor) to keep it and install
//! another one alongside. Set a [`PassMonitor`] to remove it deliberately.
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
//!     let (mut server, client) = CounterServerShared::new(Arc::new(CounterObj));
//!
//!     // The monitor must be set before serving starts.
//!     let limit = RateLimitMonitor::new(NonZeroUsize::new(10).unwrap(), Duration::from_secs(1));
//!     server.set_monitor(limit);
//!
//!     tx.send(client).await.unwrap();
//!     server.serve().await.unwrap();
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

use futures::{FutureExt, future::BoxFuture};
use std::{error::Error, fmt, sync::Arc};

use super::{Req, ReqEnum};
use crate::rch::{mpsc, oneshot};

mod concurrent;
mod incompatible_client;
mod incompatible_server;
mod log;
mod rate_limit;

pub use concurrent::ConcurrentLimitMonitor;
pub use incompatible_client::{IncompatibleClientLimitExceeded, IncompatibleClientMonitor};
pub use incompatible_server::IncompatibleServerMonitor;
pub use log::LogMonitor;
pub use rate_limit::RateLimitMonitor;

/// Allows setting the [client monitor](ClientMonitor) on a [client](super::Client).
pub trait MonitorableClient {
    /// Type of request by value (`self`).
    type Value: ReqEnum;
    /// Type of request by reference (`&self`).
    type Ref: ReqEnum;
    /// Type of request by mutable reference (`&mut self`).
    type RefMut: ReqEnum;

    /// Sets the [client monitor](ClientMonitor), replacing the one that is installed.
    ///
    /// This also removes the [monitor installed by default](self#default-monitors).
    /// Use [`add_monitor`](Self::add_monitor) to keep it.
    fn set_monitor(&mut self, monitor: impl ClientMonitor<Self::Value, Self::Ref, Self::RefMut> + 'static);

    /// Adds a [client monitor](ClientMonitor), keeping the one that is installed.
    ///
    /// The installed monitor is consulted first; see [`ChainedMonitor`] for how
    /// the decisions of both are combined.
    fn add_monitor(&mut self, monitor: impl ClientMonitor<Self::Value, Self::Ref, Self::RefMut> + 'static);
}

/// Allows monitoring each request a client makes.
pub trait ClientMonitor<Value, Ref, RefMut>: Send + Sync
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    /// Called for each request before sending it to server.
    ///
    /// The function can inspect the request and decide whether it should be
    /// sent to the server for processing or dropped.
    fn pre_call<'a>(&'a self, req: &'a Req<Value, Ref, RefMut>) -> BoxFuture<'a, CallDecision>;
}

/// Decision on how a request should be processed made by the [client monitor](ClientMonitor).
pub enum CallDecision {
    /// Process the request normally.
    ///
    /// The request is sent to the server for processing.
    Pass,
    /// Guard the request and process it normally.
    ///
    /// The request is processed as if [`Pass`](Self::Pass) is specified.
    /// However, the supplied [`CallGuard`] is held during processing and dropped
    /// once the request is finished.
    Guard(Box<dyn CallGuard>),
    /// Drop the request.
    ///
    /// The called client method fails with [`CallError::Dropped`](super::CallError::Dropped).
    Drop,
}

impl fmt::Debug for CallDecision {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Pass => write!(f, "Pass"),
            Self::Guard(_) => write!(f, "Guard"),
            Self::Drop => write!(f, "Drop"),
        }
    }
}

/// Request call guard.
///
/// It is held until the guarded request is processed and then dropped.
pub trait CallGuard: Send {
    /// Notifies the request call guard that the called method returned
    /// an error.
    fn failed(&mut self) {}

    /// Notifies the request call guard that receiving the response from the
    /// server failed.
    fn response_failed(&mut self, err: &oneshot::RecvError) {
        let _ = err;
    }
}

/// Combines two [client](ClientMonitor) or [server](ServerMonitor) monitors into one.
///
/// Construct it directly from the two monitors to combine, for example
/// `ChainedMonitor(first, second)`, and install the result on a client or server.
/// To combine more than two monitors, nest the construction, e.g.
/// `ChainedMonitor(a, ChainedMonitor(b, c))`.
///
/// For each request the two monitors are evaluated in order: first `self.0`, then
/// `self.1`. The combined decision is formed as follows:
///
///  * If a monitor drops the request ([`CallDecision::Drop`] / [`DispatchDecision::Drop`]),
///    the request is dropped and the remaining monitor is not evaluated.
///  * For a server monitor, if a monitor returns [`DispatchDecision::Error`], serving
///    stops with that error and the remaining monitor is not evaluated.
///  * Otherwise the request passes. Any guard produced by either monitor is held for
///    the duration of the request and released once it finishes. Guards are released
///    in reverse order, i.e. `self.1`'s guard is dropped before `self.0`'s, and guard
///    notifications ([`failed`](CallGuard::failed), [`response_failed`](CallGuard::response_failed)
///    and [`failed`](DispatchGuard::failed)) are forwarded to both.
///
/// Because evaluation is sequential and short-circuits, the order matters for monitors
/// that account for a request only while their returned future is awaited (such as the
/// [rate](RateLimitMonitor) and [concurrent](ConcurrentLimitMonitor)
/// limiters): a request dropped or rejected by `self.0` is never seen by `self.1`.
pub struct ChainedMonitor<A, B>(pub A, pub B);

impl<A, B, Value, Ref, RefMut> ClientMonitor<Value, Ref, RefMut> for ChainedMonitor<A, B>
where
    A: ClientMonitor<Value, Ref, RefMut>,
    B: ClientMonitor<Value, Ref, RefMut>,
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_call<'a>(&'a self, req: &'a Req<Value, Ref, RefMut>) -> BoxFuture<'a, CallDecision> {
        let pre_call_0 = self.0.pre_call(req);
        let pre_call_1 = self.1.pre_call(req);

        async move {
            let guard_0 = match pre_call_0.await {
                CallDecision::Pass => None,
                CallDecision::Guard(guard) => Some(guard),
                CallDecision::Drop => return CallDecision::Drop,
            };

            let guard_1 = match pre_call_1.await {
                CallDecision::Pass => None,
                CallDecision::Guard(guard) => Some(guard),
                CallDecision::Drop => return CallDecision::Drop,
            };

            match (guard_0, guard_1) {
                (None, None) => CallDecision::Pass,
                (Some(guard0), None) => CallDecision::Guard(guard0),
                (None, Some(guard1)) => CallDecision::Guard(guard1),
                (Some(guard0), Some(guard1)) => CallDecision::Guard(Box::new(ChainedCallGuard(guard1, guard0))),
            }
        }
        .boxed()
    }
}

struct ChainedCallGuard(Box<dyn CallGuard>, Box<dyn CallGuard>);
impl CallGuard for ChainedCallGuard {
    fn failed(&mut self) {
        self.0.failed();
        self.1.failed();
    }

    fn response_failed(&mut self, err: &oneshot::RecvError) {
        self.0.response_failed(err);
        self.1.response_failed(err);
    }
}

/// Allows setting the [server monitor](ServerMonitor) on a [server](super::ServerBase).
pub trait MonitorableServer {
    /// Type of request by value (`self`).
    type Value: ReqEnum;
    /// Type of request by reference (`&self`).
    type Ref: ReqEnum;
    /// Type of request by mutable reference (`&mut self`).
    type RefMut: ReqEnum;

    /// Sets the [server monitor](ServerMonitor), replacing the one that is installed.
    ///
    /// This also removes the [monitor installed by default](self#default-monitors).
    /// Use [`add_monitor`](Self::add_monitor) to keep it.
    fn set_monitor(&mut self, monitor: impl ServerMonitor<Self::Value, Self::Ref, Self::RefMut> + 'static);

    /// Adds a [server monitor](ServerMonitor), keeping the one that is installed.
    ///
    /// The installed monitor is consulted first; see [`ChainedMonitor`] for how
    /// the decisions of both are combined.
    fn add_monitor(&mut self, monitor: impl ServerMonitor<Self::Value, Self::Ref, Self::RefMut> + 'static);
}

/// Allows monitoring each request a server handles.
pub trait ServerMonitor<Value, Ref, RefMut>: Send
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    /// Called for each request before dispatch to its handling method.
    ///
    /// The function can inspect the request and decide whether it should be
    /// handled, dropped or the server should fail with a custom error.
    fn pre_dispatch<'a>(
        &'a mut self, req: &'a Result<Option<Req<Value, Ref, RefMut>>, mpsc::RecvError>,
    ) -> BoxFuture<'a, DispatchDecision>;
}

/// Allows setting the [request receiver monitor](ReqReceiverMonitor) on a
/// [request receiver](super::ReqReceiver).
pub trait MonitorableReqReceiver {
    /// Type of request by value (`self`).
    type Value: ReqEnum;
    /// Type of request by reference (`&self`).
    type Ref: ReqEnum;
    /// Type of request by mutable reference (`&mut self`).
    type RefMut: ReqEnum;

    /// Sets the [request receiver monitor](ReqReceiverMonitor), replacing the one that is installed.
    ///
    /// This also removes the [monitor installed by default](self#default-monitors).
    /// Use [`add_monitor`](Self::add_monitor) to keep it.
    fn set_monitor(&mut self, monitor: impl ReqReceiverMonitor<Self::Value, Self::Ref, Self::RefMut> + 'static);

    /// Adds a [request receiver monitor](ReqReceiverMonitor), keeping the one that is installed.
    ///
    /// The installed monitor is consulted first; see [`ChainedMonitor`] for how
    /// the decisions of both are combined.
    fn add_monitor(&mut self, monitor: impl ReqReceiverMonitor<Self::Value, Self::Ref, Self::RefMut> + 'static);
}

/// Allows monitoring each request a [request receiver](super::ReqReceiver) receives.
///
/// Unlike a [server monitor](ServerMonitor), it cannot guard a request or stop
/// the receiver with a custom error; it can only let a request [pass](RecvDecision::Pass)
/// or [drop](RecvDecision::Drop) it.
pub trait ReqReceiverMonitor<Value, Ref, RefMut>: Send
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    /// Called for each received request before it is returned from
    /// [`ReqReceiver::recv`](super::ReqReceiver::recv).
    ///
    /// The function can inspect the request and decide whether it should be
    /// returned to the caller or dropped.
    fn pre_recv<'a>(
        &'a mut self, req: &'a Result<Option<Req<Value, Ref, RefMut>>, mpsc::RecvError>,
    ) -> BoxFuture<'a, RecvDecision>;
}

impl<A, B, Value, Ref, RefMut> ReqReceiverMonitor<Value, Ref, RefMut> for ChainedMonitor<A, B>
where
    A: ReqReceiverMonitor<Value, Ref, RefMut>,
    B: ReqReceiverMonitor<Value, Ref, RefMut>,
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_recv<'a>(
        &'a mut self, req: &'a Result<Option<Req<Value, Ref, RefMut>>, mpsc::RecvError>,
    ) -> BoxFuture<'a, RecvDecision> {
        let pre_recv_0 = self.0.pre_recv(req);
        let pre_recv_1 = self.1.pre_recv(req);

        async move {
            match pre_recv_0.await {
                RecvDecision::Pass => (),
                RecvDecision::Drop => return RecvDecision::Drop,
            }

            pre_recv_1.await
        }
        .boxed()
    }
}

/// Decision on how a received request should be processed made by the
/// [request receiver monitor](ReqReceiverMonitor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvDecision {
    /// Return the request to the caller of [`ReqReceiver::recv`](super::ReqReceiver::recv).
    Pass,
    /// Drop the request and receive the next one.
    ///
    /// The client-side method fails with [`CallError::Dropped`](super::CallError::Dropped).
    Drop,
}

impl<A, B, Value, Ref, RefMut> ServerMonitor<Value, Ref, RefMut> for ChainedMonitor<A, B>
where
    A: ServerMonitor<Value, Ref, RefMut>,
    B: ServerMonitor<Value, Ref, RefMut>,
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_dispatch<'a>(
        &'a mut self, req: &'a Result<Option<Req<Value, Ref, RefMut>>, mpsc::RecvError>,
    ) -> BoxFuture<'a, DispatchDecision> {
        let pre_dispatch_0 = self.0.pre_dispatch(req);
        let pre_dispatch_1 = self.1.pre_dispatch(req);

        async move {
            let guard_0 = match pre_dispatch_0.await {
                DispatchDecision::Pass => None,
                DispatchDecision::Guard(guard) => Some(guard),
                DispatchDecision::Drop => return DispatchDecision::Drop,
                DispatchDecision::Error(err) => return DispatchDecision::Error(err),
            };

            let guard_1 = match pre_dispatch_1.await {
                DispatchDecision::Pass => None,
                DispatchDecision::Guard(guard) => Some(guard),
                DispatchDecision::Drop => return DispatchDecision::Drop,
                DispatchDecision::Error(err) => return DispatchDecision::Error(err),
            };

            match (guard_0, guard_1) {
                (None, None) => DispatchDecision::Pass,
                (Some(guard0), None) => DispatchDecision::Guard(guard0),
                (None, Some(guard1)) => DispatchDecision::Guard(guard1),
                (Some(guard0), Some(guard1)) => {
                    DispatchDecision::Guard(Box::new(ChainedDispatchGuard(guard1, guard0)))
                }
            }
        }
        .boxed()
    }
}

struct ChainedDispatchGuard(Box<dyn DispatchGuard>, Box<dyn DispatchGuard>);
impl DispatchGuard for ChainedDispatchGuard {
    fn failed(&mut self) {
        self.0.failed();
        self.1.failed();
    }
}

/// Request dispatch guard.
///
/// It is held until the guarded request is processed and then dropped.
pub trait DispatchGuard: Send {
    /// Notifies the request dispatch guard that the called method returned
    /// an error.
    fn failed(&mut self) {}
}

/// Decision on how a request should be processed made by the [server monitor](ServerMonitor).
pub enum DispatchDecision {
    /// Process the request normally.
    ///
    /// In case of the server monitor, the request is dispatched to the corresponding
    /// function of the remotable trait implementation.
    Pass,
    /// Guard the request and process it normally.
    ///
    /// The request is processed as if [`Pass`](Self::Pass) is specified.
    /// However, the supplied [`DispatchGuard`] is held during processing and dropped
    /// once the request is finished.
    Guard(Box<dyn DispatchGuard>),
    /// Drop the request.
    ///
    /// The client-side method fails with [`CallError::Dropped`](super::CallError::Dropped).
    Drop,
    /// Stop serving and fail returning [`ServeError::Monitor`](super::ServeError::Monitor).
    Error(Box<dyn Error + Send>),
}

impl fmt::Debug for DispatchDecision {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Pass => write!(f, "Pass"),
            Self::Guard(_) => write!(f, "Guard"),
            Self::Drop => write!(f, "Drop"),
            Self::Error(err) => f.debug_tuple("Error").field(err).finish(),
        }
    }
}

#[macro_export]
#[doc(hidden)]
macro_rules! server_monitor_pre_dispatch {
    ($monitor:expr, $req:expr) => {
        match $monitor.pre_dispatch(&$req).await {
            ::remoc::rtc::monitor::DispatchDecision::Pass => {
                ::std::boxed::Box::new(::remoc::rtc::monitor::PassGuard)
            }
            ::remoc::rtc::monitor::DispatchDecision::Guard(guard) => guard,
            ::remoc::rtc::monitor::DispatchDecision::Drop => {
                match &$req {
                    Ok(None) => (),
                    Err(err) if err.is_disconnected() => (),
                    _ => continue,
                }
                ::std::boxed::Box::new(::remoc::rtc::monitor::PassGuard)
            }
            ::remoc::rtc::monitor::DispatchDecision::Error(err) => {
                return Err(::remoc::rtc::ServeError::Monitor(err))
            }
        }
    };
    ($monitor:expr, $req:expr, $target:expr) => {
        match $monitor.pre_dispatch(&$req).await {
            ::remoc::rtc::monitor::DispatchDecision::Pass => {
                ::std::boxed::Box::new(::remoc::rtc::monitor::PassGuard)
            }
            ::remoc::rtc::monitor::DispatchDecision::Guard(guard) => guard,
            ::remoc::rtc::monitor::DispatchDecision::Drop => {
                match &$req {
                    Ok(None) => (),
                    Err(err) if err.is_disconnected() => (),
                    _ => continue,
                }
                ::std::boxed::Box::new(::remoc::rtc::monitor::PassGuard)
            }
            ::remoc::rtc::monitor::DispatchDecision::Error(err) => {
                return (Some($target), Err(::remoc::rtc::ServeError::Monitor(err)))
            }
        }
    };
}
#[doc(hidden)]
pub use crate::server_monitor_pre_dispatch;

#[macro_export]
#[doc(hidden)]
macro_rules! req_receiver_monitor_pre_recv {
    ($monitor:expr, $req:expr) => {
        match $monitor.pre_recv(&$req).await {
            ::remoc::rtc::monitor::RecvDecision::Pass => (),
            ::remoc::rtc::monitor::RecvDecision::Drop => match &$req {
                Ok(None) => (),
                Err(err) if err.is_disconnected() => (),
                _ => continue,
            },
        }
    };
}
#[doc(hidden)]
pub use crate::req_receiver_monitor_pre_recv;

/// A [client](ClientMonitor), [server](ServerMonitor) and
/// [request receiver](ReqReceiverMonitor) monitor that passes every request.
///
/// Set it to switch off the [monitors that are installed by default](self#default-monitors).
/// A request that cannot be decoded then stops serving instead of being skipped.
#[derive(Debug, Default)]
pub struct PassMonitor;

impl<Value, Ref, RefMut> ClientMonitor<Value, Ref, RefMut> for PassMonitor
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_call<'a>(&self, req: &'a Req<Value, Ref, RefMut>) -> BoxFuture<'a, CallDecision> {
        let _ = req;
        std::future::ready(CallDecision::Pass).boxed()
    }
}

impl<Value, Ref, RefMut> ServerMonitor<Value, Ref, RefMut> for PassMonitor
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_dispatch<'a>(
        &mut self, req: &'a Result<Option<Req<Value, Ref, RefMut>>, mpsc::RecvError>,
    ) -> BoxFuture<'a, DispatchDecision> {
        let _ = req;
        std::future::ready(DispatchDecision::Pass).boxed()
    }
}

impl<Value, Ref, RefMut> ReqReceiverMonitor<Value, Ref, RefMut> for PassMonitor
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_recv<'a>(
        &mut self, req: &'a Result<Option<Req<Value, Ref, RefMut>>, mpsc::RecvError>,
    ) -> BoxFuture<'a, RecvDecision> {
        let _ = req;
        std::future::ready(RecvDecision::Pass).boxed()
    }
}

#[doc(hidden)]
pub fn default_client_monitor<Value, Ref, RefMut>() -> Arc<dyn ClientMonitor<Value, Ref, RefMut>>
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    Arc::new(IncompatibleServerMonitor::new())
}

#[doc(hidden)]
pub fn default_server_monitor<Value, Ref, RefMut>() -> Box<dyn ServerMonitor<Value, Ref, RefMut>>
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    Box::new(IncompatibleClientMonitor::new())
}

#[doc(hidden)]
pub fn default_req_receiver_monitor<Value, Ref, RefMut>() -> Box<dyn ReqReceiverMonitor<Value, Ref, RefMut>>
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    Box::new(IncompatibleClientMonitor::new())
}

impl<M, Value, Ref, RefMut> ClientMonitor<Value, Ref, RefMut> for Arc<M>
where
    M: ClientMonitor<Value, Ref, RefMut> + ?Sized,
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_call<'a>(&'a self, req: &'a Req<Value, Ref, RefMut>) -> BoxFuture<'a, CallDecision> {
        (**self).pre_call(req)
    }
}

impl<M, Value, Ref, RefMut> ServerMonitor<Value, Ref, RefMut> for Box<M>
where
    M: ServerMonitor<Value, Ref, RefMut> + ?Sized,
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_dispatch<'a>(
        &'a mut self, req: &'a Result<Option<Req<Value, Ref, RefMut>>, mpsc::RecvError>,
    ) -> BoxFuture<'a, DispatchDecision> {
        (**self).pre_dispatch(req)
    }
}

impl<M, Value, Ref, RefMut> ReqReceiverMonitor<Value, Ref, RefMut> for Box<M>
where
    M: ReqReceiverMonitor<Value, Ref, RefMut> + ?Sized,
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_recv<'a>(
        &'a mut self, req: &'a Result<Option<Req<Value, Ref, RefMut>>, mpsc::RecvError>,
    ) -> BoxFuture<'a, RecvDecision> {
        (**self).pre_recv(req)
    }
}

/// Adapts a [request receiver monitor](ReqReceiverMonitor) into a
/// [server monitor](ServerMonitor).
///
/// Both are invoked at the same point of the request pipeline and
/// [`RecvDecision`] is a subset of [`DispatchDecision`], thus every decision
/// can be translated.
struct ReqReceiverMonitorAsServerMonitor<Value, Ref, RefMut>(Box<dyn ReqReceiverMonitor<Value, Ref, RefMut>>);

impl<Value, Ref, RefMut> ServerMonitor<Value, Ref, RefMut>
    for ReqReceiverMonitorAsServerMonitor<Value, Ref, RefMut>
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_dispatch<'a>(
        &'a mut self, req: &'a Result<Option<Req<Value, Ref, RefMut>>, mpsc::RecvError>,
    ) -> BoxFuture<'a, DispatchDecision> {
        let pre_recv = self.0.pre_recv(req);

        async move {
            match pre_recv.await {
                RecvDecision::Pass => DispatchDecision::Pass,
                RecvDecision::Drop => DispatchDecision::Drop,
            }
        }
        .boxed()
    }
}

/// Converts the monitor of a [request receiver](super::ReqReceiver) into the monitor of a
/// server, when the request receiver is converted into a server.
#[doc(hidden)]
pub fn req_receiver_monitor_as_server_monitor<Value, Ref, RefMut>(
    monitor: Box<dyn ReqReceiverMonitor<Value, Ref, RefMut>>,
) -> Box<dyn ServerMonitor<Value, Ref, RefMut>>
where
    Value: ReqEnum + 'static,
    Ref: ReqEnum + 'static,
    RefMut: ReqEnum + 'static,
{
    Box::new(ReqReceiverMonitorAsServerMonitor(monitor))
}

/// A [call](CallGuard) and [dispatch](DispatchGuard) guard that does nothing.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct PassGuard;

impl CallGuard for PassGuard {}
impl DispatchGuard for PassGuard {}
