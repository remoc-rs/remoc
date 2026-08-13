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
    let (server, client) = DataGeneratorServerRefMut::new(&mut gen_obj, 1);

    a_tx.send(client).await.unwrap();

    let client_task = async move {
        println!("Receiving client");
        let mut client = b_rx.recv().await.unwrap().unwrap();

        client.set_max_reply_size(16777);
        let max_item_size = client.max_reply_size();

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
        Err(remoc::rtc::ServeError::ReplySend(remoc::rch::SendingErrorKind::Send(
            remoc::rch::base::SendErrorKind::MaxItemSizeExceeded
        )))
    ));
}
