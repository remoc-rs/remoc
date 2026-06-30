#[cfg(feature = "js")]
use wasm_bindgen_test::wasm_bindgen_test;

use std::{
    error::Error,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use futures::{FutureExt, future::BoxFuture};
use serde::{Deserialize, Deserializer, Serialize};

use remoc::rtc::{
    CallDecision, ChainedMonitor, ClientMonitor, DispatchDecision, DispatchGuard, MonitorableClient,
    MonitorableServer, Req, ServeError, Server, ServerMonitor, ServerShared,
};

use crate::loop_channel;

#[remoc::rtc::remote]
pub trait Counter {
    async fn value(self) -> Result<u32, remoc::rtc::CallError>;
    async fn value_ref(&self) -> Result<u32, remoc::rtc::CallError>;
    async fn increase(&mut self, by: u32) -> Result<(), remoc::rtc::CallError>;
}

pub struct CounterObj {
    value: u32,
}

impl Counter for CounterObj {
    async fn value(self) -> Result<u32, remoc::rtc::CallError> {
        Ok(self.value)
    }

    async fn value_ref(&self) -> Result<u32, remoc::rtc::CallError> {
        Ok(self.value)
    }

    async fn increase(&mut self, by: u32) -> Result<(), remoc::rtc::CallError> {
        self.value += by;
        Ok(())
    }
}

/// Monitor that counts handled requests.
///
/// It keeps the request borrowed across an await point inside the returned
/// future, which exercises the borrowing-future lifetime `'a` of
/// [`ServerMonitor::pre_dispatch`]. Because the returned future is `Send` and
/// holds a shared reference `&req`, the request must be `Sync` (a `&T` is
/// `Send` only when `T: Sync`). The generated request types are `Send + Sync`,
/// so this is always satisfied.
struct CountingMonitor {
    count: Arc<AtomicUsize>,
}

impl<V, R, M> ServerMonitor<V, R, M> for CountingMonitor
where
    Req<V, R, M>: Sync,
    V: remoc::rtc::ReqEnum,
    R: remoc::rtc::ReqEnum,
    M: remoc::rtc::ReqEnum,
{
    fn pre_dispatch<'a>(
        &mut self, req: &'a Result<Option<Req<V, R, M>>, remoc::rch::mpsc::RecvError>,
    ) -> BoxFuture<'a, DispatchDecision> {
        let count = self.count.clone();
        async move {
            // Do some async work while keeping the request borrowed.
            futures::future::ready(()).await;
            if matches!(req, Ok(Some(_))) {
                count.fetch_add(1, Ordering::SeqCst);
            }
            DispatchDecision::Pass
        }
        .boxed()
    }
}

/// Error returned by [`RateLimitMonitor`] once the request budget is exhausted.
#[derive(Debug)]
struct RateLimited;

impl fmt::Display for RateLimited {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "too many requests")
    }
}

impl Error for RateLimited {}

/// Monitor that allows a fixed number of requests and then fails the server.
struct RateLimitMonitor {
    remaining: usize,
}

impl<V, R, M> ServerMonitor<V, R, M> for RateLimitMonitor
where
    V: remoc::rtc::ReqEnum,
    R: remoc::rtc::ReqEnum,
    M: remoc::rtc::ReqEnum,
{
    fn pre_dispatch<'a>(
        &mut self, req: &'a Result<Option<Req<V, R, M>>, remoc::rch::mpsc::RecvError>,
    ) -> BoxFuture<'a, DispatchDecision> {
        let allow = if matches!(req, Ok(Some(_))) {
            if self.remaining == 0 {
                false
            } else {
                self.remaining -= 1;
                true
            }
        } else {
            true
        };

        async move { if allow { DispatchDecision::Pass } else { DispatchDecision::Error(Box::new(RateLimited)) } }
            .boxed()
    }
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn count() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<CounterClient>().await;

    let count = Arc::new(AtomicUsize::new(0));

    println!("Creating counter server with counting monitor");
    let (mut server, client) = CounterServer::new(CounterObj { value: 0 }, 1);
    server.set_monitor(CountingMonitor { count: count.clone() });

    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let mut client = b_rx.recv().await.unwrap().unwrap();

        client.increase(20).await.unwrap();
        assert_eq!(client.value_ref().await.unwrap(), 20);
        client.increase(45).await.unwrap();
        assert_eq!(client.value().await.unwrap(), 65);
    };

    let (_, (obj, res)) = tokio::join!(client_task, server.serve());
    res.unwrap();
    assert!(obj.is_none());

    // increase + value_ref + increase + value == 4 requests.
    assert_eq!(count.load(Ordering::SeqCst), 4);
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn rate_limit() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<CounterClient>().await;

    println!("Creating counter server with rate-limiting monitor");
    let (mut server, client) = CounterServer::new(CounterObj { value: 0 }, 1);
    server.set_monitor(RateLimitMonitor { remaining: 1 });

    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let mut client = b_rx.recv().await.unwrap().unwrap();

        // First request is allowed.
        client.increase(20).await.unwrap();

        // Second request is rejected, which fails the server, so the call errors out.
        assert!(client.increase(45).await.is_err());
    };

    let (_, (obj, res)) = tokio::join!(client_task, server.serve());

    // The server failed with the monitor error but still returns the target.
    assert!(matches!(res, Err(ServeError::Monitor(_))));
    assert!(obj.is_some());
}

/// Worker trait whose only method takes `&self`, so a shared server that
/// handles requests concurrently (with spawning) is generated.
#[remoc::rtc::remote]
pub trait Worker {
    async fn work(&self) -> Result<(), remoc::rtc::CallError>;
}

pub struct WorkerObj;

impl Worker for WorkerObj {
    async fn work(&self) -> Result<(), remoc::rtc::CallError> {
        // Stay busy for a while so that concurrent requests overlap.
        remoc::exec::time::sleep(std::time::Duration::from_millis(100)).await;
        Ok(())
    }
}

/// Guard that tracks the number of in-flight requests.
///
/// It increments the in-flight counter (and the observed maximum) when created
/// in [`ServerMonitor::pre_dispatch`] and decrements it again when dropped,
/// i.e. once the request it guards is finished.
struct InFlightGuard {
    in_flight: Arc<AtomicUsize>,
}

impl InFlightGuard {
    fn new(in_flight: Arc<AtomicUsize>, max: Arc<AtomicUsize>) -> Self {
        let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        max.fetch_max(now, Ordering::SeqCst);
        Self { in_flight }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl DispatchGuard for InFlightGuard {}

/// Monitor that attaches an [`InFlightGuard`] to every handled request.
struct InFlightMonitor {
    in_flight: Arc<AtomicUsize>,
    max: Arc<AtomicUsize>,
}

impl<V, R, M> ServerMonitor<V, R, M> for InFlightMonitor
where
    V: remoc::rtc::ReqEnum,
    R: remoc::rtc::ReqEnum,
    M: remoc::rtc::ReqEnum,
{
    fn pre_dispatch<'a>(
        &mut self, req: &'a Result<Option<Req<V, R, M>>, remoc::rch::mpsc::RecvError>,
    ) -> BoxFuture<'a, DispatchDecision> {
        let decision = if matches!(req, Ok(Some(_))) {
            DispatchDecision::Guard(Box::new(InFlightGuard::new(self.in_flight.clone(), self.max.clone())))
        } else {
            DispatchDecision::Pass
        };
        async move { decision }.boxed()
    }
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn guard_in_flight() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<WorkerClient>().await;

    const N: usize = 5;

    let in_flight = Arc::new(AtomicUsize::new(0));
    let max = Arc::new(AtomicUsize::new(0));

    println!("Creating shared worker server with in-flight guard monitor");
    let (mut server, client) = WorkerServerShared::new(Arc::new(WorkerObj), 16);
    server.set_monitor(InFlightMonitor { in_flight: in_flight.clone(), max: max.clone() });

    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let client = b_rx.recv().await.unwrap().unwrap();

        // Issue all requests concurrently so they are in flight at the same time.
        let calls: Vec<_> = (0..N).map(|_| client.work()).collect();
        for res in futures::future::join_all(calls).await {
            res.unwrap();
        }
    };

    // Serve with spawning so that requests are handled concurrently.
    let (_, res) = tokio::join!(client_task, server.serve(true));
    res.unwrap();

    // Every guard has been dropped once its request finished.
    assert_eq!(in_flight.load(Ordering::SeqCst), 0);

    // All requests were in flight simultaneously, proving the guard is moved
    // into the spawned task and kept alive for the request's duration.
    assert_eq!(max.load(Ordering::SeqCst), N);
}

/// Argument type that always fails to deserialize.
///
/// It serializes fine on the client, but its [`Deserialize`] implementation
/// always fails, so any request carrying it cannot be decoded by the server.
/// This simulates a client sending malformed or incompatible data.
#[derive(Clone, Serialize)]
pub struct FailToDecode(u32);

impl<'de> Deserialize<'de> for FailToDecode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Consume the value from the wire so the framing stays intact, then fail.
        let _ = u32::deserialize(deserializer)?;
        Err(serde::de::Error::custom("intentional decode failure"))
    }
}

/// Trait with a method whose argument fails to decode and a well-formed method.
#[remoc::rtc::remote]
pub trait Decoder {
    async fn process(&self, value: FailToDecode) -> Result<(), remoc::rtc::CallError>;
    async fn ping(&self) -> Result<u32, remoc::rtc::CallError>;
}

pub struct DecoderObj;

impl Decoder for DecoderObj {
    async fn process(&self, _value: FailToDecode) -> Result<(), remoc::rtc::CallError> {
        Ok(())
    }

    async fn ping(&self) -> Result<u32, remoc::rtc::CallError> {
        Ok(42)
    }
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn incompatible_client_trips() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<DecoderClient>().await;

    println!("Creating decoder server with incompatible-client monitor (limit 3)");
    let (mut server, client) = DecoderServer::new(DecoderObj, 1);
    server.set_monitor(remoc::rtc::monitor::IncompatibleClientMonitor::new().limit(Some(3)).log_level(None));

    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let client = b_rx.recv().await.unwrap().unwrap();

        // Every request fails to decode on the server. The first three failures
        // are tolerated and the requests dropped; the fourth exceeds the limit
        // and stops the server. Each call therefore returns an error.
        for _ in 0..10 {
            assert!(client.process(FailToDecode(0)).await.is_err());
        }
    };

    let (_, (obj, res)) = tokio::join!(client_task, server.serve());

    // The server stopped with the monitor error but still returns the target.
    assert!(matches!(res, Err(ServeError::Monitor(_))));
    assert!(obj.is_some());
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn incompatible_client_tolerates() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<DecoderClient>().await;

    println!("Creating decoder server with incompatible-client monitor (limiting disabled)");
    let (mut server, client) = DecoderServer::new(DecoderObj, 1);
    server.set_monitor(remoc::rtc::monitor::IncompatibleClientMonitor::new().limit(None).log_level(None));

    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let client = b_rx.recv().await.unwrap().unwrap();

        // These requests fail to decode, but with limiting disabled they are
        // simply dropped and serving continues.
        for _ in 0..5 {
            assert!(client.process(FailToDecode(0)).await.is_err());
        }

        // A well-formed request is still served normally.
        assert_eq!(client.ping().await.unwrap(), 42);
    };

    let (_, (_obj, res)) = tokio::join!(client_task, server.serve());
    res.unwrap();
}

/// Server monitor that counts non-final decode failures and drops the offending
/// requests, so the server keeps serving and the client observes a reply
/// failure for each of them.
struct DecodeFailCounter {
    count: Arc<AtomicUsize>,
}

impl<V, R, M> ServerMonitor<V, R, M> for DecodeFailCounter
where
    V: remoc::rtc::ReqEnum,
    R: remoc::rtc::ReqEnum,
    M: remoc::rtc::ReqEnum,
{
    fn pre_dispatch<'a>(
        &mut self, req: &'a Result<Option<Req<V, R, M>>, remoc::rch::mpsc::RecvError>,
    ) -> BoxFuture<'a, DispatchDecision> {
        let decision = match req {
            Err(err) if !err.is_final() => {
                self.count.fetch_add(1, Ordering::SeqCst);
                DispatchDecision::Drop
            }
            _ => DispatchDecision::Pass,
        };
        async move { decision }.boxed()
    }
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn incompatible_server_throttles() {
    use remoc::exec::time::{Instant, sleep};
    use std::time::Duration;

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<DecoderClient>().await;

    let server_failures = Arc::new(AtomicUsize::new(0));

    println!("Creating decoder server that tolerates and counts decode failures");
    let (mut server, client) = DecoderServer::new(DecoderObj, 1);
    server.set_monitor(DecodeFailCounter { count: server_failures.clone() });

    a_tx.send(client).await.unwrap();

    let window = Duration::from_millis(200);

    let client_task = async move {
        // The client monitor is set on the received client, since it does not
        // travel with the client across the connection.
        let mut client = b_rx.recv().await.unwrap().unwrap();
        client.set_monitor(
            remoc::rtc::monitor::IncompatibleServerMonitor::new().limit(Some(1)).window(window).log_level(None),
        );

        // Every call fails to be received on the server. Once more than one
        // failure of `process` has occurred within the window, the client
        // monitor throttles further calls to it by delaying each by one window.
        let start = Instant::now();
        for _ in 0..5 {
            assert!(client.process(FailToDecode(0)).await.is_err(), "decoding should have failed on the server");
        }
        start.elapsed()
    };

    let (elapsed, (_obj, res)) = tokio::join!(client_task, server.serve());
    res.unwrap();

    // All five calls reached the server: throttling delays calls but never
    // drops them.
    assert_eq!(server_failures.load(Ordering::SeqCst), 5);

    // The first two calls passed immediately; the remaining three were each
    // delayed by one window, so the loop took noticeably longer than a single
    // window.
    assert!(
        elapsed >= window * 2,
        "calls to the failing method should have been throttled, but only took {elapsed:?}"
    );

    // A short follow-up sleep lets the connection drain cleanly before shutdown.
    sleep(Duration::from_millis(10)).await;
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn incompatible_server_tolerates() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<DecoderClient>().await;

    let server_failures = Arc::new(AtomicUsize::new(0));

    println!("Creating decoder server with incompatible-server monitor (limiting disabled)");
    let (mut server, client) = DecoderServer::new(DecoderObj, 1);
    server.set_monitor(DecodeFailCounter { count: server_failures.clone() });

    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let mut client = b_rx.recv().await.unwrap().unwrap();
        client.set_monitor(remoc::rtc::monitor::IncompatibleServerMonitor::new().limit(None).log_level(None));

        // With limiting disabled, the client never throttles calls, so all of
        // them reach the server even though they all fail to decode there.
        for _ in 0..10 {
            assert!(client.process(FailToDecode(0)).await.is_err());
        }

        // A well-formed call is still served normally.
        assert_eq!(client.ping().await.unwrap(), 42);
    };

    let (_, (_obj, res)) = tokio::join!(client_task, server.serve());
    res.unwrap();

    // All ten failing calls reached the server, none were throttled.
    assert_eq!(server_failures.load(Ordering::SeqCst), 10);
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn rate_limit_monitor_throttles() {
    use remoc::exec::time::Instant;
    use std::{num::NonZeroUsize, time::Duration};

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<CounterClient>().await;

    let window = Duration::from_millis(200);

    println!("Creating counter server with the rate-limit monitor (2 requests per window)");
    let (mut server, client) = CounterServer::new(CounterObj { value: 0 }, 1);
    server.set_monitor(
        remoc::rtc::monitor::RateLimitMonitor::new(NonZeroUsize::new(2).unwrap(), window).log_level(None),
    );

    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let client = b_rx.recv().await.unwrap().unwrap();

        // Six requests with a budget of two per window. The first two pass
        // immediately; the rate limiter then delays the rest, releasing two per
        // window. This requires waiting out two full windows in total.
        let start = Instant::now();
        for _ in 0..6 {
            assert_eq!(client.value_ref().await.unwrap(), 0);
        }
        start.elapsed()
    };

    let (elapsed, (obj, res)) = tokio::join!(client_task, server.serve());
    res.unwrap();

    // Only `&self` methods were called, so the target was never consumed.
    assert!(obj.is_some());

    // Rate limiting delays calls but never drops or fails them, so all six
    // succeeded while taking at least two window durations.
    assert!(elapsed >= window * 2, "calls should have been rate limited, but only took {elapsed:?}");
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn concurrent_limit_monitor_limits() {
    use remoc::exec::time::Instant;
    use std::{num::NonZeroUsize, time::Duration};

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<WorkerClient>().await;

    const N: usize = 5;

    println!("Creating shared worker server with the concurrent-limit monitor (limit 2)");
    let (mut server, client) = WorkerServerShared::new(Arc::new(WorkerObj), 16);
    server.set_monitor(
        remoc::rtc::monitor::ConcurrentLimitMonitor::new(NonZeroUsize::new(2).unwrap()).log_level(None),
    );

    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let client = b_rx.recv().await.unwrap().unwrap();

        // Each call keeps the worker busy for 100 ms. With at most two requests
        // processed concurrently, five overlapping calls form three sequential
        // batches (2 + 2 + 1), so the run takes roughly three work durations.
        let start = Instant::now();
        let calls: Vec<_> = (0..N).map(|_| client.work()).collect();
        for res in futures::future::join_all(calls).await {
            res.unwrap();
        }
        start.elapsed()
    };

    // Serve with spawning so that requests can be handled concurrently.
    let (elapsed, res) = tokio::join!(client_task, server.serve(true));
    res.unwrap();

    // The concurrency cap forces the five overlapping calls into three batches,
    // taking clearly longer than the single 100 ms work duration that an
    // unbounded server would need.
    assert!(
        elapsed >= Duration::from_millis(250),
        "calls should have been limited to two at a time, but only took {elapsed:?}"
    );
}

/// Server monitor that drops every request.
///
/// The accounting happens inside the returned future, so a request short-circuited
/// by a preceding monitor in a chain is never observed.
struct DropMonitor;

impl<V, R, M> ServerMonitor<V, R, M> for DropMonitor
where
    V: remoc::rtc::ReqEnum,
    R: remoc::rtc::ReqEnum,
    M: remoc::rtc::ReqEnum,
{
    fn pre_dispatch<'a>(
        &mut self, req: &'a Result<Option<Req<V, R, M>>, remoc::rch::mpsc::RecvError>,
    ) -> BoxFuture<'a, DispatchDecision> {
        let is_req = matches!(req, Ok(Some(_)));
        async move { if is_req { DispatchDecision::Drop } else { DispatchDecision::Pass } }.boxed()
    }
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn chain_server_monitors_both_apply() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<CounterClient>().await;

    let count_a = Arc::new(AtomicUsize::new(0));
    let count_b = Arc::new(AtomicUsize::new(0));

    println!("Creating counter server with two chained counting monitors");
    let (mut server, client) = CounterServer::new(CounterObj { value: 0 }, 1);
    server.set_monitor(ChainedMonitor(
        CountingMonitor { count: count_a.clone() },
        CountingMonitor { count: count_b.clone() },
    ));

    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let mut client = b_rx.recv().await.unwrap().unwrap();

        client.increase(20).await.unwrap();
        assert_eq!(client.value_ref().await.unwrap(), 20);
        client.increase(45).await.unwrap();
        assert_eq!(client.value().await.unwrap(), 65);
    };

    let (_, (obj, res)) = tokio::join!(client_task, server.serve());
    res.unwrap();
    assert!(obj.is_none());

    // Both monitors in the chain observed every one of the four requests.
    assert_eq!(count_a.load(Ordering::SeqCst), 4);
    assert_eq!(count_b.load(Ordering::SeqCst), 4);
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn chain_server_monitors_short_circuit() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<CounterClient>().await;

    let count = Arc::new(AtomicUsize::new(0));

    println!("Creating counter server with a dropping monitor chained before a counting monitor");
    let (mut server, client) = CounterServer::new(CounterObj { value: 0 }, 1);
    server.set_monitor(ChainedMonitor(DropMonitor, CountingMonitor { count: count.clone() }));

    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let mut client = b_rx.recv().await.unwrap().unwrap();

        // The first monitor drops every request, so each call fails and the
        // second monitor never sees them.
        assert!(client.increase(20).await.is_err());
        assert!(client.value_ref().await.is_err());
    };

    let (_, (obj, res)) = tokio::join!(client_task, server.serve());

    // Dropping requests does not fail the server, and the target was never consumed.
    res.unwrap();
    assert!(obj.is_some());

    // The second monitor was short-circuited by the first, so it counted nothing.
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn chain_server_monitors_guards() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<WorkerClient>().await;

    const N: usize = 5;

    let in_flight_a = Arc::new(AtomicUsize::new(0));
    let max_a = Arc::new(AtomicUsize::new(0));
    let in_flight_b = Arc::new(AtomicUsize::new(0));
    let max_b = Arc::new(AtomicUsize::new(0));

    println!("Creating shared worker server with two chained in-flight guard monitors");
    let (mut server, client) = WorkerServerShared::new(Arc::new(WorkerObj), 16);
    server.set_monitor(ChainedMonitor(
        InFlightMonitor { in_flight: in_flight_a.clone(), max: max_a.clone() },
        InFlightMonitor { in_flight: in_flight_b.clone(), max: max_b.clone() },
    ));

    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let client = b_rx.recv().await.unwrap().unwrap();

        let calls: Vec<_> = (0..N).map(|_| client.work()).collect();
        for res in futures::future::join_all(calls).await {
            res.unwrap();
        }
    };

    let (_, res) = tokio::join!(client_task, server.serve(true));
    res.unwrap();

    // Both guards were attached to every request and dropped once it finished,
    // so each monitor saw all requests in flight simultaneously and ended at zero.
    assert_eq!(in_flight_a.load(Ordering::SeqCst), 0);
    assert_eq!(max_a.load(Ordering::SeqCst), N);
    assert_eq!(in_flight_b.load(Ordering::SeqCst), 0);
    assert_eq!(max_b.load(Ordering::SeqCst), N);
}

/// Client monitor that counts the requests it sees.
///
/// The accounting happens inside the returned future, so a request short-circuited
/// by a preceding monitor in a chain is never observed.
struct CountingClientMonitor {
    count: Arc<AtomicUsize>,
}

impl<V, R, M> ClientMonitor<V, R, M> for CountingClientMonitor
where
    V: remoc::rtc::ReqEnum,
    R: remoc::rtc::ReqEnum,
    M: remoc::rtc::ReqEnum,
{
    fn pre_call<'a>(&'a self, _req: &'a Req<V, R, M>) -> BoxFuture<'a, CallDecision> {
        let count = self.count.clone();
        async move {
            count.fetch_add(1, Ordering::SeqCst);
            CallDecision::Pass
        }
        .boxed()
    }
}

/// Client monitor that drops every request before it is sent to the server.
struct DropClientMonitor;

impl<V, R, M> ClientMonitor<V, R, M> for DropClientMonitor
where
    V: remoc::rtc::ReqEnum,
    R: remoc::rtc::ReqEnum,
    M: remoc::rtc::ReqEnum,
{
    fn pre_call<'a>(&'a self, _req: &'a Req<V, R, M>) -> BoxFuture<'a, CallDecision> {
        async move { CallDecision::Drop }.boxed()
    }
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn chain_client_monitors_both_apply() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<CounterClient>().await;

    let count_a = Arc::new(AtomicUsize::new(0));
    let count_b = Arc::new(AtomicUsize::new(0));

    println!("Creating counter server; the received client gets two chained counting monitors");
    let (server, client) = CounterServer::new(CounterObj { value: 0 }, 1);

    a_tx.send(client).await.unwrap();

    let count_a_mon = count_a.clone();
    let count_b_mon = count_b.clone();
    let client_task = async move {
        let mut client = b_rx.recv().await.unwrap().unwrap();
        client.set_monitor(ChainedMonitor(
            CountingClientMonitor { count: count_a_mon },
            CountingClientMonitor { count: count_b_mon },
        ));

        client.increase(20).await.unwrap();
        assert_eq!(client.value_ref().await.unwrap(), 20);
        client.increase(45).await.unwrap();
        assert_eq!(client.value().await.unwrap(), 65);
    };

    let (_, (_obj, res)) = tokio::join!(client_task, server.serve());
    res.unwrap();

    // Both monitors in the chain observed every one of the four requests.
    assert_eq!(count_a.load(Ordering::SeqCst), 4);
    assert_eq!(count_b.load(Ordering::SeqCst), 4);
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn chain_client_monitors_short_circuit() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<CounterClient>().await;

    let count = Arc::new(AtomicUsize::new(0));

    println!("Creating counter server; the received client drops requests before a counting monitor");
    let (server, client) = CounterServer::new(CounterObj { value: 0 }, 1);

    a_tx.send(client).await.unwrap();

    let count_mon = count.clone();
    let client_task = async move {
        let mut client = b_rx.recv().await.unwrap().unwrap();
        client.set_monitor(ChainedMonitor(DropClientMonitor, CountingClientMonitor { count: count_mon }));

        // The first monitor drops every request before it is sent, so each call
        // fails and the second monitor never sees them.
        assert!(client.increase(20).await.is_err());
        assert!(client.value_ref().await.is_err());
    };

    let (_, (obj, res)) = tokio::join!(client_task, server.serve());
    res.unwrap();

    // No request ever reached the server, so the target was never consumed.
    assert!(obj.is_some());

    // The second monitor was short-circuited by the first, so it counted nothing.
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
