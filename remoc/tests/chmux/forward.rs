//! Forwarding of data and ports between two connections.

use std::time::Duration;
use wokio::time::timeout;

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use super::{connected, spawn_forwarder};
use remoc::chmux::{self, Received};

/// Sets up A <-> B <-> C, where B forwards a channel between both connections,
/// and returns the ports of A and C.
async fn forwarded() -> ((chmux::Sender, chmux::Receiver), (chmux::Sender, chmux::Receiver)) {
    let (a_client, mut b_a_server) = connected().await;
    let (b_c_client, mut c_server) = connected().await;

    let a = a_client.connect_port().await.unwrap();
    let b_a = b_a_server.accept().await.unwrap().unwrap();
    let b_c = b_c_client.connect_port().await.unwrap();
    spawn_forwarder(b_a, b_c);
    let c = c_server.accept().await.unwrap().unwrap();

    (a, c)
}

/// Data is forwarded unchanged in both directions, including messages that exceed
/// the chunk size of the forwarder and thus are transferred in chunks.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn data_survives_forwarding() {
    crate::init();
    let ((mut a_tx, mut a_rx), (mut c_tx, mut c_rx)) = forwarded().await;

    for size in [1usize, 100, 100_000] {
        let sent: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();

        a_tx.send(sent.clone().into()).await.unwrap();
        let received = Vec::from(c_rx.recv().await.unwrap().unwrap());
        assert_eq!(received, sent, "data of size {size} was altered while forwarded from A to C");

        c_tx.send(sent.clone().into()).await.unwrap();
        let received = Vec::from(a_rx.recv().await.unwrap().unwrap());
        assert_eq!(received, sent, "data of size {size} was altered while forwarded from C to A");
    }
}

/// A port sent over a forwarded channel is forwarded as well and usable in both
/// directions, and so is a port sent over that forwarded port.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn nested_ports_are_forwarded() {
    crate::init();
    let ((mut a_tx, _a_rx), (_c_tx, mut c_rx)) = forwarded().await;

    println!("Opening a port through the forwarder");
    let req = a_tx.connect_req().unwrap();
    let connect = a_tx.connect(vec![req]).await.unwrap().remove(0);
    let accept = async {
        match c_rx.recv_any().await.unwrap() {
            Some(Received::Requests(mut reqs)) => reqs.remove(0).accept().await.unwrap(),
            other => panic!("unexpected receive result: {other:?}"),
        }
    };
    let ((mut a_sub_tx, mut a_sub_rx), (mut c_sub_tx, mut c_sub_rx)) =
        futures::future::join(async { connect.await.unwrap() }, accept).await;

    println!("Using the forwarded port in both directions");
    a_sub_tx.send("through the port".into()).await.unwrap();
    assert_eq!(Vec::from(c_sub_rx.recv().await.unwrap().unwrap()), b"through the port");
    c_sub_tx.send("and back".into()).await.unwrap();
    assert_eq!(Vec::from(a_sub_rx.recv().await.unwrap().unwrap()), b"and back");

    println!("Opening a port through the forwarded port");
    let req = a_sub_tx.connect_req().unwrap();
    let connect = a_sub_tx.connect(vec![req]).await.unwrap().remove(0);
    let accept = async {
        match c_sub_rx.recv_any().await.unwrap() {
            Some(Received::Requests(mut reqs)) => reqs.remove(0).accept().await.unwrap(),
            other => panic!("unexpected receive result: {other:?}"),
        }
    };
    let ((mut a_nested_tx, _), (_, mut c_nested_rx)) =
        futures::future::join(async { connect.await.unwrap() }, accept).await;

    a_nested_tx.send("nested".into()).await.unwrap();
    assert_eq!(Vec::from(c_nested_rx.recv().await.unwrap().unwrap()), b"nested");
}

/// Closing a forwarded channel at one end is reported at the other end, rather than
/// leaving it waiting for data that will never arrive.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn closure_is_forwarded() {
    crate::init();
    let ((mut a_tx, _a_rx), (_c_tx, mut c_rx)) = forwarded().await;

    a_tx.send("last message".into()).await.unwrap();
    drop(a_tx);

    assert_eq!(Vec::from(c_rx.recv().await.unwrap().unwrap()), b"last message");
    assert!(c_rx.recv().await.unwrap().is_none(), "closure was not forwarded");
}

/// Sending to a forwarder whose onward connection is gone reports the failure instead
/// of accepting data that can never arrive.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn broken_onward_connection_is_reported() {
    crate::init();
    let ((mut a_tx, _a_rx), (c_tx, c_rx)) = forwarded().await;

    println!("Dropping the final endpoint");
    drop(c_tx);
    drop(c_rx);

    println!("Sending until the failure is reported");
    let deadline = 1000;
    for i in 0..deadline {
        match a_tx.send(vec![0; 1024].into()).await {
            Ok(()) => (),
            Err(err) => {
                assert!(err.is_disconnected(), "unexpected error: {err:?}");
                return;
            }
        }
        if i + 1 == deadline {
            panic!("sending into a broken forwarding chain never failed");
        }
        wokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// A pre-connected port stays pre-connected while it is forwarded, also when the
/// forwarder allocates a port number that differs from the id it passes on.
///
/// The forwarder keeps the id of the incoming request but allocates a fresh port for
/// the outgoing one, so both only coincide as long as the two connections hand out
/// the same port numbers.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn pre_connect_survives_forwarding() {
    crate::init();
    let limit = Duration::from_secs(60);

    let (a_client, mut b_a_server) = connected().await;
    let (b_c_client, mut c_server) = connected().await;

    let (mut a_tx, _a_rx) = timeout(limit, a_client.connect_port()).await.unwrap().unwrap();
    let b_a = timeout(limit, b_a_server.accept()).await.unwrap().unwrap().unwrap();
    let b_c = timeout(limit, b_c_client.connect_port()).await.unwrap().unwrap();
    let (_c_tx, mut c_rx) = timeout(limit, c_server.accept()).await.unwrap().unwrap().unwrap();

    // Keep ports open on the B <-> C connection, so that the port numbers of B
    // diverge from those of A.
    let mut held = Vec::new();
    for _ in 0..5 {
        let b = timeout(limit, b_c_client.connect_port()).await.unwrap().unwrap();
        let c = timeout(limit, c_server.accept()).await.unwrap().unwrap().unwrap();
        held.push((b, c));
    }

    spawn_forwarder(b_a, b_c);

    let req = timeout(limit, a_tx.connect_req().unwrap().pre_connect()).await.unwrap();
    assert!(req.is_pre_connected(), "port was not pre-connected");
    let _connect = timeout(limit, a_tx.connect(vec![req])).await.unwrap().unwrap().remove(0);

    match timeout(limit, c_rx.recv_any()).await.unwrap().unwrap() {
        Some(Received::Requests(reqs)) => {
            assert!(reqs[0].is_pre_connected(), "the pre-connect flag was lost while forwarding");
        }
        other => panic!("unexpected receive result: {other:?}"),
    }
}
