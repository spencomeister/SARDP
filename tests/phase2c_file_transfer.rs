//! Phase 2c integration test: file transfer (spec 2.6) over real loopback
//! QUIC. Covers the happy path end to end (`FileTransferRequest` ->
//! `FileTransferAccept` on `control`, then `FileChunk`s + a checksum-verified
//! `FileTransferComplete` on `file`) and all four receiver-side MUST-reject
//! error paths from spec 2.6 / 4.8: `FILE_CHUNK_OVERLAP`,
//! `FILE_CHUNK_OUT_OF_RANGE`, `FILE_INCOMPLETE_TRANSFER`,
//! `FILE_CHECKSUM_MISMATCH`. Uses in-memory pseudo file data throughout, not
//! a real filesystem.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use ed25519_dalek::SigningKey;
use sha2::{Digest, Sha256};

use sardp::file_transfer_session::{
    FileReassembly, FileTransferDecision, ReceiveOutcome, accept_file_stream, open_file_stream,
    read_file_transfer_decision, read_file_transfer_request, send_file_chunk, send_file_data,
    send_file_transfer_accept, send_file_transfer_complete, send_file_transfer_request,
};
use sardp::handshake::{client_handshake, server_handshake};
use sardp::messages::{
    FileChunk, FileTransferAccept, FileTransferComplete, FileTransferDirection, FileTransferRequest,
};
use sardp::reason_code::ReasonCode;
use sardp::{net, pki};

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

async fn connect_pair() -> (quinn::Connection, quinn::Connection) {
    let test_cert = pki::generate_test_certificate("localhost");
    let server_endpoint = net::server_endpoint(loopback(0), &test_cert);
    let server_addr = server_endpoint.local_addr().unwrap();
    let client_endpoint = net::client_endpoint(loopback(0), &test_cert.cert_der);

    let server_accept = tokio::spawn(async move {
        let incoming = server_endpoint.accept().await.expect("incoming connection");
        let connection = incoming.await.expect("server-side handshake");
        (server_endpoint, connection)
    });
    let client_connection = client_endpoint
        .connect(server_addr, "localhost")
        .expect("valid connect params")
        .await
        .expect("client-side handshake");
    let (_server_endpoint, server_connection) = server_accept.await.unwrap();
    (client_connection, server_connection)
}

/// Real handshake, so this test exercises `FileTransferRequest`/`Accept` on
/// the actual established `control` stream, matching how `ActiveMonitor` was
/// exercised in the Phase 2a test.
async fn handshake_pair() -> (
    quinn::Connection,
    sardp::handshake::ControlChannel,
    quinn::Connection,
    sardp::handshake::ControlChannel,
) {
    let (client_connection, server_connection) = connect_pair().await;
    let client_signing_key = SigningKey::from_bytes(&[0x77; 32]);
    let trusted_public_key = client_signing_key.verifying_key();
    let (client_result, server_result) = tokio::join!(
        client_handshake(
            &client_connection,
            &client_signing_key,
            "test-client",
            "alice",
            "device-1",
        ),
        server_handshake(&server_connection, "test-server", &trusted_public_key),
    );
    let (_client_outcome, _client_sm, client_control) =
        client_result.expect("client handshake succeeds");
    let (_server_outcome, _server_sm, server_control) =
        server_result.expect("server handshake succeeds");
    (
        client_connection,
        client_control,
        server_connection,
        server_control,
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
    let (mut sender_send, _sender_reader) = open_result.expect("client opens file stream");
    let (mut receiver_send, mut receiver_reader) =
        accept_stream_result.expect("server accepts file stream");

    let (send_result, receive_result) = tokio::join!(
        send_file_data(&mut sender_send, &data, 4),
        sardp::file_transfer_session::receive_file(
            &mut receiver_send,
            &mut receiver_reader,
            FILE_HANDLE,
            11,
        ),
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
async fn open_file_stream_pair() -> (
    quinn::SendStream,
    sardp::stream_reader::EnvelopeReader,
    quinn::SendStream,
    sardp::stream_reader::EnvelopeReader,
) {
    let (client_connection, _client_control, server_connection, _server_control) =
        handshake_pair().await;
    let (open_result, accept_result) = tokio::join!(
        open_file_stream(&client_connection, FILE_HANDLE),
        accept_file_stream(&server_connection, FILE_HANDLE),
    );
    let (sender_send, sender_reader) = open_result.unwrap();
    let (receiver_send, receiver_reader) = accept_result.unwrap();
    (sender_send, sender_reader, receiver_send, receiver_reader)
}

#[tokio::test]
async fn overlapping_chunk_offsets_yield_file_chunk_overlap_error() {
    let (mut sender_send, _sender_reader, mut receiver_send, mut receiver_reader) =
        open_file_stream_pair().await;

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
        sardp::file_transfer_session::receive_file(
            &mut receiver_send,
            &mut receiver_reader,
            FILE_HANDLE,
            6,
        ),
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
    let (mut sender_send, _sender_reader, mut receiver_send, mut receiver_reader) =
        open_file_stream_pair().await;

    let oversized_chunk = FileChunk {
        offset: 2,
        length: 5, // extends to byte 7, past resolved_size=4
        data: b"abcde".to_vec(),
    };
    let (send_result, receive_result) = tokio::join!(
        send_file_chunk(&mut sender_send, &oversized_chunk),
        sardp::file_transfer_session::receive_file(
            &mut receiver_send,
            &mut receiver_reader,
            FILE_HANDLE,
            4,
        ),
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
    let (mut sender_send, _sender_reader, mut receiver_send, mut receiver_reader) =
        open_file_stream_pair().await;

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
        sardp::file_transfer_session::receive_file(
            &mut receiver_send,
            &mut receiver_reader,
            FILE_HANDLE,
            6,
        ),
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
    let (mut sender_send, _sender_reader, mut receiver_send, mut receiver_reader) =
        open_file_stream_pair().await;

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
        sardp::file_transfer_session::receive_file(
            &mut receiver_send,
            &mut receiver_reader,
            FILE_HANDLE,
            6,
        ),
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
