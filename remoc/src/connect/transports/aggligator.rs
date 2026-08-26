use crate::{MyInitialReq, MyInitialRsp};
use aggligator_transport_tcp::simple::{tcp_connect, tcp_server};
use remoc::prelude::*;
use std::net::{Ipv6Addr, SocketAddr};

/// Connects to a Remoc endpoint over aggregated TCP links.
pub async fn connect(
    target: &str, port: u16,
) -> Result<
    (rch::base::Sender<MyInitialReq>, rch::base::Receiver<MyInitialRsp>),
    Box<dyn std::error::Error>,
> {
    // Links are established from every local interface to every address the target
    // resolves to, and failed links are reconnected without affecting the connection.
    let stream = tcp_connect([target], port).await?;

    // Aggligator preserves message boundaries, so it is split into a sink and a
    // stream of messages and used as a framed transport.
    let (stream_rx, stream_tx) = stream.into_split();

    let (conn, tx, rx) =
        remoc::Connect::framed(remoc::Cfg::default(), stream_tx, stream_rx).await?;
    tokio::spawn(conn);

    Ok((tx, rx))
}

/// Serves Remoc endpoints over aggregated TCP links.
pub async fn serve(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let addr = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port);

    tcp_server(addr, |stream| async move {
        let (stream_rx, stream_tx) = stream.into_split();

        let Ok((conn, tx, rx)) =
            remoc::Connect::framed(remoc::Cfg::default(), stream_tx, stream_rx).await
        else {
            return;
        };
        tokio::spawn(conn);

        serve_client(tx, rx).await;
    })
    .await?;

    Ok(())
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
