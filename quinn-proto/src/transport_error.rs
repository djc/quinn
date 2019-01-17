use std::fmt;

use bytes::{Buf, BufMut};
use slog;

use crate::coding::{self, BufExt, BufMutExt};
use rustls::internal::msgs::{codec::Codec, enums::AlertDescription};

#[derive(Copy, Clone, Eq, PartialEq)]
pub struct Error(u16);

impl Error {
    pub fn crypto(alert: AlertDescription) -> Self {
        Error(0x100 | alert.get_u8() as u16)
    }
}

impl coding::Codec for Error {
    fn decode<B: Buf>(buf: &mut B) -> coding::Result<Self> {
        Ok(Error(buf.get::<u16>()?))
    }
    fn encode<B: BufMut>(&self, buf: &mut B) {
        buf.write::<u16>(self.0)
    }
}

impl From<Error> for u16 {
    fn from(x: Error) -> u16 {
        x.0
    }
}

macro_rules! errors {
    {$($name:ident($val:expr) $desc:expr;)*} => {
        impl Error {
            $(#[doc = $desc] pub const $name: Self = Error($val);)*
        }

        impl fmt::Debug for Error {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self.0 {
                    $($val => f.write_str(stringify!($name)),)*
                    x if x >= 0x100 && x < 0x200 => write!(f, "Error::crypto({:?})", AlertDescription::read_bytes(&[self.0 as u8]).unwrap()),
                    _ => write!(f, "Error({:04x})", self.0),
                }
            }
        }

        impl fmt::Display for Error {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let x = match self.0 {
                    $($val => $desc,)*
                    _ if self.0 >= 0x100 && self.0 < 0x200 => "the cryptographic handshake failed", // FIXME: Describe specific alert
                    _ => "unknown error",
                };
                f.write_str(x)
            }
        }
    }
}

impl slog::Value for Error {
    fn serialize(
        &self,
        _: &slog::Record<'_>,
        key: slog::Key,
        serializer: &mut dyn slog::Serializer,
    ) -> slog::Result {
        serializer.emit_arguments(key, &format_args!("{:?}", self))
    }
}

errors! {
    NO_ERROR(0x0) "the connection is being closed abruptly in the absence of any error";
    INTERNAL_ERROR(0x1) "the endpoint encountered an internal error and cannot continue with the connection";
    SERVER_BUSY(0x2) "the server is currently busy and does not accept any new connections";
    FLOW_CONTROL_ERROR(0x3) "received more data than permitted in advertised data limits";
    STREAM_LIMIT_ERROR(0x4) "received a frame for a stream identifier that exceeded advertised the stream limit for the corresponding stream type";
    STREAM_STATE_ERROR(0x5) "received a frame for a stream that was not in a state that permitted that frame";
    FINAL_OFFSET_ERROR(0x6) "received a STREAM frame containing data that exceeded the previously established final offset, or a RST_STREAM frame containing a final offset that was lower than the maximum offset of data that was already received, or a RST_STREAM frame containing a different final offset to the one already established";
    FRAME_ENCODING_ERROR(0x7) "received a frame that was badly formatted";
    TRANSPORT_PARAMETER_ERROR(0x8) "received transport parameters that were badly formatted, included an invalid value, was absent even though it is mandatory, was present though it is forbidden, or is otherwise in error";
    VERSION_NEGOTIATION_ERROR(0x9) "received transport parameters that contained version negotiation parameters that disagreed with the version negotiation that was performed, constituting a potential version downgrade attack";
    PROTOCOL_VIOLATION(0xA) "detected an error with protocol compliance that was not covered by more specific error codes";
    INVALID_MIGRATION(0xC) "received a PATH_RESPONSE frame that did not correspond to any PATH_CHALLENGE frame that it previously sent";
}
