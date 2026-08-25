#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel;

// Avoid imports here to test if proc macro works without imports.

/// Serde attributes that take a value but are not interpreted by remoc must be
/// passed through to the generated request enum.
#[remoc::rtc::remote]
pub trait Storage {
    /// Method with a serde attribute taking a value on an argument.
    async fn store(&mut self, #[serde(with = "serde_bytes")] data: Vec<u8>) -> Result<(), remoc::rtc::CallError>;

    /// Method with a serde attribute taking a value on the method itself.
    #[serde(rename_all = "camelCase")]
    async fn load(&self, offset: usize) -> Result<Vec<u8>, remoc::rtc::CallError>;
}

pub struct StorageObj {
    data: Vec<u8>,
}

impl StorageObj {
    pub fn new() -> Self {
        Self { data: Vec::new() }
    }
}

impl Storage for StorageObj {
    async fn store(&mut self, data: Vec<u8>) -> Result<(), remoc::rtc::CallError> {
        self.data.extend_from_slice(&data);
        Ok(())
    }

    async fn load(&self, offset: usize) -> Result<Vec<u8>, remoc::rtc::CallError> {
        Ok(self.data[offset..].to_vec())
    }
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn serde_with() {
    use remoc::rtc::ServerRefMut;

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<StorageClient>().await;

    println!("Creating storage server");
    let mut storage_obj = StorageObj::new();
    let (server, client) = StorageServerRefMut::new(&mut storage_obj);

    println!("Sending storage client");
    a_tx.send(client).await.unwrap();

    let client_task = async move {
        println!("Receiving storage client");
        let mut client = b_rx.recv().await.unwrap().unwrap();

        println!("Storing data");
        client.store(vec![1, 2, 3]).await.unwrap();
        client.store(vec![4, 5]).await.unwrap();

        println!("Loading data");
        assert_eq!(client.load(0).await.unwrap(), vec![1, 2, 3, 4, 5]);
        assert_eq!(client.load(3).await.unwrap(), vec![4, 5]);
    };

    let ((), res) = tokio::join!(client_task, server.serve());
    res.unwrap();

    println!("Storage obj data: {:?}", storage_obj.data);
    assert_eq!(storage_obj.data, vec![1, 2, 3, 4, 5]);
}
