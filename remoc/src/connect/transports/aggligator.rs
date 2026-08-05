use aggligator_transport_tcp::simple::{tcp_connect, tcp_server};
use remoc::prelude::*;
use std::net::{Ipv6Addr, SocketAddr};

/// Connects to a Remoc endpoint over aggregated TCP links.
pub async fn connect(
    target: &str, port: u16,
) -> Result<(rch::base::Sender<String>, rch::base::Receiver<String>), Box<dyn std::error::Error>> {
    // Links are established from every local interface to every address the target
    // resolves to, and failed links are reconnected without affecting the connection.
    let stream = tcp_connect([target], port).await?;

    // The aggregated connection is an AsyncRead and AsyncWrite pair like any other.
    let (stream_rx, stream_tx) = tokio::io::split(stream);

    let (conn, tx, rx) = remoc::Connect::io(remoc::Cfg::default(), stream_rx, stream_tx).await?;
    tokio::spawn(conn);

    Ok((tx, rx))
}

/// Serves Remoc endpoints over aggregated TCP links.
pub async fn serve(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let addr = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port);

    tcp_server(addr, |stream| async move {
        let (stream_rx, stream_tx) = tokio::io::split(stream);

        let Ok((conn, tx, rx)) = remoc::Connect::io(remoc::Cfg::default(), stream_rx, stream_tx).await else {
            return;
        };
        tokio::spawn(conn);

        serve_client(tx, rx).await;
    })
    .await?;

    Ok(())
}

async fn serve_client(mut tx: rch::base::Sender<String>, mut rx: rch::base::Receiver<String>) {
    while let Ok(Some(msg)) = rx.recv().await {
        if tx.send(msg.to_uppercase()).await.is_err() {
            break;
        }
    }
}
