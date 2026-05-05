//! Associated type with extra bounds beyond `RemoteSend`.

#[cfg(feature = "js")]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel;

pub trait Doubleable: Sized {
    fn doubled(self) -> Self;
}

impl Doubleable for u32 {
    fn doubled(self) -> Self {
        self * 2
    }
}

#[remoc::rtc::remote]
pub trait Doubler {
    type Item: remoc::RemoteSend + Doubleable + Clone;

    async fn current(&self) -> Result<Self::Item, remoc::rtc::CallError>;
    async fn double(&mut self) -> Result<Self::Item, remoc::rtc::CallError>;
}

pub struct DoublerObj<T> {
    value: T,
}

impl<T: Clone> DoublerObj<T> {
    pub fn new(value: T) -> Self {
        Self { value }
    }
}

impl<T> Doubler for DoublerObj<T>
where
    T: remoc::RemoteSend + Doubleable + Clone + Sync,
{
    type Item = T;

    async fn current(&self) -> Result<T, remoc::rtc::CallError> {
        Ok(self.value.clone())
    }

    async fn double(&mut self) -> Result<T, remoc::rtc::CallError> {
        self.value = self.value.clone().doubled();
        Ok(self.value.clone())
    }
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn simple() {
    use remoc::rtc::ServerRefMut;

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<DoublerClient<u32>>().await;

    let mut obj = DoublerObj::new(3u32);
    let (server, client) = DoublerServerRefMut::new(&mut obj, 1);

    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let mut client = b_rx.recv().await.unwrap().unwrap();
        assert_eq!(client.current().await.unwrap(), 3);
        assert_eq!(client.double().await.unwrap(), 6);
        assert_eq!(client.double().await.unwrap(), 12);
    };

    let ((), res) = tokio::join!(client_task, server.serve());
    res.unwrap();
}
