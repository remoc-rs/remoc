mod cancel;
mod channel;
mod forward;
mod global_credits;
mod ports_exhausted;
mod storage;
mod tentative_accept;
mod terminate;

#[cfg(not(target_family = "wasm"))]
mod tcp;

#[cfg(unix)]
mod unix;

use futures::{StreamExt, future::try_join};
use std::time::Duration;

use crate::loop_transport;
use remoc::chmux::{self, Client, Listener};

/// Configuration used by the tests in this module.
pub fn cfg() -> chmux::Cfg {
    chmux::Cfg { connection_timeout: Some(Duration::from_secs(1)), connect_queue: 2, ..Default::default() }
}

/// Connects two multiplexers over an in-memory transport and runs them.
pub async fn connected() -> (Client, Listener) {
    connected_with_cfg(cfg(), cfg()).await
}

/// Connects two multiplexers with individual configurations and runs them.
pub async fn connected_with_cfg(a_cfg: chmux::Cfg, b_cfg: chmux::Cfg) -> (Client, Listener) {
    loop_transport!(0, a_tx, a_rx, b_tx, b_rx);
    let ((a_mux, a_client, _a_server), (b_mux, _b_client, b_server)) =
        try_join(chmux::ChMux::new(a_cfg, a_tx, a_rx), chmux::ChMux::new(b_cfg, b_tx, b_rx)).await.unwrap();

    wokio::spawn(async move {
        let _ = a_mux.run().await;
    });
    wokio::spawn(async move {
        let _ = b_mux.run().await;
    });

    (a_client, b_server)
}

/// Forwards a channel between two multiplexers, so that ports sent over it are
/// forwarded as well.
pub fn spawn_forwarder(
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
