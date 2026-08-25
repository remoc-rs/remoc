#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel;

// Avoid imports here to test if proc macro works without imports.

#[remoc::rtc::remote]
pub trait DefaultTrait: Sync {
    async fn value(&self) -> Result<u32, remoc::rtc::CallError>;

    async fn default_method(&self) -> Result<u32, remoc::rtc::CallError> {
        let a = 1;
        let b = 2;
        Ok(a + b)
    }

    fn default_method2(&self) -> impl Future<Output = Result<u32, remoc::rtc::CallError>> + Send {
        let a = 2;
        let b = 4;
        Ok(a + b)
    }
}

pub struct CounterObj {
    value: u32,
}

impl CounterObj {
    pub fn new() -> Self {
        Self { value: 0 }
    }
}

impl DefaultTrait for CounterObj {
    async fn value(&self) -> Result<u32, remoc::rtc::CallError> {
        Ok(self.value)
    }
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn simple() {
    use remoc::rtc::ServerRefMut;

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<DefaultTraitClient>().await;

    println!("Creating default server");
    let mut counter_obj = CounterObj::new();
    let (server, client) = DefaultTraitServerRefMut::new(&mut counter_obj);

    println!("Sending default client");
    a_tx.send(client).await.unwrap();

    let client_task = async move {
        println!("Receiving default client");
        let client = b_rx.recv().await.unwrap().unwrap();

        println!("value: {}", client.value().await.unwrap());
        assert_eq!(client.value().await.unwrap(), 0);

        println!("default_method: {}", client.default_method().await.unwrap());
        assert_eq!(client.default_method().await.unwrap(), 3);

        println!("default_method2: {}", client.default_method2().await.unwrap());
        assert_eq!(client.default_method2().await.unwrap(), 6);
    };

    let ((), res) = tokio::join!(client_task, server.serve());
    res.unwrap();
}
