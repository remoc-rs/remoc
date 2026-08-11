//! IO transport wrappers.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures::{Sink, SinkExt};
use std::{
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};
use tokio::io::AsyncWrite;
use tokio_util::codec::{Decoder, Encoder, FramedWrite};

use crate::varint::{VarintDecoder, varint_max, varint_u32};

/// A codec for frames delimited by a header specifying their lengths.
#[derive(Debug, Clone)]
pub(crate) struct LengthCodec {
    /// Maximum frame length.
    max_frame_len: u32,
    /// Read state
    state: DecodeState,
    /// Decode buffer size.
    decode_buffer_size: Option<usize>,
    /// Use variable integer encoding for length field.
    varint: Arc<AtomicBool>,
    /// Variable integer decoder.
    varint_decoder: VarintDecoder,
}

#[derive(Debug, Clone, Copy)]
enum DecodeState {
    Header,
    Data(Header),
}

#[derive(Debug, Clone, Copy)]
struct Header {
    length: u32,
}

impl LengthCodec {
    const MAX_HEADER_LEN: usize =
        if VarintDecoder::MAX_LENGTH > size_of::<u32>() { VarintDecoder::MAX_LENGTH } else { size_of::<u32>() };

    /// Creates a new `LengthCodec` with the default configuration values.
    pub fn new(max_frame_len: u32, varint: Arc<AtomicBool>) -> Self {
        Self {
            max_frame_len,
            state: DecodeState::Header,
            decode_buffer_size: None,
            varint,
            varint_decoder: VarintDecoder::new(),
        }
    }

    /// Reserve at least `additional` space in buffer `buf`.
    ///
    /// When a reservation needs to be performed, at least
    /// [`Self::decode_buffer_size`] is reserved.
    fn reserve(&self, buf: &mut BytesMut, mut additional: usize) {
        let rem = buf.capacity() - buf.len();
        if additional <= rem {
            return;
        }

        if let Some(decode_buffer_size) = self.decode_buffer_size {
            additional = additional.max(decode_buffer_size);
        }

        buf.reserve(additional);
    }

    fn decode_header(&mut self, src: &mut BytesMut) -> io::Result<Option<Header>> {
        let length = if !self.varint.load(Ordering::Relaxed) {
            if src.len() < Self::MAX_HEADER_LEN {
                self.reserve(src, Self::MAX_HEADER_LEN);
                return Ok(None);
            }
            src.get_u32()
        } else {
            loop {
                if src.is_empty() {
                    self.reserve(src, Self::MAX_HEADER_LEN);
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

        self.reserve(src, (length as usize).saturating_sub(src.len()));

        Ok(Some(Header { length }))
    }

    fn decode_data(&self, header: Header, src: &mut BytesMut) -> io::Result<Option<BytesMut>> {
        if src.len() < header.length as usize {
            return Ok(None);
        }

        let data = src.split_to(header.length as usize);

        Ok(Some(data))
    }
}

impl Decoder for LengthCodec {
    type Item = BytesMut;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<BytesMut>> {
        if self.decode_buffer_size.is_none() {
            self.decode_buffer_size = Some(src.capacity());
        }

        let header = match self.state {
            DecodeState::Header => match self.decode_header(src)? {
                Some(header) => {
                    self.state = DecodeState::Data(header);
                    header
                }
                None => return Ok(None),
            },
            DecodeState::Data(header) => header,
        };

        match self.decode_data(header, src)? {
            Some(data) => {
                self.state = DecodeState::Header;
                self.reserve(src, src.len().saturating_sub(Self::MAX_HEADER_LEN));
                Ok(Some(data))
            }
            None => Ok(None),
        }
    }
}

impl Encoder<Bytes> for LengthCodec {
    type Error = io::Error;

    fn encode(&mut self, data: Bytes, dst: &mut BytesMut) -> io::Result<()> {
        dst.reserve(Self::MAX_HEADER_LEN + data.len());

        let length = data.len() as u32;
        if !self.varint.load(Ordering::Relaxed) {
            dst.put_u32(length);
        } else {
            let mut buf = [0u8; varint_max::<u32>()];
            let used_buf = varint_u32(length, &mut buf);
            dst.extend_from_slice(used_buf);
        }

        dst.extend_from_slice(&data[..]);

        Ok(())
    }
}

/// Inner part of flush filter.
pub(crate) struct FilterFlushInner<W> {
    inner: W,
    pub flush_allowed: bool,
}

impl<W> FilterFlushInner<W> {
    pub fn new(inner: W) -> Self {
        Self { inner, flush_allowed: false }
    }
}

impl<W> AsyncWrite for FilterFlushInner<W>
where
    W: AsyncWrite + Unpin,
{
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut Pin::into_inner(self).inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        if !self.flush_allowed {
            return Poll::Ready(Ok(()));
        }

        Pin::new(&mut Pin::into_inner(self).inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Pin::new(&mut Pin::into_inner(self).inner).poll_shutdown(cx)
    }
}

/// Filters flushes initiated by FramedWrite.
pub(crate) struct FilterFlushOuter<W>(pub FramedWrite<FilterFlushInner<W>, LengthCodec>);

impl<W> Sink<Bytes> for FilterFlushOuter<W>
where
    W: AsyncWrite + Unpin,
{
    type Error = io::Error;

    #[inline]
    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Pin::into_inner(self).0.poll_ready_unpin(cx)
    }

    #[inline]
    fn start_send(self: Pin<&mut Self>, item: Bytes) -> Result<(), Self::Error> {
        Pin::into_inner(self).0.start_send_unpin(item)
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let this = Pin::into_inner(self);
        this.0.get_mut().flush_allowed = true;
        let res = this.0.poll_flush_unpin(cx);
        this.0.get_mut().flush_allowed = false;
        res
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let this = Pin::into_inner(self);
        this.0.get_mut().flush_allowed = true;
        let res = this.0.poll_close_unpin(cx);
        this.0.get_mut().flush_allowed = false;
        res
    }
}
