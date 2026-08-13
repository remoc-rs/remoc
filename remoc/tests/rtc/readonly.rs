#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel;

// Avoid imports here to test if proc macro works without imports.

#[remoc::rtc::remote]
pub trait ReadValue {
    async fn value(&self) -> Result<u32, remoc::rtc::CallError>;
}

pub struct ReadValueObj {
    value: u32,
}

impl ReadValueObj {
    pub fn new(value: u32) -> Self {
        Self { value }
    }
}

impl ReadValue for ReadValueObj {
    async fn value(&self) -> Result<u32, remoc::rtc::CallError> {
        Ok(self.value)
    }
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn simple() {
    use remoc::rtc::ServerRef;

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<ReadValueClient>().await;

    println!("Creating server");
    let obj = ReadValueObj::new(123);
    let (server, client) = ReadValueServerRef::new(&obj, 1);

    println!("Sending client");
    a_tx.send(client).await.unwrap();

    let client_task = async move {
        println!("Receiving client");
        let client = b_rx.recv().await.unwrap().unwrap();

        println!("value: {}", client.value().await.unwrap());
        assert_eq!(client.value().await.unwrap(), 123);
    };

    let ((), res) = tokio::join!(client_task, server.serve());
    res.unwrap();
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn closed() {
    use remoc::rtc::{Client, ServerRef};

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<ReadValueClient>().await;

    println!("Creating server");
    let obj = ReadValueObj::new(123);
    let (server, client) = ReadValueServerRef::new(&obj, 16);

    println!("Sending client");
    a_tx.send(client).await.unwrap();

    let (drop_tx, drop_rx) = tokio::sync::oneshot::channel();

    let client_task = async move {
        println!("Receiving client");
        let client = b_rx.recv().await.unwrap().unwrap();

        println!("value: {}", client.value().await.unwrap());
        assert_eq!(client.value().await.unwrap(), 123);

        assert!(!client.is_closed());
        println!("Client capacity: {}", client.capacity());

        wokio::spawn(async move {
            wokio::time::sleep(std::time::Duration::from_millis(500)).await;
            drop_tx.send(()).unwrap();
        });

        println!("Waiting for client close");
        client.closed().await;
        println!("Client closed");

        assert!(client.is_closed());
    };

    let server_task = async move {
        tokio::select! {
            biased;
            res = drop_rx => res.unwrap(),
            res = server.serve() => res.unwrap(),
        }
        println!("Dropping server");
    };

    tokio::join!(client_task, server_task);
}
