//! Pipelining: the caller creates the client and request receiver pair itself, passes
//! the request receiver into a call and uses the client without waiting for that call.

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use std::sync::Arc;

use tokio::sync::RwLock;

use remoc::{prelude::*, rtc::CallError};

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

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum OpenError {
    Denied,
    Call(CallError),
}

impl From<CallError> for OpenError {
    fn from(err: CallError) -> Self {
        Self::Call(err)
    }
}

#[rtc::remote]
pub trait Directory {
    /// Opens the counter of the given name and serves it through `counter`.
    async fn open_counter(&self, name: String, counter: CounterReqReceiver) -> Result<(), OpenError>;
}

pub struct DirectoryObj;

impl Directory for DirectoryObj {
    async fn open_counter(&self, name: String, counter: CounterReqReceiver) -> Result<(), OpenError> {
        // The access check happens before anything polls the request receiver, so
        // queued calls cannot execute before it has passed.
        if name != "allowed" {
            return Err(OpenError::Denied);
        }

        let obj = Arc::new(RwLock::new(CounterObj { value: 0 }));
        wokio::spawn(counter.into_server_shared_mut(obj).serve());

        Ok(())
    }
}

/// Connects to a served directory on the other endpoint.
async fn directory_client() -> DirectoryClient {
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<DirectoryClient>().await;

    let (server, client) = DirectoryServerShared::new(Arc::new(DirectoryObj));
    wokio::spawn(server.serve());
    a_tx.send(client).await.unwrap();

    b_rx.recv().await.unwrap().unwrap()
}

/// Calls issued on the client before the request receiver reaches the server are
/// queued and executed once it is attached to a target object.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn pipelined_calls_are_queued() {
    crate::init();
    let dir = directory_client().await;

    println!("Creating the counter client and its request receiver");
    let (mut counter, counter_rx) = CounterClient::new();

    // The work is polled first, so its requests are queued before `open_counter`
    // even sends the request receiver to the other endpoint.
    let (value, opened) = tokio::join!(
        async move {
            counter.increase(20).await.unwrap();
            counter.increase(45).await.unwrap();
            counter.value().await.unwrap()
        },
        dir.open_counter("allowed".to_string(), counter_rx),
    );

    opened.unwrap();
    assert_eq!(value, 65);
}

/// Queued calls fail when the access check rejects the request receiver.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn pipelined_calls_fail_when_access_is_denied() {
    crate::init();
    let dir = directory_client().await;

    let (mut counter, counter_rx) = CounterClient::new();

    let (value, opened) = tokio::join!(
        async move { counter.increase(20).await },
        dir.open_counter("forbidden".to_string(), counter_rx),
    );

    assert!(matches!(opened, Err(OpenError::Denied)));
    assert!(value.is_err(), "queued call must not succeed when access was denied");
}
