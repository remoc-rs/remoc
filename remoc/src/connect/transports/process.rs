use remoc::prelude::*;
use std::process::Stdio;
use tokio::process::Command;

/// Spawns a child process and connects to the Remoc endpoint it serves on its
/// standard input and output.
pub async fn connect(
    program: &str,
) -> Result<(rch::base::Sender<String>, rch::base::Receiver<String>), Box<dyn std::error::Error>> {
    let mut child = Command::new(program).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;

    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();

    let (conn, tx, rx) = remoc::Connect::io(remoc::Cfg::default(), stdout, stdin).await?;
    tokio::spawn(conn);

    Ok((tx, rx))
}

/// The counterpart running inside the child process.
pub async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let (conn, mut tx, mut rx) = remoc::Connect::io::<_, _, String, String, remoc::codec::Default>(
        remoc::Cfg::default(),
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
    .await?;
    tokio::spawn(conn);

    while let Some(msg) = rx.recv().await? {
        tx.send(msg.to_uppercase()).await?;
    }

    Ok(())
}
