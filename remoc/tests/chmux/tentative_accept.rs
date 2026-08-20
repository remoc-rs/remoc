//! Tentative acceptance of pre-connected connection requests.

use futures::{future::try_join, stream::StreamExt};
use std::time::Duration;

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_transport;
use remoc::chmux::{self, Client, Listener, Received, RecvError, SendError, TentativeAcceptError};

fn cfg() -> chmux::Cfg {
    chmux::Cfg { connection_timeout: Some(Duration::from_secs(1)), connect_queue: 2, ..Default::default() }
}

/// Connects two multiplexers over an in-memory transport and runs them.
async fn connected() -> (Client, Listener) {
    loop_transport!(0, a_tx, a_rx, b_tx, b_rx);
    let ((a_mux, a_client, _a_server), (b_mux, _b_client, b_server)) =
        try_join(chmux::ChMux::new(cfg(), a_tx, a_rx), chmux::ChMux::new(cfg(), b_tx, b_rx)).await.unwrap();

    wokio::spawn(async move {
        let _ = a_mux.run().await;
    });
    wokio::spawn(async move {
        let _ = b_mux.run().await;
    });

    (a_client, b_server)
}

/// A tentatively accepted port that is rejected afterwards is reported as rejected
/// to the requester, even though data has already been exchanged over it.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn reject_after_tentative_accept() {
    crate::init();
    let (a_client, mut b_server) = connected().await;

    // Pre-connecting allows sending data before the request has been answered.
    let req = a_client.connect_req().unwrap().pre_connect().await;
    assert!(req.is_pre_connected(), "port was not pre-connected");
    let (mut a_tx, mut a_rx) = a_client.connect(req).await.unwrap();
    a_tx.send("hello".into()).await.unwrap();

    let b_req = b_server.inspect().await.unwrap().unwrap();
    assert!(b_req.is_pre_connected());
    let (b_tx, mut b_rx, guard) = b_req.accept_tentatively().await.unwrap();

    // Data sent before the decision arrives at the tentatively accepted port.
    let msg = b_rx.recv().await.unwrap().unwrap();
    assert_eq!(Vec::from(msg), b"hello");

    // Reject the connection after having accepted it and discard the port.
    guard.reject(false).await;
    drop(b_tx);
    drop(b_rx);

    // The requester learns why the port went away.
    match a_rx.recv().await {
        Err(RecvError::Rejected { no_ports: false }) => (),
        other => panic!("unexpected receive result: {other:?}"),
    }
    loop {
        match a_tx.send("more".into()).await {
            Err(SendError::Rejected { no_ports: false }) => break,
            Ok(()) => continue,
            other => panic!("unexpected send result: {other:?}"),
        }
    }
}

/// A rejection reports whether the remote endpoint ran out of ports.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn reject_after_tentative_accept_no_ports() {
    crate::init();
    let (a_client, mut b_server) = connected().await;

    let req = a_client.connect_req().unwrap().pre_connect().await;
    let (_a_tx, mut a_rx) = a_client.connect(req).await.unwrap();

    let b_req = b_server.inspect().await.unwrap().unwrap();
    let (b_tx, b_rx, guard) = b_req.accept_tentatively().await.unwrap();
    guard.reject(true).await;
    drop(b_tx);
    drop(b_rx);

    match a_rx.recv().await {
        Err(RecvError::Rejected { no_ports: true }) => (),
        other => panic!("unexpected receive result: {other:?}"),
    }
}

/// A tentatively accepted port that is confirmed works like a normally accepted one
/// and ends without error.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn confirm_after_tentative_accept() {
    crate::init();
    let (a_client, mut b_server) = connected().await;

    let req = a_client.connect_req().unwrap().pre_connect().await;
    let (mut a_tx, mut a_rx) = a_client.connect(req).await.unwrap();
    a_tx.send("ping".into()).await.unwrap();

    let b_req = b_server.inspect().await.unwrap().unwrap();
    let (mut b_tx, mut b_rx, guard) = b_req.accept_tentatively().await.unwrap();
    guard.accept();

    assert_eq!(Vec::from(b_rx.recv().await.unwrap().unwrap()), b"ping");
    b_tx.send("pong".into()).await.unwrap();
    assert_eq!(Vec::from(a_rx.recv().await.unwrap().unwrap()), b"pong");

    // Closing the confirmed port is not reported as a rejection.
    drop(b_tx);
    drop(b_rx);
    assert!(a_rx.recv().await.unwrap().is_none(), "confirmed port reported an error");
}

/// A request that was not pre-connected cannot be accepted tentatively, since its
/// requester is still waiting for the accept/reject decision.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn tentative_accept_requires_pre_connect() {
    crate::init();
    let (a_client, mut b_server) = connected().await;

    let req = a_client.connect_req().unwrap();
    assert!(!req.is_pre_connected());
    let connect = a_client.connect(req);

    let b_req = b_server.inspect().await.unwrap().unwrap();
    assert!(!b_req.is_pre_connected());
    match b_req.accept_tentatively().await {
        Err(TentativeAcceptError::NotPreConnected) => (),
        Ok(_) => panic!("tentatively accepted a request that was not pre-connected"),
        Err(err) => panic!("unexpected error: {err:?}"),
    }

    // The request is rejected when it is dropped.
    assert!(matches!(connect.await, Err(chmux::ConnectError::Rejected)));
}

/// Forwards a channel between two multiplexers, so that ports sent over it are
/// forwarded as well.
fn spawn_forwarder(
    (mut in_tx, mut in_rx): (chmux::Sender, chmux::Receiver),
    (mut out_tx, mut out_rx): (chmux::Sender, chmux::Receiver),
) {
    wokio::spawn(async move {
        let _ = in_rx.forward(&mut out_tx).await;
    });
    wokio::spawn(async move {
        let _ = out_rx.forward(&mut in_tx).await;
    });
}

/// A port that is rejected by the final endpoint is reported as rejected to the
/// originator, even when it was pre-connected through a forwarder.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn forwarded_port_rejection_reaches_originator() {
    crate::init();

    // A <-> B <-> C, where B forwards between A and C.
    let (a_client, mut b_a_server) = connected().await;
    let (b_c_client, mut c_server) = connected().await;

    let (a_tx, a_rx) = a_client.connect_port().await.unwrap();
    let b_a = b_a_server.accept().await.unwrap().unwrap();
    let b_c = b_c_client.connect_port().await.unwrap();
    spawn_forwarder(b_a, b_c);
    let (_c_tx, mut c_rx) = c_server.accept().await.unwrap().unwrap();

    // Open a pre-connected port through the forwarder.
    let mut a_tx = a_tx;
    let req = a_tx.connect_req().unwrap().pre_connect().await;
    assert!(req.is_pre_connected(), "port was not pre-connected");
    let connect = a_tx.connect(vec![req]).await.unwrap().remove(0);
    let (_sub_tx, mut sub_rx) = connect.await.unwrap();

    // The final endpoint rejects it.
    match c_rx.recv_any().await.unwrap() {
        Some(Received::Requests(mut reqs)) => {
            assert_eq!(reqs.len(), 1);
            let req = reqs.remove(0);
            assert!(req.is_pre_connected(), "forwarded port lost its pre-connection");
            req.reject(true).await;
        }
        other => panic!("unexpected receive result: {other:?}"),
    }

    // The rejection, including its reason, arrives at the originator.
    match sub_rx.recv().await {
        Err(RecvError::Rejected { no_ports: true }) => (),
        other => panic!("unexpected receive result: {other:?}"),
    }

    drop(a_rx);
}

/// The rejection of a forwarded port must reach the originator before the port is
/// closed, also when the originator has already given up on the port.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn forwarded_port_rejection_after_originator_gave_up() {
    crate::init();

    let (a_client, mut b_a_server) = connected().await;
    let (b_c_client, mut c_server) = connected().await;

    let (mut a_tx, a_rx) = a_client.connect_port().await.unwrap();
    let b_a = b_a_server.accept().await.unwrap().unwrap();
    let b_c = b_c_client.connect_port().await.unwrap();
    spawn_forwarder(b_a, b_c);
    let (_c_tx, mut c_rx) = c_server.accept().await.unwrap().unwrap();

    let req = a_tx.connect_req().unwrap().pre_connect().await;
    let connect = a_tx.connect(vec![req]).await.unwrap().remove(0);
    let (sub_tx, sub_rx) = connect.await.unwrap();

    // The originator loses interest in the port before the rejection arrives.
    drop(sub_tx);
    drop(sub_rx);

    match c_rx.recv_any().await.unwrap() {
        Some(Received::Requests(mut reqs)) => reqs.remove(0).reject(false).await,
        other => panic!("unexpected receive result: {other:?}"),
    }

    // The rejection must not arrive for a port that has already been freed, since
    // that terminates the whole multiplexer connection.
    wokio::time::sleep(Duration::from_millis(200)).await;
    a_tx.send("still alive".into()).await.unwrap();
    drop(a_rx);
}
