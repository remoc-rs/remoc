use bytes::Bytes;
use futures::{SinkExt, StreamExt, future};
use remoc::prelude::*;
use std::io;
use wasm_bindgen_futures::spawn_local;
use websocket_web::{Msg, WebSocket};

/// Connects to a Remoc endpoint exposed over WebSocket from within a web browser.
pub async fn connect(
    url: &str,
) -> Result<(rch::base::Sender<String>, rch::base::Receiver<String>), Box<dyn std::error::Error>> {
    let websocket = WebSocket::connect(url).await?;
    let (websocket_tx, websocket_rx) = websocket.into_split();

    // Each chmux packet is carried as one binary WebSocket message.
    let transport_tx =
        websocket_tx.with(|packet: Bytes| future::ready(Ok::<_, io::Error>(Msg::Binary(packet.into()))));

    // Text messages are not part of the Remoc transport.
    let transport_rx = websocket_rx.filter_map(|message| {
        future::ready(match message {
            Ok(Msg::Binary(packet)) => Some(Ok(Bytes::from(packet))),
            Ok(Msg::Text(_)) => None,
            Err(err) => Some(Err(err)),
        })
    });

    // The browser WebSocket is a JavaScript object and thus neither `Send` nor `Sync`,
    // so the connection future must be spawned onto the current thread.
    let (conn, tx, rx) = remoc::Connect::framed(remoc::Cfg::default(), transport_tx, transport_rx).await?;
    spawn_local(async move {
        let _ = conn.await;
    });

    Ok((tx, rx))
}
