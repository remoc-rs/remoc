use bytes::Buf;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    io::{self, ErrorKind},
    pin::Pin,
    sync::Mutex,
    task::{Context, Poll, ready},
};
use tokio::io::AsyncRead;
use tokio_util::sync::ReusableBoxFuture;

use super::{SizeInfo, bin, oneshot};
use crate::{chmux::DataBuf, codec};

/// An I/O channel receiver that implements [`AsyncRead`].
///
/// Reads binary data from the underlying channel.
/// Tracks bytes read and verifies size on completion.
pub struct Receiver<Codec = codec::Default> {
    /// The underlying binary channel receiver. Uses Mutex for serialization with &self.
    bin_receiver: Mutex<Option<bin::Receiver>>,
    /// Size information (known or to be received). Uses Option to allow taking during EOF handling.
    size_info: Mutex<Option<SizeInfo<Codec>>>,
    /// Total bytes read so far.
    bytes_read: u64,
    /// Current data buffer being consumed.
    current_buf: Option<DataBuf>,
    /// State of pending async operation.
    state: ReceiverState,
    /// Whether EOF has been reached and verified.
    eof_verified: bool,
}

enum ReceiverState {
    Idle,
    Receiving(ReusableBoxFuture<'static, Result<(Option<DataBuf>, bin::Receiver), io::Error>>),
    VerifyingSize(ReusableBoxFuture<'static, Result<u64, io::Error>>),
}

impl<Codec> fmt::Debug for Receiver<Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Receiver")
            .field("size", &self.size())
            .field("bytes_read", &self.bytes_read)
            .field("eof_verified", &self.eof_verified)
            .finish()
    }
}

/// A receiver in transport.
#[derive(Debug)]
struct TransportedReceiver<Codec> {
    /// The underlying binary receiver.
    bin_receiver: bin::Receiver,
    /// Size info for transport.
    size: SizeInfo<Codec>,
}

crate::versioned::compact::impl_struct! {
    TransportedReceiver<Codec>,
    fields {
        bin_receiver: bin::Receiver => "_0",
        size: SizeInfo<Codec> => "_1",
    }
    where Codec: codec::Codec
}

impl<Codec> Receiver<Codec> {
    /// Creates a new receiver.
    pub(super) fn new(bin_receiver: bin::Receiver, size_info: SizeInfo<Codec>) -> Self {
        Self {
            bin_receiver: Mutex::new(Some(bin_receiver)),
            size_info: Mutex::new(Some(size_info)),
            bytes_read: 0,
            current_buf: None,
            state: ReceiverState::Idle,
            eof_verified: false,
        }
    }

    /// Returns the total size of the data, if known.
    ///
    /// Returns `Some(size)` for channels created with [`sized`](super::sized),
    /// or after EOF has been received for channels created with [`channel`](super::channel).
    /// Returns `None` if size is not yet known.
    pub fn size(&self) -> Option<u64> {
        match &*self.size_info.lock().unwrap() {
            Some(SizeInfo::Determined(s)) => Some(*s),
            _ => None,
        }
    }

    /// Returns the total number of bytes read so far.
    pub fn bytes_received(&self) -> u64 {
        self.bytes_read
    }

    /// Returns the remaining bytes to be read, if size is known.
    ///
    /// Returns `Some(remaining)` for channels created with [`sized`](super::sized),
    /// or after EOF has been received for channels created with [`channel`](super::channel).
    /// Returns `None` if size is not yet known.
    pub fn remaining(&self) -> Option<u64> {
        self.size().map(|s| s.saturating_sub(self.bytes_read))
    }
}

async fn receive_data(mut bin_receiver: bin::Receiver) -> Result<(Option<DataBuf>, bin::Receiver), io::Error> {
    let chmux_receiver =
        bin_receiver.get().await.map_err(|e| io::Error::new(ErrorKind::ConnectionRefused, e.to_string()))?;
    let data = chmux_receiver.recv().await.map_err(io::Error::from)?;
    Ok((data, bin_receiver))
}

async fn receive_size<Codec: codec::Codec>(size_rx: oneshot::Receiver<u64, Codec>) -> Result<u64, io::Error> {
    size_rx.await.map_err(|e| io::Error::new(ErrorKind::UnexpectedEof, e.to_string()))
}

impl<Codec> Receiver<Codec>
where
    Codec: codec::Codec,
{
    /// Polls to complete any pending receive or verify operation.
    fn poll_complete(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let ReceiverState::Receiving(ref mut fut) = self.state {
            let (data, bin_receiver) = ready!(fut.poll(cx))?;
            // Only keep bin_receiver if we got data; on EOF (None) we won't need it
            if data.is_some() {
                *self.bin_receiver.lock().unwrap() = Some(bin_receiver);
            }
            self.current_buf = data;
            self.state = ReceiverState::Idle;
        }

        if let ReceiverState::VerifyingSize(ref mut fut) = self.state {
            let expected_size = ready!(fut.poll(cx))?;
            self.state = ReceiverState::Idle;

            // Store the received size
            *self.size_info.lock().unwrap() = Some(SizeInfo::Determined(expected_size));

            if self.bytes_read != expected_size {
                return Poll::Ready(Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    format!(
                        "size mismatch: expected {} bytes, received {} bytes",
                        expected_size, self.bytes_read
                    ),
                )));
            }

            self.eof_verified = true;
        }

        Poll::Ready(Ok(()))
    }

    /// Handles EOF: verifies size matches expected or starts async verification.
    fn start_eof_verification(&mut self) -> io::Result<()> {
        let size_info = self.size_info.lock().unwrap().take();
        match size_info {
            Some(SizeInfo::Determined(expected_size)) => {
                // Put it back for size() to work
                *self.size_info.lock().unwrap() = Some(SizeInfo::Determined(expected_size));

                if self.bytes_read != expected_size {
                    return Err(io::Error::new(
                        ErrorKind::UnexpectedEof,
                        format!(
                            "size mismatch: expected {} bytes, received {} bytes",
                            expected_size, self.bytes_read
                        ),
                    ));
                }
                self.eof_verified = true;
            }
            Some(SizeInfo::Undetermined(size_rx)) => {
                self.state = ReceiverState::VerifyingSize(ReusableBoxFuture::new(receive_size(size_rx)));
            }
            None => {
                // Already verified
                self.eof_verified = true;
            }
        }
        Ok(())
    }
}

impl<Codec> AsyncRead for Receiver<Codec>
where
    Codec: codec::Codec,
{
    fn poll_read(
        mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.as_mut().get_mut();

        loop {
            // Complete any pending operations
            ready!(this.poll_complete(cx))?;

            // If EOF has been verified, we're done
            if this.eof_verified {
                return Poll::Ready(Ok(()));
            }

            // Check if we've already reached the expected size
            let remaining_allowed = match &*this.size_info.lock().unwrap() {
                Some(SizeInfo::Determined(expected)) => Some(expected.saturating_sub(this.bytes_read)),
                _ => None,
            };

            if remaining_allowed == Some(0) {
                // We've read exactly the expected amount - signal EOF
                this.eof_verified = true;
                return Poll::Ready(Ok(()));
            }

            // Try to consume any buffered data
            if let Some(ref mut data_buf) = this.current_buf {
                if data_buf.has_remaining() {
                    let chunk = data_buf.chunk();
                    let mut to_copy = chunk.len().min(buf.remaining());

                    // Don't exceed expected size
                    if let Some(remaining) = remaining_allowed {
                        to_copy = to_copy.min(remaining as usize);
                    }

                    buf.put_slice(&chunk[..to_copy]);
                    data_buf.advance(to_copy);
                    this.bytes_read += to_copy as u64;
                    return Poll::Ready(Ok(()));
                } else {
                    this.current_buf = None;
                }
            }

            // Take bin_receiver to receive more data, or handle EOF if already consumed
            let bin_receiver = this.bin_receiver.lock().unwrap().take();
            match bin_receiver {
                Some(rx) => this.state = ReceiverState::Receiving(ReusableBoxFuture::new(receive_data(rx))),
                None => this.start_eof_verification()?,
            }
        }
    }
}

impl<Codec> Serialize for Receiver<Codec>
where
    Codec: codec::Codec,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let bin_receiver =
            self.bin_receiver.lock().unwrap().take().ok_or_else(|| {
                serde::ser::Error::custom("cannot serialize: channel already connected or closed")
            })?;

        let size = self
            .size_info
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| serde::ser::Error::custom("cannot serialize: size info already consumed"))?;

        TransportedReceiver::<Codec> { bin_receiver, size }.serialize(serializer)
    }
}

impl<'de, Codec> Deserialize<'de> for Receiver<Codec>
where
    Codec: codec::Codec,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let transported: TransportedReceiver<Codec> = TransportedReceiver::deserialize(deserializer)?;
        Ok(Self::new(transported.bin_receiver, transported.size))
    }
}
