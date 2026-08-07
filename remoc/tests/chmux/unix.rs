use bytes::Bytes;
use futures::{future::try_join, stream::StreamExt};
use std::{
    fs,
    time::{Duration, Instant},
};
use tokio::{
    io::split,
    net::{UnixListener, UnixStream},
    time::sleep,
};
use tokio_util::codec::{FramedRead, FramedWrite, length_delimited::LengthDelimitedCodec};

use remoc::{chmux, exec};

async fn uds_server() {
    let _ = fs::remove_file("/tmp/chmux_test");
    let listener = UnixListener::bind("/tmp/chmux_test").unwrap();

    let (socket, _) = listener.accept().await.unwrap();
    let (socket_rx, socket_tx) = split(socket);
    let framed_tx = FramedWrite::new(socket_tx, LengthDelimitedCodec::new());
    let framed_rx = FramedRead::new(socket_rx, LengthDelimitedCodec::new());
    let framed_rx = framed_rx.map(|data| data.map(|b| b.freeze()));

    let mux_cfg = chmux::Cfg::default();
    let (mux, _, mut server) = chmux::ChMux::new(mux_cfg, framed_tx, framed_rx).await.unwrap();

    let mux_run = exec::spawn(async move { mux.run().await.unwrap() });

    while let Some((mut tx, mut rx)) = server.accept().await.unwrap() {
        println!("Server accepting request");

        tx.send("Hi from server".into()).await.unwrap();
        println!("Server sent Hi message");

        println!("Server dropping sender");
        drop(tx);
        println!("Server dropped sender");

        while let Some(msg) = rx.recv().await.unwrap() {
            println!("Server received: {}", String::from_utf8_lossy(&Vec::from(msg)));
        }
    }

    println!("Waiting for server mux to terminate...");
    mux_run.await.unwrap();
}

async fn uds_client() {
    let socket = UnixStream::connect("/tmp/chmux_test").await.unwrap();

    let (socket_rx, socket_tx) = split(socket);
    let framed_tx = FramedWrite::new(socket_tx, LengthDelimitedCodec::new());
    let framed_rx = FramedRead::new(socket_rx, LengthDelimitedCodec::new());
    let framed_rx = framed_rx.map(|data| data.map(|b| b.freeze()));

    let mux_cfg = chmux::Cfg::default();
    let (mux, client, _) = chmux::ChMux::new(mux_cfg, framed_tx, framed_rx).await.unwrap();
    let mux_run = exec::spawn(async move { mux.run().await.unwrap() });

    {
        let client = client;

        println!("Client connecting...");
        let (mut tx, mut rx) = client.connect_port().await.unwrap();
        println!("Client connected");

        tx.send("Hi from client".into()).await.unwrap();
        println!("Client sent Hi message");

        println!("Client dropping sender");
        drop(tx);
        println!("Client dropped sender");

        while let Some(msg) = rx.recv().await.unwrap() {
            println!("Client received: {}", String::from_utf8_lossy(&Vec::from(msg)));
        }

        println!("Client closing connection...");
    }

    println!("Waiting for client mux to terminate...");
    mux_run.await.unwrap();
}

#[tokio::test]
async fn uds_test() {
    crate::init();

    println!("Starting server task...");
    let server_task = exec::spawn(uds_server());
    sleep(Duration::from_millis(100)).await;

    println!("String client thread...");
    let client_task = exec::spawn(uds_client());

    println!("Waiting for server task...");
    server_task.await.unwrap();
    println!("Waiting for client thread...");
    client_task.await.unwrap();
}

/// Round-trip latency test over a real Unix domain socket.
async fn uds_round_trip_latency() -> Duration {
    const ROUND_TRIPS: u32 = 1000;

    let cfg = chmux::Cfg::default();

    // Use a unique socket path to avoid conflicts with other tests.
    let socket_path = "/tmp/chmux_latency_test";
    let _ = fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path).unwrap();

    let accept = async {
        let (socket, _) = listener.accept().await.unwrap();
        let (rx, tx) = split(socket);
        let framed_tx = FramedWrite::new(tx, LengthDelimitedCodec::new());
        let framed_rx = FramedRead::new(rx, LengthDelimitedCodec::new()).map(|data| data.map(|b| b.freeze()));
        chmux::ChMux::new(cfg.clone(), framed_tx, framed_rx).await
    };

    let connect = async {
        let socket = UnixStream::connect(socket_path).await.unwrap();
        let (rx, tx) = split(socket);
        let framed_tx = FramedWrite::new(tx, LengthDelimitedCodec::new());
        let framed_rx = FramedRead::new(rx, LengthDelimitedCodec::new()).map(|data| data.map(|b| b.freeze()));
        chmux::ChMux::new(cfg.clone(), framed_tx, framed_rx).await
    };

    let ((server_mux, _server_client, mut server), (client_mux, client, _client_server)) =
        try_join(accept, connect).await.unwrap();

    exec::spawn(async move { server_mux.run().await.unwrap() });
    exec::spawn(async move { client_mux.run().await.unwrap() });

    // Server: echo back every message it receives.
    exec::spawn(async move {
        while let Some((mut tx, mut rx)) = server.accept().await.unwrap() {
            exec::spawn(async move {
                while let Some(msg) = rx.recv().await.unwrap() {
                    tx.send(Bytes::from(msg)).await.unwrap();
                }
            });
        }
    });

    // Client: measure round-trip latency.
    let (mut tx, mut rx) = client.connect_port().await.unwrap();

    // Warm-up round-trip.
    tx.send(Bytes::from_static(b"warmup")).await.unwrap();
    rx.recv().await.unwrap().unwrap();

    let start = Instant::now();
    for i in 0..ROUND_TRIPS {
        let msg = format!("ping {i}");
        tx.send(Bytes::from(msg)).await.unwrap();
        let _reply = rx.recv().await.unwrap().unwrap();
    }
    let elapsed = start.elapsed();

    drop(tx);
    drop(rx);
    drop(client);
    drop(_server_client);
    let _ = fs::remove_file(socket_path);

    elapsed / ROUND_TRIPS
}

#[tokio::test]
async fn uds_round_trip_latency_test() {
    crate::init();

    let avg = uds_round_trip_latency().await;
    println!("Average UDS round-trip latency: {avg:?}");

    #[cfg(not(debug_assertions))]
    assert!(avg < Duration::from_millis(1), "Expected round-trip latency < 1ms, got {avg:?}");
}
