use remoc::rch::base::{StorageRef, storage_ref};
use serde::{Deserialize, Serialize};

#[cfg(feature = "js")]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel;

#[derive(Clone, Debug, PartialEq)]
struct SenderMark(u32);

#[derive(Clone, Debug, PartialEq)]
struct ReceiverMark(u32);

#[derive(Serialize, Deserialize)]
struct Msg {
    storage_ref: StorageRef,
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn both_endpoints() {
    crate::init();

    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<Msg>().await;

    println!("Sending storage reference");
    let (storage_ref, handle) = storage_ref();
    assert!(storage_ref.get().is_none(), "storage is unknown before sending");
    a_tx.send(Msg { storage_ref }).await.unwrap();

    println!("Sender obtains storage used for sending");
    let sender_storage = handle.await.unwrap();
    sender_storage.insert(SenderMark(1));
    assert_eq!(a_tx.storage().get::<SenderMark>(), Some(SenderMark(1)));

    println!("Receiver obtains storage used for receiving");
    let msg = b_rx.recv().await.unwrap().unwrap();
    let receiver_storage = msg.storage_ref.get().unwrap();
    receiver_storage.insert(ReceiverMark(2));
    assert_eq!(b_rx.storage().get::<ReceiverMark>(), Some(ReceiverMark(2)));

    println!("Each endpoint has its own storage");
    assert_eq!(receiver_storage.get::<SenderMark>(), None);
    assert_eq!(sender_storage.get::<ReceiverMark>(), None);
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn dropped_without_sending() {
    crate::init();

    println!("Dropping storage reference without sending it");
    let (storage_ref, handle) = storage_ref();
    drop(storage_ref);

    assert!(handle.await.is_none(), "handle must resolve when reference is dropped");
}

#[derive(Serialize, Deserialize)]
struct LibClient {
    storage_ref: StorageRef,
    req_tx: remoc::rch::mpsc::Sender<u32>,
}

/// A library is handed a channel by the application and has no access to the
/// connection itself, but still obtains its storage.
#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn over_nested_channel() {
    crate::init();

    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<LibClient>().await;

    let (req_tx, mut req_rx) = remoc::rch::mpsc::channel(1);
    let (storage_ref, handle) = storage_ref();
    a_tx.send(LibClient { storage_ref, req_tx }).await.unwrap();

    let sender_storage = handle.await.unwrap();
    sender_storage.insert(SenderMark(7));

    let client = b_rx.recv().await.unwrap().unwrap();
    let receiver_storage = client.storage_ref.get().unwrap();
    receiver_storage.insert(ReceiverMark(8));

    println!("Library uses its channel as usual");
    client.req_tx.send(42).await.unwrap();
    assert_eq!(req_rx.recv().await.unwrap(), Some(42));

    assert_eq!(a_tx.storage().get::<SenderMark>(), Some(SenderMark(7)));
    assert_eq!(b_rx.storage().get::<ReceiverMark>(), Some(ReceiverMark(8)));
}

/// Only the receiving endpoint requires the storage.
#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn without_handle() {
    crate::init();

    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<Msg>().await;

    a_tx.send(Msg { storage_ref: StorageRef::new() }).await.unwrap();

    let msg = b_rx.recv().await.unwrap().unwrap();
    let storage = msg.storage_ref.get().unwrap();
    storage.insert(ReceiverMark(3));
    assert_eq!(b_rx.storage().get::<ReceiverMark>(), Some(ReceiverMark(3)));
}
