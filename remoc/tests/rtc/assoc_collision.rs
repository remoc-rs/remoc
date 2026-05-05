//! Trait whose associated type name collides with the trait's generic
//! parameter name. The macro lifts associated types into bare type
//! parameters with a `__` prefix on generated artifacts, so this must
//! still compile and work end-to-end.

#[cfg(feature = "js")]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel;

#[remoc::rtc::remote]
pub trait Coll<Item>
where
    Item: remoc::RemoteSend + Clone,
{
    // Associated type with the same name as the generic parameter above.
    type Item: remoc::RemoteSend + Clone;

    async fn put(&mut self, key: Item, value: Self::Item) -> Result<(), remoc::rtc::CallError>;
    async fn last(&self) -> Result<Option<Self::Item>, remoc::rtc::CallError>;
}

pub struct CollObj<K, V> {
    last: Option<V>,
    _k: std::marker::PhantomData<K>,
}

impl<K, V> CollObj<K, V> {
    pub fn new() -> Self {
        Self { last: None, _k: std::marker::PhantomData }
    }
}

impl<K, V> Coll<K> for CollObj<K, V>
where
    K: remoc::RemoteSend + Clone + Sync,
    V: remoc::RemoteSend + Clone + Sync,
{
    type Item = V;

    async fn put(&mut self, _key: K, value: V) -> Result<(), remoc::rtc::CallError> {
        self.last = Some(value);
        Ok(())
    }

    async fn last(&self) -> Result<Option<V>, remoc::rtc::CallError> {
        Ok(self.last.clone())
    }
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn simple() {
    use remoc::rtc::ServerRefMut;

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<CollClient<u32, String>>().await;

    let mut obj = CollObj::<u32, String>::new();
    let (server, client) = CollServerRefMut::new(&mut obj, 1);

    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let mut client = b_rx.recv().await.unwrap().unwrap();
        assert_eq!(client.last().await.unwrap(), None);
        client.put(1u32, "hello".to_string()).await.unwrap();
        assert_eq!(client.last().await.unwrap(), Some("hello".to_string()));
    };

    let ((), res) = tokio::join!(client_task, server.serve());
    res.unwrap();
}
