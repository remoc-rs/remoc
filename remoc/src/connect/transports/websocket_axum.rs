use crate::{MyInitialReq, MyInitialRsp};
use axum::{
    Router,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::Response,
    routing::any,
    serve::ListenerExt,
};
use bytes::Bytes;
use futures::{SinkExt, StreamExt, future};
use remoc::prelude::*;
use tokio::net::TcpListener;

/// Serves a Remoc endpoint at `/remoc` over WebSocket.
pub async fn serve(addr: &str) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(addr).await?.tap_io(|tcp| {
        let _ = tcp.set_nodelay(true);
    });

    axum::serve(listener, router()).await?;
    Ok(())
}

/// The router serving the Remoc endpoint at `/remoc`.
pub fn router() -> Router {
    Router::new().route(
        "/remoc",
        any(async |ws: WebSocketUpgrade| -> Response { ws.on_upgrade(serve_websocket) }),
    )
}

async fn serve_websocket(websocket: WebSocket) {
    let (websocket_tx, websocket_rx) = websocket.split();

    // Each chmux packet is carried as one binary WebSocket message.
    let transport_tx = websocket_tx.with(|packet: Bytes| {
        future::ready(Ok::<_, axum::Error>(Message::Binary(packet)))
    });

    // Ping, pong, text and close messages are not part of the Remoc transport.
    let transport_rx = websocket_rx.filter_map(|message| {
        future::ready(match message {
            Ok(Message::Binary(packet)) => Some(Ok(packet)),
            Ok(_) => None,
            Err(err) => Some(Err(err)),
        })
    });

    let Ok((conn, tx, rx)) =
        remoc::Connect::framed(remoc::Cfg::default(), transport_tx, transport_rx).await
    else {
        return;
    };
    tokio::spawn(conn);

    serve_client(tx, rx).await;
}

async fn serve_client(
    mut tx: rch::base::Sender<MyInitialRsp>, mut rx: rch::base::Receiver<MyInitialReq>,
) {
    while let Ok(Some(_req)) = rx.recv().await {
        // Handle the initial request here; from this point on your application
        // exchanges further channels and remote objects over the connection.
        if tx.send(MyInitialRsp {}).await.is_err() {
            break;
        }
    }
}
