//! `input` stream (spec 2.12, 2.2.1: client-initiated, unidirectional
//! client->server, `context_id` unused/0).
//!
//! This module only covers the wire-level message read/write; it does not
//! decide *whether* a given event should be injected (that's a
//! `permission_sm.is_granted(bit::INPUT_KEYBOARD | bit::INPUT_MOUSE)` gate
//! at the call site, same pattern as `bit::VIEW` gating frame generation)
//! nor *how* it gets injected into the OS (SendInput etc., a separate,
//! inherently-`unsafe` concern that can't live in this
//! `unsafe_code = "forbid"` crate -- see 3W-1-d's later stages).

use crate::messages::{
    self, ImeComposition, ImeModeChange, KeyEvent, MouseButton, MouseMove, TextInput, Wheel,
};
use crate::prologue;
use crate::stream_kind::StreamKind;
use crate::stream_reader::{EnvelopeReader, StreamReadError, write_envelope};

#[derive(Debug)]
pub enum ReadInputError {
    Quic(quinn::ConnectionError),
    Read(StreamReadError),
    Decode(ciborium::de::Error<std::io::Error>),
    WrongStreamKind,
    UnexpectedType(u16),
}

/// Opens the `input` stream (spec 2.2.1: client-initiated, `context_id`
/// unused, 0).
pub async fn open_input_stream(
    connection: &quinn::Connection,
) -> Result<quinn::SendStream, quinn::ConnectionError> {
    let mut send = connection.open_uni().await?;
    let mut prologue_bytes = Vec::new();
    prologue::encode(StreamKind::Input, 1, 0, &mut prologue_bytes);
    // Same one-shot-per-call tradeoff as `feedback_session::open_feedback_stream`:
    // a write error here surfaces on the next real send below.
    let _ = send.write_all(&prologue_bytes).await;
    Ok(send)
}

pub async fn send_key_event(
    send: &mut quinn::SendStream,
    event: &KeyEvent,
) -> Result<(), quinn::WriteError> {
    write_envelope(send, messages::type_id::KEY_EVENT, &messages::encode(event)).await
}

pub async fn send_text_input(
    send: &mut quinn::SendStream,
    event: &TextInput,
) -> Result<(), quinn::WriteError> {
    write_envelope(
        send,
        messages::type_id::TEXT_INPUT,
        &messages::encode(event),
    )
    .await
}

pub async fn send_ime_composition(
    send: &mut quinn::SendStream,
    event: &ImeComposition,
) -> Result<(), quinn::WriteError> {
    write_envelope(
        send,
        messages::type_id::IME_COMPOSITION,
        &messages::encode(event),
    )
    .await
}

pub async fn send_mouse_move(
    send: &mut quinn::SendStream,
    event: &MouseMove,
) -> Result<(), quinn::WriteError> {
    write_envelope(
        send,
        messages::type_id::MOUSE_MOVE,
        &messages::encode(event),
    )
    .await
}

pub async fn send_mouse_button(
    send: &mut quinn::SendStream,
    event: &MouseButton,
) -> Result<(), quinn::WriteError> {
    write_envelope(
        send,
        messages::type_id::MOUSE_BUTTON,
        &messages::encode(event),
    )
    .await
}

pub async fn send_wheel(
    send: &mut quinn::SendStream,
    event: &Wheel,
) -> Result<(), quinn::WriteError> {
    write_envelope(send, messages::type_id::WHEEL, &messages::encode(event)).await
}

pub async fn send_ime_mode_change(
    send: &mut quinn::SendStream,
    event: &ImeModeChange,
) -> Result<(), quinn::WriteError> {
    write_envelope(
        send,
        messages::type_id::IME_MODE_CHANGE,
        &messages::encode(event),
    )
    .await
}

/// Any message the `input` stream can carry (spec 2.12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputMessage {
    Key(KeyEvent),
    Text(TextInput),
    ImeComposition(ImeComposition),
    MouseMove(MouseMove),
    MouseButton(MouseButton),
    Wheel(Wheel),
    ImeModeChange(ImeModeChange),
}

/// Server side: the accepted `input` stream, positioned to read event
/// Envelopes one after another. A real server keeps one of these per
/// client for the session's duration.
pub struct InputReceiver {
    reader: EnvelopeReader,
}

impl InputReceiver {
    /// Accepts the next incoming unidirectional stream and validates its
    /// `StreamPrologue` as `input`.
    pub async fn accept(connection: &quinn::Connection) -> Result<Self, ReadInputError> {
        let recv = connection
            .accept_uni()
            .await
            .map_err(ReadInputError::Quic)?;
        let mut reader = EnvelopeReader::new(recv);
        let stream_prologue = reader.read_prologue().await.map_err(ReadInputError::Read)?;
        if stream_prologue.kind != StreamKind::Input {
            return Err(ReadInputError::WrongStreamKind);
        }
        Ok(Self { reader })
    }

    /// For a caller that accepted the stream and validated its `input`
    /// prologue itself (a server dispatching every incoming
    /// unidirectional stream on `kind`, see
    /// `audio_session::accept_audio_capture_from_reader`).
    pub fn from_reader(reader: EnvelopeReader) -> Self {
        Self { reader }
    }

    /// Reads the next Envelope on this stream and decodes it as whichever
    /// spec 2.12 message type it carries.
    pub async fn read_message(&mut self) -> Result<InputMessage, ReadInputError> {
        let (type_raw, payload) = self
            .reader
            .read_envelope(StreamKind::Input.max_envelope_length())
            .await
            .map_err(ReadInputError::Read)?;
        match type_raw {
            t if t == messages::type_id::KEY_EVENT => Ok(InputMessage::Key(
                messages::decode(&payload).map_err(ReadInputError::Decode)?,
            )),
            t if t == messages::type_id::TEXT_INPUT => Ok(InputMessage::Text(
                messages::decode(&payload).map_err(ReadInputError::Decode)?,
            )),
            t if t == messages::type_id::IME_COMPOSITION => Ok(InputMessage::ImeComposition(
                messages::decode(&payload).map_err(ReadInputError::Decode)?,
            )),
            t if t == messages::type_id::MOUSE_MOVE => Ok(InputMessage::MouseMove(
                messages::decode(&payload).map_err(ReadInputError::Decode)?,
            )),
            t if t == messages::type_id::MOUSE_BUTTON => Ok(InputMessage::MouseButton(
                messages::decode(&payload).map_err(ReadInputError::Decode)?,
            )),
            t if t == messages::type_id::WHEEL => Ok(InputMessage::Wheel(
                messages::decode(&payload).map_err(ReadInputError::Decode)?,
            )),
            t if t == messages::type_id::IME_MODE_CHANGE => Ok(InputMessage::ImeModeChange(
                messages::decode(&payload).map_err(ReadInputError::Decode)?,
            )),
            other => Err(ReadInputError::UnexpectedType(other)),
        }
    }
}
