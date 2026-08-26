//! Drives one video stream Instance over a live QUIC connection (spec
//! 2.10, 2.2.1, 4.3.2), scoped to M3: opening the Instance and sending/
//! receiving `VideoStreamGeneration` -> `EncoderConfig` -> the first
//! (self-contained IDR) `VideoFrame`. Client-side decode/display is M4;
//! this module only validates the wire-level contract.

use crate::messages::{
    self, EncoderConfig, VIDEO_FRAME_FLAG_IDR, VideoFrame, VideoStreamGeneration,
};
use crate::prologue;
use crate::reason_code::ReasonCode;
use crate::stream_kind::StreamKind;
use crate::stream_reader::{EnvelopeReader, StreamReadError, write_envelope};
use crate::video_sm::{ProtocolViolation as VideoProtocolViolation, VideoInstanceSm};

#[derive(Debug)]
pub enum VideoError {
    Quic(quinn::ConnectionError),
    Write(quinn::WriteError),
    Read(StreamReadError),
    Decode(ciborium::de::Error<std::io::Error>),
    /// A message arrived out of the order spec 4.3.2 permits.
    ProtocolViolation(ReasonCode),
    /// `VideoFrame.payload_len` didn't match the actual payload byte
    /// count (spec 2.10 / `PROTOCOL.8 FRAME_LENGTH_MISMATCH`).
    FrameLengthMismatch,
    /// The opened stream declared a `kind` other than `video` (spec
    /// 2.2.1's initiator/kind table).
    WrongStreamKind,
}

impl From<StreamReadError> for VideoError {
    fn from(e: StreamReadError) -> Self {
        Self::Read(e)
    }
}

impl From<quinn::WriteError> for VideoError {
    fn from(e: quinn::WriteError) -> Self {
        Self::Write(e)
    }
}

fn violation(v: VideoProtocolViolation) -> VideoError {
    VideoError::ProtocolViolation(v.reason)
}

/// Server side (spec 2.2.1: `video` is server-initiated, unidirectional).
/// Opens a new stream for `monitor_id`, sends the Instance's setup
/// messages and its first self-contained IDR frame, and returns the open
/// `SendStream` (still open, so later frames/generations can follow in
/// later milestones) plus the resulting Instance state.
#[allow(clippy::too_many_arguments)]
pub async fn open_video_instance(
    connection: &quinn::Connection,
    monitor_id: u64,
    generation: u64,
    config_id: u64,
    encoder_config: EncoderConfig,
    idr_payload: Vec<u8>,
    capture_ts: u64,
    encode_done_ts: u64,
) -> Result<(quinn::SendStream, VideoInstanceSm), VideoError> {
    let mut sm = VideoInstanceSm::new();
    let mut send = connection.open_uni().await.map_err(VideoError::Quic)?;

    let mut prologue_bytes = Vec::new();
    prologue::encode(StreamKind::Video, 1, monitor_id, &mut prologue_bytes);
    send.write_all(&prologue_bytes).await?;
    sm.on_prologue_sent().map_err(violation)?;

    let generation_msg = VideoStreamGeneration {
        generation,
        config_id,
    };
    write_envelope(
        &mut send,
        messages::type_id::VIDEO_STREAM_GENERATION,
        &messages::encode(&generation_msg),
    )
    .await?;
    sm.on_generation_sent().map_err(violation)?;

    write_envelope(
        &mut send,
        messages::type_id::ENCODER_CONFIG,
        &messages::encode(&encoder_config),
    )
    .await?;
    sm.on_encoder_config_sent().map_err(violation)?;

    let payload_len = idr_payload.len() as u64;
    let frame = VideoFrame {
        generation,
        frame_id: 0,
        config_id,
        flags: VIDEO_FRAME_FLAG_IDR,
        capture_ts,
        encode_done_ts,
        width: encoder_config.width,
        height: encoder_config.height,
        payload_len,
        payload: idr_payload,
    };
    write_envelope(
        &mut send,
        messages::type_id::VIDEO_FRAME,
        &messages::encode(&frame),
    )
    .await?;
    sm.on_first_idr_sent().map_err(violation)?;

    Ok((send, sm))
}

/// The setup messages plus first frame of a video Instance, as observed
/// by the receiving side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoInstanceIntro {
    pub monitor_id: u64,
    pub generation: VideoStreamGeneration,
    pub encoder_config: EncoderConfig,
    pub first_frame: VideoFrame,
}

/// Client side: accepts the next incoming unidirectional stream and reads
/// back a full Instance intro (`StreamPrologue`, `VideoStreamGeneration`,
/// `EncoderConfig`, and the first `VideoFrame`), validating message order
/// (spec 4.3.2) and `VideoFrame.payload_len` (spec 2.10) along the way.
///
/// This is wire-level validation only: it does not decode or display the
/// frame (spec 4's decode/display client behavior is M4's scope).
pub async fn read_video_instance_intro(
    connection: &quinn::Connection,
) -> Result<VideoInstanceIntro, VideoError> {
    let mut sm = VideoInstanceSm::new();
    let mut recv = connection.accept_uni().await.map_err(VideoError::Quic)?;
    let mut reader = EnvelopeReader::new(&mut recv);

    let stream_prologue = reader.read_prologue().await?;
    if stream_prologue.kind != StreamKind::Video {
        return Err(VideoError::WrongStreamKind);
    }
    sm.on_prologue_sent().map_err(violation)?;

    let max_len = StreamKind::Video.max_envelope_length();

    let (type_raw, payload) = reader.read_envelope(max_len).await?;
    if type_raw != messages::type_id::VIDEO_STREAM_GENERATION {
        return Err(VideoError::ProtocolViolation(
            ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE,
        ));
    }
    let generation: VideoStreamGeneration =
        messages::decode(&payload).map_err(VideoError::Decode)?;
    sm.on_generation_sent().map_err(violation)?;

    let (type_raw, payload) = reader.read_envelope(max_len).await?;
    if type_raw != messages::type_id::ENCODER_CONFIG {
        return Err(VideoError::ProtocolViolation(
            ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE,
        ));
    }
    let encoder_config: EncoderConfig = messages::decode(&payload).map_err(VideoError::Decode)?;
    sm.on_encoder_config_sent().map_err(violation)?;

    let (type_raw, payload) = reader.read_envelope(max_len).await?;
    if type_raw != messages::type_id::VIDEO_FRAME {
        return Err(VideoError::ProtocolViolation(
            ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE,
        ));
    }
    let first_frame: VideoFrame = messages::decode(&payload).map_err(VideoError::Decode)?;
    if !first_frame.payload_len_is_consistent() {
        return Err(VideoError::FrameLengthMismatch);
    }
    sm.on_first_idr_sent().map_err(violation)?;

    Ok(VideoInstanceIntro {
        monitor_id: stream_prologue.context_id,
        generation,
        encoder_config,
        first_frame,
    })
}
