//! Sending a request receiver to a remote endpoint.

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use remoc::{
    prelude::*,
    rtc::{CallError, Req},
};

use crate::loop_channel;

#[rtc::remote]
pub trait Counter {
    async fn value(&self) -> Result<u32, CallError>;
    async fn increase(&mut self, by: u32) -> Result<(), CallError>;
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn simple_req_remote() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<CounterReqReceiver>().await;

    println!("Creating counter request receiver");
    let (req_rx, mut client) = CounterReqReceiver::new();

    println!("Sending counter request receiver to remote endpoint");
    a_tx.send(req_rx).await.unwrap();

    let server_task = wokio::spawn(async move {
        println!("Receiving counter request receiver");
        let mut req_rx = b_rx.recv().await.unwrap().unwrap();

        let mut value = 0;
        while let Some(req) = req_rx.recv().await.unwrap() {
            match req {
                Req::Ref(CounterReqRef::Value { __reply_tx }) => {
                    // Replying with `complete` works the same way as for a pipelined method.
                    __reply_tx.complete(Ok(value)).await;
                }
                Req::RefMut(CounterReqRefMut::Increase { __reply_tx, by }) => {
                    value += by;
                    let _ = __reply_tx.send(Ok(()));
                }
                _ => (),
            }
        }

        value
    });

    println!("value: {}", client.value().await.unwrap());
    assert_eq!(client.value().await.unwrap(), 0);

    println!("add 20");
    client.increase(20).await.unwrap();
    assert_eq!(client.value().await.unwrap(), 20);

    println!("add 45");
    client.increase(45).await.unwrap();
    assert_eq!(client.value().await.unwrap(), 65);

    drop(client);

    let value = server_task.await.unwrap();
    println!("Counter value: {value}");
    assert_eq!(value, 65);
}
