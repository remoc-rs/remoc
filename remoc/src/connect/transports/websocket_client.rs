use crate::{MyInitialReq, MyInitialRsp};
use bytes::Bytes;
use futures::{SinkExt, StreamExt, future};
use remoc::prelude::*;
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{Error, Message},
};

/// Connects to a Remoc endpoint exposed over WebSocket.
pub async fn connect(
    url: &str,
) -> Result<
    (rch::base::Sender<MyInitialReq>, rch::base::Receiver<MyInitialRsp>),
    Box<dyn std::error::Error>,
> {
    let (websocket, _response) = connect_async_with_config(url, None, true).await?;
    let (websocket_tx, websocket_rx) = websocket.split();

    // Each chmux packet is carried as one binary WebSocket message.
    let transport_tx = websocket_tx
        .with(|packet: Bytes| future::ready(Ok::<_, Error>(Message::Binary(packet))));

    // Ping, pong, text and close messages are not part of the Remoc transport.
    let transport_rx = websocket_rx.filter_map(|message| {
        future::ready(match message {
            Ok(Message::Binary(packet)) => Some(Ok(packet)),
            Ok(_) => None,
            Err(err) => Some(Err(err)),
        })
    });

    let (conn, tx, rx) =
        remoc::Connect::framed(remoc::Cfg::default(), transport_tx, transport_rx).await?;
    tokio::spawn(conn);

    Ok((tx, rx))
}
