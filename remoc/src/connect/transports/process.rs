use crate::{MyInitialReq, MyInitialRsp};
use remoc::prelude::*;
use std::process::Stdio;
use tokio::process::Command;

/// Spawns a child process and connects to the Remoc endpoint it serves on its
/// standard input and output.
pub async fn connect(
    program: &str,
) -> Result<
    (rch::base::Sender<MyInitialReq>, rch::base::Receiver<MyInitialRsp>),
    Box<dyn std::error::Error>,
> {
    let mut child =
        Command::new(program).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;

    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();

    let (conn, tx, rx) = remoc::Connect::io(remoc::Cfg::default(), stdout, stdin).await?;
    tokio::spawn(conn);

    Ok((tx, rx))
}

/// The counterpart running inside the child process.
pub async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    // Nothing here constrains the channel types, so they are named explicitly:
    // this end sends responses and receives requests.
    let (conn, mut tx, mut rx) =
        remoc::Connect::io::<_, _, MyInitialRsp, MyInitialReq, remoc::codec::Default>(
            remoc::Cfg::default(),
            tokio::io::stdin(),
            tokio::io::stdout(),
        )
        .await?;
    tokio::spawn(conn);

    while let Some(_req) = rx.recv().await? {
        // Handle the initial request here; from this point on your application
        // exchanges further channels and remote objects over the connection.
        tx.send(MyInitialRsp {}).await?;
    }

    Ok(())
}
