use bytes::{Buf, Bytes, BytesMut};
use std::io;

/// Writes to an internal memory buffer with a limited maximum size.
pub struct LimitedBytesWriter {
    limit: usize,
    buf: BytesMut,
    overflown: bool,
}

impl LimitedBytesWriter {
    /// Creates a new limited writer.
    pub fn new(limit: usize) -> Self {
        Self { limit, buf: BytesMut::new(), overflown: false }
    }

    /// Returns the write buffer, if no overflow has occurred.
    /// Otherwise None is returned.
    pub fn into_inner(self) -> Option<BytesMut> {
        if self.overflown { None } else { Some(self.buf) }
    }

    /// True if limit has been reached.
    pub fn overflow(&self) -> bool {
        self.overflown
    }
}

impl io::Write for LimitedBytesWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.buf.len() + buf.len() <= self.limit && !self.overflown {
            self.buf.extend_from_slice(buf);
            Ok(buf.len())
        } else {
            self.overflown = true;
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "limit reached"))
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Forwards data over a mpsc channel.
///
/// This must not be used in an async thread.
pub struct ChannelBytesWriter {
    tx: tokio::sync::mpsc::Sender<BytesMut>,
    written: usize,
}

impl ChannelBytesWriter {
    /// Creates a new forwarding writer.
    pub fn new(tx: tokio::sync::mpsc::Sender<BytesMut>) -> Self {
        Self { tx, written: 0 }
    }

    /// Written bytes.
    ///
    /// Saturates at usize::MAX;
    pub fn written(&self) -> usize {
        self.written
    }
}

impl io::Write for ChannelBytesWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.tx.blocking_send(buf.into()) {
            Ok(()) => {
                self.written = self.written.saturating_add(buf.len());
                Ok(buf.len())
            }
            Err(_) => Err(io::Error::new(io::ErrorKind::BrokenPipe, "channel closed")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Reads data from an mpsc channel.
///
/// This must not be used in an async thread.
pub struct ChannelBytesReader {
    rx: tokio::sync::mpsc::Receiver<Result<Bytes, ()>>,
    buf: Bytes,
    failed: bool,
}

impl ChannelBytesReader {
    /// Creates a new reader.
    pub fn new(rx: tokio::sync::mpsc::Receiver<Result<Bytes, ()>>) -> Self {
        Self { rx, buf: Bytes::new(), failed: false }
    }
}

impl io::Read for ChannelBytesReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.buf.is_empty() {
            if self.failed {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "channel closed"));
            }

            match self.rx.blocking_recv() {
                Some(Ok(buf)) => self.buf = buf,
                Some(Err(())) => self.failed = true,
                None => return Ok(0),
            }
        }

        let len = buf.len().min(self.buf.len());
        buf[..len].copy_from_slice(&self.buf[..len]);
        self.buf.advance(len);
        Ok(len)
    }
}
