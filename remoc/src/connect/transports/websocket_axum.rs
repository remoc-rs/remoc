use axum::{
    Router,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::Response,
    routing::any,
};
use bytes::Bytes;
use futures::{SinkExt, StreamExt, future};
use remoc::prelude::*;

/// Serves a Remoc endpoint at `/remoc` over WebSocket.
pub fn router() -> Router {
    Router::new().route("/remoc", any(async |ws: WebSocketUpgrade| -> Response { ws.on_upgrade(serve) }))
}

async fn serve(websocket: WebSocket) {
    let (websocket_tx, websocket_rx) = websocket.split();

    // Each chmux packet is carried as one binary WebSocket message.
    let transport_tx =
        websocket_tx.with(|packet: Bytes| future::ready(Ok::<_, axum::Error>(Message::Binary(packet))));

    // Ping, pong, text and close messages are not part of the Remoc transport.
    let transport_rx = websocket_rx.filter_map(|message| {
        future::ready(match message {
            Ok(Message::Binary(packet)) => Some(Ok(packet)),
            Ok(_) => None,
            Err(err) => Some(Err(err)),
        })
    });

    let Ok((conn, tx, rx)) = remoc::Connect::framed(remoc::Cfg::default(), transport_tx, transport_rx).await
    else {
        return;
    };
    tokio::spawn(conn);

    serve_client(tx, rx).await;
}

async fn serve_client(mut tx: rch::base::Sender<String>, mut rx: rch::base::Receiver<String>) {
    while let Ok(Some(msg)) = rx.recv().await {
        if tx.send(msg.to_uppercase()).await.is_err() {
            break;
        }
    }
}
