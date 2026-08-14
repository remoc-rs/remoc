//! Scratch test: does a receiver that forbids global credits actually limit
//! how much the sender can put in flight?

use futures::{future::try_join, stream::StreamExt};
use std::time::Duration;
use wokio::time::timeout;

use crate::loop_transport;
use remoc::chmux::{self, DynamicBuffer};

fn cfg() -> chmux::Cfg {
    chmux::Cfg {
        connection_timeout: Some(Duration::from_secs(5)),
        chunk_size: 1024,
        port_receive_buffer: 8 * 1024,
        shared_receive_buffer: DynamicBuffer::new(512 * 1024, 512 * 1024),
        ..Default::default()
    }
}

/// Sends 1 kB messages into a channel nobody reads and returns how many were
/// accepted before the sender stalled.
async fn in_flight_until_stalled(forbid_global: bool) -> usize {
    loop_transport!(0, a_tx, a_rx, b_tx, b_rx);
    let ((a_mux, a_client, _), (b_mux, _, mut b_server)) =
        try_join(chmux::ChMux::new(cfg(), a_tx, a_rx), chmux::ChMux::new(cfg(), b_tx, b_rx)).await.unwrap();

    wokio::spawn(async move { a_mux.run().await.unwrap() });
    wokio::spawn(async move { b_mux.run().await.unwrap() });

    // The receiving side accepts the port, optionally forbids global credits and then
    // never receives anything.
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    wokio::spawn(async move {
        let (_tx, mut rx) = b_server.accept().await.unwrap().unwrap();
        rx.set_global_credits_allowed(!forbid_global);
        let _ = ready_tx.send(());
        std::future::pending::<()>().await;
        drop(rx);
    });

    let (mut tx, _rx) = a_client.connect_port().await.unwrap();
    ready_rx.await.unwrap();
    // Give the inhibit message time to reach this side before we start sending.
    wokio::time::sleep(Duration::from_millis(200)).await;

    let msg = vec![0u8; 1024];
    let mut sent = 0;
    while timeout(Duration::from_millis(200), tx.send(msg.clone().into())).await.is_ok() {
        sent += 1;
        if sent > 2000 {
            break;
        }
    }
    sent
}

#[tokio::test]
async fn forbidding_global_credits_limits_in_flight_data() {
    crate::init();

    let allowed = in_flight_until_stalled(false).await;
    let forbidden = in_flight_until_stalled(true).await;
    println!("in flight: {allowed} kB with global credits, {forbidden} kB without");

    assert!(
        forbidden < allowed / 2,
        "forbidding global credits should cut in-flight data sharply, \
         but got {forbidden} kB versus {allowed} kB"
    );
}
