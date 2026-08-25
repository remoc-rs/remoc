//! Chaining pipelined calls: a client hands over a request receiver through a client
//! that it has itself only just handed a request receiver to.

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use std::sync::Arc;

use tokio::sync::RwLock;

use remoc::{prelude::*, rtc::CallError};

use crate::loop_channel;

#[rtc::remote]
pub trait Leaf {
    async fn increase(&mut self, by: u32) -> Result<(), CallError>;
    async fn value(&self) -> Result<u32, CallError>;
}

pub struct LeafObj {
    value: u32,
}

impl Leaf for LeafObj {
    async fn increase(&mut self, by: u32) -> Result<(), CallError> {
        self.value += by;
        Ok(())
    }

    async fn value(&self) -> Result<u32, CallError> {
        Ok(self.value)
    }
}

#[rtc::remote]
pub trait Middle {
    async fn set(&mut self, value: u32) -> Result<(), CallError>;

    #[pipelinable]
    async fn open_leaf(&self) -> Result<LeafClient, CallError>;
}

pub struct MiddleObj {
    leaf: Arc<RwLock<LeafObj>>,
}

impl Middle for MiddleObj {
    async fn set(&mut self, value: u32) -> Result<(), CallError> {
        self.leaf.write().await.value = value;
        Ok(())
    }

    async fn open_leaf(&self) -> Result<LeafClient, CallError> {
        let (server, client) = LeafServerSharedMut::new(self.leaf.clone(), 1);
        wokio::spawn(server.serve(true));
        Ok(client)
    }
}

#[rtc::remote]
pub trait Root {
    #[pipelinable]
    async fn open_middle(&self) -> Result<MiddleClient, CallError>;
}

pub struct RootObj {
    middle: Arc<RwLock<MiddleObj>>,
}

impl Root for RootObj {
    async fn open_middle(&self) -> Result<MiddleClient, CallError> {
        let (server, client) = MiddleServerSharedMut::new(self.middle.clone(), 1);
        wokio::spawn(server.serve(true));
        Ok(client)
    }
}

/// Connects to a served root object on the other endpoint.
async fn root_client() -> RootClient {
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<RootClient>().await;

    let leaf = Arc::new(RwLock::new(LeafObj { value: 0 }));
    let middle = Arc::new(RwLock::new(MiddleObj { leaf }));
    let (server, client) = RootServerShared::new(Arc::new(RootObj { middle }), 1);
    wokio::spawn(server.serve(true));
    a_tx.send(client).await.unwrap();

    b_rx.recv().await.unwrap().unwrap()
}

/// Hands a request receiver to the root object, uses the resulting client to hand a
/// second request receiver over, and calls the object behind that one — all without
/// awaiting any result in between.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn chained_pipelines() -> Result<(), CallError> {
    crate::init();
    let root = root_client().await;

    let (mut middle, middle_rx) = MiddleClient::new(4);
    let (mut leaf, leaf_rx) = LeafClient::new(4);

    // `set` reaches the middle object before `open_leaf` does, and the leaf object is
    // only served once `open_leaf` has been handled, so `increase` sees the value set
    // above: the resulting value proves the ordering across all three levels.
    let value = rtc::calls!(
        root.open_middle_pipelined(middle_rx);
        middle.set_call(5);
        middle.open_leaf_pipelined(leaf_rx);
        leaf.increase_call(3);
        leaf.value_call()
    );

    assert_eq!(value, 8);

    Ok(())
}
