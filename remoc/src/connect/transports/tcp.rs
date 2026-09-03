use crate::{MyInitialReq, MyInitialRsp};
use remoc::prelude::*;
use tokio::net::{TcpListener, TcpStream};

/// Connects to a Remoc endpoint over TCP.
pub async fn connect(
    addr: &str,
) -> Result<
    (rch::base::Sender<MyInitialReq>, rch::base::Receiver<MyInitialRsp>),
    Box<dyn std::error::Error>,
> {
    let socket = TcpStream::connect(addr).await?;
    socket.set_nodelay(true)?;

    let (socket_rx, socket_tx) = socket.into_split();

    let (conn, tx, rx) =
        remoc::Connect::io(remoc::Cfg::default(), socket_rx, socket_tx).await?;
    tokio::spawn(conn);

    Ok((tx, rx))
}

/// Serves Remoc endpoints over TCP.
pub async fn serve(addr: &str) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(addr).await?;

    loop {
        let (socket, _peer) = listener.accept().await?;

        tokio::spawn(async move {
            let Ok(()) = socket.set_nodelay(true) else { return };

            let (socket_rx, socket_tx) = socket.into_split();

            let Ok((conn, tx, rx)) =
                remoc::Connect::io(remoc::Cfg::default(), socket_rx, socket_tx).await
            else {
                return;
            };
            tokio::spawn(conn);

            serve_client(tx, rx).await;
        });
    }
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
