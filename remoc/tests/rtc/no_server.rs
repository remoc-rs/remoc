//! Generating no server variant at all.

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use remoc::{
    prelude::*,
    rtc::{CallError, Req},
};

/// An empty `server(...)` list generates the client and the request receiver,
/// but no server.
#[rtc::remote(server())]
pub trait Counter {
    async fn value(&self) -> Result<u32, CallError>;
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn serve_request_receiver() {
    crate::init();

    let (mut req_rx, client) = CounterReqReceiver::<remoc::codec::Default>::new();

    let task = wokio::spawn(async move {
        while let Some(req) = req_rx.recv().await.unwrap() {
            match req {
                Req::Ref(CounterReqRef::Value { __responder }) => {
                    let _ = __responder.send(Ok(42));
                }
                _ => unreachable!(),
            }
        }
    });

    assert_eq!(client.value().await.unwrap(), 42);

    drop(client);
    task.await.unwrap();
}
