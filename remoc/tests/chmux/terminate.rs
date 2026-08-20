//! Forced termination of a multiplexer.

use futures::{StreamExt, channel::oneshot, future::try_join};
use std::time::Duration;
use wokio::time::{sleep, timeout};

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use super::cfg;
use crate::loop_transport;
use remoc::chmux::{self, Client, Listener};

/// An endpoint together with a handle that completes once its multiplexer stopped.
struct Endpoint {
    client: Client,
    listener: Listener,
    stopped: oneshot::Receiver<()>,
}

/// Connects two multiplexers and reports when each of them stops running.
async fn connected() -> (Endpoint, Endpoint) {
    loop_transport!(0, a_tx, a_rx, b_tx, b_rx);
    let ((a_mux, a_client, a_listener), (b_mux, b_client, b_listener)) =
        try_join(chmux::ChMux::new(cfg(), a_tx, a_rx), chmux::ChMux::new(cfg(), b_tx, b_rx)).await.unwrap();

    let (a_stopped_tx, a_stopped) = oneshot::channel();
    wokio::spawn(async move {
        let _ = a_mux.run().await;
        let _ = a_stopped_tx.send(());
    });

    let (b_stopped_tx, b_stopped) = oneshot::channel();
    wokio::spawn(async move {
        let _ = b_mux.run().await;
        let _ = b_stopped_tx.send(());
    });

    (
        Endpoint { client: a_client, listener: a_listener, stopped: a_stopped },
        Endpoint { client: b_client, listener: b_listener, stopped: b_stopped },
    )
}

/// Terminating stops the local multiplexer and is noticed by the remote endpoint.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn terminate_stops_both_multiplexers() {
    crate::init();
    let (a, b) = connected().await;

    a.client.terminate();

    timeout(Duration::from_secs(5), a.stopped).await.expect("terminated multiplexer kept running").unwrap();
    timeout(Duration::from_secs(5), b.stopped)
        .await
        .expect("remote multiplexer did not notice the termination")
        .unwrap();
}

/// Ports that are open when the multiplexer is terminated report the termination
/// at both endpoints, instead of waiting for data that cannot arrive.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn terminate_fails_open_ports() {
    crate::init();
    let (a, mut b) = connected().await;

    let (mut a_tx, mut a_rx) = a.client.connect_port().await.unwrap();
    let (mut b_tx, mut b_rx) = b.listener.accept().await.unwrap().unwrap();
    a_tx.send("hello".into()).await.unwrap();
    assert_eq!(Vec::from(b_rx.recv().await.unwrap().unwrap()), b"hello");

    println!("Terminating");
    a.client.terminate();

    // Errors are reported asynchronously, thus a send that was already accepted for
    // transfer may still succeed; the termination must be reported soon afterwards.
    let a_send = timeout(Duration::from_secs(5), send_until_error(&mut a_tx))
        .await
        .expect("sending on a terminated multiplexer never reported the termination");
    println!("send at terminating endpoint: {a_send:?}");

    let b_send = timeout(Duration::from_secs(5), send_until_error(&mut b_tx))
        .await
        .expect("sending at the remote endpoint never reported the termination");
    println!("send at remote endpoint: {b_send:?}");

    // Receiving must not wait for data that cannot arrive any more.
    let a_recv = timeout(Duration::from_secs(5), a_rx.recv())
        .await
        .expect("receiving on a terminated multiplexer did not return");
    println!("receive at terminating endpoint: {a_recv:?}");
    assert!(a_recv.is_err(), "receiving reported a clean end although the multiplexer was terminated");

    // Data that arrived before the termination is still delivered, but afterwards the
    // termination must be reported instead of a clean end of the channel.
    let b_recv = timeout(Duration::from_secs(5), recv_until_error(&mut b_rx))
        .await
        .expect("receiving at the remote endpoint never reported the termination");
    println!("receive at remote endpoint: {b_recv:?}");
}

/// Receives until receiving reports an error and returns it.
async fn recv_until_error(rx: &mut chmux::Receiver) -> chmux::RecvError {
    loop {
        match rx.recv().await {
            Ok(Some(_)) => (),
            Ok(None) => panic!("receiving reported a clean end although the multiplexer was terminated"),
            Err(err) => return err,
        }
    }
}

/// Sends until sending reports an error and returns it.
async fn send_until_error(tx: &mut chmux::Sender) -> chmux::SendError {
    loop {
        match tx.send("after".into()).await {
            Ok(()) => sleep(Duration::from_millis(10)).await,
            Err(err) => return err,
        }
    }
}

/// No new port can be opened after terminating.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn terminate_prevents_new_ports() {
    crate::init();
    let (a, _b) = connected().await;

    a.client.terminate();

    let res = timeout(Duration::from_secs(5), a.client.connect_port())
        .await
        .expect("connecting on a terminated multiplexer did not return");
    println!("connect after terminate: {res:?}");
    assert!(res.is_err(), "connecting succeeded on a terminated multiplexer");
}

/// A listener waiting for connections stops waiting when the multiplexer is terminated.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn terminate_ends_pending_accept() {
    crate::init();
    let (a, mut b) = connected().await;

    let accept = wokio::spawn(async move { b.listener.accept().await });
    a.client.terminate();

    let res = timeout(Duration::from_secs(5), accept)
        .await
        .expect("accepting did not return after the connection was terminated")
        .unwrap();
    println!("accept after terminate: {res:?}");
}
