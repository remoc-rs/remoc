//! Tentative acceptance of pre-connected connection requests.

use std::time::Duration;
use wokio::time::timeout;

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use super::{cfg, connected, connected_with_cfg, spawn_forwarder};
use remoc::chmux::{self, OnPortsExhausted, Received, RecvError, SendError, TentativeAcceptError};

/// Time an exchange is given before the test fails.
const LIMIT: Duration = Duration::from_secs(60);

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

/// A forwarder that runs out of ports rejects the port it cannot forward, but keeps
/// forwarding the channel that carried the request.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn forwarded_port_exhaustion_keeps_channel_alive() {
    crate::init();

    // A <-> B <-> C, where the connection between A and B must allow enough concurrent
    // connect requests, while B has no port left for the connection to C once the
    // channel that is forwarded has been established.
    const PORTS: usize = 4;
    let wide_cfg = chmux::Cfg { connect_queue: PORTS as u16 * 2, ..cfg() };
    let (a_client, mut b_a_server) = connected_with_cfg(wide_cfg.clone(), wide_cfg).await;
    let b_cfg = chmux::Cfg { max_ports: 1, connect_queue: 1, ports_exhausted: OnPortsExhausted::Fail, ..cfg() };
    let (b_c_client, mut c_server) = connected_with_cfg(b_cfg, cfg()).await;

    let (mut a_tx, mut a_rx) = a_client.connect_port().await.unwrap();
    let b_a = b_a_server.accept().await.unwrap().unwrap();
    let b_c = b_c_client.connect_port().await.unwrap();
    spawn_forwarder(b_a, b_c);
    let (mut c_tx, mut c_rx) = c_server.accept().await.unwrap().unwrap();

    // Request ports that the forwarder has no local port for.
    let mut reqs = Vec::new();
    for _ in 0..PORTS {
        reqs.push(a_tx.connect_req().unwrap().pre_connect().await);
    }
    let connects = a_tx.connect(reqs).await.unwrap();

    // Each of them is reported as rejected for lack of ports.
    for connect in connects {
        let (_sub_tx, mut sub_rx) = connect.await.unwrap();
        match sub_rx.recv().await {
            Err(RecvError::Rejected { no_ports: true }) => (),
            other => panic!("unexpected receive result: {other:?}"),
        }
    }

    // The channel that carried the requests keeps working in both directions.
    a_tx.send("still forwarding".into()).await.unwrap();
    assert_eq!(Vec::from(c_rx.recv().await.unwrap().unwrap()), b"still forwarding");
    c_tx.send("in both directions".into()).await.unwrap();
    assert_eq!(Vec::from(a_rx.recv().await.unwrap().unwrap()), b"in both directions");
}

/// A pre-connected port that is sent over a port arrives pre-connected, also when it
/// carries an id that differs from its port number.
///
/// The id and the pre-connect flag are encoded independently of each other, so a
/// differing id must not affect whether the port is pre-connected.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn pre_connect_survives_id() {
    crate::init();
    let (a_client, mut b_server) = connected().await;

    let (mut a_tx, _a_rx) = timeout(LIMIT, a_client.connect_port()).await.unwrap().unwrap();
    let (_b_tx, mut b_rx) = timeout(LIMIT, b_server.accept()).await.unwrap().unwrap().unwrap();

    let req = timeout(LIMIT, a_tx.connect_req().unwrap().with_id(4711).pre_connect()).await.unwrap();
    assert!(req.is_pre_connected(), "port was not pre-connected");
    let _connect = timeout(LIMIT, a_tx.connect(vec![req])).await.unwrap().unwrap().remove(0);

    match timeout(LIMIT, b_rx.recv_any()).await.unwrap().unwrap() {
        Some(Received::Requests(reqs)) => {
            assert_eq!(reqs[0].id(), 4711, "the id was lost");
            assert!(reqs[0].is_pre_connected(), "the pre-connect flag was lost in transit");
        }
        other => panic!("unexpected receive result: {other:?}"),
    }
}

/// Only the ports that are pre-connected arrive pre-connected, when a batch of ports
/// mixes pre-connected and normal ones.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn pre_connect_of_mixed_ports() {
    crate::init();
    let (a_client, mut b_server) = connected().await;

    let (mut a_tx, _a_rx) = timeout(LIMIT, a_client.connect_port()).await.unwrap().unwrap();
    let (_b_tx, mut b_rx) = timeout(LIMIT, b_server.accept()).await.unwrap().unwrap().unwrap();

    // Every second port is pre-connected and all of them carry a custom id.
    let mut reqs = Vec::new();
    for i in 0..4u32 {
        let req = a_tx.connect_req().unwrap().with_id(100 + i);
        let req = if i % 2 == 0 { timeout(LIMIT, req.pre_connect()).await.unwrap() } else { req };
        assert_eq!(req.is_pre_connected(), i % 2 == 0);
        reqs.push(req);
    }
    let _connects = timeout(LIMIT, a_tx.connect(reqs)).await.unwrap().unwrap();

    match timeout(LIMIT, b_rx.recv_any()).await.unwrap().unwrap() {
        Some(Received::Requests(reqs)) => {
            assert_eq!(reqs.len(), 4, "not all ports arrived");
            for (i, req) in reqs.iter().enumerate() {
                assert_eq!(req.id(), 100 + i as u32, "the id of port {i} was lost");
                assert_eq!(
                    req.is_pre_connected(),
                    i % 2 == 0,
                    "port {i} arrived with the wrong pre-connect state"
                );
            }
        }
        other => panic!("unexpected receive result: {other:?}"),
    }
}
