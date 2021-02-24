use thiserror::Error;

use crate::{
    bytes_source::BytesSource, connection::send_buffer::SendBuffer, frame, VarInt, Written,
};

#[derive(Debug)]
pub(super) struct Send {
    pub(super) max_data: u64,
    pub(super) state: SendState,
    pub(super) pending: SendBuffer,
    pub(super) priority: i32,
    /// Whether a frame containing a FIN bit must be transmitted, even if we don't have any new data
    pub(super) fin_pending: bool,
    /// Whether this stream is in the `connection_blocked` list of `Streams`
    pub(super) connection_blocked: bool,
    /// The reason the peer wants us to stop, if `STOP_SENDING` was received
    pub(super) stop_reason: Option<VarInt>,
}

impl Send {
    pub(super) fn new(max_data: VarInt) -> Self {
        Self {
            max_data: max_data.into(),
            state: SendState::Ready,
            pending: SendBuffer::new(),
            priority: 0,
            fin_pending: false,
            connection_blocked: false,
            stop_reason: None,
        }
    }

    /// Whether the stream has been reset
    pub(super) fn is_reset(&self) -> bool {
        matches!(self.state, SendState::ResetSent { .. })
    }

    pub(super) fn finish(&mut self) -> Result<(), FinishError> {
        if let Some(error_code) = self.stop_reason {
            Err(FinishError::Stopped(error_code))
        } else if self.state == SendState::Ready {
            self.state = SendState::DataSent {
                finish_acked: false,
            };
            self.fin_pending = true;
            Ok(())
        } else {
            Err(FinishError::UnknownStream)
        }
    }

    pub(super) fn write<S: BytesSource>(
        &mut self,
        source: &mut S,
        limit: u64,
    ) -> Result<Written, WriteError> {
        if !self.is_writable() {
            return Err(WriteError::UnknownStream);
        }
        if let Some(error_code) = self.stop_reason {
            return Err(WriteError::Stopped(error_code));
        }
        let budget = self.max_data - self.pending.offset();
        if budget == 0 {
            return Err(WriteError::Blocked);
        }
        let mut limit = limit.min(budget) as usize;

        let mut result = Written::default();
        loop {
            let (chunk, chunks_consumed) = source.pop_chunk(limit);
            result.chunks += chunks_consumed;
            result.bytes += chunk.len();

            if chunk.is_empty() {
                break;
            }

            limit -= chunk.len();
            self.pending.write(chunk);
        }

        Ok(result)
    }

    /// Update stream state due to a reset sent by the local application
    pub(super) fn reset(&mut self) {
        use SendState::*;
        if let DataSent { .. } | Ready = self.state {
            self.state = ResetSent;
        }
    }

    /// Handle STOP_SENDING
    pub(super) fn stop(&mut self, error_code: VarInt) {
        self.stop_reason = Some(error_code);
    }

    /// Returns whether the stream has been finished and all data has been acknowledged by the peer
    pub(super) fn ack(&mut self, frame: frame::StreamMeta) -> bool {
        self.pending.ack(frame.offsets);
        match self.state {
            SendState::DataSent {
                ref mut finish_acked,
            } => {
                *finish_acked |= frame.fin;
                *finish_acked && self.pending.is_fully_acked()
            }
            _ => false,
        }
    }

    /// Handle increase to stream-level flow control limit
    ///
    /// Returns whether the stream was unblocked
    pub(super) fn increase_max_data(&mut self, offset: u64) -> bool {
        if offset <= self.max_data || self.state != SendState::Ready {
            return false;
        }
        let was_blocked = self.pending.offset() == self.max_data;
        self.max_data = offset;
        was_blocked
    }

    pub(super) fn offset(&self) -> u64 {
        self.pending.offset()
    }

    pub(super) fn is_pending(&self) -> bool {
        self.pending.has_unsent_data() || self.fin_pending
    }

    pub(super) fn is_writable(&self) -> bool {
        matches!(self.state, SendState::Ready)
    }
}

/// Errors triggered while writing to a send stream
#[derive(Debug, Error, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum WriteError {
    /// The peer is not able to accept additional data, or the connection is congested.
    ///
    /// If the peer issues additional flow control credit, a [`StreamEvent::Writable`] event will
    /// be generated, indicating that retrying the write might succeed.
    ///
    /// [`StreamEvent::Writable`]: crate::StreamEvent::Writable
    #[error("unable to accept further writes")]
    Blocked,
    /// The peer is no longer accepting data on this stream, and it has been implicitly reset. The
    /// stream cannot be finished or further written to.
    ///
    /// Carries an application-defined error code.
    ///
    /// [`StreamEvent::Finished`]: crate::StreamEvent::Finished
    #[error("stopped by peer: code {0}")]
    Stopped(VarInt),
    /// The stream has not been opened or has already been finished or reset
    #[error("unknown stream")]
    UnknownStream,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(super) enum SendState {
    /// Sending new data
    Ready,
    /// Stream was finished; now sending retransmits only
    DataSent { finish_acked: bool },
    /// Sent RESET
    ResetSent,
}

/// Reasons why attempting to finish a stream might fail
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FinishError {
    /// The peer is no longer accepting data on this stream. No
    /// [`StreamEvent::Finished`] event will be emitted for this stream.
    ///
    /// Carries an application-defined error code.
    ///
    /// [`StreamEvent::Finished`]: crate::StreamEvent::Finished
    #[error("stopped by peer: code {0}")]
    Stopped(VarInt),
    /// The stream has not been opened or was already finished or reset
    #[error("unknown stream")]
    UnknownStream,
}
