//! Multiple associated types in one trait.

#[cfg(feature = "js")]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel;

#[remoc::rtc::remote]
pub trait KeyValue {
    type Key: remoc::RemoteSend + Clone;
    type Value: remoc::RemoteSend + Clone;

    async fn get(&self, key: Self::Key) -> Result<Option<Self::Value>, remoc::rtc::CallError>;
    async fn put(&mut self, key: Self::Key, value: Self::Value) -> Result<(), remoc::rtc::CallError>;
}

pub struct KeyValueObj {
    map: std::collections::HashMap<String, u32>,
}

impl KeyValueObj {
    pub fn new() -> Self {
        Self { map: std::collections::HashMap::new() }
    }
}

impl KeyValue for KeyValueObj {
    type Key = String;
    type Value = u32;

    async fn get(&self, key: String) -> Result<Option<u32>, remoc::rtc::CallError> {
        Ok(self.map.get(&key).copied())
    }

    async fn put(&mut self, key: String, value: u32) -> Result<(), remoc::rtc::CallError> {
        self.map.insert(key, value);
        Ok(())
    }
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn simple() {
    use remoc::rtc::ServerRefMut;

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<KeyValueClient<String, u32>>().await;

    let mut obj = KeyValueObj::new();
    let (server, client) = KeyValueServerRefMut::new(&mut obj, 1);

    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let mut client = b_rx.recv().await.unwrap().unwrap();
        assert_eq!(client.get("a".to_string()).await.unwrap(), None);
        client.put("a".to_string(), 7).await.unwrap();
        assert_eq!(client.get("a".to_string()).await.unwrap(), Some(7));
    };

    let ((), res) = tokio::join!(client_task, server.serve());
    res.unwrap();
}
