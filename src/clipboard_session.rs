//! Clipboard exchange (spec 2.7). A `clipboard` stream is opened by
//! whichever side's clipboard content just changed (the "announcer",
//! spec 2.2.1's per-exchange initiator -- unlike every other stream kind,
//! either client or server may open one), bidirectional, with
//! `StreamPrologue.context_id` set to the exchange's `request_id`. The
//! announcer sends `ClipboardFormats`; the other side (the "requester")
//! may reply with `ClipboardRequest` naming one advertised format, and
//! the announcer replies with `ClipboardData` or (spec 4.8:
//! `POLICY.6 CLIPBOARD_FORMAT_TOO_LARGE`, among other reasons)
//! `ClipboardError`.

use crate::messages::{
    self, ClipboardData, ClipboardError, ClipboardFormats, ClipboardRequest, FormatNamespace,
};
use crate::prologue;
use crate::reason_code::ReasonCode;
use crate::stream_kind::StreamKind;
use crate::stream_reader::{EnvelopeReader, StreamReadError, write_envelope};

/// Requester-side timeout (spec 2.7: "既定5秒(SHOULD、設定可能)").
pub mod defaults {
    use std::time::Duration;
    pub const CLIPBOARD_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
}

#[derive(Debug)]
pub enum ClipboardSessionError {
    Quic(quinn::ConnectionError),
    Write(quinn::WriteError),
    Read(StreamReadError),
    Decode(ciborium::de::Error<std::io::Error>),
    /// The opened/accepted stream declared a `kind` other than `clipboard`
    /// (spec 2.2.1).
    WrongStreamKind,
    /// A message arrived with a type this exchange doesn't expect next.
    UnexpectedType(u16),
    /// `ClipboardFormats.request_id` didn't match the stream's own
    /// `context_id` (spec 2.2.1/2.7).
    RequestIdMismatch {
        context_id: u64,
        request_id: u64,
    },
    /// Spec 2.7: no `ClipboardData`/`ClipboardError` within
    /// `CLIPBOARD_RESPONSE_TIMEOUT`.
    ResponseTimeout,
}

impl From<StreamReadError> for ClipboardSessionError {
    fn from(e: StreamReadError) -> Self {
        Self::Read(e)
    }
}

impl From<quinn::WriteError> for ClipboardSessionError {
    fn from(e: quinn::WriteError) -> Self {
        Self::Write(e)
    }
}

/// Announcer side: opens a new bidirectional `clipboard` stream (spec
/// 2.2.1: `context_id` = `request_id`) and sends `ClipboardFormats`.
pub async fn announce_clipboard_formats(
    connection: &quinn::Connection,
    formats: &ClipboardFormats,
) -> Result<(quinn::SendStream, EnvelopeReader), ClipboardSessionError> {
    let (mut send, recv) = connection
        .open_bi()
        .await
        .map_err(ClipboardSessionError::Quic)?;

    let mut prologue_bytes = Vec::new();
    prologue::encode(
        StreamKind::Clipboard,
        1,
        formats.request_id,
        &mut prologue_bytes,
    );
    send.write_all(&prologue_bytes).await?;

    write_envelope(
        &mut send,
        messages::type_id::CLIPBOARD_FORMATS,
        &messages::encode(formats),
    )
    .await?;

    Ok((send, EnvelopeReader::new(recv)))
}

/// Requester side: accepts the incoming `clipboard` stream, validates its
/// prologue, and reads the `ClipboardFormats` off it. Returns the still-open
/// `SendStream`/`EnvelopeReader` (the same bidirectional stream carries the
/// rest of the exchange) alongside the decoded message.
pub async fn accept_clipboard_formats(
    connection: &quinn::Connection,
) -> Result<(quinn::SendStream, EnvelopeReader, ClipboardFormats), ClipboardSessionError> {
    let (send, recv) = connection
        .accept_bi()
        .await
        .map_err(ClipboardSessionError::Quic)?;
    let mut reader = EnvelopeReader::new(recv);

    let stream_prologue = reader.read_prologue().await?;
    if stream_prologue.kind != StreamKind::Clipboard {
        return Err(ClipboardSessionError::WrongStreamKind);
    }

    let (type_raw, payload) = reader
        .read_envelope(StreamKind::Clipboard.max_envelope_length())
        .await?;
    if type_raw != messages::type_id::CLIPBOARD_FORMATS {
        return Err(ClipboardSessionError::UnexpectedType(type_raw));
    }
    let formats: ClipboardFormats =
        messages::decode(&payload).map_err(ClipboardSessionError::Decode)?;
    if formats.request_id != stream_prologue.context_id {
        return Err(ClipboardSessionError::RequestIdMismatch {
            context_id: stream_prologue.context_id,
            request_id: formats.request_id,
        });
    }

    Ok((send, reader, formats))
}

/// Requester side: sends `ClipboardRequest`, then waits up to `timeout`
/// for `ClipboardData` or `ClipboardError`. See
/// [`request_clipboard_data`] for the spec-default timeout.
pub async fn request_clipboard_data_with_timeout(
    send: &mut quinn::SendStream,
    reader: &mut EnvelopeReader,
    request: &ClipboardRequest,
    timeout: std::time::Duration,
) -> Result<Result<ClipboardData, ClipboardError>, ClipboardSessionError> {
    write_envelope(
        send,
        messages::type_id::CLIPBOARD_REQUEST,
        &messages::encode(request),
    )
    .await?;

    tokio::time::timeout(timeout, async {
        let (type_raw, payload) = reader
            .read_envelope(StreamKind::Clipboard.max_envelope_length())
            .await?;
        if type_raw == messages::type_id::CLIPBOARD_DATA {
            let data: ClipboardData =
                messages::decode(&payload).map_err(ClipboardSessionError::Decode)?;
            Ok(Ok(data))
        } else if type_raw == messages::type_id::CLIPBOARD_ERROR {
            let error: ClipboardError =
                messages::decode(&payload).map_err(ClipboardSessionError::Decode)?;
            Ok(Err(error))
        } else {
            Err(ClipboardSessionError::UnexpectedType(type_raw))
        }
    })
    .await
    .map_err(|_elapsed| ClipboardSessionError::ResponseTimeout)?
}

/// Requester side, spec-default `CLIPBOARD_RESPONSE_TIMEOUT` (5s).
pub async fn request_clipboard_data(
    send: &mut quinn::SendStream,
    reader: &mut EnvelopeReader,
    request: &ClipboardRequest,
) -> Result<Result<ClipboardData, ClipboardError>, ClipboardSessionError> {
    request_clipboard_data_with_timeout(send, reader, request, defaults::CLIPBOARD_RESPONSE_TIMEOUT)
        .await
}

/// Announcer side: reads the next `ClipboardRequest` off the stream.
pub async fn read_clipboard_request(
    reader: &mut EnvelopeReader,
) -> Result<ClipboardRequest, ClipboardSessionError> {
    let (type_raw, payload) = reader
        .read_envelope(StreamKind::Clipboard.max_envelope_length())
        .await?;
    if type_raw != messages::type_id::CLIPBOARD_REQUEST {
        return Err(ClipboardSessionError::UnexpectedType(type_raw));
    }
    messages::decode(&payload).map_err(ClipboardSessionError::Decode)
}

/// Announcer side: replies to a `ClipboardRequest` with `ClipboardData` if
/// `data.len()` is within `max_size` (spec 2.7's MAY per-format policy
/// limit, independent of the `clipboard` stream's own 16MiB hard limit
/// enforced at the Envelope layer), otherwise `ClipboardError{reason:
/// POLICY_CLIPBOARD_FORMAT_TOO_LARGE}` (spec 4.8). `max_size: None` means
/// no policy limit is configured -- only the stream's own hard limit
/// applies (enforced separately by `Envelope::parse` on the receiving
/// end, not by this function).
pub async fn respond_to_clipboard_request(
    send: &mut quinn::SendStream,
    request_id: u64,
    namespace: FormatNamespace,
    format_id: String,
    data: Vec<u8>,
    max_size: Option<usize>,
) -> Result<(), quinn::WriteError> {
    if let Some(max) = max_size
        && data.len() > max
    {
        return send_clipboard_error(
            send,
            &ClipboardError {
                request_id,
                reason: ReasonCode::POLICY_CLIPBOARD_FORMAT_TOO_LARGE,
            },
        )
        .await;
    }
    let response = ClipboardData {
        request_id,
        namespace,
        format_id,
        data,
    };
    write_envelope(
        send,
        messages::type_id::CLIPBOARD_DATA,
        &messages::encode(&response),
    )
    .await
}

/// Announcer side: sends `ClipboardError` directly (e.g. for reasons
/// other than the format-too-large check `respond_to_clipboard_request`
/// already covers).
pub async fn send_clipboard_error(
    send: &mut quinn::SendStream,
    error: &ClipboardError,
) -> Result<(), quinn::WriteError> {
    write_envelope(
        send,
        messages::type_id::CLIPBOARD_ERROR,
        &messages::encode(error),
    )
    .await
}
