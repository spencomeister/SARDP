//! File transfer (spec 2.6). `FileTransferRequest`/`FileTransferAccept`/
//! `FileTransferReject` travel on the existing `control` stream -- the
//! client always sends `FileTransferRequest`, regardless of `direction`.
//! Once the server issues a `file_handle` via `FileTransferAccept`,
//! whichever side `direction` names as the sender opens a `file` stream
//! (`StreamPrologue.context_id = file_handle`) and streams `FileChunk`s,
//! ending with `FileTransferComplete` (SHA-256 over the whole file) or, on
//! one of spec 2.6's MUST-reject conditions, `FileTransferError` (spec 4.8:
//! `PROTOCOL.10 FILE_CHUNK_OVERLAP`, `PROTOCOL.11 FILE_CHUNK_OUT_OF_RANGE`,
//! `PROTOCOL.12 FILE_INCOMPLETE_TRANSFER`, `PROTOCOL.13
//! FILE_CHECKSUM_MISMATCH`).
//!
//! This module operates entirely on in-memory buffers (no real filesystem),
//! matching this PoC's simplified scope for file transfer.

use sha2::{Digest, Sha256};

use crate::messages::{
    self, FileChunk, FileTransferAccept, FileTransferComplete, FileTransferError,
    FileTransferReject, FileTransferRequest,
};
use crate::prologue;
use crate::reason_code::ReasonCode;
use crate::stream_kind::StreamKind;
use crate::stream_reader::{EnvelopeReader, StreamReadError, write_envelope};

#[derive(Debug)]
pub enum FileTransferSessionError {
    Quic(quinn::ConnectionError),
    Write(quinn::WriteError),
    Read(StreamReadError),
    Decode(ciborium::de::Error<std::io::Error>),
    /// The opened/accepted stream declared a `kind` other than `file`.
    WrongStreamKind,
    /// A message arrived with a type this exchange doesn't expect next.
    UnexpectedType(u16),
    /// The `file` stream's `context_id` didn't match the `file_handle`
    /// issued in `FileTransferAccept` (spec 2.6).
    FileHandleMismatch {
        context_id: u64,
        file_handle: u64,
    },
}

impl From<StreamReadError> for FileTransferSessionError {
    fn from(e: StreamReadError) -> Self {
        Self::Read(e)
    }
}

impl From<quinn::WriteError> for FileTransferSessionError {
    fn from(e: quinn::WriteError) -> Self {
        Self::Write(e)
    }
}

// --- control-stream: FileTransferRequest / Accept / Reject -----------------

pub async fn send_file_transfer_request(
    send: &mut quinn::SendStream,
    request: &FileTransferRequest,
) -> Result<(), quinn::WriteError> {
    write_envelope(
        send,
        messages::type_id::FILE_TRANSFER_REQUEST,
        &messages::encode(request),
    )
    .await
}

pub async fn read_file_transfer_request(
    reader: &mut EnvelopeReader,
) -> Result<FileTransferRequest, FileTransferSessionError> {
    let (type_raw, payload) = reader
        .read_envelope(StreamKind::Control.max_envelope_length())
        .await?;
    if type_raw != messages::type_id::FILE_TRANSFER_REQUEST {
        return Err(FileTransferSessionError::UnexpectedType(type_raw));
    }
    messages::decode(&payload).map_err(FileTransferSessionError::Decode)
}

pub async fn send_file_transfer_accept(
    send: &mut quinn::SendStream,
    accept: &FileTransferAccept,
) -> Result<(), quinn::WriteError> {
    write_envelope(
        send,
        messages::type_id::FILE_TRANSFER_ACCEPT,
        &messages::encode(accept),
    )
    .await
}

pub async fn send_file_transfer_reject(
    send: &mut quinn::SendStream,
    reject: &FileTransferReject,
) -> Result<(), quinn::WriteError> {
    write_envelope(
        send,
        messages::type_id::FILE_TRANSFER_REJECT,
        &messages::encode(reject),
    )
    .await
}

/// The server's response to a `FileTransferRequest` (spec 2.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileTransferDecision {
    Accept(FileTransferAccept),
    Reject(FileTransferReject),
}

pub async fn read_file_transfer_decision(
    reader: &mut EnvelopeReader,
) -> Result<FileTransferDecision, FileTransferSessionError> {
    let (type_raw, payload) = reader
        .read_envelope(StreamKind::Control.max_envelope_length())
        .await?;
    if type_raw == messages::type_id::FILE_TRANSFER_ACCEPT {
        let accept = messages::decode(&payload).map_err(FileTransferSessionError::Decode)?;
        Ok(FileTransferDecision::Accept(accept))
    } else if type_raw == messages::type_id::FILE_TRANSFER_REJECT {
        let reject = messages::decode(&payload).map_err(FileTransferSessionError::Decode)?;
        Ok(FileTransferDecision::Reject(reject))
    } else {
        Err(FileTransferSessionError::UnexpectedType(type_raw))
    }
}

// --- `file` stream: opening, chunks, completion/error ----------------------

/// Sender side: opens the `file` stream once a `file_handle` has been
/// issued (spec 2.6: `StreamPrologue.context_id = file_handle`).
pub async fn open_file_stream(
    connection: &quinn::Connection,
    file_handle: u64,
) -> Result<(quinn::SendStream, EnvelopeReader), FileTransferSessionError> {
    let (mut send, recv) = connection
        .open_bi()
        .await
        .map_err(FileTransferSessionError::Quic)?;

    let mut prologue_bytes = Vec::new();
    prologue::encode(StreamKind::File, 1, file_handle, &mut prologue_bytes);
    send.write_all(&prologue_bytes).await?;

    Ok((send, EnvelopeReader::new(recv)))
}

/// Receiver side: accepts the `file` stream and validates its `context_id`
/// against the `file_handle` this side itself issued.
pub async fn accept_file_stream(
    connection: &quinn::Connection,
    expected_file_handle: u64,
) -> Result<(quinn::SendStream, EnvelopeReader), FileTransferSessionError> {
    let (send, recv) = connection
        .accept_bi()
        .await
        .map_err(FileTransferSessionError::Quic)?;
    let mut reader = EnvelopeReader::new(recv);

    let stream_prologue = reader.read_prologue().await?;
    if stream_prologue.kind != StreamKind::File {
        return Err(FileTransferSessionError::WrongStreamKind);
    }
    if stream_prologue.context_id != expected_file_handle {
        return Err(FileTransferSessionError::FileHandleMismatch {
            context_id: stream_prologue.context_id,
            file_handle: expected_file_handle,
        });
    }

    Ok((send, reader))
}

pub async fn send_file_chunk(
    send: &mut quinn::SendStream,
    chunk: &FileChunk,
) -> Result<(), quinn::WriteError> {
    write_envelope(
        send,
        messages::type_id::FILE_CHUNK,
        &messages::encode(chunk),
    )
    .await
}

pub async fn send_file_transfer_complete(
    send: &mut quinn::SendStream,
    complete: &FileTransferComplete,
) -> Result<(), quinn::WriteError> {
    write_envelope(
        send,
        messages::type_id::FILE_TRANSFER_COMPLETE,
        &messages::encode(complete),
    )
    .await
}

pub async fn send_file_transfer_error(
    send: &mut quinn::SendStream,
    error: &FileTransferError,
) -> Result<(), quinn::WriteError> {
    write_envelope(
        send,
        messages::type_id::FILE_TRANSFER_ERROR,
        &messages::encode(error),
    )
    .await
}

/// Sender-side convenience: splits `data` into `chunk_size`-sized
/// `FileChunk`s, sends them in order, then sends `FileTransferComplete`
/// with the SHA-256 checksum of the whole file.
pub async fn send_file_data(
    send: &mut quinn::SendStream,
    data: &[u8],
    chunk_size: usize,
) -> Result<(), quinn::WriteError> {
    for (index, chunk_bytes) in data.chunks(chunk_size.max(1)).enumerate() {
        let offset = (index * chunk_size) as u64;
        let chunk = FileChunk {
            offset,
            length: chunk_bytes.len() as u32,
            data: chunk_bytes.to_vec(),
        };
        send_file_chunk(send, &chunk).await?;
    }

    let mut hasher = Sha256::new();
    hasher.update(data);
    send_file_transfer_complete(
        send,
        &FileTransferComplete {
            checksum: hasher.finalize().to_vec(),
        },
    )
    .await
}

// --- receiver-side reassembly + validation (spec 2.6 MUST-reject rules) ---

/// Tracks received byte ranges for one in-progress `file`-stream transfer
/// and enforces spec 2.6's MUST-reject rules: no overlapping offsets,
/// nothing beyond `resolved_size`, no gaps outstanding at
/// `FileTransferComplete`, and a matching SHA-256 checksum.
pub struct FileReassembly {
    resolved_size: u64,
    /// Non-overlapping `(offset, length)` ranges received so far, in
    /// arrival order (not sorted -- `is_complete` sorts a copy).
    received_ranges: Vec<(u64, u64)>,
    data: Vec<u8>,
}

impl FileReassembly {
    pub fn new(resolved_size: u64) -> Self {
        Self {
            resolved_size,
            received_ranges: Vec::new(),
            data: vec![0u8; resolved_size as usize],
        }
    }

    /// Applies one `FileChunk`. Returns `Err(reason)` per spec 2.6's
    /// MUST-reject rules if the chunk's declared `length` doesn't match its
    /// actual `data` length, extends beyond `resolved_size`, or overlaps a
    /// previously-received range; the caller sends `FileTransferError` with
    /// this reason and aborts the transfer.
    pub fn apply_chunk(&mut self, chunk: &FileChunk) -> Result<(), ReasonCode> {
        if chunk.data.len() != chunk.length as usize {
            // Spec 2.6 doesn't name a distinct reason for this case ("length"
            // validated independently of the Envelope's own length); treated
            // as out-of-range since it's equally a malformed chunk.
            return Err(ReasonCode::PROTOCOL_FILE_CHUNK_OUT_OF_RANGE);
        }
        let end = chunk
            .offset
            .checked_add(u64::from(chunk.length))
            .ok_or(ReasonCode::PROTOCOL_FILE_CHUNK_OUT_OF_RANGE)?;
        if end > self.resolved_size {
            return Err(ReasonCode::PROTOCOL_FILE_CHUNK_OUT_OF_RANGE);
        }
        for &(existing_offset, existing_len) in &self.received_ranges {
            let existing_end = existing_offset + existing_len;
            if chunk.offset < existing_end && end > existing_offset {
                return Err(ReasonCode::PROTOCOL_FILE_CHUNK_OVERLAP);
            }
        }

        self.data[chunk.offset as usize..end as usize].copy_from_slice(&chunk.data);
        self.received_ranges
            .push((chunk.offset, u64::from(chunk.length)));
        Ok(())
    }

    /// Spec 2.6: MUST reject `FileTransferComplete` while gaps remain.
    pub fn is_complete(&self) -> bool {
        let mut ranges = self.received_ranges.clone();
        ranges.sort_unstable_by_key(|&(offset, _)| offset);
        let mut covered = 0u64;
        for (offset, len) in ranges {
            if offset > covered {
                return false;
            }
            covered = covered.max(offset + len);
        }
        covered >= self.resolved_size
    }

    /// Verifies a `FileTransferComplete.checksum` (SHA-256 over the entire
    /// file) against what was actually reassembled.
    pub fn verify_checksum(&self, checksum: &[u8]) -> bool {
        let mut hasher = Sha256::new();
        hasher.update(&self.data);
        hasher.finalize().as_slice() == checksum
    }

    pub fn into_data(self) -> Vec<u8> {
        self.data
    }
}

/// The result of driving [`receive_file`] to completion: either the fully
/// reassembled file (checksum verified) or the `FileTransferError` that was
/// both sent to the peer and returned here.
#[derive(Debug)]
pub enum ReceiveOutcome {
    Complete(Vec<u8>),
    Error(FileTransferError),
}

/// Receiver side: reads `FileChunk`s off the `file` stream until either a
/// verified `FileTransferComplete` or one of spec 2.6's MUST-reject
/// conditions is hit, sending `FileTransferError` back on the latter.
pub async fn receive_file(
    send: &mut quinn::SendStream,
    reader: &mut EnvelopeReader,
    file_handle: u64,
    resolved_size: u64,
) -> Result<ReceiveOutcome, FileTransferSessionError> {
    let mut reassembly = FileReassembly::new(resolved_size);
    loop {
        let (type_raw, payload) = reader
            .read_envelope(StreamKind::File.max_envelope_length())
            .await?;
        if type_raw == messages::type_id::FILE_CHUNK {
            let chunk: FileChunk =
                messages::decode(&payload).map_err(FileTransferSessionError::Decode)?;
            if let Err(reason) = reassembly.apply_chunk(&chunk) {
                let error = FileTransferError {
                    file_handle,
                    reason,
                };
                send_file_transfer_error(send, &error).await?;
                return Ok(ReceiveOutcome::Error(error));
            }
        } else if type_raw == messages::type_id::FILE_TRANSFER_COMPLETE {
            let complete: FileTransferComplete =
                messages::decode(&payload).map_err(FileTransferSessionError::Decode)?;
            if !reassembly.is_complete() {
                let error = FileTransferError {
                    file_handle,
                    reason: ReasonCode::PROTOCOL_FILE_INCOMPLETE_TRANSFER,
                };
                send_file_transfer_error(send, &error).await?;
                return Ok(ReceiveOutcome::Error(error));
            }
            if !reassembly.verify_checksum(&complete.checksum) {
                let error = FileTransferError {
                    file_handle,
                    reason: ReasonCode::PROTOCOL_FILE_CHECKSUM_MISMATCH,
                };
                send_file_transfer_error(send, &error).await?;
                return Ok(ReceiveOutcome::Error(error));
            }
            return Ok(ReceiveOutcome::Complete(reassembly.into_data()));
        } else if type_raw == messages::type_id::FILE_TRANSFER_ERROR {
            let error: FileTransferError =
                messages::decode(&payload).map_err(FileTransferSessionError::Decode)?;
            return Ok(ReceiveOutcome::Error(error));
        } else {
            return Err(FileTransferSessionError::UnexpectedType(type_raw));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(offset: u64, data: &[u8]) -> FileChunk {
        FileChunk {
            offset,
            length: data.len() as u32,
            data: data.to_vec(),
        }
    }

    #[test]
    fn non_overlapping_chunks_covering_the_whole_file_are_complete() {
        let mut reassembly = FileReassembly::new(6);
        reassembly.apply_chunk(&chunk(0, b"abc")).unwrap();
        reassembly.apply_chunk(&chunk(3, b"def")).unwrap();
        assert!(reassembly.is_complete());
        assert_eq!(reassembly.into_data(), b"abcdef");
    }

    #[test]
    fn a_gap_is_not_complete() {
        let mut reassembly = FileReassembly::new(6);
        reassembly.apply_chunk(&chunk(0, b"ab")).unwrap();
        // bytes 2..3 never arrive
        reassembly.apply_chunk(&chunk(3, b"def")).unwrap();
        assert!(!reassembly.is_complete());
    }

    #[test]
    fn overlapping_offsets_are_rejected() {
        let mut reassembly = FileReassembly::new(6);
        reassembly.apply_chunk(&chunk(0, b"abc")).unwrap();
        let err = reassembly.apply_chunk(&chunk(2, b"xyz")).unwrap_err();
        assert_eq!(err, ReasonCode::PROTOCOL_FILE_CHUNK_OVERLAP);
    }

    #[test]
    fn chunk_beyond_resolved_size_is_rejected() {
        let mut reassembly = FileReassembly::new(4);
        let err = reassembly.apply_chunk(&chunk(2, b"abc")).unwrap_err();
        assert_eq!(err, ReasonCode::PROTOCOL_FILE_CHUNK_OUT_OF_RANGE);
    }

    #[test]
    fn checksum_verification_matches_sha256_of_reassembled_bytes() {
        let mut reassembly = FileReassembly::new(3);
        reassembly.apply_chunk(&chunk(0, b"abc")).unwrap();
        let mut hasher = Sha256::new();
        hasher.update(b"abc");
        let checksum = hasher.finalize().to_vec();
        assert!(reassembly.verify_checksum(&checksum));
        assert!(!reassembly.verify_checksum(&[0u8; 32]));
    }
}
