#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel;

// Avoid imports here to test if proc macro works without imports.

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum IncreaseError {
    Overflow,
    Call(remoc::rtc::CallError),
}

impl From<remoc::rtc::CallError> for IncreaseError {
    fn from(err: remoc::rtc::CallError) -> Self {
        Self::Call(err)
    }
}

pub trait CheckedAddable
where
    Self: Sized,
{
    fn checked_add(self, rhs: Self) -> Option<Self>;
    fn my_clone(&self) -> Self;
}

impl CheckedAddable for u8 {
    fn checked_add(self, rhs: Self) -> Option<Self> {
        self.checked_add(rhs)
    }

    fn my_clone(&self) -> Self {
        *self
    }
}

#[remoc::rtc::remote]
pub trait GenericCounter<T>
where
    T: remoc::RemoteSend + CheckedAddable + Default + Sync,
{
    async fn value(&self) -> Result<T, remoc::rtc::CallError>;
    #[no_cancel]
    async fn increase(&mut self, #[serde(default)] by: T) -> Result<(), IncreaseError>;
}

pub struct GenericCounterObj<T> {
    value: T,
    watchers: Vec<remoc::rch::watch::Sender<T>>,
}

impl<T> GenericCounterObj<T>
where
    T: Default,
{
    pub fn new() -> Self {
        Self { value: T::default(), watchers: Vec::new() }
    }
}

impl<T> GenericCounter<T> for GenericCounterObj<T>
where
    T: remoc::RemoteSend + CheckedAddable + Default + Sync,
{
    async fn value(&self) -> Result<T, remoc::rtc::CallError> {
        Ok(self.value.my_clone())
    }

    async fn increase(&mut self, by: T) -> Result<(), IncreaseError> {
        match self.value.my_clone().checked_add(by) {
            Some(new_value) => self.value = new_value,
            None => return Err(IncreaseError::Overflow),
        }

        for watch in &self.watchers {
            let _ = watch.send(self.value.my_clone());
        }

        Ok(())
    }
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn simple() {
    use remoc::rtc::Server;

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<GenericCounterClient<u8>>().await;

    println!("Creating generic counter server");
    let counter_obj = GenericCounterObj::new();
    let (server, client) = GenericCounterServer::new(counter_obj);

    println!("Sending generic counter client");
    a_tx.send(client).await.unwrap();

    let client_task = async move {
        println!("Receiving counter client");
        let mut client = b_rx.recv().await.unwrap().unwrap();

        println!("value: {}", client.value().await.unwrap());
        assert_eq!(client.value().await.unwrap(), 0);

        println!("add 20");
        client.increase(20).await.unwrap();
        println!("value: {}", client.value().await.unwrap());
        assert_eq!(client.value().await.unwrap(), 20);

        println!("add 45");
        client.increase(45).await.unwrap();
        println!("value: {}", client.value().await.unwrap());
        assert_eq!(client.value().await.unwrap(), 65);
    };

    let ((), (_counter_obj, res)) = tokio::join!(client_task, server.serve());
    res.unwrap();
}
