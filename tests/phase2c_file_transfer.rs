//! Phase 2c integration test: file transfer (spec 2.6) over real loopback
//! QUIC. Covers the happy path end to end (`FileTransferRequest` ->
//! `FileTransferAccept` on `control`, then `FileChunk`s + a checksum-verified
//! `FileTransferComplete` on `file`) and all four receiver-side MUST-reject
//! error paths from spec 2.6 / 4.8: `FILE_CHUNK_OVERLAP`,
//! `FILE_CHUNK_OUT_OF_RANGE`, `FILE_INCOMPLETE_TRANSFER`,
//! `FILE_CHECKSUM_MISMATCH`. Uses in-memory pseudo file data throughout, not
//! a real filesystem.

mod common;

use common::connect_pair;

use std::time::Duration;

use sha2::{Digest, Sha256};

use sardp::file_handle_store::FileHandleStore;
use sardp::file_transfer_session::{
    FileReassembly, FileTransferDecision, FileTransferOutcome, ReceiveOutcome, accept_file_stream,
    open_file_stream, read_file_transfer_decision, read_file_transfer_request,
    receive_file_with_timeout, run_file_transfer, send_file_chunk, send_file_data,
    send_file_transfer_accept, send_file_transfer_complete, send_file_transfer_request,
};
use sardp::messages::{
    FileChunk, FileTransferAccept, FileTransferComplete, FileTransferDirection, FileTransferRequest,
};
use sardp::reason_code::ReasonCode;

/// Real handshake, so these tests exercise `FileTransferRequest`/`Accept`
/// on the actual established `control` stream, matching how
/// `ActiveMonitor` was exercised in the Phase 2a test.
async fn handshake_pair() -> (
    quinn::Connection,
    sardp::handshake::ControlChannel,
    quinn::Connection,
    sardp::handshake::ControlChannel,
) {
    let (client_connection, client, server_connection, server) = common::handshake_pair(0x77).await;
    (
        client_connection,
        client.control,
        server_connection,
        server.control,
    )
}

const FILE_HANDLE: u64 = 0xF11E_0001;

/// Client uploads; server is the receiver. Drives `FileTransferRequest` ->
/// `FileTransferAccept` on `control`, then the client opens `file` and sends
/// chunked data ending in a checksum-verified `FileTransferComplete`.
#[tokio::test]
async fn upload_round_trips_end_to_end_with_checksum_verified() {
    let (client_connection, mut client_control, server_connection, mut server_control) =
        handshake_pair().await;

    let request = FileTransferRequest {
        request_id: 1,
        direction: FileTransferDirection::Upload,
        virtual_path: "/uploads/report.pdf".into(),
        declared_size: 11,
    };
    let (send_result, read_result) = tokio::join!(
        send_file_transfer_request(&mut client_control.send, &request),
        read_file_transfer_request(&mut server_control.reader),
    );
    send_result.expect("client sends FileTransferRequest");
    let received_request = read_result.expect("server reads FileTransferRequest");
    assert_eq!(received_request, request);

    let accept = FileTransferAccept {
        request_id: request.request_id,
        file_handle: FILE_HANDLE,
        resolved_size: 11,
        expiry_ts: 1_000_000,
    };
    let (send_result, read_result) = tokio::join!(
        send_file_transfer_accept(&mut server_control.send, &accept),
        read_file_transfer_decision(&mut client_control.reader),
    );
    send_result.expect("server sends FileTransferAccept");
    let decision = read_result.expect("client reads the decision");
    assert_eq!(decision, FileTransferDecision::Accept(accept));

    let data = b"hello file!".to_vec();
    let (open_result, accept_stream_result) = tokio::join!(
        open_file_stream(&client_connection, FILE_HANDLE),
        accept_file_stream(&server_connection, FILE_HANDLE),
    );
    let mut sender_send = open_result.expect("client opens file stream");
    let mut receiver_reader = accept_stream_result.expect("server accepts file stream");

    let (send_result, receive_result) = tokio::join!(
        send_file_data(&mut sender_send, &data, 4),
        sardp::file_transfer_session::receive_file(&mut receiver_reader, FILE_HANDLE, 11),
    );
    send_result.expect("client sends all chunks + FileTransferComplete");
    let outcome = receive_result.expect("server receives without a transport error");
    match outcome {
        ReceiveOutcome::Complete(received_data) => assert_eq!(received_data, data),
        ReceiveOutcome::Error(error) => panic!("unexpected FileTransferError: {error:?}"),
    }
}

/// Constructs an already-handshaken client/server pair with a `file` stream
/// open under `FILE_HANDLE`, skipping the control-stream negotiation (it is
/// already covered by the happy-path test above) so each error-path test can
/// focus on the chunk sequence under test.
async fn open_file_stream_pair() -> (quinn::SendStream, sardp::stream_reader::EnvelopeReader) {
    let (client_connection, _client_control, server_connection, _server_control) =
        handshake_pair().await;
    let (open_result, accept_result) = tokio::join!(
        open_file_stream(&client_connection, FILE_HANDLE),
        accept_file_stream(&server_connection, FILE_HANDLE),
    );
    let sender_send = open_result.unwrap();
    let receiver_reader = accept_result.unwrap();
    (sender_send, receiver_reader)
}

#[tokio::test]
async fn overlapping_chunk_offsets_yield_file_chunk_overlap_error() {
    let (mut sender_send, mut receiver_reader) = open_file_stream_pair().await;

    let (send_result, receive_result) = tokio::join!(
        async {
            send_file_chunk(
                &mut sender_send,
                &FileChunk {
                    offset: 0,
                    length: 3,
                    data: b"abc".to_vec(),
                },
            )
            .await?;
            // Overlaps bytes [0,3) already received.
            send_file_chunk(
                &mut sender_send,
                &FileChunk {
                    offset: 2,
                    length: 3,
                    data: b"xyz".to_vec(),
                },
            )
            .await
        },
        sardp::file_transfer_session::receive_file(&mut receiver_reader, FILE_HANDLE, 6),
    );
    send_result.expect("client sends both chunks");
    let outcome =
        receive_result.expect("server surfaces a FileTransferError, not a transport error");
    match outcome {
        ReceiveOutcome::Error(error) => {
            assert_eq!(error.file_handle, FILE_HANDLE);
            assert_eq!(error.reason, ReasonCode::PROTOCOL_FILE_CHUNK_OVERLAP);
        }
        ReceiveOutcome::Complete(_) => panic!("expected FileTransferError, got Complete"),
    }
}

#[tokio::test]
async fn chunk_beyond_resolved_size_yields_file_chunk_out_of_range_error() {
    let (mut sender_send, mut receiver_reader) = open_file_stream_pair().await;

    let oversized_chunk = FileChunk {
        offset: 2,
        length: 5, // extends to byte 7, past resolved_size=4
        data: b"abcde".to_vec(),
    };
    let (send_result, receive_result) = tokio::join!(
        send_file_chunk(&mut sender_send, &oversized_chunk),
        sardp::file_transfer_session::receive_file(&mut receiver_reader, FILE_HANDLE, 4),
    );
    send_result.expect("client sends the oversized chunk");
    let outcome =
        receive_result.expect("server surfaces a FileTransferError, not a transport error");
    match outcome {
        ReceiveOutcome::Error(error) => {
            assert_eq!(error.file_handle, FILE_HANDLE);
            assert_eq!(error.reason, ReasonCode::PROTOCOL_FILE_CHUNK_OUT_OF_RANGE);
        }
        ReceiveOutcome::Complete(_) => panic!("expected FileTransferError, got Complete"),
    }
}

#[tokio::test]
async fn complete_with_a_gap_still_outstanding_yields_file_incomplete_transfer_error() {
    let (mut sender_send, mut receiver_reader) = open_file_stream_pair().await;

    let (send_result, receive_result) = tokio::join!(
        async {
            // Only bytes [0,3) arrive; [3,6) never does.
            send_file_chunk(
                &mut sender_send,
                &FileChunk {
                    offset: 0,
                    length: 3,
                    data: b"abc".to_vec(),
                },
            )
            .await?;
            let mut hasher = Sha256::new();
            hasher.update(b"abcdef");
            send_file_transfer_complete(
                &mut sender_send,
                &FileTransferComplete {
                    checksum: hasher.finalize().to_vec(),
                },
            )
            .await
        },
        sardp::file_transfer_session::receive_file(&mut receiver_reader, FILE_HANDLE, 6),
    );
    send_result.expect("client sends the partial chunk + Complete");
    let outcome =
        receive_result.expect("server surfaces a FileTransferError, not a transport error");
    match outcome {
        ReceiveOutcome::Error(error) => {
            assert_eq!(error.file_handle, FILE_HANDLE);
            assert_eq!(error.reason, ReasonCode::PROTOCOL_FILE_INCOMPLETE_TRANSFER);
        }
        ReceiveOutcome::Complete(_) => panic!("expected FileTransferError, got Complete"),
    }
}

#[tokio::test]
async fn wrong_checksum_yields_file_checksum_mismatch_error() {
    let (mut sender_send, mut receiver_reader) = open_file_stream_pair().await;

    let (send_result, receive_result) = tokio::join!(
        async {
            send_file_chunk(
                &mut sender_send,
                &FileChunk {
                    offset: 0,
                    length: 6,
                    data: b"abcdef".to_vec(),
                },
            )
            .await?;
            send_file_transfer_complete(
                &mut sender_send,
                &FileTransferComplete {
                    checksum: vec![0u8; 32], // deliberately wrong
                },
            )
            .await
        },
        sardp::file_transfer_session::receive_file(&mut receiver_reader, FILE_HANDLE, 6),
    );
    send_result.expect("client sends the full chunk + a wrong-checksum Complete");
    let outcome =
        receive_result.expect("server surfaces a FileTransferError, not a transport error");
    match outcome {
        ReceiveOutcome::Error(error) => {
            assert_eq!(error.file_handle, FILE_HANDLE);
            assert_eq!(error.reason, ReasonCode::PROTOCOL_FILE_CHECKSUM_MISMATCH);
        }
        ReceiveOutcome::Complete(_) => panic!("expected FileTransferError, got Complete"),
    }
}

/// Spec 4.7 `FILE_TRANSFER_STALL_TIMEOUT` (30s default): if no further
/// `FileChunk`/`FileTransferComplete` arrives within the timeout of the
/// last message, the receiver treats the stream as stalled.
#[tokio::test]
async fn no_further_chunks_within_the_stall_timeout_yields_stall_timeout_error() {
    let (mut sender_send, mut receiver_reader) = open_file_stream_pair().await;

    let first_chunk = FileChunk {
        offset: 0,
        length: 3,
        data: b"abc".to_vec(),
    };
    // Small override so the test doesn't wait out the real 30s default;
    // the sender above sends one chunk and then genuinely goes silent
    // (not a full file, and no FileTransferComplete), so this must elapse
    // and surface a stall timeout rather than hanging or timing out the
    // test itself.
    let (send_result, receive_result) = tokio::join!(
        send_file_chunk(&mut sender_send, &first_chunk),
        receive_file_with_timeout(
            &mut receiver_reader,
            FILE_HANDLE,
            6,
            Duration::from_millis(50)
        ),
    );
    send_result.expect("client sends the one chunk");
    let outcome =
        receive_result.expect("server surfaces a FileTransferError, not a transport error");
    match outcome {
        ReceiveOutcome::Error(error) => {
            assert_eq!(error.file_handle, FILE_HANDLE);
            assert_eq!(error.reason, ReasonCode::TRANSPORT_STREAM_STALL_TIMEOUT);
        }
        ReceiveOutcome::Complete(_) => panic!("expected FileTransferError, got Complete"),
    }
}

/// DR-038: for a Download, the *sender* (here, the server) must open the
/// `file` stream itself rather than accept one -- unlike the old
/// bidirectional design, a unidirectional stream only lets its opener
/// write, so an implementation that still had the server merely `accept`
/// (as if the client always opened, regardless of direction) could never
/// actually send the client anything.
#[tokio::test]
async fn run_file_transfer_download_direction_has_the_sender_open_the_stream() {
    let (client_connection, server_connection) = connect_pair().await;
    let store = FileHandleStore::new();
    let session_id = [0x11; 16];
    let (file_handle, _expiry_ts) = store.issue(
        session_id,
        "alice".into(),
        FileTransferDirection::Download,
        11,
        Duration::from_secs(60),
    );
    let request = FileTransferRequest {
        request_id: 1,
        direction: FileTransferDirection::Download,
        virtual_path: "/downloads/report.pdf".into(),
        declared_size: 11,
    };

    let (server_result, client_result) = tokio::join!(
        run_file_transfer(
            &server_connection,
            &store,
            session_id,
            "alice",
            &request,
            file_handle,
        ),
        async {
            let mut reader = accept_file_stream(&client_connection, file_handle)
                .await
                .expect("client accepts the server-opened file stream");
            sardp::file_transfer_session::receive_file(&mut reader, file_handle, 11).await
        },
    );
    assert!(matches!(server_result, Ok(FileTransferOutcome::Done)));
    match client_result.expect("client receives without a transport error") {
        ReceiveOutcome::Complete(_) => {}
        ReceiveOutcome::Error(error) => panic!("unexpected FileTransferError: {error:?}"),
    }
}

/// DR-038: `run_file_transfer` no longer writes `FileTransferError` onto
/// any stream itself (it has no `SendStream` for `file`, which is now
/// unidirectional, and no access to `control` either) -- it surfaces a
/// receiver-detected error as `FileTransferOutcome::ReportError` data,
/// leaving the actual `control`-stream report to its caller.
#[tokio::test]
async fn run_file_transfer_upload_reports_a_receiver_detected_error_as_data() {
    let (client_connection, server_connection) = connect_pair().await;
    let store = FileHandleStore::new();
    let session_id = [0x22; 16];
    let (file_handle, _expiry_ts) = store.issue(
        session_id,
        "alice".into(),
        FileTransferDirection::Upload,
        6,
        Duration::from_secs(60),
    );
    let request = FileTransferRequest {
        request_id: 1,
        direction: FileTransferDirection::Upload,
        virtual_path: "/uploads/report.pdf".into(),
        declared_size: 6,
    };

    let (server_result, client_result) = tokio::join!(
        run_file_transfer(
            &server_connection,
            &store,
            session_id,
            "alice",
            &request,
            file_handle,
        ),
        async {
            let mut send = open_file_stream(&client_connection, file_handle)
                .await
                .expect("client opens the file stream");
            send_file_chunk(
                &mut send,
                &FileChunk {
                    offset: 0,
                    length: 6,
                    data: b"abcdef".to_vec(),
                },
            )
            .await?;
            send_file_transfer_complete(
                &mut send,
                &FileTransferComplete {
                    checksum: vec![0u8; 32], // deliberately wrong
                },
            )
            .await
        },
    );
    client_result.expect("client sends the chunk + a wrong-checksum Complete");
    match server_result.expect("run_file_transfer itself doesn't error") {
        FileTransferOutcome::ReportError(error) => {
            assert_eq!(error.file_handle, file_handle);
            assert_eq!(error.reason, ReasonCode::PROTOCOL_FILE_CHECKSUM_MISMATCH);
        }
        FileTransferOutcome::Done => panic!("expected ReportError, got Done"),
    }
}

/// `FileReassembly` is unit-tested directly in `file_transfer_session`; this
/// just double-checks the module's re-export is reachable from an
/// integration test too (a compile-time smoke check).
#[test]
fn file_reassembly_is_reachable_from_integration_tests() {
    let mut reassembly = FileReassembly::new(3);
    reassembly
        .apply_chunk(&FileChunk {
            offset: 0,
            length: 3,
            data: b"abc".to_vec(),
        })
        .unwrap();
    assert!(reassembly.is_complete());
}
