//! The `#[pipelinable]` attribute.

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use std::sync::Arc;

use tokio::sync::RwLock;

use remoc::{
    prelude::*,
    rtc::{CallError, ConsumedExt},
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
    /// Opens the counter of the given name.
    #[pipelinable]
    async fn open_counter(&self, name: String) -> Result<CounterClient, OpenError>;
}

pub struct DirectoryObj;

impl Directory for DirectoryObj {
    async fn open_counter(&self, name: String) -> Result<CounterClient, OpenError> {
        if name != "allowed" {
            return Err(OpenError::Denied);
        }

        let obj = Arc::new(RwLock::new(CounterObj { value: 0 }));
        let (server, client) = CounterServerSharedMut::new(obj, 1);
        wokio::spawn(server.serve(true));

        Ok(client)
    }
}

/// Connects to a served directory on the other endpoint.
async fn directory_client() -> DirectoryClient {
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<DirectoryClient>().await;

    let (server, client) = DirectoryServerShared::new(Arc::new(DirectoryObj), 1);
    wokio::spawn(server.serve(true));
    a_tx.send(client).await.unwrap();

    b_rx.recv().await.unwrap().unwrap()
}

/// The ordinary method still works.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn normal_call() {
    crate::init();
    let dir = directory_client().await;

    let mut counter = dir.open_counter("allowed".to_string()).await.unwrap();
    counter.increase(20).await.unwrap();
    assert_eq!(counter.value().await.unwrap(), 20);
}

/// Calls made on the pipelined client before the session is established are queued.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn pipelined_call() {
    crate::init();
    let dir = directory_client().await;

    let (mut counter, counter_rx) = CounterClient::new(4);

    let (value, opened) = tokio::join!(
        async {
            counter.increase(20).await.unwrap();
            counter.increase(45).await.unwrap();
            let value = counter.value().await.unwrap();
            drop(counter);
            value
        },
        dir.open_counter_pipelined("allowed".to_string(), counter_rx),
    );

    assert_eq!(value, 65);

    // The session returns the client, so it can be used directly afterwards.
    let mut counter = opened.unconsumed().unwrap();
    assert_eq!(counter.value().await.unwrap(), 65);
    counter.increase(5).await.unwrap();
    assert_eq!(counter.value().await.unwrap(), 70);
}

/// Queued calls fail when the session is rejected.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn pipelined_call_denied() {
    crate::init();
    let dir = directory_client().await;

    let (mut counter, counter_rx) = CounterClient::new(4);

    let (value, opened) = tokio::join!(
        async {
            let res = counter.increase(20).await;
            drop(counter);
            res
        },
        dir.open_counter_pipelined("forbidden".to_string(), counter_rx),
    );

    assert!(matches!(opened, Err(OpenError::Denied)));
    assert!(value.is_err());
}

/// A vault that serves the request receiver directly instead of forwarding to a client.
#[rtc::remote]
pub trait Vault: Send + Sync {
    #[pipelinable]
    async fn open_counter(&self, name: String) -> Result<CounterClient, OpenError>;
}

pub struct VaultObj;

impl Vault for VaultObj {
    async fn open_counter(&self, name: String) -> Result<CounterClient, OpenError> {
        if name != "allowed" {
            return Err(OpenError::Denied);
        }

        let obj = Arc::new(RwLock::new(CounterObj { value: 0 }));
        let (server, client) = CounterServerSharedMut::new(obj, 1);
        wokio::spawn(server.serve(true));

        Ok(client)
    }

    /// Overrides the default implementation, which would forward the requests to the
    /// client returned by `open_counter`, and serves them directly instead.
    async fn open_counter_pipelined(
        &self, name: String, __req_rx: CounterReqReceiver,
    ) -> Result<Option<CounterClient>, OpenError> {
        if name != "allowed" {
            return Err(OpenError::Denied);
        }

        let obj = Arc::new(RwLock::new(CounterObj { value: 0 }));
        __req_rx.into_server_shared_mut(obj.clone()).serve(true).await.map_err(CallError::from)?;

        // Hand back a client so that the counter can be used after the session.
        let (server, client) = CounterServerSharedMut::new(obj, 1);
        wokio::spawn(server.serve(true));

        Ok(Some(client))
    }
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn pipelined_call_with_overridden_twin() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<VaultClient>().await;

    let (server, client) = VaultServerShared::new(Arc::new(VaultObj), 1);
    wokio::spawn(server.serve(true));
    a_tx.send(client).await.unwrap();
    let vault = b_rx.recv().await.unwrap().unwrap();

    let (mut counter, counter_rx) = CounterClient::new(4);

    let (value, opened) = tokio::join!(
        async {
            counter.increase(20).await.unwrap();
            counter.increase(45).await.unwrap();
            let value = counter.value().await.unwrap();
            drop(counter);
            value
        },
        vault.open_counter_pipelined("allowed".to_string(), counter_rx),
    );

    assert_eq!(value, 65);

    let counter = opened.unconsumed().unwrap();
    assert_eq!(counter.value().await.unwrap(), 65);
}

#[rtc::remote]
pub trait Session {
    async fn get(&self) -> Result<u32, CallError>;
    async fn close(self) -> Result<(), CallError>;
}

pub struct SessionObj;

impl Session for SessionObj {
    async fn get(&self) -> Result<u32, CallError> {
        Ok(42)
    }

    async fn close(self) -> Result<(), CallError> {
        Ok(())
    }
}

#[rtc::remote]
pub trait Sessions {
    #[pipelinable(open_pipelined)]
    async fn open(&self) -> Result<SessionClient, CallError>;
}

pub struct SessionsObj;

impl Sessions for SessionsObj {
    async fn open(&self) -> Result<SessionClient, CallError> {
        let (server, client) = SessionServer::new(SessionObj, 1);
        wokio::spawn(server.serve());
        Ok(client)
    }
}

/// An object consumed by a method taking `self` by value yields no client.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn pipelined_call_consuming_the_object() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<SessionsClient>().await;

    let (server, client) = SessionsServerShared::new(Arc::new(SessionsObj), 1);
    wokio::spawn(server.serve(true));
    a_tx.send(client).await.unwrap();
    let sessions = b_rx.recv().await.unwrap().unwrap();

    let (session, session_rx) = SessionClient::new(4);

    let (value, opened) = tokio::join!(
        async {
            let value = session.get().await.unwrap();
            session.close().await.unwrap();
            value
        },
        sessions.open_pipelined(session_rx),
    );

    assert_eq!(value, 42);

    // The object was consumed, so no client is returned.
    assert!(opened.as_ref().unwrap().is_none());
    assert!(matches!(opened.unconsumed(), Err(CallError::Consumed)));
}

/// A request receiver consumer handling requests as messages replies with
/// `PipelinableReplyTo::complete`, which handles both the normal and the pipelined case.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn manual_req_receiver_completes_pipelined_call() {
    use remoc::rtc::Req;

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<DirectoryClient>().await;

    let (mut req_rx, client) = DirectoryReqReceiver::new(1);
    a_tx.send(client).await.unwrap();
    let dir = b_rx.recv().await.unwrap().unwrap();

    wokio::spawn(async move {
        while let Some(req) = req_rx.recv().await.unwrap() {
            if let Req::Ref(DirectoryReqRef::OpenCounter { __reply_tx, name }) = req {
                let result = if name == "allowed" {
                    let obj = Arc::new(RwLock::new(CounterObj { value: 0 }));
                    let (server, client) = CounterServerSharedMut::new(obj, 1);
                    wokio::spawn(server.serve(true));
                    Ok(client)
                } else {
                    Err(OpenError::Denied)
                };

                // Completing a pipelined request runs the whole session, so it is
                // spawned to keep receiving further requests.
                assert!(__reply_tx.is_pipelined());
                wokio::spawn(async move {
                    // Awaiting the returned handle reports whether the reply, which is
                    // sent once the session has finished, was transmitted.
                    __reply_tx.complete(result).await.await.unwrap();
                });
            }
        }
    });

    let (mut counter, counter_rx) = CounterClient::new(4);

    let (value, opened) = tokio::join!(
        async {
            counter.increase(20).await.unwrap();
            let value = counter.value().await.unwrap();
            drop(counter);
            value
        },
        dir.open_counter_pipelined("allowed".to_string(), counter_rx),
    );

    assert_eq!(value, 20);

    let counter = opened.unconsumed().unwrap();
    assert_eq!(counter.value().await.unwrap(), 20);
}
