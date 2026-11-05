//! Local connections over Windows named pipes.

use std::{io, time::Duration};
use tokio::{
    net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions},
    time::sleep,
};

/// Default local pipe name.
pub const DEFAULT_ENDPOINT: &str = r"\\.\pipe\remoc-timerd";

/// A connected server-side pipe.
pub type Stream = NamedPipeServer;

/// A named-pipe listener with an instance ready for the next client.
pub struct Listener {
    pipe: NamedPipeServer,
    endpoint: String,
}

impl Listener {
    /// Claim the pipe name, failing if a daemon already owns an instance.
    pub fn bind(endpoint: &str) -> io::Result<Self> {
        let pipe = ServerOptions::new().first_pipe_instance(true).create(endpoint)?;
        Ok(Self { pipe, endpoint: endpoint.to_owned() })
    }

    /// Accept a client and immediately make another instance available.
    pub async fn accept(&mut self) -> io::Result<Stream> {
        self.pipe.connect().await?;
        let next = ServerOptions::new().create(&self.endpoint)?;
        Ok(std::mem::replace(&mut self.pipe, next))
    }
}

/// Connect to a running daemon, retrying when its pipe instances are busy.
pub async fn connect(endpoint: &str) -> io::Result<NamedPipeClient> {
    const ERROR_PIPE_BUSY: i32 = 231;

    loop {
        match ClientOptions::new().open(endpoint) {
            Ok(pipe) => return Ok(pipe),
            Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                sleep(Duration::from_millis(50)).await;
            }
            Err(err) => return Err(err),
        }
    }
}
