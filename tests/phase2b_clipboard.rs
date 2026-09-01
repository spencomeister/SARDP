//! Phase 2b integration test: clipboard exchange (spec 2.7) over real
//! loopback QUIC. Covers the happy path (request_id correlation end to
//! end), the `CLIPBOARD_FORMAT_TOO_LARGE` policy-rejection path, and
//! `CLIPBOARD_RESPONSE_TIMEOUT` actually firing when the announcer never
//! responds. Uses fixed test-fixture clipboard content throughout, not a
//! real OS clipboard (out of scope for this PoC).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use sardp::clipboard_session::{
    ClipboardSessionError, accept_clipboard_formats, announce_clipboard_formats,
    read_clipboard_request, request_clipboard_data, request_clipboard_data_with_timeout,
    respond_to_clipboard_request,
};
use sardp::messages::{
    self, ClipboardFormatEntry, ClipboardFormats, ClipboardRequest, FormatNamespace,
};
use sardp::reason_code::ReasonCode;
use sardp::stream_kind::StreamKind;
use sardp::stream_reader::write_envelope;
use sardp::{net, pki, prologue};

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

/// Spec 2.2.1: `clipboard`'s initiator is "whichever side's clipboard
/// content just changed" -- for this test the server plays announcer and
/// the client plays requester, but nothing about the mechanism is
/// server/client-specific (either function works from either side).
#[tokio::test]
async fn request_id_is_correlated_end_to_end_and_data_round_trips() {
    let (client_connection, server_connection) = connect_pair().await;

    const REQUEST_ID: u64 = 42;
    let formats = ClipboardFormats {
        request_id: REQUEST_ID,
        formats: vec![
            ClipboardFormatEntry {
                namespace: FormatNamespace::Mime,
                format_id: "text/plain".into(),
            },
            ClipboardFormatEntry {
                namespace: FormatNamespace::Win32,
                format_id: "CF_UNICODETEXT".into(),
            },
        ],
    };

    let (announce_result, accept_result) = tokio::join!(
        announce_clipboard_formats(&server_connection, &formats),
        accept_clipboard_formats(&client_connection),
    );
    let (mut announcer_send, mut announcer_reader) =
        announce_result.expect("server announces formats");
    let (mut requester_send, mut requester_reader, received_formats) =
        accept_result.expect("client accepts the clipboard stream");

    assert_eq!(received_formats, formats);

    let request = ClipboardRequest {
        request_id: REQUEST_ID,
        namespace: FormatNamespace::Mime,
        format_id: "text/plain".into(),
    };
    let fixture_data = b"hello from the test fixture clipboard".to_vec();

    let (requester_result, announcer_result) = tokio::join!(
        request_clipboard_data(&mut requester_send, &mut requester_reader, &request),
        async {
            let received_request = read_clipboard_request(&mut announcer_reader)
                .await
                .expect("announcer reads the ClipboardRequest");
            assert_eq!(received_request, request);
            respond_to_clipboard_request(
                &mut announcer_send,
                received_request.request_id,
                received_request.namespace,
                received_request.format_id,
                fixture_data.clone(),
                None,
            )
            .await
        },
    );
    announcer_result.expect("announcer responds");
    let data = requester_result
        .expect("requester doesn't time out")
        .expect("announcer sent ClipboardData, not ClipboardError");

    assert_eq!(data.request_id, REQUEST_ID);
    assert_eq!(data.namespace, FormatNamespace::Mime);
    assert_eq!(data.format_id, "text/plain");
    assert_eq!(data.data, fixture_data);
}

#[tokio::test]
async fn oversized_format_is_rejected_with_clipboard_error() {
    let (client_connection, server_connection) = connect_pair().await;

    const REQUEST_ID: u64 = 7;
    let formats = ClipboardFormats {
        request_id: REQUEST_ID,
        formats: vec![ClipboardFormatEntry {
            namespace: FormatNamespace::Mime,
            format_id: "image/png".into(),
        }],
    };

    let (announce_result, accept_result) = tokio::join!(
        announce_clipboard_formats(&server_connection, &formats),
        accept_clipboard_formats(&client_connection),
    );
    let (mut announcer_send, mut announcer_reader) = announce_result.unwrap();
    let (mut requester_send, mut requester_reader, _) = accept_result.unwrap();

    let request = ClipboardRequest {
        request_id: REQUEST_ID,
        namespace: FormatNamespace::Mime,
        format_id: "image/png".into(),
    };
    // A policy limit of 10 bytes; the "actual" clipboard content is
    // larger, so the announcer must reply ClipboardError rather than
    // ClipboardData (spec 2.7's MAY per-format policy limit, spec 4.8
    // POLICY.6 CLIPBOARD_FORMAT_TOO_LARGE).
    let oversized_data = vec![0xAB; 1024];
    const POLICY_MAX_SIZE: usize = 10;

    let (requester_result, announcer_result) = tokio::join!(
        request_clipboard_data(&mut requester_send, &mut requester_reader, &request),
        async {
            let received_request = read_clipboard_request(&mut announcer_reader).await.unwrap();
            respond_to_clipboard_request(
                &mut announcer_send,
                received_request.request_id,
                received_request.namespace,
                received_request.format_id,
                oversized_data.clone(),
                Some(POLICY_MAX_SIZE),
            )
            .await
        },
    );
    announcer_result.expect("announcer responds");
    let response = requester_result.expect("requester doesn't time out");

    let error = response.expect_err("oversized format must yield ClipboardError, not data");
    assert_eq!(error.request_id, REQUEST_ID);
    assert_eq!(error.reason, ReasonCode::POLICY_CLIPBOARD_FORMAT_TOO_LARGE);
}

#[tokio::test]
async fn requester_times_out_if_announcer_never_responds() {
    let (client_connection, server_connection) = connect_pair().await;

    const REQUEST_ID: u64 = 99;
    let formats = ClipboardFormats {
        request_id: REQUEST_ID,
        formats: vec![ClipboardFormatEntry {
            namespace: FormatNamespace::Mime,
            format_id: "text/plain".into(),
        }],
    };

    let (announce_result, accept_result) = tokio::join!(
        announce_clipboard_formats(&server_connection, &formats),
        accept_clipboard_formats(&client_connection),
    );
    // Bind (not discard) the announcer's send/reader: dropping an
    // unfinished SendStream implicitly resets it, which would deliver the
    // requester an early, unrelated stream-reset error instead of
    // genuinely exercising the timeout (see other tests in this crate's
    // suite for the same gotcha with QUIC bidi streams).
    let (_announcer_send, _announcer_reader) = announce_result.unwrap();
    let (mut requester_send, mut requester_reader, _) = accept_result.unwrap();

    let request = ClipboardRequest {
        request_id: REQUEST_ID,
        namespace: FormatNamespace::Mime,
        format_id: "text/plain".into(),
    };

    // Small override so the test doesn't wait out the real 5s default;
    // the announcer above never reads the request nor replies, so this
    // must elapse and surface ResponseTimeout.
    let result = request_clipboard_data_with_timeout(
        &mut requester_send,
        &mut requester_reader,
        &request,
        Duration::from_millis(50),
    )
    .await;

    assert!(matches!(
        result,
        Err(ClipboardSessionError::ResponseTimeout)
    ));
}

/// spec 2.2.1/2.7: `ClipboardFormats.request_id` must match the stream's
/// own `context_id`. This deliberately crafts a mismatched stream (bypassing
/// `announce_clipboard_formats`, which always keeps them equal) to verify
/// `accept_clipboard_formats` rejects it at runtime rather than only in a
/// debug assertion that a release build would compile away.
#[tokio::test]
async fn request_id_mismatch_between_prologue_context_id_and_clipboard_formats_is_rejected() {
    let (client_connection, server_connection) = connect_pair().await;

    const CONTEXT_ID: u64 = 1;
    const MISMATCHED_REQUEST_ID: u64 = 2;
    let formats = ClipboardFormats {
        request_id: MISMATCHED_REQUEST_ID,
        formats: vec![ClipboardFormatEntry {
            namespace: FormatNamespace::Mime,
            format_id: "text/plain".into(),
        }],
    };

    let (mut send, _accept_result) = tokio::join!(
        async {
            let (mut send, _recv) = server_connection.open_bi().await.expect("open_bi");
            let mut prologue_bytes = Vec::new();
            prologue::encode(StreamKind::Clipboard, 1, CONTEXT_ID, &mut prologue_bytes);
            send.write_all(&prologue_bytes)
                .await
                .expect("write prologue");
            write_envelope(
                &mut send,
                messages::type_id::CLIPBOARD_FORMATS,
                &messages::encode(&formats),
            )
            .await
            .expect("write ClipboardFormats");
            send
        },
        accept_clipboard_formats(&client_connection),
    );
    // Keep the send stream open until the requester has read the mismatched
    // message; otherwise dropping it early would reset the stream first.
    let result = _accept_result;

    assert!(matches!(
        result,
        Err(ClipboardSessionError::RequestIdMismatch {
            context_id: CONTEXT_ID,
            request_id: MISMATCHED_REQUEST_ID,
        })
    ));

    send.finish().ok();
}
