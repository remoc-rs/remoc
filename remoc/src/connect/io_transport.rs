//! IO transport wrappers.

use bytes::{Buf, BufMut, Bytes, BytesMut, buf::Limit};
use futures::{Sink, Stream, ready};
use std::{
    collections::VecDeque,
    io::{self, IoSlice},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::io::poll_read_buf;

use crate::varint::{VarintDecoder, varint_max, varint_u32};

/// A codec for the length headers of frames.
#[derive(Debug, Clone)]
pub(crate) struct LengthCodec {
    /// Maximum frame length.
    max_frame_len: u32,
    /// Use variable integer encoding for length field.
    varint: Arc<AtomicBool>,
    /// Variable integer decoder.
    varint_decoder: VarintDecoder,
}

impl LengthCodec {
    /// Creates a new `LengthCodec` with the default configuration values.
    pub fn new(max_frame_len: u32, varint: Arc<AtomicBool>) -> Self {
        Self { max_frame_len, varint, varint_decoder: VarintDecoder::new() }
    }

    /// Decodes the length header at the start of `src`, removing it.
    ///
    /// Returns `None` if `src` does not contain the complete header yet.
    fn decode_header(&mut self, src: &mut BytesMut) -> io::Result<Option<usize>> {
        let length = if !self.varint.load(Ordering::Relaxed) {
            if src.len() < size_of::<u32>() {
                return Ok(None);
            }
            src.get_u32_le()
        } else {
            loop {
                if src.is_empty() {
                    return Ok(None);
                }

                if let Some(length) = self.varint_decoder.decode(src.get_u8())? {
                    break length;
                }
            }
        };

        if length > self.max_frame_len {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame exceeds maximum size"));
        }

        Ok(Some(length as usize))
    }

    /// Whether part of a length header has been decoded.
    fn has_partial_header(&self) -> bool {
        !self.varint_decoder.is_empty()
    }

    /// Appends the length header of a frame of `len` bytes to `dst`.
    fn encode_header(&self, len: usize, dst: &mut BytesMut) {
        let length = len as u32;
        if !self.varint.load(Ordering::Relaxed) {
            dst.put_u32_le(length);
        } else {
            let mut buf = [0u8; varint_max::<u32>()];
            let used_buf = varint_u32(length, &mut buf);
            dst.extend_from_slice(used_buf);
        }
    }
}

/// Maximum number of buffers passed to a vectored write.
const MAX_SLICES: usize = 64;

/// Sink writing frames, each prefixed by its length header, to an [AsyncWrite].
///
/// Length headers and small frames are collected in a buffer, while large frames
/// are written directly from their own buffers using vectored writes, so that
/// their data is not copied. The underlying writer is only flushed when the sink
/// is flushed or closed.
pub(crate) struct FrameSink<W> {
    inner: W,
    codec: LengthCodec,
    /// Data to write, in order, followed by the data in `buf`.
    queue: VecDeque<Bytes>,
    /// Bytes in `queue`.
    queued: usize,
    /// Collects length headers and small frames.
    buf: BytesMut,
    /// Amount of pending data from which no further frames are accepted
    /// until it has been written.
    backpressure: usize,
}

impl<W: AsyncWrite + Unpin> FrameSink<W> {
    /// Minimum size of a frame to be written directly from its buffer.
    const DIRECT_MIN: usize = 4_096;

    /// Creates a new frame sink, buffering up to `capacity` bytes before writing.
    pub fn new(inner: W, codec: LengthCodec, capacity: usize) -> Self {
        Self {
            inner,
            codec,
            queue: VecDeque::new(),
            queued: 0,
            buf: BytesMut::with_capacity(capacity),
            backpressure: capacity,
        }
    }

    /// Amount of data not written yet.
    fn pending(&self) -> usize {
        self.queued + self.buf.len()
    }

    /// Writes all pending data to the underlying writer.
    fn poll_write_pending(&mut self, cx: &mut Context) -> Poll<io::Result<()>> {
        if !self.buf.is_empty() {
            self.queued += self.buf.len();
            self.queue.push_back(self.buf.split().freeze());
        }

        while self.queued > 0 {
            let mut slices = [IoSlice::new(&[]); MAX_SLICES];
            let cnt = self.queue.len().min(MAX_SLICES);
            for (slice, data) in slices.iter_mut().zip(&self.queue) {
                *slice = IoSlice::new(data);
            }

            let mut n = ready!(Pin::new(&mut self.inner).poll_write_vectored(cx, &slices[..cnt]))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }

            self.queued -= n;
            while n > 0 {
                let front = self.queue.front_mut().unwrap();
                if n < front.len() {
                    front.advance(n);
                    n = 0;
                } else {
                    n -= front.len();
                    self.queue.pop_front();
                }
            }
        }

        Poll::Ready(Ok(()))
    }
}

impl<W: AsyncWrite + Unpin> Sink<Bytes> for FrameSink<W> {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let this = Pin::into_inner(self);
        if this.pending() >= this.backpressure {
            ready!(this.poll_write_pending(cx))?;
        }
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Bytes) -> Result<(), Self::Error> {
        let this = Pin::into_inner(self);
        this.codec.encode_header(item.len(), &mut this.buf);

        if item.len() >= Self::DIRECT_MIN && this.inner.is_write_vectored() {
            this.queued += this.buf.len() + item.len();
            this.queue.push_back(this.buf.split().freeze());
            this.queue.push_back(item);
        } else {
            this.buf.extend_from_slice(&item);
        }

        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let this = Pin::into_inner(self);
        ready!(this.poll_write_pending(cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let this = Pin::into_inner(self);
        ready!(this.poll_write_pending(cx))?;
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

/// Size of reads while no frame is being received.
const READ_SIZE: usize = 8_192;

/// Stream reading frames, each prefixed by its length header, from an [AsyncRead].
///
/// Frames that have been received completely while reading are copied out of the
/// read buffer, so that it can be reused. The rest of a frame that has not been
/// received completely is read directly into its own buffer, so that its data is
/// not copied.
pub(crate) struct FrameStream<R> {
    inner: R,
    codec: LengthCodec,
    /// Received data not decoded yet.
    buf: BytesMut,
    /// Frame whose rest is being read directly into it, limited to its remaining length.
    frame: Option<Limit<BytesMut>>,
}

impl<R: AsyncRead + Unpin> FrameStream<R> {
    /// Creates a new frame stream.
    pub fn new(inner: R, codec: LengthCodec) -> Self {
        Self { inner, codec, buf: BytesMut::with_capacity(READ_SIZE), frame: None }
    }
}

impl<R: AsyncRead + Unpin> Stream for FrameStream<R> {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let this = Pin::into_inner(self);

        loop {
            match &mut this.frame {
                Some(frame) if frame.remaining_mut() > 0 => {
                    if ready!(poll_read_buf(Pin::new(&mut this.inner), cx, frame))? == 0 {
                        return Poll::Ready(Some(Err(io::ErrorKind::UnexpectedEof.into())));
                    }
                }
                Some(_) => return Poll::Ready(this.frame.take().map(|frame| Ok(frame.into_inner().freeze()))),
                None => match this.codec.decode_header(&mut this.buf)? {
                    Some(length) if this.buf.len() >= length => {
                        let frame = Bytes::copy_from_slice(&this.buf[..length]);
                        this.buf.advance(length);
                        return Poll::Ready(Some(Ok(frame)));
                    }
                    Some(length) => {
                        let mut frame = BytesMut::with_capacity(length);
                        frame.extend_from_slice(&this.buf);
                        this.buf.clear();
                        let remaining = length - frame.len();
                        this.frame = Some(frame.limit(remaining));
                    }
                    None => {
                        if this.buf.capacity() - this.buf.len() < READ_SIZE {
                            this.buf.reserve(READ_SIZE);
                        }
                        if ready!(poll_read_buf(Pin::new(&mut this.inner), cx, &mut this.buf))? == 0 {
                            if this.buf.is_empty() && !this.codec.has_partial_header() {
                                return Poll::Ready(None);
                            }
                            return Poll::Ready(Some(Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "connection closed within a frame",
                            ))));
                        }
                    }
                },
            }
        }
    }
}
