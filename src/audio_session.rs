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

/// Server-side, spec 2.13 + KNOWN_ISSUES.md #12: accepts the incoming
/// `audio_capture` stream (client -> server), but only hands it back if
/// `audio_capture_granted`. Otherwise, stops the stream
/// ([`EnvelopeReader::stop`]) once its `kind` is confirmed and returns
/// `Ok(None)` -- the same "refuse rather than silently accept" shape as
/// DR-037's file-handle ownership check, applied here to a permission
/// check instead.
pub async fn accept_audio_capture_gated(
    connection: &quinn::Connection,
    audio_capture_granted: bool,
) -> Result<Option<(AudioConfig, AudioFrameReader)>, AudioError> {
    let recv = connection.accept_uni().await.map_err(AudioError::Quic)?;
    let mut reader = EnvelopeReader::new(recv);

    let stream_prologue = reader.read_prologue().await?;
    if stream_prologue.kind != StreamKind::AudioCapture {
        return Err(AudioError::WrongStreamKind);
    }
    accept_audio_capture_from_reader(reader, audio_capture_granted).await
}

/// The part of [`accept_audio_capture_gated`] after the `StreamPrologue`,
/// for a caller that accepts every incoming unidirectional stream in one
/// place and dispatches on `kind` (a server that also accepts `input`
/// streams can't have two `accept_uni()` callers racing for the same
/// stream). `reader` must be positioned just past an `audio_capture`
/// prologue.
pub async fn accept_audio_capture_from_reader(
    mut reader: EnvelopeReader,
    audio_capture_granted: bool,
) -> Result<Option<(AudioConfig, AudioFrameReader)>, AudioError> {
    if !audio_capture_granted {
        reader.stop(quinn::VarInt::from_u32(0));
        return Ok(None);
    }

    let max_len = StreamKind::AudioCapture.max_envelope_length();
    let (type_raw, payload) = reader.read_envelope(max_len).await?;
    if type_raw != messages::type_id::AUDIO_CONFIG {
        return Err(AudioError::ProtocolViolation(
            ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE,
        ));
    }
    let config: AudioConfig = messages::decode(&payload).map_err(AudioError::Decode)?;

    Ok(Some((
        config,
        AudioFrameReader {
            reader,
            kind: StreamKind::AudioCapture,
        },
    )))
}

/// A synthetic 16-bit PCM tone standing in for a real Opus payload (this
/// PoC has no real audio device or encoder, per scope): `frequency_hz` Hz,
/// half amplitude, at `sample_rate` samples/sec.
pub fn generate_sine_wave_payload(samples: usize, sample_rate: u32, frequency_hz: f64) -> Vec<u8> {
    (0..samples)
        .flat_map(|i| {
            let t = i as f64 / f64::from(sample_rate);
            let sample =
                (f64::from(i16::MAX) * 0.5 * (2.0 * std::f64::consts::PI * frequency_hz * t).sin())
                    as i16;
            sample.to_le_bytes()
        })
        .collect()
}

/// A synthetic silent 16-bit PCM buffer, standing in for a real captured
/// (but currently quiet) microphone input.
pub fn generate_silence_payload(samples: usize) -> Vec<u8> {
    vec![0u8; samples * 2]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_wave_payload_has_two_bytes_per_sample() {
        let payload = generate_sine_wave_payload(960, 48_000, 440.0);
        assert_eq!(payload.len(), 960 * 2);
    }

    #[test]
    fn sine_wave_payload_starts_at_zero_crossing() {
        // sin(2*pi*f*0) == 0 regardless of frequency/sample_rate.
        let payload = generate_sine_wave_payload(4, 48_000, 440.0);
        let first_sample = i16::from_le_bytes([payload[0], payload[1]]);
        assert_eq!(first_sample, 0);
    }

    #[test]
    fn sine_wave_payload_is_deterministic() {
        let a = generate_sine_wave_payload(100, 48_000, 440.0);
        let b = generate_sine_wave_payload(100, 48_000, 440.0);
        assert_eq!(a, b);
    }

    #[test]
    fn silence_payload_is_all_zero_bytes_of_the_right_length() {
        let payload = generate_silence_payload(960);
        assert_eq!(payload.len(), 960 * 2);
        assert!(payload.iter().all(|&b| b == 0));
    }
}
