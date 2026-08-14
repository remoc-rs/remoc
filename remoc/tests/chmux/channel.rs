use futures::{
    channel::{mpsc, oneshot},
    future::{join_all, try_join},
    sink::SinkExt,
    stream::StreamExt,
};
use std::time::Duration;
use tracing::Instrument;
use wokio::time::{sleep, timeout};

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_transport;
use remoc::chmux::{self, OnPortsExhausted, ReceiverStream, SendError};

fn cfg() -> chmux::Cfg {
    chmux::Cfg {
        connection_timeout: Some(Duration::from_secs(1)),
        max_ports: 20,
        ports_exhausted: OnPortsExhausted::Fail,
        max_data_size: 1_000_000,
        max_received_ports: 100,
        chunk_size: 9,
        port_receive_buffer: 4,
        shared_send_queue: 3,
        connect_queue: 2,
        ..Default::default()
    }
}

fn cfg2() -> chmux::Cfg {
    chmux::Cfg {
        connection_timeout: Some(Duration::from_secs(1)),
        max_ports: 20,
        ports_exhausted: OnPortsExhausted::Timeout(Duration::from_secs(5)),
        max_data_size: 1_000_000,
        max_received_ports: 100,
        chunk_size: 4,
        port_receive_buffer: 4,
        shared_send_queue: 1,
        connect_queue: 1,
        ..Default::default()
    }
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn basic() {
    crate::init();

    println!("Connecting...");
    loop_transport!(0, a_tx, a_rx, b_tx, b_rx);
    let ((a_mux, a_client, a_server), (b_mux, b_client, mut b_server)) =
        try_join(chmux::ChMux::new(cfg(), a_tx, a_rx), chmux::ChMux::new(cfg2(), b_tx, b_rx)).await.unwrap();
    println!("Connected: a_mux={:?}, b_mux={:?}", a_mux, b_mux);

    let (a_mux_done_tx, a_mux_done_rx) = oneshot::channel();
    wokio::spawn(
        async move {
            println!("A mux run");
            a_mux.run().await.expect("a_mux");
            let _ = a_mux_done_tx.send(());
        }
        .instrument(tracing::info_span!("A mux")),
    );

    let (b_mux_done_tx, b_mux_done_rx) = oneshot::channel();
    wokio::spawn(
        async move {
            println!("B mux run");
            b_mux.run().await.expect("b_mux");
            let _ = b_mux_done_tx.send(());
        }
        .instrument(tracing::info_span!("B mux")),
    );

    const N_MSG: usize = 500;

    let (server_done_tx, server_done_rx) = oneshot::channel();
    wokio::spawn(async move {
        println!("B server start");
        while let Some((mut tx, mut rx)) = b_server.accept().await.unwrap() {
            wokio::spawn(async move {
                while let Some(msg) = rx.recv().await.unwrap() {
                    println!("Server received: {}", String::from_utf8(msg.into()).unwrap());
                }
                println!("Server receiver ended");
            });

            for i in 0..N_MSG {
                let msg = format!("message no {i}");
                match tx.send(msg.clone().into()).await {
                    Ok(()) => (),
                    Err(SendError::Closed { gracefully }) => {
                        println!("Client closed connection with gracefully={gracefully}");
                        break;
                    }
                    other => other.unwrap(),
                }
                if i.is_multiple_of(100) {
                    tx.flush().await;
                }
                println!("Server sent: {msg}");
            }

            drop(tx);
            println!("Server dropped transmitter");
        }

        println!("B Server quit");
        drop(b_client);
        let _ = server_done_tx.send(());
    });

    {
        println!("A client connecting 1...");
        let ret: Result<(chmux::Sender, chmux::Receiver), _> = a_client.connect_port().await;
        println!("A client connect result: {:?}", ret);
    }

    println!("Delay test...");
    sleep(Duration::from_secs(3)).await;

    println!("A client connecting 2...");
    let (mut tx, mut rx): (chmux::Sender, chmux::Receiver) = a_client.connect_port().await.unwrap();
    println!("A client connected.");

    let mut n_recv = 0;
    while let Some(msg) = rx.recv().await.unwrap() {
        let s = String::from_utf8(msg.into()).unwrap();
        println!("A client received: {}", s);
        n_recv += 1;

        println!("A client replying...");
        tx.send(format!("Reply: {}", s).into()).await.unwrap();
        println!("A client replied");
    }
    if n_recv != N_MSG {
        panic!("received not equal number messages than sent");
    }
    println!("A client receiver closed");

    drop(tx);
    drop(rx);
    drop(a_client);

    println!("Waiting for server");
    server_done_rx.await.unwrap();

    drop(a_server);

    println!("Waiting for multiplexers");
    a_mux_done_rx.await.unwrap();
    b_mux_done_rx.await.unwrap();
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn receiver_stream() {
    crate::init();

    loop_transport!(0, a_tx, a_rx, b_tx, b_rx);
    let ((a_mux, a_client, a_server), (b_mux, b_client, mut b_server)) =
        try_join(chmux::ChMux::new(cfg(), a_tx, a_rx), chmux::ChMux::new(cfg2(), b_tx, b_rx)).await.unwrap();

    let (a_mux_done_tx, a_mux_done_rx) = oneshot::channel();
    wokio::spawn(
        async move {
            a_mux.run().await.expect("a_mux");
            let _ = a_mux_done_tx.send(());
        }
        .instrument(tracing::info_span!("A mux")),
    );

    let (b_mux_done_tx, b_mux_done_rx) = oneshot::channel();
    wokio::spawn(
        async move {
            b_mux.run().await.expect("b_mux");
            let _ = b_mux_done_tx.send(());
        }
        .instrument(tracing::info_span!("B mux")),
    );

    const N_MSG: usize = 100;

    let (server_done_tx, server_done_rx) = oneshot::channel();
    wokio::spawn(async move {
        while let Some((mut tx, rx)) = b_server.accept().await.unwrap() {
            let mut n = 0;
            let mut rx = ReceiverStream::from(rx);
            while let Some(msg) = rx.next().await {
                let msg = msg.unwrap();
                tx.send(msg.into()).await.unwrap();

                n += 1;
                if n == N_MSG {
                    rx.close();
                }
            }
        }

        drop(b_client);
        let _ = server_done_tx.send(());
    });

    let (mut tx, mut rx): (chmux::Sender, chmux::Receiver) = a_client.connect_port().await.unwrap();

    let mut n = 0;
    loop {
        let s = format!("{n}");
        let data = s.as_bytes().to_vec();
        match tx.send(data.clone().into()).await {
            Ok(()) => (),
            Err(err) if err.is_closed() => break,
            Err(err) => panic!("send error: {err}"),
        }

        n += 1;

        let reply = rx.recv().await.unwrap().unwrap();
        let reply_data: Vec<u8> = reply.into();
        assert_eq!(data, reply_data);
    }
    assert!(n > N_MSG);

    drop(tx);
    drop(rx);
    drop(a_client);

    println!("Waiting for server");
    server_done_rx.await.unwrap();

    drop(a_server);

    println!("Waiting for multiplexers");
    a_mux_done_rx.await.unwrap();
    b_mux_done_rx.await.unwrap();
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn all_received() {
    crate::init();

    loop_transport!(0, a_tx, a_rx, b_tx, b_rx);
    let ((a_mux, a_client, a_server), (b_mux, b_client, mut b_server)) =
        try_join(chmux::ChMux::new(cfg(), a_tx, a_rx), chmux::ChMux::new(cfg2(), b_tx, b_rx)).await.unwrap();

    let (a_mux_done_tx, a_mux_done_rx) = oneshot::channel();
    wokio::spawn(async move {
        a_mux.run().await.expect("a_mux");
        let _ = a_mux_done_tx.send(());
    });

    let (b_mux_done_tx, b_mux_done_rx) = oneshot::channel();
    wokio::spawn(async move {
        b_mux.run().await.expect("b_mux");
        let _ = b_mux_done_tx.send(());
    });

    let (mut trigger_recv_tx, mut trigger_recv_rx) = mpsc::channel(16);

    const COUNT: usize = 10;

    let (server_done_tx, server_done_rx) = oneshot::channel();
    wokio::spawn(async move {
        while let Some((_tx, mut rx)) = b_server.accept().await.unwrap() {
            let mut count = 0;
            loop {
                println!("waiting for trigger {count}");
                let _ = trigger_recv_rx.recv().await;
                println!("receiving {count}");
                let Some(message) = rx.recv().await.unwrap() else { break };
                let message: Vec<u8> = message.into();
                assert_eq!(message, format!("message {count}").into_bytes());
                count += 1;
            }
            assert_eq!(count, COUNT);
        }

        drop(b_client);
        let _ = server_done_tx.send(());
    });

    let (mut tx, rx): (chmux::Sender, chmux::Receiver) = a_client.connect_port().await.unwrap();
    assert!(tx.is_all_received_supported());
    tx.set_global_credits_allowed(false);
    // Need one more trigger, since receive notification will be sent by subsequent rx.recv() call
    // after receiving a message.
    trigger_recv_tx.send(()).await.unwrap();
    for count in 0..COUNT {
        println!("send {count}");
        tx.send(format!("message {count}").into()).await.unwrap();
        timeout(Duration::from_millis(100), tx.all_received()).await.expect_err("not received yet");
        trigger_recv_tx.send(()).await.unwrap();
        timeout(Duration::from_millis(100), tx.all_received()).await.expect("should now have been received");
    }

    drop(trigger_recv_tx);
    let reports: Vec<_> = (0..20).map(|_| tx.all_received()).collect();
    timeout(Duration::from_secs(1), join_all(reports)).await.expect("received reports timed out");

    drop(tx);
    drop(rx);
    drop(a_client);
    server_done_rx.await.unwrap();

    drop(a_server);
    a_mux_done_rx.await.unwrap();
    b_mux_done_rx.await.unwrap();
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn all_received_receiver_dropped() {
    crate::init();

    loop_transport!(0, a_tx, a_rx, b_tx, b_rx);
    let ((a_mux, a_client, a_server), (b_mux, b_client, mut b_server)) =
        try_join(chmux::ChMux::new(cfg(), a_tx, a_rx), chmux::ChMux::new(cfg2(), b_tx, b_rx)).await.unwrap();

    let (a_mux_done_tx, a_mux_done_rx) = oneshot::channel();
    wokio::spawn(async move {
        a_mux.run().await.expect("a_mux");
        let _ = a_mux_done_tx.send(());
    });

    let (b_mux_done_tx, b_mux_done_rx) = oneshot::channel();
    wokio::spawn(async move {
        b_mux.run().await.expect("b_mux");
        let _ = b_mux_done_tx.send(());
    });

    let (receiver_ready_tx, receiver_ready_rx) = oneshot::channel();
    let (drop_receiver_tx, drop_receiver_rx) = oneshot::channel();
    let (server_done_tx, server_done_rx) = oneshot::channel();
    wokio::spawn(async move {
        let Some((_tx, rx)) = b_server.accept().await.unwrap() else { panic!("server closed unexpectedly") };
        let _ = receiver_ready_tx.send(());
        let _ = drop_receiver_rx.await;
        drop(rx);

        drop(b_client);
        let _ = server_done_tx.send(());
    });

    let (mut tx, rx): (chmux::Sender, chmux::Receiver) = a_client.connect_port().await.unwrap();
    assert!(tx.is_all_received_supported());
    receiver_ready_rx.await.unwrap();
    tx.send("message".into()).await.unwrap();

    let reports = (0..4).map(|_| tx.all_received()).collect::<Vec<_>>();
    let (reports_started_tx, reports_started_rx) = oneshot::channel();
    let (reports_done_tx, reports_done_rx) = oneshot::channel();
    wokio::spawn(async move {
        let _ = reports_started_tx.send(());
        join_all(reports).await;
        let _ = reports_done_tx.send(());
    });
    reports_started_rx.await.unwrap();
    sleep(Duration::from_millis(10)).await;

    drop_receiver_tx.send(()).unwrap();
    timeout(Duration::from_secs(1), reports_done_rx)
        .await
        .expect("all received reports did not resolve after receiver drop")
        .unwrap();

    drop(tx);
    drop(rx);
    drop(a_client);
    server_done_rx.await.unwrap();

    drop(a_server);
    a_mux_done_rx.await.unwrap();
    b_mux_done_rx.await.unwrap();
}

#[cfg_attr(
    all(not(all(target_family = "wasm", feature = "js")), not(target_family = "wasm")),
    tokio::test(flavor = "multi_thread", worker_threads = 8)
)]
#[cfg_attr(all(not(all(target_family = "wasm", feature = "js")), target_family = "wasm"), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn hangup() {
    crate::init();

    println!("Connecting...");
    loop_transport!(0, a_tx, a_rx, b_tx, b_rx);
    let ((a_mux, a_client, a_server), (b_mux, b_client, mut b_server)) =
        try_join(chmux::ChMux::new(cfg(), a_tx, a_rx), chmux::ChMux::new(cfg2(), b_tx, b_rx)).await.unwrap();
    println!("Connected: a_mux={:?}, b_mux={:?}", a_mux, b_mux);

    let (a_mux_done_tx, a_mux_done_rx) = oneshot::channel();
    wokio::spawn(
        async move {
            println!("A mux start");
            a_mux.run().await.unwrap();
            let _ = a_mux_done_tx.send(());
        }
        .instrument(tracing::info_span!("A mux")),
    );

    let (b_mux_done_tx, b_mux_done_rx) = oneshot::channel();
    wokio::spawn(
        async move {
            println!("B mux start");
            b_mux.run().await.unwrap();
            let _ = b_mux_done_tx.send(());
        }
        .instrument(tracing::info_span!("B mux")),
    );

    let (server_done_tx, server_done_rx) = oneshot::channel();
    wokio::spawn(async move {
        println!("B server start");
        while let Some((mut tx, mut rx)) = b_server.accept().await.unwrap() {
            while let Some(msg) = rx.recv().await.unwrap() {
                let msg = Vec::from(msg);
                println!("Server received: {}", msg[0]);
                if msg[0] == 100 {
                    break;
                }
            }
            println!("Server dropping receiver");
            drop(rx);

            tx.send("Server 1 msg".into()).await.unwrap();
            tx.send("Server 2nd message".into()).await.unwrap();

            println!("Server closed connection");
        }
        println!("B Server quit");
        drop(b_client);
        let _ = server_done_tx.send(());
    });

    println!("A client connecting to service...");
    let (mut tx, mut rx): (chmux::Sender, chmux::Receiver) = a_client.connect_port().await.unwrap();
    println!("A client connected.");

    let hn = tx.closed();

    let mut i = 0u16;
    loop {
        let msg = vec![i as u8];
        match tx.send(msg.into()).await {
            Ok(()) => println!("A client sent: {i}"),
            Err(err) => {
                println!("A client send error: {err:?}");
                break;
            }
        }
        i += 1;
    }
    if i <= 100 {
        panic!("Send error too early");
    }

    println!("Waiting for hangup notification...");
    hn.await;

    loop {
        match rx.recv().await.unwrap() {
            Some(msg) => {
                println!("Client received: {}", String::from_utf8(msg.into()).unwrap());
            }
            None => {
                println!("Client receive end");
                break;
            }
        }
    }

    println!("A client receiver closed");

    drop(tx);
    drop(rx);
    drop(a_client);

    println!("Waiting for server close");
    server_done_rx.await.unwrap();

    drop(a_server);

    println!("Waiting for multiplexers");
    a_mux_done_rx.await.unwrap();
    b_mux_done_rx.await.unwrap();
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn no_pre_connect() {
    crate::init();

    println!("Connecting...");
    loop_transport!(0, a_tx, a_rx, b_tx, b_rx);
    let ((a_mux, a_client, a_server), (b_mux, b_client, mut b_server)) =
        try_join(chmux::ChMux::new(cfg(), a_tx, a_rx), chmux::ChMux::new(cfg2(), b_tx, b_rx)).await.unwrap();
    println!("Connected: a_mux={:?}, b_mux={:?}", a_mux, b_mux);

    let (a_mux_done_tx, a_mux_done_rx) = oneshot::channel();
    wokio::spawn(
        async move {
            println!("A mux run");
            a_mux.run().await.expect("a_mux");
            let _ = a_mux_done_tx.send(());
        }
        .instrument(tracing::info_span!("A mux")),
    );

    let (b_mux_done_tx, b_mux_done_rx) = oneshot::channel();
    wokio::spawn(
        async move {
            println!("B mux run");
            b_mux.run().await.expect("b_mux");
            let _ = b_mux_done_tx.send(());
        }
        .instrument(tracing::info_span!("B mux")),
    );

    const N_MSG: usize = 500;

    let (server_done_tx, server_done_rx) = oneshot::channel();
    wokio::spawn(async move {
        println!("B server start");
        while let Some((mut tx, mut rx)) = b_server.accept().await.unwrap() {
            wokio::spawn(async move {
                while let Some(msg) = rx.recv().await.unwrap() {
                    println!("Server received: {}", String::from_utf8(msg.into()).unwrap());
                }
                println!("Server receiver ended");
            });

            for i in 0..N_MSG {
                let msg = format!("message no {i}");
                match tx.send(msg.clone().into()).await {
                    Ok(()) => (),
                    Err(SendError::Closed { gracefully }) => {
                        println!("Client closed connection with gracefully={gracefully}");
                        break;
                    }
                    other => other.unwrap(),
                }
                if i.is_multiple_of(100) {
                    tx.flush().await;
                }
                println!("Server sent: {msg}");
            }

            drop(tx);
            println!("Server dropped transmitter");
        }

        println!("B Server quit");
        drop(b_client);
        let _ = server_done_tx.send(());
    });

    {
        println!("A client connecting 1...");
        let connect = a_client.connect(a_client.connect_req().unwrap().no_wait());
        println!("A client connect_ext result: {connect:?}");
        let res = connect.await;
        println!("A client connect result: {res:?}");
    }

    println!("Delay test...");
    sleep(Duration::from_secs(3)).await;

    println!("A client connecting 2...");
    let (mut tx, mut rx): (chmux::Sender, chmux::Receiver) =
        a_client.connect(a_client.connect_req().unwrap().no_wait()).await.expect("Connect failed");
    println!("A client connected.");

    let mut n_recv = 0;
    while let Some(msg) = rx.recv().await.unwrap() {
        let s = String::from_utf8(msg.into()).unwrap();
        println!("A client received: {}", s);
        n_recv += 1;

        println!("A client replying...");
        tx.send(format!("Reply: {}", s).into()).await.unwrap();
        println!("A client replied");
    }
    if n_recv != N_MSG {
        panic!("received not equal number messages than sent");
    }
    println!("A client receiver closed");

    drop(tx);
    drop(rx);
    drop(a_client);

    println!("Waiting for server");
    server_done_rx.await.unwrap();

    drop(a_server);

    println!("Waiting for multiplexers");
    a_mux_done_rx.await.unwrap();
    b_mux_done_rx.await.unwrap();
}
