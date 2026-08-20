use remoc::rch::oneshot;

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel;

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn simple() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<(oneshot::Sender<i16>, oneshot::Receiver<i16>)>().await;

    println!("Sending remote oneshot channel sender and receiver");
    let (tx, rx) = oneshot::channel();
    a_tx.send((tx, rx)).await.unwrap();
    println!("Receiving remote oneshot channel sender and receiver");
    let (tx, rx) = b_rx.recv().await.unwrap().unwrap();

    let i = 512;
    println!("Sending {i}");
    let mut sending = tx.send(i).unwrap();

    let r = rx.await.unwrap();
    println!("Received {r}");
    assert_eq!(i, r, "send/receive mismatch");

    sending.try_result().unwrap().unwrap();
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn close() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<oneshot::Sender<i16>>().await;

    println!("Sending remote oneshot channel sender");
    let (tx, mut rx) = oneshot::channel();
    a_tx.send(tx).await.unwrap();
    println!("Receiving remote oneshot channel sender");
    let tx = b_rx.recv().await.unwrap().unwrap();

    assert!(!tx.is_closed());

    println!("Closing receiver");
    rx.close();

    println!("Waiting for close notification");
    tx.closed().await;

    match tx.send(0) {
        Ok(_) => panic!("send after close succeeded"),
        Err(err) if err.is_closed() => (),
        Err(err) => panic!("wrong error after close: {err}"),
    }
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn forward_local() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<oneshot::Receiver<i16>>().await;

    let (local_tx, local_rx) = tokio::sync::oneshot::channel::<i16>();

    println!("Forwarding local oneshot receiver over remote channel");
    let (forward, rx) = oneshot::forward(local_rx);
    a_tx.send(rx).await.unwrap();
    let rx = b_rx.recv().await.unwrap().unwrap();

    let i = 1234;
    local_tx.send(i).unwrap();

    let r = rx.await.unwrap();
    println!("Received {r}");
    assert_eq!(i, r, "forwarded value mismatch");

    println!("Waiting for forward task");
    forward.await.unwrap();
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn forwarded_local() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<oneshot::Receiver<i16>>().await;

    let (local_tx, local_rx) = tokio::sync::oneshot::channel::<i16>();

    println!("Forwarding local oneshot receiver via Receiver::forwarded");
    let rx = oneshot::Receiver::forwarded(local_rx);
    a_tx.send(rx).await.unwrap();
    let rx = b_rx.recv().await.unwrap().unwrap();

    let i = 4321;
    local_tx.send(i).unwrap();

    let r = rx.await.unwrap();
    assert_eq!(i, r, "forwarded value mismatch");
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn forward_local_dropped() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<oneshot::Receiver<i16>>().await;

    let (local_tx, local_rx) = tokio::sync::oneshot::channel::<i16>();

    println!("Forwarding local oneshot receiver, then dropping local sender");
    let (forward, rx) = oneshot::forward(local_rx);
    a_tx.send(rx).await.unwrap();
    let rx = b_rx.recv().await.unwrap().unwrap();

    drop(local_tx);

    match rx.await {
        Ok(_) => panic!("expected receive to fail after local sender dropped"),
        Err(err) => println!("Got expected error: {err}"),
    }

    forward.await.unwrap();
}

/// Classification of send errors into the reason why the channel was closed.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn closed_reason_of_send_errors() {
    use remoc::rch::{ClosedReason, SendErrorExt, oneshot::SendError};

    assert_eq!(SendError::Closed(1i16).closed_reason(), Some(ClosedReason::Closed));
    assert_eq!(SendError::<i16>::Dropped.closed_reason(), Some(ClosedReason::Dropped));
    assert_eq!(SendError::<i16>::Failed.closed_reason(), Some(ClosedReason::Failed));

    // The reason for closure determines what the predicates report.
    for err in [SendError::Closed(1i16), SendError::Dropped, SendError::Failed] {
        assert_eq!(
            SendErrorExt::is_closed(&err),
            matches!(err.closed_reason(), Some(ClosedReason::Closed | ClosedReason::Dropped)),
            "is_closed does not follow closed_reason for {err:?}"
        );
    }
}

/// Sending to a receiver that the remote endpoint dropped reports that it was
/// dropped, rather than an unspecified failure.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn send_to_dropped_receiver_reports_dropped() {
    crate::init();
    let ((mut a_tx, _a_rx), (_b_tx, mut b_rx)) = loop_channel::<oneshot::Receiver<i16>>().await;

    println!("Sending the receiver to the remote endpoint");
    let (tx, rx) = oneshot::channel();
    a_tx.send(rx).await.unwrap();

    println!("Taking the receiver over and dropping it");
    let remote_rx = b_rx.recv().await.unwrap().unwrap();
    drop(remote_rx);

    println!("Waiting for the closure to be reported");
    tx.closed().await;

    match tx.send(1) {
        Ok(_) => panic!("send succeeded although the receiver was dropped"),
        Err(remoc::rch::oneshot::SendError::Dropped) => (),
        Err(other) => panic!("unexpected error: {other:?}"),
    }
}
