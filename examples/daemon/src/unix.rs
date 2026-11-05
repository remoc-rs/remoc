//! Local connections over Unix domain sockets.

use std::{fs, io, os::unix::fs::PermissionsExt};
use tokio::net::{UnixListener, UnixStream};

/// Default socket path, relative to the working directory.
pub const DEFAULT_ENDPOINT: &str = "timerd.sock";

/// A connected local client or server.
pub type Stream = UnixStream;

/// A listening socket, removed when the daemon shuts down normally.
pub struct Listener {
    inner: UnixListener,
    endpoint: String,
}

impl Listener {
    /// Bind without removing an existing socket.
    pub fn bind(endpoint: &str) -> io::Result<Self> {
        let listener = Self { inner: UnixListener::bind(endpoint)?, endpoint: endpoint.to_owned() };
        fs::set_permissions(endpoint, fs::Permissions::from_mode(0o600))?;
        Ok(listener)
    }

    /// Accept another client.
    pub async fn accept(&mut self) -> io::Result<Stream> {
        let (stream, _) = self.inner.accept().await?;
        Ok(stream)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        match fs::remove_file(&self.endpoint) {
            Ok(()) => (),
            Err(err) if err.kind() == io::ErrorKind::NotFound => (),
            Err(err) => eprintln!("Cannot remove socket {}: {err}", self.endpoint),
        }
    }
}

/// Connect to a running daemon.
pub async fn connect(endpoint: &str) -> io::Result<Stream> {
    UnixStream::connect(endpoint).await
}
