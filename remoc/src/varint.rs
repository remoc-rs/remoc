//! Variable integer encoding.

use byteorder::{LE, ReadBytesExt, WriteBytesExt};
use std::{
    io::{Error, ErrorKind, Result},
    mem,
};

/// Variable integer writer.
pub(crate) trait VarintWrite {
    /// Write u32 in variable integer encoding if `varint` is true.
    fn write_v32(&mut self, n: u32, varint: bool) -> Result<usize>;
}

impl<T: std::io::Write> VarintWrite for T {
    fn write_v32(&mut self, n: u32, varint: bool) -> Result<usize> {
        if !varint {
            return self.write_u32::<LE>(n).map(|_| size_of::<u32>());
        }

        let mut buf = [0u8; varint_max::<u32>()];
        let used_buf = varint_u32(n, &mut buf);
        self.write_all(used_buf)?;
        Ok(used_buf.len())
    }
}

/// Variable integer reader.
pub(crate) trait VarintRead {
    /// Read u32 in variable integer encoding if `varint` is true.
    fn read_v32(&mut self, varint: bool) -> Result<u32>;
}

impl<T: std::io::Read> VarintRead for T {
    fn read_v32(&mut self, varint: bool) -> Result<u32> {
        if !varint {
            return self.read_u32::<LE>();
        }

        let mut decoder = VarintDecoder::new();
        loop {
            let val = self.read_u8()?;
            if let Some(out) = decoder.decode(val)? {
                break Ok(out);
            }
        }
    }
}

/// Variable integer decoder.
#[derive(Default, Clone, Debug)]
pub(crate) struct VarintDecoder {
    value: u32,
    pos: usize,
}

impl VarintDecoder {
    /// Maximum length of one variable integer.
    pub const MAX_LENGTH: usize = varint_max::<u32>();

    /// Creates a new instance.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode value.
    pub fn decode(&mut self, val: u8) -> Result<Option<u32>> {
        let carry = (val & 0x7F) as u32;
        self.value |= carry << (7 * self.pos);

        if (val & 0x80) == 0 {
            if self.pos == varint_max::<u32>() - 1 && val > max_of_last_byte::<u32>() {
                Err(Error::new(ErrorKind::InvalidData, "invalid varint"))
            } else {
                self.pos = 0;
                Ok(Some(mem::take(&mut self.value)))
            }
        } else {
            self.pos += 1;
            if self.pos < varint_max::<u32>() {
                Ok(None)
            } else {
                Err(Error::new(ErrorKind::InvalidData, "invalid varint"))
            }
        }
    }
}

/// Returns the maximum number of bytes required to encode T.
pub(crate) const fn varint_max<T: Sized>() -> usize {
    const BITS_PER_BYTE: usize = 8;
    const BITS_PER_VARINT_BYTE: usize = 7;

    let bits = size_of::<T>() * BITS_PER_BYTE;
    let roundup_bits = bits + (BITS_PER_VARINT_BYTE - 1);
    roundup_bits / BITS_PER_VARINT_BYTE
}

/// Returns the maximum value stored in the last encoded byte.
pub(crate) const fn max_of_last_byte<T: Sized>() -> u8 {
    let max_bits = size_of::<T>() * 8;
    let extra_bits = max_bits % 7;
    (1 << extra_bits) - 1
}

/// Encode u32 in variable integer encoding.
pub(crate) fn varint_u32(n: u32, out: &mut [u8; varint_max::<u32>()]) -> &mut [u8] {
    let mut value = n;
    for i in 0..varint_max::<u32>() {
        out[i] = value.to_le_bytes()[0];
        if value < 128 {
            return &mut out[..=i];
        }

        out[i] |= 0x80;
        value >>= 7;
    }
    debug_assert_eq!(value, 0);
    &mut out[..]
}
