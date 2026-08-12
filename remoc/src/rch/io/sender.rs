use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    io::{self, ErrorKind},
    mem,
    pin::Pin,
    sync::Mutex,
    task::{Context, Poll, ready},
};
use tokio::io::AsyncWrite;
use tokio_util::sync::ReusableBoxFuture;

use super::{bin, oneshot};
use crate::codec;

/// Size handling mode for the sender.
#[derive(Debug)]
pub(super) enum SizeMode<Codec> {
    /// Size is known (either upfront or after shutdown).
    Known(u64),
    /// Size is unknown. Will send final byte count via oneshot on shutdown,
    /// then transition to Known.
    Unknown(oneshot::Sender<u64, Codec>),
}

crate::versioned::compact::impl_enum! {
    SizeMode<Codec>,
    variants {
        Known(size: u64) => "_0",
        Unknown(tx: oneshot::Sender<u64, Codec>) => "_1",
    }
    where Codec: codec::Codec
}

/// An I/O channel sender that implements [`AsyncWrite`].
///
/// Writes binary data to the underlying channel.
/// Tracks bytes written and enforces size limits if specified.
pub struct Sender<Codec = codec::Default> {
    /// The underlying binary channel sender. Uses Mutex for serialization with &self.
    bin_sender: Mutex<Option<bin::Sender>>,
    /// Size handling mode.
    size_mode: Mutex<SizeMode<Codec>>,
    /// Total bytes written so far.
    bytes_written: u64,
    /// Cached chunk size from chmux sender.
    chunk_size: Option<usize>,
    /// Pending connect operation, if any.
    connecting: Option<ReusableBoxFuture<'static, Result<(bin::Sender, usize), io::Error>>>,
    /// Pending send operation, if any.
    sending: Option<ReusableBoxFuture<'static, Result<(bin::Sender, u64), io::Error>>>,
}

impl<Codec> fmt::Debug for Sender<Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Sender")
            .field("expected_size", &self.expected_size())
            .field("bytes_written", &self.bytes_written)
            .finish()
    }
}

/// A sender in transport.
struct TransportedSender<Codec> {
    /// The underlying binary sender. None if already closed.
    bin_sender: Option<bin::Sender>,
    /// Size handling mode.
    size_mode: SizeMode<Codec>,
    /// Total bytes written so far.
    bytes_written: u64,
}

crate::versioned::compact::impl_struct! {
    TransportedSender<Codec>,
    fields {
        bin_sender: Option<bin::Sender> => "_0",
        size_mode: SizeMode<Codec> => "_1",
        bytes_written: u64 => "_2",
    }
    where Codec: codec::Codec
}

impl<Codec> Sender<Codec> {
    /// Creates a new sender.
    pub(super) fn new(bin_sender: bin::Sender, size_mode: SizeMode<Codec>) -> Self {
        Self {
            bin_sender: Mutex::new(Some(bin_sender)),
            size_mode: Mutex::new(size_mode),
            bytes_written: 0,
            chunk_size: None,
            connecting: None,
            sending: None,
        }
    }

    /// Returns the total number of bytes written so far.
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Returns the expected size, if known.
    pub fn expected_size(&self) -> Option<u64> {
        match &*self.size_mode.lock().unwrap() {
            SizeMode::Known(expected) => Some(*expected),
            SizeMode::Unknown(_) => None,
        }
    }

    /// Returns the remaining bytes that can be written, if size is known.
    pub fn remaining(&self) -> Option<u64> {
        self.expected_size().map(|s| s.saturating_sub(self.bytes_written))
    }
}

async fn send_data(mut bin_sender: bin::Sender, data: Bytes) -> Result<(bin::Sender, u64), io::Error> {
    let len = data.len() as u64;
    let chmux_sender =
        bin_sender.get().await.map_err(|e| io::Error::new(ErrorKind::ConnectionRefused, e.to_string()))?;
    chmux_sender.send(data).await.map_err(io::Error::from)?;
    Ok((bin_sender, len))
}

async fn connect_sender(mut bin_sender: bin::Sender) -> Result<(bin::Sender, usize), io::Error> {
    let chmux_sender =
        bin_sender.get().await.map_err(|e| io::Error::new(ErrorKind::ConnectionRefused, e.to_string()))?;
    let chunk_size = chmux_sender.chunk_size();
    Ok((bin_sender, chunk_size))
}

impl<Codec> Sender<Codec> {
    /// Polls to complete any pending connect or send operations.
    fn poll_complete(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(future) = &mut self.connecting {
            let (bin_sender, chunk_size) = ready!(future.poll(cx))?;
            self.chunk_size = Some(chunk_size);
            *self.bin_sender.lock().unwrap() = Some(bin_sender);
            self.connecting = None;
        }

        if let Some(future) = &mut self.sending {
            let (bin_sender, _bytes_sent) = ready!(future.poll(cx))?;
            *self.bin_sender.lock().unwrap() = Some(bin_sender);
            self.sending = None;
        }

        Poll::Ready(Ok(()))
    }

    /// Ensures connection is established and returns the chunk size.
    /// After this returns Ready(Ok(_)), bin_sender is back in place.
    fn poll_chunk_size(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        // Complete any pending operations
        ready!(self.poll_complete(cx))?;

        // If we already have chunk_size, return it
        if let Some(chunk_size) = self.chunk_size {
            return Poll::Ready(Ok(chunk_size));
        }

        // Start connecting
        let bin_sender = self
            .bin_sender
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| io::Error::new(ErrorKind::BrokenPipe, "channel closed"))?;

        self.connecting = Some(ReusableBoxFuture::new(connect_sender(bin_sender)));
        ready!(self.poll_complete(cx))?;

        Poll::Ready(Ok(self.chunk_size.unwrap()))
    }
}

impl<Codec> AsyncWrite for Sender<Codec>
where
    Codec: codec::Codec,
{
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.as_mut().get_mut();

        // Complete any pending operations first
        ready!(this.poll_complete(cx))?;

        // Ensure connection and get chunk size
        let chunk_size = ready!(this.poll_chunk_size(cx))?;

        // Take bin_sender
        let bin_sender = this
            .bin_sender
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| io::Error::new(ErrorKind::BrokenPipe, "channel closed"))?;

        if buf.is_empty() {
            *this.bin_sender.lock().unwrap() = Some(bin_sender);
            return Poll::Ready(Ok(0));
        }

        // Check size limit and calculate max write
        let max_write = match &*this.size_mode.lock().unwrap() {
            SizeMode::Known(expected) => {
                if this.bytes_written >= *expected {
                    *this.bin_sender.lock().unwrap() = Some(bin_sender);
                    return Poll::Ready(Err(io::Error::new(
                        ErrorKind::WriteZero,
                        format!("size limit of {} bytes reached", expected),
                    )));
                }
                let remaining = *expected - this.bytes_written;
                buf.len().min(remaining as usize)
            }
            SizeMode::Unknown(_) => buf.len(),
        };

        let write_len = max_write.min(chunk_size);
        this.bytes_written += write_len as u64;

        this.sending =
            Some(ReusableBoxFuture::new(send_data(bin_sender, Bytes::copy_from_slice(&buf[..write_len]))));
        Poll::Ready(Ok(write_len))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.as_mut().get_mut();
        ready!(this.poll_complete(cx))?;
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.as_mut().get_mut();

        // Complete any pending operations
        ready!(this.poll_complete(cx))?;

        // Close channel.
        *this.bin_sender.lock().unwrap() = None;

        // Handle size verification and notification based on mode
        match mem::replace(&mut *this.size_mode.lock().unwrap(), SizeMode::Known(this.bytes_written)) {
            SizeMode::Known(expected) if this.bytes_written == expected => Poll::Ready(Ok(())),
            SizeMode::Known(expected) => Poll::Ready(Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                format!(
                    "not enough data written: expected {} bytes but only {} bytes were written",
                    expected, this.bytes_written
                ),
            ))),
            SizeMode::Unknown(tx) => {
                let _ = tx.send(this.bytes_written);
                Poll::Ready(Ok(()))
            }
        }
    }
}

impl<Codec> Serialize for Sender<Codec>
where
    Codec: codec::Codec,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let bin_sender = self.bin_sender.lock().unwrap().take();
        let size_mode = mem::replace(
            &mut *self.size_mode.lock().unwrap(),
            SizeMode::Known(0), // Placeholder, sender is consumed anyway
        );

        TransportedSender { bin_sender, size_mode, bytes_written: self.bytes_written }.serialize(serializer)
    }
}

impl<'de, Codec> Deserialize<'de> for Sender<Codec>
where
    Codec: codec::Codec,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let transported: TransportedSender<Codec> = TransportedSender::deserialize(deserializer)?;

        Ok(Self {
            bin_sender: Mutex::new(transported.bin_sender),
            size_mode: Mutex::new(transported.size_mode),
            bytes_written: transported.bytes_written,
            chunk_size: None,
            connecting: None,
            sending: None,
        })
    }
}
