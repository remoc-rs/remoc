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

/// Serde attributes on trait methods are applied to the generated request enum
/// variants, while serde attributes on method arguments are applied to the
/// variant fields.
#[remoc::rtc::remote]
pub trait Counter {
    /// Method with renamed request variant.
    #[serde(rename = "_0")]
    async fn value(&self) -> Result<u32, remoc::rtc::CallError>;

    /// Method without renamed request variant.
    async fn watch(&mut self) -> Result<remoc::rch::watch::Receiver<u32>, remoc::rtc::CallError>;

    /// Method with renamed request variant and renamed fields.
    #[no_cancel]
    #[serde(rename = "_1")]
    async fn increase(
        &mut self, #[serde(rename = "_0")] by: u32,
        #[serde(rename = "_1")]
        #[serde(default)]
        twice: bool,
    ) -> Result<(), IncreaseError>;
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
    async fn value(&self) -> Result<u32, remoc::rtc::CallError> {
        Ok(self.value)
    }

    async fn watch(&mut self) -> Result<remoc::rch::watch::Receiver<u32>, remoc::rtc::CallError> {
        let (tx, rx) = remoc::rch::watch::channel(self.value);
        self.watchers.push(tx);
        Ok(rx)
    }

    async fn increase(&mut self, by: u32, twice: bool) -> Result<(), IncreaseError> {
        let by = if twice { by * 2 } else { by };

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
async fn rename() {
    use remoc::rtc::ServerRefMut;

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<CounterClient>().await;

    println!("Creating counter server");
    let mut counter_obj = CounterObj::new();
    let (server, client) = CounterServerRefMut::new(&mut counter_obj, 1);

    println!("Sending counter client");
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
        client.increase(20, false).await.unwrap();
        println!("value: {}", client.value().await.unwrap());
        assert_eq!(client.value().await.unwrap(), 20);

        println!("add 2 * 5");
        client.increase(5, true).await.unwrap();
        println!("value: {}", client.value().await.unwrap());
        assert_eq!(client.value().await.unwrap(), 30);
    };

    let ((), res) = tokio::join!(client_task, server.serve());
    res.unwrap();

    println!("Counter obj value: {}", counter_obj.value);
    assert_eq!(counter_obj.value, 30);
}
