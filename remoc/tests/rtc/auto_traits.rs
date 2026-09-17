//! The generated client, servers and request receiver are `Send` and `Sync`.

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use remoc::{codec, prelude::*, rtc::CallError};

#[rtc::remote]
pub trait Counter {
    async fn value(&self) -> Result<u32, CallError>;
}

pub struct CounterObj;

impl Counter for CounterObj {
    async fn value(&self) -> Result<u32, CallError> {
        Ok(0)
    }
}

fn assert_send_sync<T: Send + Sync>() {}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
fn send_sync() {
    assert_send_sync::<CounterClient<codec::Default>>();
    assert_send_sync::<CounterReqReceiver<codec::Default>>();
    assert_send_sync::<CounterServer<CounterObj, codec::Default>>();
    assert_send_sync::<CounterServerRef<CounterObj, codec::Default>>();
    assert_send_sync::<CounterServerRefMut<CounterObj, codec::Default>>();
    assert_send_sync::<CounterServerShared<CounterObj, codec::Default>>();
    assert_send_sync::<CounterServerSharedMut<CounterObj, codec::Default>>();
}
