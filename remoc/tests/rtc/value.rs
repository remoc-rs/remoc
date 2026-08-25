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

#[remoc::rtc::remote]
pub trait Counter {
    async fn value(self) -> Result<u32, remoc::rtc::CallError>;
    async fn value_plus(self, add: u32) -> Result<u32, remoc::rtc::CallError>;
    async fn value_ref(&self) -> Result<u32, remoc::rtc::CallError>;
    #[no_cancel]
    async fn increase(&mut self, #[serde(default)] by: u32) -> Result<(), IncreaseError>;
}

pub struct CounterObj {
    value: u32,
    watchers: Vec<remoc::rch::watch::Sender<u32>>,
}

impl CounterObj {
    pub fn new() -> Self {
        Self { value: 0, watchers: Vec::new() }
    }
}

impl Counter for CounterObj {
    async fn value(self) -> Result<u32, remoc::rtc::CallError> {
        Ok(self.value)
    }

    async fn value_plus(self, add: u32) -> Result<u32, remoc::rtc::CallError> {
        Ok(self.value + add)
    }

    async fn value_ref(&self) -> Result<u32, remoc::rtc::CallError> {
        Ok(self.value)
    }

    async fn increase(&mut self, by: u32) -> Result<(), IncreaseError> {
        match self.value.checked_add(by) {
            Some(new_value) => self.value = new_value,
            None => return Err(IncreaseError::Overflow),
        }

        for watch in &self.watchers {
            let _ = watch.send(self.value);
        }

        Ok(())
    }
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn simple() {
    use remoc::rtc::Server;

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<CounterClient>().await;

    println!("Creating counter server");
    let counter_obj = CounterObj::new();
    let (server, client) = CounterServer::new(counter_obj);

    println!("Sending counter client");
    a_tx.send(client).await.unwrap();

    let client_task = async move {
        println!("Receiving counter client");
        let mut client = b_rx.recv().await.unwrap().unwrap();

        println!("add 20");
        client.increase(20).await.unwrap();

        println!("add 45");
        client.increase(45).await.unwrap();

        let value = client.value().await.unwrap();
        println!("value: {}", value);
        assert_eq!(value, 65);
    };

    let (_, (counter_obj_opt, res)) = tokio::join!(client_task, server.serve());
    res.unwrap();
    assert!(counter_obj_opt.is_none());
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn simple_plus() {
    use remoc::rtc::Server;

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<CounterClient>().await;

    println!("Creating counter server");
    let counter_obj = CounterObj::new();
    let (server, client) = CounterServer::new(counter_obj);

    println!("Sending counter client");
    a_tx.send(client).await.unwrap();

    let client_task = async move {
        println!("Receiving counter client");
        let mut client = b_rx.recv().await.unwrap().unwrap();

        println!("add 20");
        client.increase(20).await.unwrap();

        println!("add 45");
        client.increase(45).await.unwrap();

        let value = client.value_plus(10).await.unwrap();
        println!("value: {}", value);
        assert_eq!(value, 75);
    };

    let (_, (counter_obj_opt, res)) = tokio::join!(client_task, server.serve());
    res.unwrap();
    assert!(counter_obj_opt.is_none());
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn simple_spawn() {
    use remoc::rtc::Server;

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<CounterClient>().await;

    println!("Spawning counter server");
    let counter_obj = CounterObj::new();
    let (server, client) = CounterServer::new(counter_obj);
    let server_task = wokio::spawn(async move {
        let (counter_obj_opt, res) = server.serve().await;
        res.unwrap();
        let counter_obj = counter_obj_opt.unwrap();
        println!("Server done");

        let value = counter_obj.value;
        println!("Counter obj value: {}", value);
        assert_eq!(value, 65);
    });

    println!("Sending counter client");
    a_tx.send(client).await.unwrap();

    println!("Receiving counter client");
    let mut client = b_rx.recv().await.unwrap().unwrap();

    println!("value: {}", client.value_ref().await.unwrap());
    assert_eq!(client.value_ref().await.unwrap(), 0);

    println!("add 20");
    client.increase(20).await.unwrap();
    println!("value: {}", client.value_ref().await.unwrap());
    assert_eq!(client.value_ref().await.unwrap(), 20);

    println!("add 45");
    client.increase(45).await.unwrap();
    println!("value: {}", client.value_ref().await.unwrap());
    assert_eq!(client.value_ref().await.unwrap(), 65);

    drop(client);
    println!("waiting for server to terminate");
    server_task.await.unwrap();
}
