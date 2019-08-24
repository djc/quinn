use std::{cmp, io};

use bytes::BytesMut;
use quinn::RecvStream;
use tokio_codec::{Decoder, FramedRead};
use tokio_io::AsyncRead;

use super::proto::frame::{self, HttpFrame, PartialData};

pub type FrameStream = FramedRead<RecvStream, FrameDecoder>;

#[derive(Default)]
pub struct FrameDecoder {
    partial: Option<PartialData>,
    expected: Option<usize>,
    size_limit: usize,
}

impl FrameDecoder {
    pub fn stream<T: AsyncRead>(stream: T, size_limit: usize) -> FramedRead<T, Self> {
        FramedRead::new(
            stream,
            FrameDecoder {
                expected: None,
                partial: None,
                size_limit,
            },
        )
    }
}

macro_rules! decode {
    ($buf:ident, $dec:expr) => {{
        let mut cur = io::Cursor::new(&$buf);
        let decoded = $dec(&mut cur);
        (cur.position() as usize, decoded)
    }};
}

impl Decoder for FrameDecoder {
    type Item = HttpFrame;
    type Error = Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() == 0 {
            return Ok(None);
        }
        if let Some(min) = self.expected {
            if src.len() < min {
                return Ok(None);
            }
        }

        if let Some(ref mut partial) = self.partial {
            let (pos, frame) = decode!(src, |cur| HttpFrame::Data(partial.decode_data(cur)));
            src.advance(pos);

            if partial.remaining() == 0 {
                self.expected = None;
                self.partial = None;
            } else {
                self.expected = Some(cmp::min(partial.remaining(), self.size_limit));
            }

            return Ok(Some(frame));
        }

        let (pos, decoded) = decode!(src, |cur| HttpFrame::decode(cur));

        match decoded {
            Err(frame::Error::IncompleteData(min)) if min > self.size_limit => {
                let (pos, decoded) = decode!(src, |cur| PartialData::decode(cur, self.size_limit));
                let (partial, frame) = decoded?;
                src.advance(pos);
                self.expected = Some(cmp::min(partial.remaining(), self.size_limit));
                self.partial = Some(partial);
                Ok(Some(HttpFrame::Data(frame)))
            }
            Err(frame::Error::Incomplete(min)) | Err(frame::Error::IncompleteData(min)) => {
                self.expected = Some(min);
                Ok(None)
            }
            Err(e) => Err(e)?,
            Ok(frame) => {
                src.advance(pos);
                self.expected = None;
                Ok(Some(frame))
            }
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Proto(frame::Error),
    Io(io::Error),
}

impl From<frame::Error> for Error {
    fn from(err: frame::Error) -> Self {
        Error::Proto(err)
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::frame;

    #[test]
    fn one_frame() {
        let frame = frame::HeadersFrame {
            encoded: b"salut"[..].into(),
        };

        let mut buf = BytesMut::with_capacity(16);
        frame.encode(&mut buf);

        let mut decoder = FrameDecoder::default();
        assert_matches!(decoder.decode(&mut buf), Ok(Some(HttpFrame::Headers(_))));
    }

    #[test]
    fn incomplete_frame() {
        let frame = frame::HeadersFrame {
            encoded: b"salut"[..].into(),
        };

        let mut buf = BytesMut::with_capacity(16);
        frame.encode(&mut buf);
        buf.truncate(buf.len() - 1);

        let mut decoder = FrameDecoder::default();
        assert_matches!(decoder.decode(&mut buf.into()), Ok(None));
    }

    #[test]
    fn two_frames_then_incomplete() {
        let frames = [
            HttpFrame::Headers(frame::HeadersFrame {
                encoded: b"header"[..].into(),
            }),
            HttpFrame::Data(frame::DataFrame {
                payload: b"body"[..].into(),
            }),
            HttpFrame::Headers(frame::HeadersFrame {
                encoded: b"trailer"[..].into(),
            }),
        ];

        let mut buf = BytesMut::with_capacity(64);
        for frame in frames.iter() {
            frame.encode(&mut buf);
        }
        buf.truncate(buf.len() - 1);

        let mut decoder = FrameDecoder::default();
        assert_matches!(decoder.decode(&mut buf), Ok(Some(HttpFrame::Headers(_))));
        assert_matches!(decoder.decode(&mut buf), Ok(Some(HttpFrame::Data(_))));
        assert_matches!(decoder.decode(&mut buf), Ok(None));
    }
}
