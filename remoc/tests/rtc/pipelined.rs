//! The `#[pipelinable]` attribute.

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
    async fn multiply(&mut self, by: u32) -> Result<(), CallError>;
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

    async fn multiply(&mut self, by: u32) -> Result<(), CallError> {
        self.value *= by;
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

/// Error of a pipelined series of calls, which the session and call errors
/// converge into.
#[derive(Debug)]
#[allow(dead_code)]
pub enum WorkError {
    Open(OpenError),
    Call(CallError),
}

impl From<OpenError> for WorkError {
    fn from(err: OpenError) -> Self {
        Self::Open(err)
    }
}

impl From<CallError> for WorkError {
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
        let (server, client) = CounterServerSharedMut::new(obj);
        wokio::spawn(server.serve());

        Ok(client)
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

    let (mut counter, counter_rx) = CounterClient::new();

    // The request receiver is handed over immediately, so the counter can be used
    // without waiting for the session call to complete.
    let session = dir.open_counter_pipelined("allowed".to_string(), counter_rx).await;

    counter.increase(20).await.unwrap();
    counter.increase(45).await.unwrap();
    assert_eq!(counter.value().await.unwrap(), 65);

    session.await.unwrap();

    // The client stays usable after the session call has completed.
    counter.increase(5).await.unwrap();
    assert_eq!(counter.value().await.unwrap(), 70);
}

/// Queued calls fail when the session is rejected.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn pipelined_call_denied() {
    crate::init();
    let dir = directory_client().await;

    let (mut counter, counter_rx) = CounterClient::new();

    let (value, opened) = tokio::join!(
        async move { counter.increase(20).await },
        dir.open_counter_pipelined("forbidden".to_string(), counter_rx),
    );

    assert!(matches!(opened.await, Err(OpenError::Denied)));
    assert!(value.is_err());
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
        let (server, client) = SessionServer::new(SessionObj);
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

    let (server, client) = SessionsServerShared::new(Arc::new(SessionsObj));
    wokio::spawn(server.serve());
    a_tx.send(client).await.unwrap();
    let sessions = b_rx.recv().await.unwrap().unwrap();

    let (session, session_rx) = SessionClient::new();

    let (value, opened) = tokio::join!(
        async move {
            let value = session.get().await.unwrap();
            session.close().await.unwrap();
            value
        },
        sessions.open_pipelined(session_rx),
    );

    assert_eq!(value, 42);

    // The session itself succeeded; the object was consumed by the caller.
    opened.await.unwrap();
}

/// A request receiver consumer handling requests as messages replies with
/// `PipelinableReplyTo::complete`, which handles both the normal and the pipelined case.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn manual_req_receiver_completes_pipelined_call() {
    use remoc::rtc::Req;

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<DirectoryClient>().await;

    let (mut req_rx, client) = DirectoryReqReceiver::new();
    a_tx.send(client).await.unwrap();
    let dir = b_rx.recv().await.unwrap().unwrap();

    wokio::spawn(async move {
        while let Some(req) = req_rx.recv().await.unwrap() {
            if let Req::Ref(DirectoryReqRef::OpenCounter { __reply_tx, name }) = req {
                let result = if name == "allowed" {
                    let obj = Arc::new(RwLock::new(CounterObj { value: 0 }));
                    let (server, client) = CounterServerSharedMut::new(obj);
                    wokio::spawn(server.serve());
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

    let (mut counter, counter_rx) = CounterClient::new();

    let session = dir.open_counter_pipelined("allowed".to_string(), counter_rx).await;

    counter.increase(20).await.unwrap();
    assert_eq!(counter.value().await.unwrap(), 20);

    session.await.unwrap();
}

/// Chains calls on the pipelined client using the `calls!` macro.
///
/// `increase` and `multiply` do not commute, so the resulting value proves that the
/// requests were executed in the order the calls were started.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn calls_macro() -> Result<(), WorkError> {
    crate::init();
    let dir = directory_client().await;

    let (mut counter, counter_rx) = CounterClient::new();

    // The session and the calls have differing error types, which are converted into
    // the error type of this function.
    let value = rtc::calls!(
        dir.open_counter_pipelined("allowed".to_string(), counter_rx);
        counter.increase_call(2);
        counter.multiply_call(10);
        counter.increase_call(3);
        counter.value_call()
    );

    // (0 + 2) * 10 + 3, not 0 + 2 + 3 then * 10.
    assert_eq!(value, 23);

    Ok(())
}

/// A trailing semicolon discards the value of the last call, as in a block.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn calls_macro_without_value() -> Result<(), WorkError> {
    crate::init();
    let dir = directory_client().await;

    let (mut counter, counter_rx) = CounterClient::new();

    rtc::calls!(
        dir.open_counter_pipelined("allowed".to_string(), counter_rx);
        counter.increase_call(2);
        counter.multiply_call(10);
    );

    assert_eq!(counter.value().await?, 20);

    Ok(())
}

/// Without a session the macro chains calls on an already connected client.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn calls_macro_without_session() -> Result<(), WorkError> {
    crate::init();
    let dir = directory_client().await;

    let mut counter = dir.open_counter("allowed".to_string()).await?;

    let value = rtc::calls!(
        counter.increase_call(2);
        counter.multiply_call(10);
        counter.value_call()
    );

    assert_eq!(value, 20);

    Ok(())
}

/// A failing session is reported instead of the calls failing because of it.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn calls_macro_reports_session_error() {
    crate::init();
    let dir = directory_client().await;

    let (mut counter, counter_rx) = CounterClient::new();

    // Obtaining the result as a value, rather than propagating it, requires an async
    // block that states the error type.
    let value = async {
        Ok::<_, WorkError>(rtc::calls!(
            dir.open_counter_pipelined("forbidden".to_string(), counter_rx);
            counter.increase_call(2);
            counter.value_call()
        ))
    }
    .await;

    assert!(matches!(value, Err(WorkError::Open(OpenError::Denied))));
}

/// `Call::map_err` brings calls with differing error types to a common one, so that
/// they can be awaited together using `try_join!`, which requires them to match.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn call_map_err() {
    crate::init();
    let dir = directory_client().await;

    let (mut counter, counter_rx) = CounterClient::new();

    // Error type `OpenError`.
    let session = dir.open_counter_pipelined("allowed".to_string(), counter_rx).await;
    // Error type `CallError`.
    let increase = counter.increase_call(20).await;
    let value = counter.value_call().await;

    let (_, _, value) = tokio::try_join!(
        session.map_err(WorkError::from),
        increase.map_err(WorkError::from),
        value.map_err(WorkError::from),
    )
    .unwrap();

    assert_eq!(value, 20);
}
