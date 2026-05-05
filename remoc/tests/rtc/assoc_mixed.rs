//! Mixed: trait has both a regular generic parameter and an associated type.

#[cfg(feature = "js")]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel;

#[remoc::rtc::remote]
pub trait Mixed<T>
where
    T: remoc::RemoteSend + Clone,
{
    type Item: remoc::RemoteSend + Clone;

    async fn pair(&self, lhs: T) -> Result<(T, Self::Item), remoc::rtc::CallError>;
    async fn replace(&mut self, item: Self::Item) -> Result<Self::Item, remoc::rtc::CallError>;
}

pub struct MixedObj<U> {
    item: U,
}

impl<U: Clone> MixedObj<U> {
    pub fn new(item: U) -> Self {
        Self { item }
    }
}

impl<T, U> Mixed<T> for MixedObj<U>
where
    T: remoc::RemoteSend + Clone,
    U: remoc::RemoteSend + Clone + Sync,
{
    type Item = U;

    async fn pair(&self, lhs: T) -> Result<(T, U), remoc::rtc::CallError> {
        Ok((lhs, self.item.clone()))
    }

    async fn replace(&mut self, item: U) -> Result<U, remoc::rtc::CallError> {
        let old = std::mem::replace(&mut self.item, item);
        Ok(old)
    }
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn simple() {
    use remoc::rtc::ServerRefMut;

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<MixedClient<u8, String>>().await;

    let mut obj = MixedObj::new("hello".to_string());
    let (server, client) = MixedServerRefMut::new(&mut obj, 1);

    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let mut client = b_rx.recv().await.unwrap().unwrap();
        assert_eq!(client.pair(5u8).await.unwrap(), (5u8, "hello".to_string()));
        let prev = client.replace("world".to_string()).await.unwrap();
        assert_eq!(prev, "hello");
        assert_eq!(client.pair(1u8).await.unwrap(), (1u8, "world".to_string()));
    };

    let ((), res) = tokio::join!(client_task, server.serve());
    res.unwrap();
}
