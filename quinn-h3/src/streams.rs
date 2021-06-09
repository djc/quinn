use std::{
    collections::VecDeque,
    convert::TryFrom,
    future::Future,
    io, mem,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::{Buf, Bytes};
use futures::{
    io::{AsyncRead, AsyncWrite},
    ready,
};
use quinn::{OpenUni, RecvStream, SendStream};
use quinn_proto::VarInt;
use tracing::trace;

use crate::{
    frame::{FrameDecoder, FrameStream},
    proto::{ErrorCode, StreamType},
    Error,
};

pub enum NewUni {
    Control(FrameStream),
    Push(PushStream),
    Encoder(RecvStream),
    Decoder(RecvStream),
    Reserved,
}

impl TryFrom<(StreamType, RecvStream)> for NewUni {
    type Error = Error;
    fn try_from(value: (StreamType, RecvStream)) -> Result<Self, Self::Error> {
        let (ty, recv) = value;
        Ok(match ty {
            StreamType::CONTROL => NewUni::Control(FrameDecoder::stream(recv)),
            StreamType::PUSH => NewUni::Push(PushStream(FrameDecoder::stream(recv))),
            StreamType::ENCODER => NewUni::Encoder(recv),
            StreamType::DECODER => NewUni::Decoder(recv),
            t if t.0 > 0x21 && (t.0 - 0x21) % 0x1f == 0 => NewUni::Reserved,
            _ => return Err(Error::UnknownStream(ty.0)),
        })
    }
}

#[must_use = "futures/streams/sinks do nothing unless you `.await` or poll them"]
pub struct RecvUni {
    inner: Option<RecvUniInner>,
}

pub struct RecvUniInner {
    stream: RecvStream,
    buf: [u8; VarInt::MAX_SIZE],
    expected: usize,
    len: usize,
}

impl RecvUni {
    pub fn new(stream: RecvStream) -> Self {
        Self {
            inner: Some(RecvUniInner {
                stream,
                buf: [0; VarInt::MAX_SIZE],
                expected: 1,
                len: 0,
            }),
        }
    }
}

impl Future for RecvUni {
    type Output = Result<NewUni, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        loop {
            let inner = match self.inner {
                None => panic!("polled after resolved"),
                Some(ref mut inner) => inner,
            };

            let read = ready!(Pin::new(&mut inner.stream)
                .poll_read(cx, &mut inner.buf[inner.len..inner.expected]))?;
            if read == 0 {
                return Poll::Ready(Err(Error::peer("Uni stream closed before type received")));
            };

            inner.len += read;
            if inner.len == 1 {
                inner.expected = VarInt::encoded_size(inner.buf[0]);
            }
            if inner.len != inner.expected {
                continue;
            }

            let mut cur = io::Cursor::new(&inner.buf);
            let ty =
                StreamType::decode(&mut cur).map_err(|_| Error::internal("stream type decode"))?;
            match mem::replace(&mut self.inner, None) {
                Some(inner) => return Poll::Ready(NewUni::try_from((ty, inner.stream))),
                _ => unreachable!(),
            };
        }
    }
}

pub struct PushStream(FrameStream);

#[must_use = "futures/streams/sinks do nothing unless you `.await` or poll them"]
pub struct SendUni {
    ty: StreamType,
    state: SendUniState,
    data: VecDeque<Bytes>,
}

impl SendUni {
    pub(super) fn new(ty: StreamType, open_uni: quinn::OpenUni) -> Self {
        Self {
            ty,
            state: SendUniState::Opening(open_uni),
            data: VecDeque::with_capacity(2),
        }
    }
}

enum SendUniState {
    Opening(OpenUni),
    Idle(SendStream),
    Sending(SendStream, Bytes),
    Transitive,
}

impl SendUni {
    pub fn push(&mut self, data: Bytes) {
        self.data.push_back(data);
    }
}

/// Send all buffers from self.data, return `Poll::Ready(Ok(()))` when there is nothing more to be done
impl Future for SendUni {
    type Output = Result<(), Error>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let is_empty = self.data.is_empty();
        let ty = self.ty.clone();
        loop {
            match self.state {
                SendUniState::Opening(ref mut o) => {
                    if is_empty {
                        return Poll::Ready(Ok(()));
                    }
                    let send = ready!(Pin::new(o).poll(cx))?;
                    trace!("opening {} stream", ty);
                    self.state = SendUniState::Sending(send, self.ty.encoded());
                }
                SendUniState::Idle(_) => match self.data.pop_front() {
                    Some(d) => match mem::replace(&mut self.state, SendUniState::Transitive) {
                        SendUniState::Idle(s) => self.state = SendUniState::Sending(s, d),
                        _ => unreachable!(),
                    },
                    None => return Poll::Ready(Ok(())),
                },
                SendUniState::Sending(ref mut send, ref mut data) => {
                    let wrote = ready!(Pin::new(send).poll_write(cx, data))?;
                    data.advance(wrote);
                    trace!("sent {} bytes on {} stream", wrote, ty);
                    if data.is_empty() {
                        self.state = match mem::replace(&mut self.state, SendUniState::Transitive) {
                            SendUniState::Sending(s, _) => match self.data.pop_front() {
                                Some(d) => SendUniState::Sending(s, d),
                                None => SendUniState::Idle(s),
                            },
                            _ => unreachable!(),
                        };
                    }
                }
                _ => panic!("SendUni state machine fault"),
            }
        }
    }
}

pub trait Reset {
    fn reset(&mut self, code: ErrorCode);
}
