//! Buffers bytes read from a QUIC `RecvStream` so `StreamPrologue`/
//! `Envelope` parsing (incremental, per M1) can be retried as more data
//! arrives, without losing bytes that belong to the next frame. Shared by
//! the `control` stream (handshake) and `video` stream (video session)
//! readers.

use crate::{envelope, prologue};

/// Reading from the underlying stream failed, or it ended mid-frame.
#[derive(Debug)]
pub enum StreamReadError {
    Read(quinn::ReadError),
    ClosedEarly,
    Prologue(prologue::PrologueError),
    Envelope(envelope::EnvelopeError),
}

impl From<quinn::ReadError> for StreamReadError {
    fn from(e: quinn::ReadError) -> Self {
        Self::Read(e)
    }
}

pub struct EnvelopeReader<'s> {
    recv: &'s mut quinn::RecvStream,
    buf: Vec<u8>,
}

impl<'s> EnvelopeReader<'s> {
    pub fn new(recv: &'s mut quinn::RecvStream) -> Self {
        Self {
            recv,
            buf: Vec::new(),
        }
    }

    async fn fill_more(&mut self) -> Result<(), StreamReadError> {
        let mut chunk = [0u8; 4096];
        let n = self
            .recv
            .read(&mut chunk)
            .await?
            .ok_or(StreamReadError::ClosedEarly)?;
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(())
    }

    pub async fn read_prologue(&mut self) -> Result<prologue::StreamPrologue, StreamReadError> {
        loop {
            match prologue::parse(&self.buf) {
                Ok(Some((p, consumed))) => {
                    self.buf.drain(..consumed);
                    return Ok(p);
                }
                Ok(None) => self.fill_more().await?,
                Err(e) => return Err(StreamReadError::Prologue(e)),
            }
        }
    }

    /// Reads one Envelope and returns its `type_raw` plus payload bytes.
    /// `max_length` is the destination stream kind's length limit (spec
    /// 2.1.1 table).
    pub async fn read_envelope(
        &mut self,
        max_length: u64,
    ) -> Result<(u16, Vec<u8>), StreamReadError> {
        loop {
            match envelope::parse(&self.buf, max_length) {
                Ok(Some((env, consumed))) => {
                    let result = (env.type_raw, env.payload.to_vec());
                    self.buf.drain(..consumed);
                    return Ok(result);
                }
                Ok(None) => self.fill_more().await?,
                Err(e) => return Err(StreamReadError::Envelope(e)),
            }
        }
    }
}

/// Encodes and writes one Envelope to `send`.
pub async fn write_envelope(
    send: &mut quinn::SendStream,
    type_raw: u16,
    payload: &[u8],
) -> Result<(), quinn::WriteError> {
    let mut buf = Vec::new();
    envelope::encode(type_raw, payload, &mut buf);
    send.write_all(&buf).await
}
