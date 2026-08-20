//! Behavior when all local ports are in use.

use std::time::Duration;
use wokio::time::{sleep, timeout};

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use super::{cfg, connected_with_cfg};
use remoc::chmux::{self, ConnectError, Listener, OnPortsExhausted, Sender};

const MAX_PORTS: u32 = 2;

/// Accepts everything the remote endpoint connects and keeps each port open until
/// the remote endpoint closes it.
///
/// A port number is released only once both endpoints have dropped the port, since
/// until then the peer may still use it.
fn spawn_acceptor(mut listener: Listener) {
    wokio::spawn(async move {
        while let Ok(Some((tx, mut rx))) = listener.accept().await {
            wokio::spawn(async move {
                while let Ok(Some(_)) = rx.recv().await {}
                drop(tx);
            });
        }
    });
}

/// Connects a multiplexer whose local ports are limited and exhausts them.
async fn exhausted(on_exhausted: OnPortsExhausted) -> (chmux::Client, Vec<(Sender, chmux::Receiver)>) {
    let a_cfg = chmux::Cfg { max_ports: MAX_PORTS, connect_queue: 1, ports_exhausted: on_exhausted, ..cfg() };
    let (client, listener) = connected_with_cfg(a_cfg, cfg()).await;
    spawn_acceptor(listener);

    let mut ports = Vec::new();
    for i in 0..MAX_PORTS {
        let port = timeout(Duration::from_secs(5), client.connect_port())
            .await
            .unwrap_or_else(|_| panic!("connecting port {i} timed out"))
            .unwrap_or_else(|err| panic!("connecting port {i} failed: {err}"));
        ports.push(port);
    }

    (client, ports)
}

/// With [`OnPortsExhausted::Fail`] connecting fails at once when no port is available.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn fail_reports_exhaustion_without_waiting() {
    crate::init();
    let (client, _ports) = exhausted(OnPortsExhausted::Fail).await;

    match timeout(Duration::from_millis(500), client.connect_port()).await {
        Ok(Err(ConnectError::LocalPortsExhausted)) => (),
        Ok(other) => panic!("unexpected connect result: {other:?}"),
        Err(_) => panic!("connecting waited although ports are configured to fail when exhausted"),
    }
}

/// With [`OnPortsExhausted::Timeout`] connecting waits and succeeds when a port
/// becomes available in time.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn timeout_waits_for_a_port_to_become_available() {
    crate::init();
    let (client, mut ports) = exhausted(OnPortsExhausted::Timeout(Duration::from_secs(10))).await;

    let connect = wokio::spawn(async move { client.connect_port().await });

    println!("Releasing a port while connecting is waiting");
    sleep(Duration::from_millis(200)).await;
    ports.pop();

    match timeout(Duration::from_secs(10), connect).await {
        Ok(Ok(Ok(_port))) => (),
        Ok(other) => panic!("unexpected connect result: {other:?}"),
        Err(_) => panic!("connecting did not use the port that became available"),
    }
}

/// With [`OnPortsExhausted::Timeout`] connecting reports exhaustion once the
/// configured time has elapsed without a port becoming available.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn timeout_reports_exhaustion_after_waiting() {
    crate::init();
    let (client, _ports) = exhausted(OnPortsExhausted::Timeout(Duration::from_millis(300))).await;

    match timeout(Duration::from_secs(5), client.connect_port()).await {
        Ok(Err(ConnectError::LocalPortsExhausted)) => (),
        Ok(other) => panic!("unexpected connect result: {other:?}"),
        Err(_) => panic!("connecting waited longer than the configured timeout"),
    }
}
