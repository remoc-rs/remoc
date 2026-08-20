// The lr channel is deprecated, but must keep working until it is removed.
#![allow(deprecated)]

use remoc::rch::lr;

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel;

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn send_sender() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<lr::Sender<i16>>().await;

    println!("Sending remote lr channel sender");
    let (tx, mut rx) = lr::channel();
    a_tx.send(tx).await.unwrap();
    println!("Receiving remote lr channel sender");
    let mut tx = b_rx.recv().await.unwrap().unwrap();

    for i in 1..1024 {
        println!("Sending {i}");
        tx.send(i).await.unwrap();
        let r = rx.recv().await.unwrap().unwrap();
        println!("Received {r}");
        assert_eq!(i, r, "send/receive mismatch");
    }

    println!("Verifying that channel is open");
    assert!(!tx.is_closed().await.unwrap());

    println!("Closing channel");
    rx.close().await;
    tx.closed().await.unwrap().await;
    assert!(tx.is_closed().await.unwrap());

    println!("Trying send after close");
    match tx.send(0).await {
        Ok(_) => panic!("send succeeded after close"),
        Err(err) if err.is_closed() => (),
        Err(_) => panic!("wrong error after close"),
    }
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn send_receiver() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<lr::Receiver<i16>>().await;

    println!("Sending remote lr channel receiver");
    let (mut tx, rx) = lr::channel();
    a_tx.send(rx).await.unwrap();
    println!("Receiving remote lr channel receiver");
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    for i in 1..1024 {
        println!("Sending {i}");
        tx.send(i).await.unwrap();
        let r = rx.recv().await.unwrap().unwrap();
        println!("Received {r}");
        assert_eq!(i, r, "send/receive mismatch");
    }

    println!("Verifying that channel is open");
    assert!(!tx.is_closed().await.unwrap());

    println!("Dropping receiver");
    drop(rx);
    tx.closed().await.unwrap().await;
    assert!(tx.is_closed().await.unwrap());

    println!("Trying send after receiver drop");
    match tx.send(0).await {
        Ok(_) => panic!("send succeeded after close"),
        Err(err) if err.is_disconnected() => (),
        Err(_) => panic!("wrong error after close"),
    }
}
