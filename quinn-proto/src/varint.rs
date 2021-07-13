use std::{convert::TryInto, fmt};

use bytes::{Buf, BufMut};
use thiserror::Error;

use crate::coding::{self, Codec, UnexpectedEnd};

#[cfg(feature = "arbitrary")]
use arbitrary::Arbitrary;

/// An integer less than 2^62
///
/// Values of this type are suitable for encoding as QUIC variable-length integer.
// It would be neat if we could express to Rust that the top two bits are available for use as enum
// discriminants
#[derive(Default, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct VarInt(pub(crate) u64);

impl VarInt {
    /// The largest representable value
    pub const MAX: VarInt = VarInt((1 << 62) - 1);
    /// The largest encoded value length
    pub const MAX_SIZE: usize = 8;

    /// Construct a `VarInt` infallibly
    pub const fn from_u32(x: u32) -> Self {
        VarInt(x as u64)
    }

    /// Succeeds iff `x` < 2^62
    pub fn from_u64(x: u64) -> Result<Self, VarIntBoundsExceeded> {
        if x < 2u64.pow(62) {
            Ok(VarInt(x))
        } else {
            Err(VarIntBoundsExceeded)
        }
    }

    /// Create a VarInt without ensuring it's in range
    ///
    /// # Safety
    ///
    /// `x` must be less than 2^62.
    pub const unsafe fn from_u64_unchecked(x: u64) -> Self {
        VarInt(x)
    }

    /// Extract the integer value
    pub const fn into_inner(self) -> u64 {
        self.0
    }

    /// Compute the number of bytes needed to encode this value
    pub fn size(self) -> usize {
        let x = self.0;
        if x < 2u64.pow(6) {
            1
        } else if x < 2u64.pow(14) {
            2
        } else if x < 2u64.pow(30) {
            4
        } else if x < 2u64.pow(62) {
            8
        } else {
            unreachable!("malformed VarInt");
        }
    }

    /// Length of an encoded value from its first byte
    pub fn encoded_size(first: u8) -> usize {
        2usize.pow((first >> 6) as u32)
    }
}

impl From<VarInt> for u64 {
    fn from(x: VarInt) -> u64 {
        x.0
    }
}

impl From<u8> for VarInt {
    fn from(x: u8) -> Self {
        VarInt(x.into())
    }
}

impl From<u16> for VarInt {
    fn from(x: u16) -> Self {
        VarInt(x.into())
    }
}

impl From<u32> for VarInt {
    fn from(x: u32) -> Self {
        VarInt(x.into())
    }
}

impl std::convert::TryFrom<u64> for VarInt {
    type Error = VarIntBoundsExceeded;
    /// Succeeds iff `x` < 2^62
    fn try_from(x: u64) -> Result<Self, VarIntBoundsExceeded> {
        VarInt::from_u64(x)
    }
}

impl std::convert::TryFrom<u128> for VarInt {
    type Error = VarIntBoundsExceeded;
    /// Succeeds iff `x` < 2^62
    fn try_from(x: u128) -> Result<Self, VarIntBoundsExceeded> {
        VarInt::from_u64(x.try_into().map_err(|_| VarIntBoundsExceeded)?)
    }
}

impl std::convert::TryFrom<usize> for VarInt {
    type Error = VarIntBoundsExceeded;
    /// Succeeds iff `x` < 2^62
    fn try_from(x: usize) -> Result<Self, VarIntBoundsExceeded> {
        VarInt::try_from(x as u64)
    }
}

impl fmt::Debug for VarInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Display for VarInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(feature = "arbitrary")]
impl Arbitrary for VarInt {
    fn arbitrary(u: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<Self> {
        Ok(VarInt(u.int_in_range(0..=VarInt::MAX.0)?))
    }
}

/// Error returned when constructing a `VarInt` from a value >= 2^62
#[derive(Debug, Copy, Clone, Eq, PartialEq, Error)]
#[error("value too large for varint encoding")]
pub struct VarIntBoundsExceeded;

impl Codec for VarInt {
    fn decode<B: Buf>(r: &mut B) -> coding::Result<Self> {
        if !r.has_remaining() {
            return Err(UnexpectedEnd);
        }
        let mut buf = [0; 8];
        // This does not make use of `r.get_u8()` to avoid the additional bounds
        // check. We already performed this one earlier on.
        buf[0] = r.chunk()[0];
        r.advance(1);

        let tag = buf[0] >> 6;
        buf[0] &= 0b0011_1111;
        let x = match tag {
            0b00 => u64::from(buf[0]),
            0b01 => {
                if r.remaining() < 1 {
                    return Err(UnexpectedEnd);
                }

                // A little-endian buffer is used here since that's our most
                // likely target platform and it performs slightly better than
                // the be conversion. For bigger numbers we don't bother, because
                // it makes the byte copy more complicated.
                let mut b = [0, buf[0]];
                b[0] = r.chunk()[0];
                r.advance(1);

                u64::from(u16::from_le_bytes(b))
            }
            0b10 => {
                if r.remaining() < 3 {
                    return Err(UnexpectedEnd);
                }

                // We performed the bounds check upfront
                unsafe { copy_unchecked(r, &mut buf[1..4]) };

                u64::from(u32::from_be_bytes(buf[..4].try_into().unwrap()))
            }
            0b11 => {
                if r.remaining() < 7 {
                    return Err(UnexpectedEnd);
                }

                // We performed the bounds check upfront
                unsafe { copy_unchecked(r, &mut buf[1..8]) };

                u64::from_be_bytes(buf)
            }
            _ => unreachable!(),
        };
        Ok(VarInt(x))
    }

    fn encode<B: BufMut>(&self, w: &mut B) {
        let x = self.0;
        if x < 2u64.pow(6) {
            w.put_u8(x as u8);
        } else if x < 2u64.pow(14) {
            w.put_u16(0b01 << 14 | x as u16);
        } else if x < 2u64.pow(30) {
            w.put_u32(0b10 << 30 | x as u32);
        } else if x < 2u64.pow(62) {
            w.put_u64(0b11 << 62 | x);
        } else {
            unreachable!("malformed VarInt")
        }
    }
}

unsafe fn copy_unchecked<B: Buf>(src: &mut B, dst: &mut [u8]) {
    let mut off = 0;
    let len = dst.len();

    while off < dst.len() {
        let chunk = src.chunk();
        let to_copy = chunk.len().min(len - off);

        std::ptr::copy_nonoverlapping(chunk.as_ptr(), dst[off..].as_mut_ptr(), to_copy);

        off += to_copy;
        src.advance(to_copy);
    }
}
