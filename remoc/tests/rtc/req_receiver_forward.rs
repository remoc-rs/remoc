//! Forwarding the requests of a request receiver to a client.

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use futures::{FutureExt, future::BoxFuture};
use tokio::sync::RwLock;

use remoc::{
    prelude::*,
    rtc::monitor::{CallDecision, ClientMonitor, RecvDecision, ReqReceiverMonitor},
    rtc::{CallError, Req},
};

use crate::loop_channel;

#[rtc::remote]
pub trait Counter {
    async fn value(&self) -> Result<u32, CallError>;
    async fn increase(&mut self, by: u32) -> Result<(), CallError>;
}

pub struct CounterObj {
    value: u32,
}

impl Counter for CounterObj {
    async fn value(&self) -> Result<u32, CallError> {
        Ok(self.value)
    }

    async fn increase(&mut self, by: u32) -> Result<(), CallError> {
        self.value += by;
        Ok(())
    }
}

/// Creates a request receiver and client using the default codec.
fn counter_pair() -> (CounterReqReceiver, CounterClient) {
    CounterReqReceiver::new()
}

/// Serves a counter object and returns it together with its client.
fn serve_counter() -> (Arc<RwLock<CounterObj>>, CounterClient) {
    let obj = Arc::new(RwLock::new(CounterObj { value: 0 }));
    let (server, client) = CounterServerSharedMut::new(obj.clone());
    wokio::spawn(server.serve());
    (obj, client)
}

/// Exercises a counter client, leaving the counter at 65.
async fn use_counter(client: &mut CounterClient) {
    assert_eq!(client.value().await.unwrap(), 0);
    client.increase(20).await.unwrap();
    assert_eq!(client.value().await.unwrap(), 20);
    client.increase(45).await.unwrap();
    assert_eq!(client.value().await.unwrap(), 65);
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn forward_to_local_client() {
    crate::init();

    let (obj, target_client) = serve_counter();

    let (req_rx, mut client) = counter_pair();
    let forwarding = wokio::spawn(req_rx.forward(target_client));

    use_counter(&mut client).await;
    drop(client);

    forwarding.await.unwrap().unwrap();
    assert_eq!(obj.read().await.value, 65);
}

/// The client the requests are forwarded to is on a remote endpoint, so that the
/// replies travel from there directly back to the original caller.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn forward_to_remote_client() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<CounterClient>().await;

    println!("Serving counter object and sending its client to the other endpoint");
    let (obj, target_client) = serve_counter();
    a_tx.send(target_client).await.unwrap();

    println!("Receiving the remote counter client");
    let remote_client = b_rx.recv().await.unwrap().unwrap();

    println!("Forwarding a request receiver to it");
    let (req_rx, mut client) = counter_pair();
    let forwarding = wokio::spawn(req_rx.forward(remote_client));

    use_counter(&mut client).await;
    drop(client);

    forwarding.await.unwrap().unwrap();
    assert_eq!(obj.read().await.value, 65);
}

/// Forwarding ends once the client that the requests are forwarded to is disconnected.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn forward_ends_when_target_is_gone() {
    crate::init();

    // A client whose request receiver has been dropped, i.e. nothing serves it.
    let (target_rx, target_client) = counter_pair();
    drop(target_rx);

    let (req_rx, client) = counter_pair();
    let forwarding = wokio::spawn(req_rx.forward(target_client));

    forwarding.await.unwrap().unwrap();
    assert!(client.value().await.is_err());
}

/// Request receiver monitor that counts the requests it passes through.
struct CountingReqMonitor {
    count: Arc<AtomicUsize>,
}

impl<V, R, M> ReqReceiverMonitor<V, R, M> for CountingReqMonitor
where
    V: rtc::ReqEnum,
    R: rtc::ReqEnum,
    M: rtc::ReqEnum,
{
    fn pre_recv<'a>(
        &'a mut self, req: &'a Result<Option<Req<V, R, M>>, rch::mpsc::RecvError>,
    ) -> BoxFuture<'a, RecvDecision> {
        let count = self.count.clone();
        let is_req = matches!(req, Ok(Some(_)));
        async move {
            if is_req {
                count.fetch_add(1, Ordering::SeqCst);
            }
            RecvDecision::Pass
        }
        .boxed()
    }
}

/// The monitor of the request receiver is applied to forwarded requests.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn forward_uses_req_receiver_monitor() {
    crate::init();

    let count = Arc::new(AtomicUsize::new(0));
    let (obj, target_client) = serve_counter();

    let (mut req_rx, mut client) = counter_pair();
    req_rx.set_monitor(CountingReqMonitor { count: count.clone() });
    let forwarding = wokio::spawn(req_rx.forward(target_client));

    use_counter(&mut client).await;
    drop(client);

    forwarding.await.unwrap().unwrap();
    assert_eq!(obj.read().await.value, 65);

    // The monitor observed every one of the five requests.
    assert_eq!(count.load(Ordering::SeqCst), 5);
}

/// Client monitor that drops every request.
struct DropClientMonitor;

impl<V, R, M> ClientMonitor<V, R, M> for DropClientMonitor
where
    V: rtc::ReqEnum,
    R: rtc::ReqEnum,
    M: rtc::ReqEnum,
{
    fn pre_call<'a>(&'a self, req: &'a Req<V, R, M>) -> BoxFuture<'a, CallDecision> {
        let _ = req;
        async move { CallDecision::Drop }.boxed()
    }
}

/// The monitor of the client the requests are forwarded to can drop requests.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn forward_uses_client_monitor() {
    crate::init();

    let (obj, mut target_client) = serve_counter();
    target_client.set_monitor(DropClientMonitor);

    let (req_rx, mut client) = counter_pair();
    let forwarding = wokio::spawn(req_rx.forward(target_client));

    assert!(matches!(client.value().await, Err(CallError::Dropped)));
    assert!(matches!(client.increase(20).await, Err(CallError::Dropped)));
    drop(client);

    forwarding.await.unwrap().unwrap();
    assert_eq!(obj.read().await.value, 0);
}
