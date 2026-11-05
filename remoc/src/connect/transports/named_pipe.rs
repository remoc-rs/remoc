use crate::{MyInitialReq, MyInitialRsp};
use remoc::prelude::*;
use std::time::Duration;
use tokio::{
    net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions},
    time::sleep,
};

const ERROR_PIPE_BUSY: i32 = 231;

/// Connects to a Remoc endpoint over a Windows named pipe.
pub async fn connect(
    pipe_name: &str,
) -> Result<
    (rch::base::Sender<MyInitialReq>, rch::base::Receiver<MyInitialRsp>),
    Box<dyn std::error::Error>,
> {
    let pipe = loop {
        match ClientOptions::new().open(pipe_name) {
            Ok(pipe) => break pipe,
            Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                sleep(Duration::from_millis(50)).await;
            }
            Err(err) => return Err(err.into()),
        }
    };
    let (pipe_rx, pipe_tx) = tokio::io::split(pipe);

    let (conn, tx, rx) =
        remoc::Connect::io(remoc::Cfg::default(), pipe_rx, pipe_tx).await?;
    tokio::spawn(conn);

    Ok((tx, rx))
}

/// Serves Remoc endpoints over a local Windows named pipe.
pub async fn serve(pipe_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut pipe = ServerOptions::new().first_pipe_instance(true).create(pipe_name)?;

    loop {
        pipe.connect().await?;
        let client = pipe;

        // Keep a listening instance available before handling this client.
        pipe = ServerOptions::new().create(pipe_name)?;

        tokio::spawn(async move {
            if let Err(err) = serve_client(client).await {
                eprintln!("Named-pipe client failed: {err}");
            }
        });
    }
}

async fn serve_client(pipe: NamedPipeServer) -> Result<(), Box<dyn std::error::Error>> {
    let (pipe_rx, pipe_tx) = tokio::io::split(pipe);
    let (conn, mut tx, mut rx) =
        remoc::Connect::io::<_, _, MyInitialRsp, MyInitialReq, remoc::codec::Default>(
            remoc::Cfg::default(),
            pipe_rx,
            pipe_tx,
        )
        .await?;
    tokio::spawn(conn);

    while let Some(_req) = rx.recv().await? {
        tx.send(MyInitialRsp {}).await?;
    }

    Ok(())
}
