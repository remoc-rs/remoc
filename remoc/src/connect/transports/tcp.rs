use remoc::prelude::*;
use tokio::net::{TcpListener, TcpStream};

/// Connects to a Remoc endpoint over TCP.
pub async fn connect(
    addr: &str,
) -> Result<(rch::base::Sender<String>, rch::base::Receiver<String>), Box<dyn std::error::Error>> {
    let socket = TcpStream::connect(addr).await?;
    let (socket_rx, socket_tx) = socket.into_split();

    let (conn, tx, rx) = remoc::Connect::io(remoc::Cfg::default(), socket_rx, socket_tx).await?;
    tokio::spawn(conn);

    Ok((tx, rx))
}

/// Serves Remoc endpoints over TCP.
pub async fn serve(addr: &str) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(addr).await?;

    loop {
        let (socket, _peer) = listener.accept().await?;

        tokio::spawn(async move {
            let (socket_rx, socket_tx) = socket.into_split();

            let Ok((conn, tx, rx)) = remoc::Connect::io(remoc::Cfg::default(), socket_rx, socket_tx).await else {
                return;
            };
            tokio::spawn(conn);

            serve_client(tx, rx).await;
        });
    }
}

async fn serve_client(mut tx: rch::base::Sender<String>, mut rx: rch::base::Receiver<String>) {
    while let Ok(Some(msg)) = rx.recv().await {
        if tx.send(msg.to_uppercase()).await.is_err() {
            break;
        }
    }
}
