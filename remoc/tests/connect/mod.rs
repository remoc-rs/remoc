//! Establishing connections over the provided transports.

use futures::{StreamExt, future::try_join};
use std::time::Duration;
use wokio::time::timeout;

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use remoc::{Cfg, Connect, ConnectError};

/// A value large enough to be transferred in multiple chunks.
fn payload(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 251) as u8).collect()
}

/// A connection carries values in both directions and ends cleanly when the
/// remote endpoint goes away.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn framed_transport_round_trip() {
    crate::init();
    crate::loop_transport!(0, a_tx, a_rx, b_tx, b_rx);

    let a = Connect::framed::<_, _, _, _, Vec<u8>, Vec<u8>, remoc::codec::Default>(Cfg::default(), a_tx, a_rx);
    let b = Connect::framed::<_, _, _, _, Vec<u8>, Vec<u8>, remoc::codec::Default>(Cfg::default(), b_tx, b_rx);
    let ((a_conn, mut a_tx, mut a_rx), (b_conn, mut b_tx, mut b_rx)) = try_join(a, b).await.unwrap();

    wokio::spawn(async move {
        let _ = a_conn.await;
    });
    wokio::spawn(async move {
        let _ = b_conn.await;
    });

    let sent = payload(100_000);
    a_tx.send(sent.clone()).await.unwrap();
    assert_eq!(b_rx.recv().await.unwrap().unwrap(), sent, "value was altered in transfer");

    b_tx.send(sent.clone()).await.unwrap();
    assert_eq!(a_rx.recv().await.unwrap().unwrap(), sent, "value was altered in transfer");

    println!("Dropping one endpoint");
    drop(b_tx);
    drop(b_rx);
    assert!(a_rx.recv().await.unwrap().is_none(), "closure of the remote endpoint was not reported");
}

/// Endpoints agree on a common configuration, even when their configurations differ
/// substantially.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn differing_configurations_are_negotiated() {
    crate::init();
    crate::loop_transport!(0, a_tx, a_rx, b_tx, b_rx);

    let a_cfg = Cfg {
        chunk_size: 4,
        port_receive_buffer: 4,
        max_data_size: 1_000_000,
        shared_send_queue: 1,
        ..Default::default()
    };
    let b_cfg = Cfg {
        chunk_size: 60_000,
        port_receive_buffer: 1_000_000,
        max_data_size: 1_000_000,
        shared_send_queue: 16,
        ..Default::default()
    };

    let a = Connect::framed::<_, _, _, _, Vec<u8>, Vec<u8>, remoc::codec::Default>(a_cfg, a_tx, a_rx);
    let b = Connect::framed::<_, _, _, _, Vec<u8>, Vec<u8>, remoc::codec::Default>(b_cfg, b_tx, b_rx);
    let ((a_conn, mut a_tx, mut a_rx), (b_conn, mut b_tx, mut b_rx)) = try_join(a, b).await.unwrap();

    wokio::spawn(async move {
        let _ = a_conn.await;
    });
    wokio::spawn(async move {
        let _ = b_conn.await;
    });

    // The receiving side determines the chunk size, thus both directions exercise
    // a different one.
    let sent = payload(200_000);
    a_tx.send(sent.clone()).await.unwrap();
    assert_eq!(b_rx.recv().await.unwrap().unwrap(), sent, "value was altered from small to large chunks");

    b_tx.send(sent.clone()).await.unwrap();
    assert_eq!(a_rx.recv().await.unwrap().unwrap(), sent, "value was altered from large to small chunks");
}

/// A loopback connection delivers what is sent over it.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn loopback_delivers() {
    crate::init();
    let (conn, mut tx, mut rx) =
        Connect::loopback::<Vec<u8>, Vec<u8>, remoc::codec::Default>(Cfg::default()).await;
    wokio::spawn(async move {
        let _ = conn.await;
    });

    let sent = payload(10_000);
    tx.send(sent.clone()).await.unwrap();
    assert_eq!(rx.recv().await.unwrap().unwrap(), sent, "value was altered in loopback");
}

/// Connecting to a transport that does not speak the protocol fails instead of
/// waiting forever.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn garbage_transport_is_rejected() {
    crate::init();
    let (sink, _sink_rx) = futures::channel::mpsc::channel::<bytes::Bytes>(16);
    let stream = futures::stream::iter(vec![
        Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"this is not a remoc connection")),
        Ok(bytes::Bytes::from_static(b"and neither is this")),
    ]);

    let res = timeout(
        Duration::from_secs(10),
        Connect::framed::<_, _, _, _, Vec<u8>, Vec<u8>, remoc::codec::Default>(Cfg::default(), sink, stream),
    )
    .await;

    match res {
        Ok(Err(ConnectError::ChMux(err))) => println!("Connecting failed with: {err}"),
        Ok(Err(err)) => println!("Connecting failed with: {err}"),
        Ok(Ok(_)) => panic!("connecting to a garbage transport succeeded"),
        Err(_) => panic!("connecting to a garbage transport did not return"),
    }
}

/// Connecting to a transport that never answers does not wait forever.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn silent_transport_times_out() {
    crate::init();
    let (sink, _sink_rx) = futures::channel::mpsc::channel::<bytes::Bytes>(16);
    let (_stream_tx, stream_rx) = futures::channel::mpsc::channel::<bytes::Bytes>(16);
    let stream = stream_rx.map(Ok::<_, std::io::Error>);

    let cfg = Cfg { connection_timeout: Some(Duration::from_millis(500)), ..Default::default() };
    match timeout(
        Duration::from_secs(10),
        Connect::framed::<_, _, _, _, Vec<u8>, Vec<u8>, remoc::codec::Default>(cfg, sink, stream),
    )
    .await
    {
        Ok(Err(err)) => println!("Connecting failed with: {err}"),
        Ok(Ok(_)) => panic!("connecting to a silent transport succeeded"),
        Err(_) => panic!("connecting to a silent transport did not honor the connection timeout"),
    }
}
