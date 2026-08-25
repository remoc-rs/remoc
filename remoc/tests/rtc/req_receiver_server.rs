//! Constructing servers from a request receiver.

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
    rtc::{CallError, RecvDecision, Req, ReqReceiverMonitor},
};

use crate::loop_channel;

#[rtc::remote(clone)]
pub trait Counter {
    async fn value(&self) -> Result<u32, CallError>;
    async fn increase(&mut self, by: u32) -> Result<(), CallError>;
}

pub struct CounterObj {
    value: u32,
}

impl CounterObj {
    fn new() -> Self {
        Self { value: 0 }
    }
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

#[rtc::remote]
pub trait Reader {
    async fn get(&self) -> Result<u32, CallError>;
}

pub struct ReaderObj {
    value: u32,
}

impl Reader for ReaderObj {
    async fn get(&self) -> Result<u32, CallError> {
        Ok(self.value)
    }
}

/// Creates a counter request receiver and client using the default codec.
fn counter_pair() -> (CounterReqReceiver, CounterClient) {
    CounterReqReceiver::new()
}

/// Creates a reader request receiver and client using the default codec.
fn reader_pair() -> (ReaderReqReceiver, ReaderClient) {
    ReaderReqReceiver::new()
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
async fn server_value() {
    crate::init();

    let (req_rx, mut client) = counter_pair();
    let server = CounterServer::from_req_receiver(CounterObj::new(), req_rx);
    let server_task = wokio::spawn(server.serve());

    use_counter(&mut client).await;
    drop(client);

    let (target, res) = server_task.await.unwrap();
    res.unwrap();
    assert_eq!(target.unwrap().value, 65);
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn server_ref_mut() {
    crate::init();

    let mut obj = CounterObj::new();
    let (req_rx, mut client) = counter_pair();
    let server = req_rx.into_server_ref_mut(&mut obj);

    let client_task = async move {
        use_counter(&mut client).await;
    };

    let ((), res) = tokio::join!(client_task, server.serve());
    res.unwrap();
    assert_eq!(obj.value, 65);
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn server_shared_mut() {
    crate::init();

    let obj = Arc::new(RwLock::new(CounterObj::new()));
    let (req_rx, mut client) = counter_pair();
    let server = CounterServerSharedMut::from_req_receiver(obj.clone(), req_rx);
    let server_task = wokio::spawn(server.serve(true));

    use_counter(&mut client).await;
    drop(client);

    server_task.await.unwrap().unwrap();
    assert_eq!(obj.read().await.value, 65);
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn server_ref() {
    crate::init();

    let obj = ReaderObj { value: 42 };
    let (req_rx, client) = reader_pair();
    let server = req_rx.into_server_ref(&obj);

    let client_task = async move {
        let client = client;
        assert_eq!(client.get().await.unwrap(), 42);
    };

    let ((), res) = tokio::join!(client_task, server.serve());
    res.unwrap();
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn server_shared() {
    crate::init();

    let obj = Arc::new(ReaderObj { value: 42 });
    let (req_rx, client) = reader_pair();
    let server = req_rx.into_server_shared(obj);
    let server_task = wokio::spawn(server.serve(true));

    assert_eq!(client.get().await.unwrap(), 42);
    drop(client);

    server_task.await.unwrap().unwrap();
}

/// A request receiver that is sent to a remote endpoint is served there.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn server_from_remote_req_receiver() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<CounterReqReceiver>().await;

    println!("Creating counter request receiver");
    let (req_rx, mut client) = counter_pair();

    println!("Sending counter request receiver to remote endpoint");
    a_tx.send(req_rx).await.unwrap();

    let server_task = wokio::spawn(async move {
        println!("Receiving counter request receiver");
        let req_rx = b_rx.recv().await.unwrap().unwrap();

        println!("Serving it");
        let obj = Arc::new(RwLock::new(CounterObj::new()));
        let server = CounterServerSharedMut::from_req_receiver(obj.clone(), req_rx);
        server.serve(true).await.unwrap();

        obj.read().await.value
    });

    use_counter(&mut client).await;
    drop(client);

    assert_eq!(server_task.await.unwrap(), 65);
}

/// Requests that are queued before the conversion are served afterwards.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn queued_requests_are_served() {
    crate::init();

    // A request buffer of one call makes it deterministic when the request is queued.
    let (req_rx, client): (CounterReqReceiver, CounterClient) = ReqReceiver::with_request_buffer(1);
    assert_eq!(client.capacity(), 1);

    let mut caller = client.clone();
    let call_task = wokio::spawn(async move { caller.increase(20).await.unwrap() });

    println!("Waiting for the request to be queued");
    while client.capacity() > 0 {
        wokio::task::yield_now().await;
    }

    println!("Converting request receiver with queued request into server");
    let obj = Arc::new(RwLock::new(CounterObj::new()));
    let server = CounterServerSharedMut::from_req_receiver(obj.clone(), req_rx);
    let server_task = wokio::spawn(server.serve(true));

    call_task.await.unwrap();
    drop(client);

    server_task.await.unwrap().unwrap();
    assert_eq!(obj.read().await.value, 20);
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

/// Request receiver monitor that drops every request.
struct DropReqMonitor;

impl<V, R, M> ReqReceiverMonitor<V, R, M> for DropReqMonitor
where
    V: rtc::ReqEnum,
    R: rtc::ReqEnum,
    M: rtc::ReqEnum,
{
    fn pre_recv<'a>(
        &'a mut self, req: &'a Result<Option<Req<V, R, M>>, rch::mpsc::RecvError>,
    ) -> BoxFuture<'a, RecvDecision> {
        let is_req = matches!(req, Ok(Some(_)));
        async move { if is_req { RecvDecision::Drop } else { RecvDecision::Pass } }.boxed()
    }
}

/// A monitor set on the request receiver keeps working on the server.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn monitor_is_kept() {
    crate::init();

    let count = Arc::new(AtomicUsize::new(0));

    let (mut req_rx, mut client) = counter_pair();
    req_rx.set_monitor(CountingReqMonitor { count: count.clone() });

    let obj = Arc::new(RwLock::new(CounterObj::new()));
    let server = CounterServerSharedMut::from_req_receiver(obj.clone(), req_rx);
    let server_task = wokio::spawn(server.serve(true));

    use_counter(&mut client).await;
    drop(client);

    server_task.await.unwrap().unwrap();
    assert_eq!(obj.read().await.value, 65);

    // The monitor observed every one of the five requests.
    assert_eq!(count.load(Ordering::SeqCst), 5);
}

/// A dropping monitor set on the request receiver keeps dropping on the server.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn dropping_monitor_is_kept() {
    crate::init();

    let (mut req_rx, client) = counter_pair();
    req_rx.set_monitor(DropReqMonitor);

    let obj = Arc::new(RwLock::new(CounterObj::new()));
    let server = CounterServerSharedMut::from_req_receiver(obj.clone(), req_rx);
    let server_task = wokio::spawn(server.serve(true));

    assert!(matches!(client.value().await, Err(CallError::Dropped)));
    drop(client);

    server_task.await.unwrap().unwrap();
    assert_eq!(obj.read().await.value, 0);
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn into_server() {
    crate::init();

    let (req_rx, mut client) = counter_pair();
    let server_task = wokio::spawn(req_rx.into_server(CounterObj::new()).serve());

    use_counter(&mut client).await;
    drop(client);

    let (target, res) = server_task.await.unwrap();
    res.unwrap();
    assert_eq!(target.unwrap().value, 65);
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn into_server_shared_mut() {
    crate::init();

    let obj = Arc::new(RwLock::new(CounterObj::new()));
    let (req_rx, mut client) = counter_pair();
    let server_task = wokio::spawn(req_rx.into_server_shared_mut(obj.clone()).serve(true));

    use_counter(&mut client).await;
    drop(client);

    server_task.await.unwrap().unwrap();
    assert_eq!(obj.read().await.value, 65);
}

/// A generic trait with an associated type, so that the conversions must project the
/// associated type of the request receiver through the target object.
#[rtc::remote]
pub trait Store<K>
where
    K: remoc::RemoteSend + Clone + Sync,
{
    type Item: remoc::RemoteSend + Clone;

    async fn get(&self) -> Result<Self::Item, CallError>;
    async fn put(&mut self, key: K, item: Self::Item) -> Result<(), CallError>;
}

pub struct StoreObj<K, V> {
    key: Option<K>,
    item: V,
}

impl<K, V> Store<K> for StoreObj<K, V>
where
    K: remoc::RemoteSend + Clone + Sync,
    V: remoc::RemoteSend + Clone + Sync,
{
    type Item = V;

    async fn get(&self) -> Result<V, CallError> {
        Ok(self.item.clone())
    }

    async fn put(&mut self, key: K, item: V) -> Result<(), CallError> {
        self.key = Some(key);
        self.item = item;
        Ok(())
    }
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn into_server_with_assoc_type() {
    crate::init();

    let obj = Arc::new(RwLock::new(StoreObj { key: None, item: 1u32 }));
    let (req_rx, mut client): (StoreReqReceiver<String, u32>, _) = ReqReceiver::new();
    let server_task = wokio::spawn(req_rx.into_server_shared_mut(obj.clone()).serve(true));

    assert_eq!(client.get().await.unwrap(), 1);
    client.put("key".to_string(), 42).await.unwrap();
    assert_eq!(client.get().await.unwrap(), 42);
    drop(client);

    server_task.await.unwrap().unwrap();
    assert_eq!(obj.read().await.key.as_deref(), Some("key"));
}
