use std::{cmp::Ordering, io, ops::Range, str};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use thiserror::Error;

use crate::{
    coding::{self, BufExt, BufMutExt},
    crypto, ConnectionId,
};

// Due to packet number encryption, it is impossible to fully decode a header
// (which includes a variable-length packet number) without crypto context.
// The crypto context (represented by the `Crypto` type in Quinn) is usually
// part of the `Connection`, or can be derived from the destination CID for
// Initial packets.
//
// To cope with this, we decode the invariant header (which should be stable
// across QUIC versions), which gives us the destination CID and allows us
// to inspect the version and packet type (which depends on the version).
// This information allows us to fully decode and decrypt the packet.
#[derive(Debug)]
pub struct PartialDecode {
    plain_header: PlainHeader,
    buf: io::Cursor<BytesMut>,
}

impl PartialDecode {
    #![allow(clippy::len_without_is_empty)]
    pub fn new(
        bytes: BytesMut,
        local_cid_len: usize,
        supported_versions: &[u32],
    ) -> Result<(Self, Option<BytesMut>), PacketDecodeError> {
        let mut buf = io::Cursor::new(bytes);
        let plain_header = PlainHeader::decode(&mut buf, local_cid_len, supported_versions)?;
        let dgram_len = buf.get_ref().len();
        let packet_len = plain_header
            .payload_len()
            .map(|len| (buf.position() + len) as usize)
            .unwrap_or(dgram_len);
        match dgram_len.cmp(&packet_len) {
            Ordering::Equal => Ok((Self { plain_header, buf }, None)),
            Ordering::Less => Err(PacketDecodeError::InvalidHeader(
                "packet too short to contain payload length",
            )),
            Ordering::Greater => {
                let rest = Some(buf.get_mut().split_off(packet_len));
                Ok((Self { plain_header, buf }, rest))
            }
        }
    }

    /// The underlying partially-decoded packet data
    pub(crate) fn data(&self) -> &[u8] {
        &self.buf.get_ref()
    }

    pub(crate) fn has_long_header(&self) -> bool {
        !matches!(self.plain_header, PlainHeader::Short { .. })
    }

    pub(crate) fn is_initial(&self) -> bool {
        self.space() == Some(SpaceId::Initial)
    }

    pub(crate) fn space(&self) -> Option<SpaceId> {
        use self::PlainHeader::*;
        match self.plain_header {
            Initial { .. } => Some(SpaceId::Initial),
            Long {
                ty: LongType::Handshake,
                ..
            } => Some(SpaceId::Handshake),
            Long {
                ty: LongType::ZeroRtt,
                ..
            } => Some(SpaceId::Data),
            Short { .. } => Some(SpaceId::Data),
            _ => None,
        }
    }

    pub(crate) fn is_0rtt(&self) -> bool {
        match self.plain_header {
            PlainHeader::Long { ty, .. } => ty == LongType::ZeroRtt,
            _ => false,
        }
    }

    pub(crate) fn dst_cid(&self) -> ConnectionId {
        self.plain_header.dst_cid()
    }

    /// Length of QUIC packet being decoded
    pub fn len(&self) -> usize {
        self.buf.get_ref().len()
    }

    pub(crate) fn finish<H>(self, header_crypto: Option<&H>) -> Result<Packet, PacketDecodeError>
    where
        H: crypto::HeaderKey,
    {
        use self::PlainHeader::*;
        let Self {
            plain_header,
            mut buf,
        } = self;

        if let Initial {
            dst_cid,
            src_cid,
            token_pos,
            version,
            ..
        } = plain_header
        {
            let number = Self::decrypt_header(&mut buf, header_crypto.unwrap())?;
            let header_len = buf.position() as usize;
            let mut bytes = buf.into_inner();

            let header_data = bytes.split_to(header_len).freeze();
            let token = header_data.slice(token_pos.start..token_pos.end);
            return Ok(Packet {
                header: Header::Initial {
                    dst_cid,
                    src_cid,
                    token,
                    number,
                    version,
                },
                header_data,
                payload: bytes,
            });
        }

        let header = match plain_header {
            Long {
                ty,
                dst_cid,
                src_cid,
                version,
                ..
            } => Header::Long {
                ty,
                dst_cid,
                src_cid,
                number: Self::decrypt_header(&mut buf, header_crypto.unwrap())?,
                version,
            },
            Retry {
                dst_cid,
                src_cid,
                version,
            } => Header::Retry {
                dst_cid,
                src_cid,
                version,
            },
            Short { spin, dst_cid, .. } => {
                let number = Self::decrypt_header(&mut buf, header_crypto.unwrap())?;
                let key_phase = buf.get_ref()[0] & KEY_PHASE_BIT != 0;
                Header::Short {
                    spin,
                    key_phase,
                    dst_cid,
                    number,
                }
            }
            VersionNegotiate {
                random,
                dst_cid,
                src_cid,
            } => Header::VersionNegotiate {
                random,
                dst_cid,
                src_cid,
            },
            Initial { .. } => unreachable!(),
        };

        let header_len = buf.position() as usize;
        let mut bytes = buf.into_inner();
        Ok(Packet {
            header,
            header_data: bytes.split_to(header_len).freeze(),
            payload: bytes,
        })
    }

    fn decrypt_header<H>(
        buf: &mut io::Cursor<BytesMut>,
        header_crypto: &H,
    ) -> Result<PacketNumber, PacketDecodeError>
    where
        H: crypto::HeaderKey,
    {
        let packet_length = buf.get_ref().len();
        let pn_offset = buf.position() as usize;
        if packet_length < pn_offset + 4 + header_crypto.sample_size() {
            return Err(PacketDecodeError::InvalidHeader(
                "packet too short to extract header protection sample",
            ));
        }

        header_crypto.decrypt(pn_offset, buf.get_mut());

        let len = PacketNumber::decode_len(buf.get_ref()[0]);
        PacketNumber::decode(len, buf)
    }
}

pub(crate) struct Packet {
    pub(crate) header: Header,
    pub(crate) header_data: Bytes,
    pub(crate) payload: BytesMut,
}

impl Packet {
    pub fn reserved_bits_valid(&self) -> bool {
        let mask = match self.header {
            Header::Short { .. } => SHORT_RESERVED_BITS,
            _ => LONG_RESERVED_BITS,
        };
        self.header_data[0] & mask == 0
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Header {
    Initial {
        dst_cid: ConnectionId,
        src_cid: ConnectionId,
        token: Bytes,
        number: PacketNumber,
        version: u32,
    },
    Long {
        ty: LongType,
        dst_cid: ConnectionId,
        src_cid: ConnectionId,
        number: PacketNumber,
        version: u32,
    },
    Retry {
        dst_cid: ConnectionId,
        src_cid: ConnectionId,
        version: u32,
    },
    Short {
        spin: bool,
        key_phase: bool,
        dst_cid: ConnectionId,
        number: PacketNumber,
    },
    VersionNegotiate {
        random: u8,
        src_cid: ConnectionId,
        dst_cid: ConnectionId,
    },
}

impl Header {
    pub(crate) fn encode(&self, w: &mut Vec<u8>) -> PartialEncode {
        use self::Header::*;
        let start = w.len();
        match *self {
            Initial {
                ref dst_cid,
                ref src_cid,
                ref token,
                number,
                version,
            } => {
                w.write(u8::from(LongHeaderType::Initial) | number.tag());
                w.write(version);
                dst_cid.encode_long(w);
                src_cid.encode_long(w);
                w.write_var(token.len() as u64);
                w.put_slice(token);
                w.write::<u16>(0); // Placeholder for payload length; see `set_payload_length`
                number.encode(w);
                PartialEncode {
                    start,
                    header_len: w.len() - start,
                    pn: Some((number.len(), true)),
                }
            }
            Long {
                ty,
                ref dst_cid,
                ref src_cid,
                number,
                version,
            } => {
                w.write(u8::from(LongHeaderType::Standard(ty)) | number.tag());
                w.write(version);
                dst_cid.encode_long(w);
                src_cid.encode_long(w);
                w.write::<u16>(0); // Placeholder for payload length; see `set_payload_length`
                number.encode(w);
                PartialEncode {
                    start,
                    header_len: w.len() - start,
                    pn: Some((number.len(), true)),
                }
            }
            Retry {
                ref dst_cid,
                ref src_cid,
                version,
            } => {
                w.write(u8::from(LongHeaderType::Retry));
                w.write(version);
                dst_cid.encode_long(w);
                src_cid.encode_long(w);
                PartialEncode {
                    start,
                    header_len: w.len() - start,
                    pn: None,
                }
            }
            Short {
                spin,
                key_phase,
                ref dst_cid,
                number,
            } => {
                w.write(
                    FIXED_BIT
                        | if key_phase { KEY_PHASE_BIT } else { 0 }
                        | if spin { SPIN_BIT } else { 0 }
                        | number.tag(),
                );
                w.put_slice(dst_cid);
                number.encode(w);
                PartialEncode {
                    start,
                    header_len: w.len() - start,
                    pn: Some((number.len(), false)),
                }
            }
            VersionNegotiate {
                ref random,
                ref dst_cid,
                ref src_cid,
            } => {
                w.write(0x80u8 | random);
                w.write::<u32>(0);
                dst_cid.encode_long(w);
                src_cid.encode_long(w);
                PartialEncode {
                    start,
                    header_len: w.len() - start,
                    pn: None,
                }
            }
        }
    }

    /// Whether the packet is encrypted on the wire
    pub(crate) fn is_protected(&self) -> bool {
        !matches!(
            *self,
            Header::Retry { .. } | Header::VersionNegotiate { .. }
        )
    }

    pub(crate) fn number(&self) -> Option<PacketNumber> {
        use self::Header::*;
        Some(match *self {
            Initial { number, .. } => number,
            Long { number, .. } => number,
            Short { number, .. } => number,
            _ => {
                return None;
            }
        })
    }

    pub(crate) fn space(&self) -> SpaceId {
        use self::Header::*;
        match *self {
            Short { .. } => SpaceId::Data,
            Long {
                ty: LongType::ZeroRtt,
                ..
            } => SpaceId::Data,
            Long {
                ty: LongType::Handshake,
                ..
            } => SpaceId::Handshake,
            _ => SpaceId::Initial,
        }
    }

    pub(crate) fn key_phase(&self) -> bool {
        match *self {
            Header::Short { key_phase, .. } => key_phase,
            _ => false,
        }
    }

    pub(crate) fn is_short(&self) -> bool {
        matches!(*self, Header::Short { .. })
    }

    pub(crate) fn is_1rtt(&self) -> bool {
        self.is_short()
    }

    pub(crate) fn is_0rtt(&self) -> bool {
        matches!(
            *self,
            Header::Long {
                ty: LongType::ZeroRtt,
                ..
            }
        )
    }

    pub(crate) fn dst_cid(&self) -> &ConnectionId {
        use self::Header::*;
        match *self {
            Initial { ref dst_cid, .. } => dst_cid,
            Long { ref dst_cid, .. } => dst_cid,
            Retry { ref dst_cid, .. } => dst_cid,
            Short { ref dst_cid, .. } => dst_cid,
            VersionNegotiate { ref dst_cid, .. } => dst_cid,
        }
    }
}

pub(crate) struct PartialEncode {
    pub start: usize,
    pub header_len: usize,
    // Packet number length, payload length needed
    pn: Option<(usize, bool)>,
}

impl PartialEncode {
    pub(crate) fn finish<K, H>(self, buf: &mut [u8], header_crypto: &H, crypto: Option<(u64, &K)>)
    where
        K: crypto::PacketKey,
        H: crypto::HeaderKey,
    {
        let PartialEncode { header_len, pn, .. } = self;
        let (pn_len, write_len) = match pn {
            Some((pn_len, write_len)) => (pn_len, write_len),
            None => return,
        };

        let pn_pos = header_len - pn_len;
        if write_len {
            let len = buf.len() - header_len + pn_len;
            assert!(len < 2usize.pow(14)); // Fits in reserved space
            let mut slice = &mut buf[pn_pos - 2..pn_pos];
            slice.put_u16(len as u16 | 0b01 << 14);
        }

        if let Some((number, crypto)) = crypto {
            crypto.encrypt(number, buf, header_len);
        }

        debug_assert!(
            pn_pos + 4 + header_crypto.sample_size() <= buf.len(),
            "packet must be padded to at least {} bytes for header protection sampling",
            pn_pos + 4 + header_crypto.sample_size()
        );
        header_crypto.encrypt(pn_pos, buf);
    }
}

#[derive(Debug)]
pub(crate) enum PlainHeader {
    Initial {
        dst_cid: ConnectionId,
        src_cid: ConnectionId,
        token_pos: Range<usize>,
        len: u64,
        version: u32,
    },
    Long {
        ty: LongType,
        dst_cid: ConnectionId,
        src_cid: ConnectionId,
        len: u64,
        version: u32,
    },
    Retry {
        dst_cid: ConnectionId,
        src_cid: ConnectionId,
        version: u32,
    },
    Short {
        first: u8,
        spin: bool,
        dst_cid: ConnectionId,
    },
    VersionNegotiate {
        random: u8,
        dst_cid: ConnectionId,
        src_cid: ConnectionId,
    },
}

impl PlainHeader {
    fn dst_cid(&self) -> ConnectionId {
        use self::PlainHeader::*;
        match self {
            Initial { dst_cid, .. } => *dst_cid,
            Long { dst_cid, .. } => *dst_cid,
            Retry { dst_cid, .. } => *dst_cid,
            Short { dst_cid, .. } => *dst_cid,
            VersionNegotiate { dst_cid, .. } => *dst_cid,
        }
    }

    fn payload_len(&self) -> Option<u64> {
        use self::PlainHeader::*;
        match self {
            Initial { len, .. } | Long { len, .. } => Some(*len),
            _ => None,
        }
    }

    fn decode(
        buf: &mut io::Cursor<BytesMut>,
        local_cid_len: usize,
        supported_versions: &[u32],
    ) -> Result<Self, PacketDecodeError> {
        let first = buf.get::<u8>()?;
        if first & LONG_HEADER_FORM == 0 {
            let spin = first & SPIN_BIT != 0;
            if buf.remaining() < local_cid_len {
                return Err(PacketDecodeError::InvalidHeader("cid out of bounds"));
            }

            Ok(PlainHeader::Short {
                first,
                spin,
                dst_cid: ConnectionId::from_buf(buf, local_cid_len),
            })
        } else {
            let version = buf.get::<u32>()?;

            let dst_cid = ConnectionId::decode_long(buf)
                .ok_or(PacketDecodeError::InvalidHeader("malformed cid"))?;
            let src_cid = ConnectionId::decode_long(buf)
                .ok_or(PacketDecodeError::InvalidHeader("malformed cid"))?;

            // TODO: Support long CIDs for compatibility with future QUIC versions
            if version == 0 {
                let random = first & !LONG_HEADER_FORM;
                return Ok(PlainHeader::VersionNegotiate {
                    random,
                    dst_cid,
                    src_cid,
                });
            }

            if !supported_versions.contains(&version) {
                return Err(PacketDecodeError::UnsupportedVersion {
                    src_cid,
                    dst_cid,
                    version,
                });
            }

            match LongHeaderType::from_byte(first)? {
                LongHeaderType::Initial => {
                    let token_len = buf.get_var()? as usize;
                    let token_start = buf.position() as usize;
                    buf.advance(token_len);

                    let len = buf.get_var()?;
                    Ok(PlainHeader::Initial {
                        dst_cid,
                        src_cid,
                        token_pos: token_start..token_start + token_len,
                        len,
                        version,
                    })
                }
                LongHeaderType::Retry => Ok(PlainHeader::Retry {
                    dst_cid,
                    src_cid,
                    version,
                }),
                LongHeaderType::Standard(ty) => Ok(PlainHeader::Long {
                    ty,
                    dst_cid,
                    src_cid,
                    len: buf.get_var()?,
                    version,
                }),
            }
        }
    }
}

// An encoded packet number
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) enum PacketNumber {
    U8(u8),
    U16(u16),
    U24(u32),
    U32(u32),
}

impl PacketNumber {
    pub(crate) fn new(n: u64, largest_acked: u64) -> Self {
        let range = (n - largest_acked) * 2;
        if range < 1 << 8 {
            PacketNumber::U8(n as u8)
        } else if range < 1 << 16 {
            PacketNumber::U16(n as u16)
        } else if range < 1 << 24 {
            PacketNumber::U24(n as u32)
        } else if range < 1 << 32 {
            PacketNumber::U32(n as u32)
        } else {
            panic!("packet number too large to encode")
        }
    }

    pub(crate) fn len(self) -> usize {
        use self::PacketNumber::*;
        match self {
            U8(_) => 1,
            U16(_) => 2,
            U24(_) => 3,
            U32(_) => 4,
        }
    }

    pub(crate) fn encode<W: BufMut>(self, w: &mut W) {
        use self::PacketNumber::*;
        match self {
            U8(x) => w.write(x),
            U16(x) => w.write(x),
            U24(x) => w.put_uint(u64::from(x), 3),
            U32(x) => w.write(x),
        }
    }

    pub(crate) fn decode<R: Buf>(len: usize, r: &mut R) -> Result<PacketNumber, PacketDecodeError> {
        use self::PacketNumber::*;
        let pn = match len {
            1 => U8(r.get()?),
            2 => U16(r.get()?),
            3 => U24(r.get_uint(3) as u32),
            4 => U32(r.get()?),
            _ => unreachable!(),
        };
        Ok(pn)
    }

    pub(crate) fn decode_len(tag: u8) -> usize {
        1 + (tag & 0x03) as usize
    }

    fn tag(self) -> u8 {
        use self::PacketNumber::*;
        match self {
            U8(_) => 0b00,
            U16(_) => 0b01,
            U24(_) => 0b10,
            U32(_) => 0b11,
        }
    }

    pub(crate) fn expand(self, expected: u64) -> u64 {
        // From Appendix A
        use self::PacketNumber::*;
        let truncated = match self {
            U8(x) => u64::from(x),
            U16(x) => u64::from(x),
            U24(x) => u64::from(x),
            U32(x) => u64::from(x),
        };
        let nbits = self.len() * 8;
        let win = 1 << nbits;
        let hwin = win / 2;
        let mask = win - 1;
        // The incoming packet number should be greater than expected - hwin and less than or equal
        // to expected + hwin
        //
        // This means we can't just strip the trailing bits from expected and add the truncated
        // because that might yield a value outside the window.
        //
        // The following code calculates a candidate value and makes sure it's within the packet
        // number window.
        let candidate = (expected & !mask) | truncated;
        if expected.checked_sub(hwin).map_or(false, |x| candidate <= x) {
            candidate + win
        } else if candidate > expected + hwin && candidate > win {
            candidate - win
        } else {
            candidate
        }
    }
}

/// Long packet type including non-uniform cases
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LongHeaderType {
    Initial,
    Retry,
    Standard(LongType),
}

impl LongHeaderType {
    fn from_byte(b: u8) -> Result<Self, PacketDecodeError> {
        use self::{LongHeaderType::*, LongType::*};
        if b & FIXED_BIT == 0 {
            return Err(PacketDecodeError::InvalidHeader("fixed bit unset"));
        }
        debug_assert!(b & LONG_HEADER_FORM != 0, "not a long packet");
        Ok(match (b & 0x30) >> 4 {
            0x0 => Initial,
            0x1 => Standard(ZeroRtt),
            0x2 => Standard(Handshake),
            0x3 => Retry,
            _ => unreachable!(),
        })
    }
}

impl From<LongHeaderType> for u8 {
    fn from(ty: LongHeaderType) -> u8 {
        use self::{LongHeaderType::*, LongType::*};
        match ty {
            Initial => LONG_HEADER_FORM | FIXED_BIT,
            Standard(ZeroRtt) => LONG_HEADER_FORM | FIXED_BIT | (0x1 << 4),
            Standard(Handshake) => LONG_HEADER_FORM | FIXED_BIT | (0x2 << 4),
            Retry => LONG_HEADER_FORM | FIXED_BIT | (0x3 << 4),
        }
    }
}

/// Long packet types with uniform header structure
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LongType {
    Handshake,
    ZeroRtt,
}

#[derive(Debug, Error, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum PacketDecodeError {
    #[error("unsupported version {version:x}")]
    UnsupportedVersion {
        src_cid: ConnectionId,
        dst_cid: ConnectionId,
        version: u32,
    },
    #[error("invalid header: {0}")]
    InvalidHeader(&'static str),
}

impl From<coding::UnexpectedEnd> for PacketDecodeError {
    fn from(_: coding::UnexpectedEnd) -> Self {
        PacketDecodeError::InvalidHeader("unexpected end of packet")
    }
}

pub(crate) const LONG_HEADER_FORM: u8 = 0x80;
const FIXED_BIT: u8 = 0x40;
pub(crate) const SPIN_BIT: u8 = 0x20;
const SHORT_RESERVED_BITS: u8 = 0x18;
const LONG_RESERVED_BITS: u8 = 0x0c;
const KEY_PHASE_BIT: u8 = 0x04;

/// Packet number space identifiers
#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub enum SpaceId {
    /// Unprotected packets, used to bootstrap the handshake
    Initial = 0,
    Handshake = 1,
    /// Application data space, used for 0-RTT and post-handshake/1-RTT packets
    Data = 2,
}

impl SpaceId {
    pub fn iter() -> impl Iterator<Item = Self> {
        [SpaceId::Initial, SpaceId::Handshake, SpaceId::Data]
            .iter()
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DEFAULT_SUPPORTED_VERSIONS;
    use hex_literal::hex;
    use std::io;

    fn check_pn(typed: PacketNumber, encoded: &[u8]) {
        let mut buf = Vec::new();
        typed.encode(&mut buf);
        assert_eq!(&buf[..], encoded);
        let decoded = PacketNumber::decode(typed.len(), &mut io::Cursor::new(&buf)).unwrap();
        assert_eq!(typed, decoded);
    }

    #[test]
    fn roundtrip_packet_numbers() {
        check_pn(PacketNumber::U8(0x7f), &hex!("7f"));
        check_pn(PacketNumber::U16(0x80), &hex!("0080"));
        check_pn(PacketNumber::U16(0x3fff), &hex!("3fff"));
        check_pn(PacketNumber::U32(0x0000_4000), &hex!("0000 4000"));
        check_pn(PacketNumber::U32(0xffff_ffff), &hex!("ffff ffff"));
    }

    #[test]
    fn pn_encode() {
        check_pn(PacketNumber::new(0x10, 0), &hex!("10"));
        check_pn(PacketNumber::new(0x100, 0), &hex!("0100"));
        check_pn(PacketNumber::new(0x10000, 0), &hex!("010000"));
    }

    #[test]
    fn pn_expand_roundtrip() {
        for expected in 0..1024 {
            for actual in expected..1024 {
                assert_eq!(actual, PacketNumber::new(actual, expected).expand(expected));
            }
        }
    }

    #[cfg(feature = "rustls")]
    #[test]
    fn header_encoding() {
        use crate::{
            crypto::{rustls::TlsSession, PacketKey, Session},
            Side,
        };

        let dcid = ConnectionId::new(&hex!("06b858ec6f80452b"));
        let client = TlsSession::initial_keys(&dcid, Side::Client);
        let mut buf = Vec::new();
        let header = Header::Initial {
            number: PacketNumber::U8(0),
            src_cid: ConnectionId::new(&[]),
            dst_cid: dcid,
            token: Bytes::new(),
            version: DEFAULT_SUPPORTED_VERSIONS[0],
        };
        let encode = header.encode(&mut buf);
        let header_len = buf.len();
        buf.resize(header_len + 16 + client.packet.local.tag_len(), 0);
        encode.finish(
            &mut buf,
            &client.header.local,
            Some((0, &client.packet.local)),
        );

        for byte in &buf {
            print!("{:02x}", byte);
        }
        println!();
        assert_eq!(
            buf[..],
            hex!(
                "c6ff00001d0806b858ec6f80452b0000402180
                 3ef50807b84191a196f760a6dad1e9d1c6b3e7486fc03729ad1174a9d182574a"
            )[..]
        );

        let server = TlsSession::initial_keys(&dcid, Side::Server);
        let supported_versions = DEFAULT_SUPPORTED_VERSIONS.to_vec();
        let decode = PartialDecode::new(buf.as_slice().into(), 0, &supported_versions)
            .unwrap()
            .0;
        let mut packet = decode.finish(Some(&server.header.remote)).unwrap();
        assert_eq!(
            packet.header_data[..],
            hex!("c0ff00001d0806b858ec6f80452b0000402100")[..]
        );
        server
            .packet
            .remote
            .decrypt(0, &packet.header_data, &mut packet.payload)
            .unwrap();
        assert_eq!(packet.payload[..], [0; 16]);
        match packet.header {
            Header::Initial {
                number: PacketNumber::U8(0),
                ..
            } => {}
            _ => {
                panic!("unexpected header {:?}", packet.header);
            }
        }
    }
}
