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

use remoc::rtc::{
    DispatchDecision, DispatchGuard, MonitorableServer, Req, ServeError, Server, ServerMonitor, ServerShared,
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
