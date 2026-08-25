#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel;

// Avoid imports here to test if proc macro works without imports.

#[remoc::rtc::remote]
pub trait DataGenerator {
    async fn data(&self, size: usize) -> Result<Vec<u8>, remoc::rtc::CallError>;
}

pub struct DataGeneratorObj {}

impl DataGeneratorObj {
    pub fn new() -> Self {
        Self {}
    }
}

impl DataGenerator for DataGeneratorObj {
    async fn data(&self, size: usize) -> Result<Vec<u8>, remoc::rtc::CallError> {
        Ok(vec![1; size])
    }
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn max_item_size_exceeded() {
    use remoc::rtc::{Client, ServerRefMut};

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<DataGeneratorClient>().await;

    let mut gen_obj = DataGeneratorObj::new();
    let (server, client) = DataGeneratorServerRefMut::new(&mut gen_obj);

    a_tx.send(client).await.unwrap();

    let client_task = async move {
        println!("Receiving client");
        let mut client = b_rx.recv().await.unwrap().unwrap();

        client.set_max_response_size(16777);
        let max_item_size = client.max_response_size();

        let elems = max_item_size / 10;
        println!("Getting {elems} elements, which is under limit");
        let rxed = client.data(elems).await.unwrap();
        println!("Received {} elements", rxed.len());
        assert_eq!(rxed.len(), elems);

        let elems = max_item_size * 10;
        println!("Getting {elems} elements, which is over limit");
        let rxed = client.data(elems).await;
        assert!(matches!(rxed, Err(remoc::rtc::CallError::Dropped)));
    };

    let ((), res) = tokio::join!(client_task, server.serve());
    assert!(matches!(
        res,
        Err(remoc::rtc::ServeError::ResponseSend(remoc::rch::SendingErrorKind::Send(
            remoc::rch::base::SendErrorKind::MaxItemSizeExceeded
        )))
    ));
}

#[remoc::rtc::remote]
pub trait Greeter {
    async fn greet(&self) -> Result<String, remoc::rtc::CallError>;
}

pub struct GreeterObj;

impl Greeter for GreeterObj {
    async fn greet(&self) -> Result<String, remoc::rtc::CallError> {
        Ok("hello".to_string())
    }
}

/// A client whose object has stopped being served says so, rather than reporting
/// the request as having failed while being processed.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn calling_an_unserved_object() {
    use remoc::rtc::ServerShared;
    use std::sync::Arc;

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<GreeterClient>().await;

    let (server, client) = GreeterServerShared::new(Arc::new(GreeterObj));
    a_tx.send(client).await.unwrap();

    let client = b_rx.recv().await.unwrap().unwrap();

    // While it is served, calls work.
    let serving = wokio::spawn(async move { server.serve().await });
    assert_eq!(client.greet().await.unwrap(), "hello");

    // Once serving has finished, they are refused with a reason that says why.
    serving.abort();
    let _ = serving.await;

    // The connection itself is still up; there is simply nobody serving.
    let err = client.greet().await.unwrap_err();
    assert!(matches!(err, remoc::rtc::CallError::NotServed), "unexpected error: {err:?}");
    assert!(err.to_string().contains("no longer served"), "unhelpful message: {err}");
}
