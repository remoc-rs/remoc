use rand::{Rng, RngExt};
use std::time::Duration;

#[cfg(feature = "js")]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel;
use remoc::{
    codec::StreamingUnavailable,
    exec,
    exec::time::timeout,
    rch::{
        DEFAULT_MAX_ITEM_SIZE,
        base::{RecvError, SendError, SendErrorKind},
    },
};

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn negation() {
    crate::init();
    let ((mut a_tx, mut a_rx), (mut b_tx, mut b_rx)) = loop_channel::<i32>().await;

    let reply_task = exec::spawn(async move {
        while let Some(i) = b_rx.recv().await.unwrap() {
            match b_tx.send(-i).await {
                Ok(()) => (),
                Err(err) if err.is_closed() => break,
                Err(err) => panic!("reply sending failed: {err}"),
            }
        }
    });

    for i in 1..1024 {
        println!("Sending {i}");
        a_tx.send(i).await.unwrap();
        let r = a_rx.recv().await.unwrap().unwrap();
        println!("Received {r}");
        assert_eq!(i, -r, "wrong reply");
    }
    drop(a_tx);

    reply_task.await.expect("reply task failed");
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn big_msg() {
    crate::init();
    let ((mut a_tx, mut a_rx), (mut b_tx, mut b_rx)) = loop_channel::<Vec<u8>>().await;

    let reply_task = exec::spawn(async move {
        while let Some(mut msg) = b_rx.recv().await.unwrap() {
            msg.reverse();
            match b_tx.send(msg).await {
                Ok(()) => (),
                Err(err) if err.is_closed() => break,
                Err(err) => panic!("reply sending failed: {err}"),
            }
        }
    });

    let mut rng = rand::rng();
    for _ in 1..10 {
        let size = rng.random_range(0..1_000_000);
        let mut data = vec![0u8; size];
        rng.fill_bytes(&mut data);

        let data_send = data.clone();
        println!("Sending message of length {}", data.len());
        let res = a_tx.send(data_send).await;
        if let Err(err) = &res {
            println!("Send error: {err}");
            match &err.kind {
                SendErrorKind::Serialize(ser)
                    if ser.0.is::<StreamingUnavailable>() && !remoc::exec::are_threads_available().await =>
                {
                    println!("Okay, because no threads available");
                    return;
                }
                _ => (),
            }
        }
        res.unwrap();

        let mut data_recv = a_rx.recv().await.unwrap().unwrap();
        println!("Received reply of length {}", data_recv.len());
        data_recv.reverse();
        assert_eq!(data, data_recv, "wrong reply");
    }
    drop(a_tx);

    reply_task.await.expect("reply task failed");
}

#[tokio::test]
#[cfg(not(target_family = "wasm"))]
async fn tcp_big_msg() {
    crate::init();
    let ((mut a_tx, mut a_rx), (mut b_tx, mut b_rx)) = crate::tcp_loop_channel::<Vec<u8>>(9877).await;

    let reply_task = exec::spawn(async move {
        while let Some(mut msg) = b_rx.recv().await.unwrap() {
            msg.reverse();
            match b_tx.send(msg).await {
                Ok(()) => (),
                Err(err) if err.is_closed() => break,
                Err(err) => panic!("reply sending failed: {err}"),
            }
        }
    });

    let mut rng = rand::rng();
    for _ in 1..10 {
        let size = rng.random_range(0..1_000_000);
        let mut data = vec![0u8; size];
        rng.fill_bytes(&mut data);

        let data_send = data.clone();
        println!("Sending message of length {}", data.len());
        a_tx.send(data_send).await.unwrap();
        let mut data_recv = a_rx.recv().await.unwrap().unwrap();
        println!("Received reply of length {}", data_recv.len());
        data_recv.reverse();
        assert_eq!(data, data_recv, "wrong reply");
    }
    drop(a_tx);

    reply_task.await.expect("reply task failed");
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn close_notify() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<u8>().await;

    println!("Checking that channel is not closed");
    assert!(!a_tx.is_closed());
    let close_notify = a_tx.closed();
    if timeout(Duration::from_secs(1), close_notify).await.is_ok() {
        panic!("close notification before closure");
    }

    println!("Closing channel");
    b_rx.close().await;

    println!("Waiting for close notification");
    a_tx.closed().await;
    assert!(a_tx.is_closed());

    println!("Testing if send fails");
    if a_tx.send(1).await.is_ok() {
        panic!("send succeeded after closure");
    }
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn oversized_msg_send_error() {
    crate::init();
    let ((mut a_tx, _a_rx), (_b_tx, mut b_rx)) = loop_channel::<Vec<u8>>().await;

    exec::spawn(async move {
        let _ = b_rx.recv().await;
    });

    let data: Vec<u8> = vec![1u8; 2 * DEFAULT_MAX_ITEM_SIZE];
    println!("Sending message of length {}", data.len());
    let res = a_tx.send(data).await;

    if remoc::exec::are_threads_available().await {
        assert!(
            matches!(res, Err(SendError { kind: SendErrorKind::MaxItemSizeExceeded, .. })),
            "sending oversized item must fail"
        )
    } else {
        assert!(
            matches!(res, Err(SendError { kind: SendErrorKind::Serialize(ser), .. }) if ser.0.is::<StreamingUnavailable>()),
            "sending oversized item must fail"
        )
    }
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn oversized_msg_recv_error() {
    crate::init();
    let ((mut a_tx, _a_rx), (_b_tx, mut b_rx)) = loop_channel::<Vec<u8>>().await;

    exec::spawn(async move {
        let data: Vec<u8> = vec![1u8; 100];
        println!("Sending message of length {}", data.len());
        a_tx.send(data).await.unwrap();
    });

    b_rx.set_max_item_size(10);
    let res = b_rx.recv().await;
    assert!(matches!(res, Err(RecvError::MaxItemSizeExceeded)), "receiving oversized item must fail")
}
