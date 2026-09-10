//! Audio (spec 2.13): `AudioConfig` sent once, then a continuous run of
//! `AudioFrame`s (Opus payload) on one of two fixed unidirectional stream
//! kinds -- `audio_playback` (server->client, `StreamKind::AudioPlayback`)
//! or `audio_capture` (client->server, `StreamKind::AudioCapture`).
//!
//! Both directions share this exact wire shape (spec 2.13 defines
//! `AudioConfig`/`AudioFrame` once, not per-direction), so this module's
//! functions take the relevant `StreamKind` as a parameter rather than
//! duplicating the logic per direction.
//!
//! Unlike `video` (spec 2.10/4.3.2), audio has no per-generation setup or
//! state machine: spec 2.13 explicitly says Opus's short reference window
//! and loss tolerance make video-style generation management unnecessary.
//!
//! Per DR-035 (same rationale as `VideoFrameHeader`/`VideoFramePayload`),
//! `AudioFrame` is split on the wire into an `AudioFrameHeader` (CBOR)
//! Envelope immediately followed by a raw-bytes `AudioFramePayload`
//! Envelope (the Opus data itself, unwrapped).

use crate::messages::{self, AudioConfig, AudioFrameHeader};
use crate::prologue;
use crate::reason_code::ReasonCode;
use crate::stream_kind::StreamKind;
use crate::stream_reader::{EnvelopeReader, StreamReadError, write_envelope};

#[derive(Debug)]
pub enum AudioError {
    Quic(quinn::ConnectionError),
    Write(quinn::WriteError),
    Read(StreamReadError),
    Decode(ciborium::de::Error<std::io::Error>),
    /// A message arrived that isn't valid at this point in the exchange
    /// (e.g. anything other than `AudioFrameHeader` immediately followed
    /// by `AudioFramePayload` after the initial `AudioConfig`).
    ProtocolViolation(ReasonCode),
    /// `AudioFrameHeader.payload_len` didn't match the immediately
    /// following `AudioFramePayload` Envelope's actual byte count (same
    /// cross-check as video's `PROTOCOL.8 FRAME_LENGTH_MISMATCH`, though
    /// spec 2.13 doesn't define a dedicated `ReasonCode` for the audio
    /// case).
    FrameLengthMismatch,
    /// The opened/accepted stream declared a `kind` other than the
    /// expected `audio_playback`/`audio_capture`.
    WrongStreamKind,
}

impl From<StreamReadError> for AudioError {
    fn from(e: StreamReadError) -> Self {
        Self::Read(e)
    }
}

impl From<quinn::WriteError> for AudioError {
    fn from(e: quinn::WriteError) -> Self {
        Self::Write(e)
    }
}

/// Opens a new `audio_playback`/`audio_capture` stream (`kind` picks
/// which) and sends `AudioConfig` (spec 2.2.1: `context_id` unused, 0).
pub async fn open_audio_stream(
    connection: &quinn::Connection,
    kind: StreamKind,
    config: &AudioConfig,
) -> Result<quinn::SendStream, AudioError> {
    let mut send = connection.open_uni().await.map_err(AudioError::Quic)?;

    let mut prologue_bytes = Vec::new();
    prologue::encode(kind, 1, 0, &mut prologue_bytes);
    send.write_all(&prologue_bytes).await?;

    write_envelope(
        &mut send,
        messages::type_id::AUDIO_CONFIG,
        &messages::encode(config),
    )
    .await?;

    Ok(send)
}

/// Sends one `AudioFrame` (`AudioFrameHeader` + raw `AudioFramePayload`,
/// DR-035) on an already-open audio stream.
pub async fn send_audio_frame(
    send: &mut quinn::SendStream,
    sequence: u64,
    capture_ts: u64,
    duration_us: u32,
    payload: &[u8],
) -> Result<(), AudioError> {
    let header = AudioFrameHeader {
        sequence,
        capture_ts,
        duration_us,
        payload_len: payload.len() as u64,
    };
    write_envelope(
        send,
        messages::type_id::AUDIO_FRAME_HEADER,
        &messages::encode(&header),
    )
    .await?;
    write_envelope(send, messages::type_id::AUDIO_FRAME_PAYLOAD, payload).await?;
    Ok(())
}

/// Accepts the next incoming unidirectional stream, validates it's the
/// expected `kind`, and reads back its `AudioConfig`. Returns an
/// [`AudioFrameReader`] positioned to read the stream's subsequent frames.
pub async fn accept_audio_stream(
    connection: &quinn::Connection,
    expected_kind: StreamKind,
) -> Result<(AudioConfig, AudioFrameReader), AudioError> {
    let recv = connection.accept_uni().await.map_err(AudioError::Quic)?;
    let mut reader = EnvelopeReader::new(recv);

    let stream_prologue = reader.read_prologue().await?;
    if stream_prologue.kind != expected_kind {
        return Err(AudioError::WrongStreamKind);
    }

    let max_len = expected_kind.max_envelope_length();
    let (type_raw, payload) = reader.read_envelope(max_len).await?;
    if type_raw != messages::type_id::AUDIO_CONFIG {
        return Err(AudioError::ProtocolViolation(
            ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE,
        ));
    }
    let config: AudioConfig = messages::decode(&payload).map_err(AudioError::Decode)?;

    Ok((
        config,
        AudioFrameReader {
            reader,
            kind: expected_kind,
        },
    ))
}

/// Reads an audio stream's `AudioFrame`s one after another, after
/// `AudioConfig`. Constructed via [`accept_audio_stream`].
pub struct AudioFrameReader {
    reader: EnvelopeReader,
    kind: StreamKind,
}

impl AudioFrameReader {
    /// Reads the next `AudioFrameHeader` + `AudioFramePayload` pair,
    /// enforcing "payload immediately follows header" and the
    /// length-match rule (same shape as `VideoFrameReader::read_next_frame`).
    pub async fn read_next_frame(&mut self) -> Result<(AudioFrameHeader, Vec<u8>), AudioError> {
        let max_len = self.kind.max_envelope_length();

        let (type_raw, payload) = self.reader.read_envelope(max_len).await?;
        if type_raw != messages::type_id::AUDIO_FRAME_HEADER {
            return Err(AudioError::ProtocolViolation(
                ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE,
            ));
        }
        let header: AudioFrameHeader = messages::decode(&payload).map_err(AudioError::Decode)?;

        let (type_raw, frame_payload) = self.reader.read_envelope(max_len).await?;
        if type_raw != messages::type_id::AUDIO_FRAME_PAYLOAD {
            return Err(AudioError::ProtocolViolation(
                ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE,
            ));
        }
        if frame_payload.len() as u64 != header.payload_len {
            return Err(AudioError::FrameLengthMismatch);
        }

        Ok((header, frame_payload))
    }
}
