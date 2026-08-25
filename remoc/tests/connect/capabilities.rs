//! Connections to an endpoint that announces reduced capabilities.
//!
//! Disabling a capability makes one endpoint behave like an older remote
//! endpoint, which exercises the compatibility paths taken against it.

use futures::StreamExt;
use std::time::Duration;
use wokio::time::timeout;

#[cfg(feature = "rtc")]
use std::sync::Arc;
#[cfg(feature = "rtc")]
use tokio::sync::RwLock;

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use remoc::{
    Cfg, Connect,
    chmux::Capabilities,
    rch::{base, mpsc, oneshot},
};

#[cfg(feature = "rtc")]
use remoc::{rch::watch, rtc::ServerSharedMut};

/// Time an exchange is given before the test fails.
const LIMIT: Duration = Duration::from_secs(60);

/// Size of the data within a transferred value.
///
/// This exceeds the default chunk size, so that chunking and flow control are
/// exercised as well.
const DATA_SIZE: usize = 100_000;

/// Number of items sent over a transferred channel.
const ITEMS: usize = 16;

/// Data of the specified size.
fn data(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 251) as u8).collect()
}

/// The item with the specified index.
fn item(index: usize) -> Vec<u8> {
    data(index * 1_000)
}

/// A counter that is served over a downgraded connection.
#[cfg(feature = "rtc")]
#[remoc::rtc::remote]
pub trait Counter {
    /// The current value.
    async fn value(&self) -> Result<u32, remoc::rtc::CallError>;

    /// Increases the value and returns the new value.
    async fn increase(&mut self, by: u32) -> Result<u32, remoc::rtc::CallError>;

    /// A channel that follows the value.
    async fn watch(&mut self) -> Result<watch::Receiver<u32>, remoc::rtc::CallError>;
}

#[cfg(feature = "rtc")]
#[derive(Default)]
pub struct CounterObj {
    value: u32,
    watchers: Vec<watch::Sender<u32>>,
}

#[cfg(feature = "rtc")]
impl Counter for CounterObj {
    async fn value(&self) -> Result<u32, remoc::rtc::CallError> {
        Ok(self.value)
    }

    async fn increase(&mut self, by: u32) -> Result<u32, remoc::rtc::CallError> {
        self.value += by;

        for watcher in &self.watchers {
            let _ = watcher.send(self.value);
        }

        Ok(self.value)
    }

    async fn watch(&mut self) -> Result<watch::Receiver<u32>, remoc::rtc::CallError> {
        let (tx, rx) = watch::channel(self.value);
        self.watchers.push(tx);
        Ok(rx)
    }
}

/// A value transferred over a downgraded connection.
///
/// It carries channels and a remote object, so that the types Remoc transfers
/// for them are subject to the negotiated capabilities as well.
#[derive(serde::Serialize, serde::Deserialize)]
struct Payload {
    text: String,
    data: Vec<u8>,
    /// Carries items back to the sender of the value.
    items: mpsc::Sender<Vec<u8>>,
    /// Reports the size of the received data back to the sender of the value.
    size: oneshot::Sender<usize>,
    /// Calls the counter served by the sender of the value.
    #[cfg(feature = "rtc")]
    counter: CounterClient,
}

/// Connects two endpoints, where the remote one announces the given capabilities.
async fn connect_with(
    remote: Capabilities,
) -> ((base::Sender<Payload>, base::Receiver<Payload>), (base::Sender<Payload>, base::Receiver<Payload>)) {
    crate::loop_transport!(0, a_tx, a_rx, b_tx, b_rx);

    let a_cfg = Cfg::default();
    let b_cfg = Cfg { capabilities: remote, ..Default::default() };

    let a = Connect::framed::<_, _, _, _, Payload, Payload, remoc::codec::Default>(a_cfg, a_tx, a_rx);
    let b = Connect::framed::<_, _, _, _, Payload, Payload, remoc::codec::Default>(b_cfg, b_tx, b_rx);
    let ((a_conn, a_tx, a_rx), (b_conn, b_tx, b_rx)) = futures::future::try_join(a, b).await.unwrap();

    wokio::spawn(async move {
        let _ = a_conn.await;
    });
    wokio::spawn(async move {
        let _ = b_conn.await;
    });

    ((a_tx, a_rx), (b_tx, b_rx))
}

/// Sends a value over the connection and exercises everything it carries.
///
/// Both endpoints are driven concurrently, since flow control blocks the sender
/// until the receiving endpoint consumes the data.
async fn transfer(mut tx: base::Sender<Payload>, mut rx: base::Receiver<Payload>) {
    // The sending endpoint serves the counter.
    #[cfg(feature = "rtc")]
    let counter_obj = Arc::new(RwLock::new(CounterObj::default()));
    #[cfg(feature = "rtc")]
    let (counter_server, counter_client) = CounterServerSharedMut::new(counter_obj.clone(), 4);
    #[cfg(feature = "rtc")]
    let counter_task = wokio::spawn(async move { counter_server.serve(true).await.unwrap() });

    let (items_tx, mut items_rx) = mpsc::with_local_buffer(4);
    let (size_tx, size_rx) = oneshot::channel();

    let sent = Payload {
        text: "value".to_string(),
        data: data(DATA_SIZE),
        items: items_tx,
        size: size_tx,
        #[cfg(feature = "rtc")]
        counter: counter_client,
    };

    let receiving = wokio::spawn(async move {
        let received = rx.recv().await.expect("receiving failed").expect("the connection ended");
        assert_eq!(received.text, "value", "the text was altered");
        assert_eq!(received.data, data(DATA_SIZE), "the data was altered");

        // The channels the value carries must work over the downgraded connection.
        for index in 0..ITEMS {
            received.items.send(item(index)).await.expect("sending an item failed");
        }
        received.size.send(received.data.len()).expect("reporting the size failed");

        // So must calls on the remote object it carries.
        #[cfg(feature = "rtc")]
        {
            let mut counter = received.counter;
            let mut value_rx = counter.watch().await.expect("obtaining the watch channel failed");

            assert_eq!(counter.value().await.unwrap(), 0, "the counter did not start at zero");
            assert_eq!(counter.increase(20).await.unwrap(), 20, "the call had no effect");
            assert_eq!(counter.increase(22).await.unwrap(), 42, "the call had no effect");
            assert_eq!(counter.value().await.unwrap(), 42, "the calls had no effect");

            // The channel returned by a call must follow the calls made afterwards.
            while *value_rx.borrow_and_update().unwrap() != 42 {
                value_rx.changed().await.expect("the watch channel ended early");
            }
        }
    });

    tx.send(sent).await.expect("sending the value failed");

    for index in 0..ITEMS {
        let received = items_rx.recv().await.expect("receiving an item failed").expect("the channel ended");
        assert_eq!(received, item(index), "an item was altered");
    }
    assert!(items_rx.recv().await.unwrap().is_none(), "the channel did not end");

    assert_eq!(size_rx.await.unwrap(), DATA_SIZE, "the data did not arrive completely");

    receiving.await.unwrap();

    #[cfg(feature = "rtc")]
    {
        counter_task.await.unwrap();
        assert_eq!(counter_obj.read().await.value, 42, "the calls did not reach the served object");
    }
}

/// Transfers a value in both directions and uses everything it carries.
async fn exchange(remote: Capabilities) {
    let ((a_tx, a_rx), (b_tx, b_rx)) = connect_with(remote).await;

    // Both directions are exercised at the same time, so that ports are opened
    // concurrently by both endpoints.
    let both = futures::future::join(transfer(a_tx, b_rx), transfer(b_tx, a_rx));
    timeout(LIMIT, both).await.expect("the exchange blocked");
}

/// The full set of capabilities is the baseline every other case is compared to.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn all_capabilities() {
    crate::init();
    exchange(Capabilities::default()).await;
}

/// An endpoint without port id support is served without port ids.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn without_port_id() {
    crate::init();
    exchange(Capabilities { port_id: false, ..Default::default() }).await;
}

/// An endpoint without the compact format receives the old representations.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn without_compact_transported() {
    crate::init();
    exchange(Capabilities { compact_transported: false, ..Default::default() }).await;
}

/// An endpoint that does not report received data.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn without_received_report() {
    crate::init();
    exchange(Capabilities { received_report: false, ..Default::default() }).await;
}

/// An endpoint that does not support port side specification or pre-connecting.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn without_port_side_and_pre_connect() {
    crate::init();
    exchange(Capabilities { port_side: false, pre_connect: false, ..Default::default() }).await;
}

/// An endpoint that does not support variable integer encoding.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn without_varint() {
    crate::init();
    exchange(Capabilities { varint: false, ..Default::default() }).await;
}

/// An endpoint that has global credits disabled, which is independent of the
/// capabilities announced after them.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn without_global_credits() {
    crate::init();
    exchange(Capabilities { global_credits: false, ..Default::default() }).await;
}

/// An endpoint that announces no capability at all.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn without_any_optional_capability() {
    crate::init();
    exchange(Capabilities {
        global_credits: false,
        received_report: false,
        port_side: false,
        pre_connect: false,
        varint: false,
        compact_transported: false,
        port_id: false,
        postbag_version: postbag::cfg::Version::Postbag0_4,
        ..Default::default()
    })
    .await;
}
