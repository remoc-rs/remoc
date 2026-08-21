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
}

impl CheckedAddable for u8 {
    fn checked_add(self, rhs: Self) -> Option<Self> {
        self.checked_add(rhs)
    }
}

#[remoc::rtc::remote]
pub trait GenericCounter<T>
where
    T: remoc::RemoteSend + CheckedAddable + Clone + Default + Sync,
{
    async fn value(&self) -> Result<T, remoc::rtc::CallError>;
    async fn watch(&mut self) -> Result<remoc::rch::watch::Receiver<T>, remoc::rtc::CallError>;
    #[no_cancel]
    async fn increase(&mut self, #[serde(default)] by: T) -> Result<(), IncreaseError>;
}

fn _assert_req_receiver_remote_sendable<T>()
where
    T: remoc::RemoteSend + CheckedAddable + Clone + Default + Sync,
{
    fn assert<X: remoc::RemoteSend>() {}
    assert::<GenericCounterReqReceiver<T>>();
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
    T: remoc::RemoteSend + CheckedAddable + Clone + Default + Sync,
{
    async fn value(&self) -> Result<T, remoc::rtc::CallError> {
        Ok(self.value.clone())
    }

    async fn watch(&mut self) -> Result<remoc::rch::watch::Receiver<T>, remoc::rtc::CallError> {
        let (tx, rx) = remoc::rch::watch::channel(self.value.clone());
        self.watchers.push(tx);
        Ok(rx)
    }

    async fn increase(&mut self, by: T) -> Result<(), IncreaseError> {
        match self.value.clone().checked_add(by) {
            Some(new_value) => self.value = new_value,
            None => return Err(IncreaseError::Overflow),
        }

        for watch in &self.watchers {
            let _ = watch.send(self.value.clone());
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
    let (server, client) = GenericCounterServer::new(counter_obj, 1);

    println!("Sending generic counter client");
    a_tx.send(client).await.unwrap();

    let client_task = async move {
        println!("Receiving counter client");
        let mut client = b_rx.recv().await.unwrap().unwrap();

        println!("Spawning watch...");
        let mut watch_rx = client.watch().await.unwrap();
        wokio::spawn(async move {
            while watch_rx.changed().await.is_ok() {
                println!("Watch value: {}", *watch_rx.borrow_and_update().unwrap());
            }
        });

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
