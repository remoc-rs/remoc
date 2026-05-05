//! Single associated type, no bounds beyond `RemoteSend`.

#[cfg(feature = "js")]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel;

#[remoc::rtc::remote]
pub trait Storage {
    type Item: remoc::RemoteSend + Clone;

    async fn get(&self) -> Result<Self::Item, remoc::rtc::CallError>;
    async fn set(&mut self, value: Self::Item) -> Result<(), remoc::rtc::CallError>;
}

pub struct StorageObj<T> {
    value: T,
}

impl<T: Clone> StorageObj<T> {
    pub fn new(value: T) -> Self {
        Self { value }
    }
}

impl<T> Storage for StorageObj<T>
where
    T: remoc::RemoteSend + Clone + Sync,
{
    type Item = T;

    async fn get(&self) -> Result<T, remoc::rtc::CallError> {
        Ok(self.value.clone())
    }

    async fn set(&mut self, value: T) -> Result<(), remoc::rtc::CallError> {
        self.value = value;
        Ok(())
    }
}

/// Non-generic implementer with a fixed associated type.
pub struct FixedStorage {
    value: String,
}

impl FixedStorage {
    pub fn new() -> Self {
        Self { value: String::new() }
    }
}

impl Storage for FixedStorage {
    type Item = String;

    async fn get(&self) -> Result<String, remoc::rtc::CallError> {
        Ok(self.value.clone())
    }

    async fn set(&mut self, value: String) -> Result<(), remoc::rtc::CallError> {
        self.value = value;
        Ok(())
    }
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn simple() {
    use remoc::rtc::ServerRefMut;

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<StorageClient<u32>>().await;

    let mut obj = StorageObj::new(0u32);
    let (server, client) = StorageServerRefMut::new(&mut obj, 1);

    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let mut client = b_rx.recv().await.unwrap().unwrap();
        assert_eq!(client.get().await.unwrap(), 0);
        client.set(42).await.unwrap();
        assert_eq!(client.get().await.unwrap(), 42);
    };

    let ((), res) = tokio::join!(client_task, server.serve());
    res.unwrap();
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn fixed() {
    use remoc::rtc::ServerRefMut;

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<StorageClient<String>>().await;

    let mut obj = FixedStorage::new();
    let (server, client) = StorageServerRefMut::new(&mut obj, 1);

    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let mut client = b_rx.recv().await.unwrap().unwrap();
        assert_eq!(client.get().await.unwrap(), String::new());
        client.set("hello".to_string()).await.unwrap();
        assert_eq!(client.get().await.unwrap(), "hello".to_string());
    };

    let ((), res) = tokio::join!(client_task, server.serve());
    res.unwrap();
}
