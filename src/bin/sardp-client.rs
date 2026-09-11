//! SARDP PoC reference client (Phase 1: "実バイナリ化"). Connects to
//! `sardp-server`, completes the handshake and TimeSync, receives the
//! single-monitor video Instance, and decodes each frame via ffmpeg.
//! Real window/screen display is out of scope for this sandbox (cannot be
//! verified here); instead every decoded frame's embedded timecode is
//! logged, proving the wire protocol end-to-end the same way M4-M6's
//! tests did. Sends real `TransportFeedback` after every frame (spec
//! 2.14), which is what actually lets `sardp-server`'s backpressure
//! mechanism (spec 2.10, DR-029) do anything when the two binaries run
//! against each other.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use ed25519_dalek::SigningKey;

use sardp::audio_session;
use sardp::clipboard_session;
use sardp::connection_sm::{ConnectionSm, defaults as timeouts};
use sardp::decoder;
use sardp::feedback_session::{self, FrameTimestamps};
use sardp::handshake::client_handshake;
use sardp::messages::{self, AudioCodec, AudioConfig, SessionClose};
use sardp::permission_set::bit;
use sardp::reason_code::ReasonCode;
use sardp::reconnection::client_reconnect;
use sardp::session_file::{SavedSession, read_saved_session, write_saved_session};
use sardp::stream_reader::write_envelope;
use sardp::timecode_frame::extract_timecode;
use sardp::timesync::client_time_sync;
use sardp::video_session::accept_video_instance;
use sardp::{StreamKind, clock, dev_identity, net, pki};

struct Args {
    server: SocketAddr,
    trust_cert: PathBuf,
    tls_hostname: String,
    signing_key: SigningKey,
    user_id: String,
    device_id: String,
    client_name: String,
    target_latency_us: u32,
    /// Where to persist `session_id`/`reconnect_token` across process
    /// runs, so a *second* invocation of this binary can demonstrate
    /// spec 4.6 reconnection (`SessionReauthenticate`) against a real
    /// `sardp-server`, instead of always doing a fresh `ClientHello`
    /// handshake. Not needed for a single long-running client -- only
    /// for the "kill this process, run it again" demo/test scenario,
    /// since a real always-up client would auto-reconnect in-process
    /// instead (out of scope for this PoC).
    session_file: Option<PathBuf>,
}

fn print_help() {
    println!(
        "sardp-client -- SARDP PoC reference client\n\n\
USAGE:\n    sardp-client --server <ADDR> --trust-cert <PATH> [OPTIONS]\n\n\
OPTIONS:\n\
    --server <ADDR>           Server address to connect to (required)\n\
    --trust-cert <PATH>       PEM certificate to trust as the server's root\n\
                              (required -- see sardp-server's startup log)\n\
    --tls-hostname <NAME>     Hostname the server's certificate was issued\n\
                              for (default localhost)\n\
    --signing-key-seed <HEX>  32-byte hex Ed25519 seed to sign auth with\n\
                              (default: PoC fixed dev key)\n\
    --user-id <ID>            (default demo-user)\n\
    --device-id <ID>          (default demo-device)\n\
    --client-name <NAME>      (default sardp-client)\n\
    --target-latency-us <N>   TransportFeedback.target_latency_us (default 50000)\n\
    --session-file <PATH>     Persist session_id/reconnect_token here; if the\n\
                              file already exists on startup, reconnect\n\
                              (SessionReauthenticate, spec 4.6) instead of a\n\
                              fresh handshake (demo/test use only)\n\
    --help                    Show this message"
    );
}

fn parse_args() -> Args {
    let mut server: Option<SocketAddr> = None;
    let mut trust_cert: Option<PathBuf> = None;
    let mut tls_hostname = "localhost".to_string();
    let mut signing_key = dev_identity::dev_signing_key();
    let mut user_id = "demo-user".to_string();
    let mut device_id = "demo-device".to_string();
    let mut client_name = "sardp-client".to_string();
    let mut target_latency_us = 50_000u32;
    let mut session_file: Option<PathBuf> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--server" => {
                server = Some(
                    args.next()
                        .expect("--server requires a value")
                        .parse()
                        .expect("invalid --server address"),
                )
            }
            "--trust-cert" => {
                trust_cert = Some(PathBuf::from(
                    args.next().expect("--trust-cert requires a value"),
                ))
            }
            "--tls-hostname" => {
                tls_hostname = args.next().expect("--tls-hostname requires a value")
            }
            "--signing-key-seed" => {
                let hex = args.next().expect("--signing-key-seed requires a value");
                let seed = dev_identity::parse_hex32(&hex).expect("invalid --signing-key-seed");
                signing_key = SigningKey::from_bytes(&seed);
            }
            "--user-id" => user_id = args.next().expect("--user-id requires a value"),
            "--device-id" => device_id = args.next().expect("--device-id requires a value"),
            "--client-name" => client_name = args.next().expect("--client-name requires a value"),
            "--target-latency-us" => {
                target_latency_us = args
                    .next()
                    .expect("--target-latency-us requires a value")
                    .parse()
                    .expect("invalid --target-latency-us")
            }
            "--session-file" => {
                session_file = Some(PathBuf::from(
                    args.next().expect("--session-file requires a value"),
                ))
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}");
                print_help();
                std::process::exit(2);
            }
        }
    }

    let Some(server) = server else {
        eprintln!("--server is required\n");
        print_help();
        std::process::exit(2);
    };
    let Some(trust_cert) = trust_cert else {
        eprintln!("--trust-cert is required (see sardp-server's startup log)\n");
        print_help();
        std::process::exit(2);
    };

    Args {
        server,
        trust_cert,
        tls_hostname,
        signing_key,
        user_id,
        device_id,
        client_name,
        target_latency_us,
        session_file,
    }
}

#[derive(Debug)]
#[allow(dead_code)]
enum AppError {
    Quic(quinn::ConnectionError),
    Connect(quinn::ConnectError),
    Handshake(sardp::handshake::HandshakeError),
    TimeSync(sardp::timesync::TimeSyncError),
    Video(sardp::video_session::VideoError),
    Read(sardp::stream_reader::StreamReadError),
    Write(quinn::WriteError),
    Decode(sardp::decoder::DecodeError),
    Join(tokio::task::JoinError),
    Violation(ReasonCode),
    Audio(sardp::audio_session::AudioError),
    Clipboard(sardp::clipboard_session::ClipboardSessionError),
}

impl From<quinn::ConnectionError> for AppError {
    fn from(e: quinn::ConnectionError) -> Self {
        Self::Quic(e)
    }
}
impl From<sardp::handshake::HandshakeError> for AppError {
    fn from(e: sardp::handshake::HandshakeError) -> Self {
        Self::Handshake(e)
    }
}
impl From<sardp::timesync::TimeSyncError> for AppError {
    fn from(e: sardp::timesync::TimeSyncError) -> Self {
        Self::TimeSync(e)
    }
}
impl From<sardp::video_session::VideoError> for AppError {
    fn from(e: sardp::video_session::VideoError) -> Self {
        Self::Video(e)
    }
}
impl From<sardp::stream_reader::StreamReadError> for AppError {
    fn from(e: sardp::stream_reader::StreamReadError) -> Self {
        Self::Read(e)
    }
}
impl From<quinn::WriteError> for AppError {
    fn from(e: quinn::WriteError) -> Self {
        Self::Write(e)
    }
}
impl From<tokio::task::JoinError> for AppError {
    fn from(e: tokio::task::JoinError) -> Self {
        Self::Join(e)
    }
}
impl From<sardp::ProtocolViolation> for AppError {
    fn from(v: sardp::ProtocolViolation) -> Self {
        Self::Violation(v.reason)
    }
}
impl From<sardp::decoder::DecodeError> for AppError {
    fn from(e: sardp::decoder::DecodeError) -> Self {
        Self::Decode(e)
    }
}
impl From<sardp::audio_session::AudioError> for AppError {
    fn from(e: sardp::audio_session::AudioError) -> Self {
        Self::Audio(e)
    }
}
impl From<sardp::clipboard_session::ClipboardSessionError> for AppError {
    fn from(e: sardp::clipboard_session::ClipboardSessionError) -> Self {
        Self::Clipboard(e)
    }
}

#[tokio::main]
async fn main() {
    let args = parse_args();

    let trusted_cert = pki::load_trusted_cert_pem(&args.trust_cert)
        .unwrap_or_else(|e| panic!("failed to load --trust-cert {:?}: {e}", args.trust_cert));

    let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
    let endpoint = net::client_endpoint(bind_addr, &trusted_cert);

    eprintln!("connecting to {}...", args.server);
    let connection = endpoint
        .connect(args.server, &args.tls_hostname)
        .unwrap_or_else(|e| panic!("invalid connect parameters: {e}"))
        .await
        .unwrap_or_else(|e| panic!("failed to connect to {}: {e}", args.server));
    eprintln!("connected, handshaking...");

    if let Err(e) = run(connection, &args).await {
        eprintln!("session ended: {e:?}");
        std::process::exit(1);
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for ctrl-c");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

/// See `sardp-server`'s `close_gracefully` for the known nuance: the peer
/// may see the connection end via a QUIC-level `ApplicationClosed` reason
/// instead of reading this `SessionClose` Envelope, depending on timing.
async fn close_gracefully(
    connection: &quinn::Connection,
    send: &mut quinn::SendStream,
    reason: ReasonCode,
) {
    let _ = write_envelope(
        send,
        messages::type_id::SESSION_CLOSE,
        &messages::encode(&SessionClose { reason }),
    )
    .await;
    let _ = send.finish();
    tokio::time::sleep(timeouts::CLOSING_GRACE_PERIOD).await;
    connection.close(0u32.into(), b"session closed");
}

async fn run(connection: quinn::Connection, args: &Args) -> Result<(), AppError> {
    let saved_session = args.session_file.as_deref().and_then(read_saved_session);

    let (
        session_id,
        reconnect_token,
        granted_permissions,
        mut connection_sm,
        mut control,
        is_resumed,
    ) = if let Some(saved) = saved_session {
        eprintln!(
            "found --session-file state, reconnecting (session_id={:x?})...",
            saved.session_id
        );
        // A fresh process has no live ConnectionSm from the prior run
        // to resume -- replay it through the same states that run
        // actually reached before the connection was lost, so
        // `client_reconnect`'s internal `resume()` (Suspended ->
        // Active) has a valid starting point.
        let mut connection_sm = ConnectionSm::new();
        connection_sm
            .complete_handshake()
            .expect("fresh SM: Handshaking -> Authenticating");
        connection_sm
            .complete_authentication()
            .expect("fresh SM: Authenticating -> Authenticated");
        connection_sm
            .on_channel_live()
            .expect("fresh SM: Authenticated -> Active");
        connection_sm
            .suspend()
            .expect("fresh SM: Active -> Suspended");

        let (outcome, control) = client_reconnect(
            &connection,
            &mut connection_sm,
            saved.session_id,
            saved.reconnect_token,
            &saved.user_id,
        )
        .await?;
        eprintln!(
            "reconnected: session_id={:x?} granted_permissions={:#b}",
            outcome.session_id, outcome.granted_permissions
        );
        (
            outcome.session_id,
            outcome.reconnect_token,
            outcome.granted_permissions,
            connection_sm,
            control,
            true,
        )
    } else {
        let (outcome, connection_sm, control) = client_handshake(
            &connection,
            &args.signing_key,
            &args.client_name,
            &args.user_id,
            &args.device_id,
        )
        .await?;
        eprintln!(
            "authenticated: session_id={:x?} granted_permissions={:#b}",
            outcome.session_id, outcome.granted_permissions
        );
        (
            outcome.session_id,
            outcome.reconnect_token,
            outcome.granted_permissions,
            connection_sm,
            control,
            false,
        )
    };

    if let Some(path) = &args.session_file {
        write_saved_session(
            path,
            &SavedSession {
                session_id,
                reconnect_token,
                user_id: args.user_id.clone(),
            },
        );
    }

    let timesync = client_time_sync(&mut control).await?;
    eprintln!(
        "TimeSync: offset_us={} rtt_us={}",
        timesync.offset_us, timesync.rtt_us
    );

    let (intro, mut frame_reader) = tokio::time::timeout(
        timeouts::SESSION_SETUP_TIMEOUT,
        accept_video_instance(&connection),
    )
    .await
    .unwrap_or_else(|_elapsed| {
        panic!(
            "SESSION_SETUP_TIMEOUT ({:?}) elapsed waiting for the video Instance",
            timeouts::SESSION_SETUP_TIMEOUT
        )
    })?;
    if !is_resumed {
        // A resumed connection_sm is already Active (client_reconnect's
        // resume() skips straight there); only a fresh handshake needs
        // this Authenticated -> Active transition.
        connection_sm.on_channel_live()?;
    }
    eprintln!(
        "video channel Live (monitor {}, {}x{}), connection Active",
        intro.monitor_id, intro.encoder_config.width, intro.encoder_config.height
    );

    let mut feedback_send = feedback_session::open_feedback_stream(&connection).await?;

    process_frame(
        &mut feedback_send,
        timesync.offset_us,
        args.target_latency_us,
        intro.first_frame_header,
        intro.first_frame_payload,
    )
    .await?;

    let mut keepalive_interval = tokio::time::interval(timeouts::KEEPALIVE_INTERVAL);
    let mut last_activity = tokio::time::Instant::now();

    // KNOWN_ISSUES.md #12: audio_capture (client -> server). Only opened
    // if granted at handshake/reconnect time -- unlike sardp-server's
    // AUDIO_PLAYBACK, which reacts live to later admin grants, this client
    // has no interactive control surface to react to a later
    // PermissionUpdate with, so a grant that arrives after this check
    // simply won't be acted on (same conservative-by-default situation as
    // FILE_UP/FILE_DOWN, spec 4.5/handshake.rs).
    let audio_config = AudioConfig {
        codec: AudioCodec::Opus,
        sample_rate: 48_000,
        channels: 1,
        frame_duration_ms: 20,
    };
    let audio_samples_per_frame = (u64::from(audio_config.sample_rate)
        * u64::from(audio_config.frame_duration_ms)
        / 1000) as usize;
    let mut audio_capture_send = if granted_permissions & bit::AUDIO_CAPTURE != 0 {
        match audio_session::open_audio_stream(&connection, StreamKind::AudioCapture, &audio_config)
            .await
        {
            Ok(send) => {
                eprintln!("opened audio_capture stream");
                Some(send)
            }
            Err(e) => {
                eprintln!("failed to open audio_capture stream: {e:?}");
                None
            }
        }
    } else {
        None
    };
    let mut audio_capture_sequence = 0u64;
    let mut audio_capture_interval = tokio::time::interval(Duration::from_millis(u64::from(
        audio_config.frame_duration_ms,
    )));
    audio_capture_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // audio_playback (server -> client): always listening, regardless of
    // this client's own granted_permissions, since sardp-server only
    // opens its side once AUDIO_PLAYBACK is (possibly later) granted via
    // its own admin command -- see that binary's matching comment.
    let mut audio_playback_reader: Option<audio_session::AudioFrameReader> = None;

    loop {
        let idle_deadline = last_activity + timeouts::IDLE_TIMEOUT;
        tokio::select! {
            biased;
            () = shutdown_signal() => {
                eprintln!("shutting down, closing session");
                close_gracefully(&connection, &mut control.send, ReasonCode::NONE).await;
                return Ok(());
            }
            () = tokio::time::sleep_until(idle_deadline) => {
                eprintln!("IDLE_TIMEOUT ({:?} since last activity)", timeouts::IDLE_TIMEOUT);
                close_gracefully(&connection, &mut control.send, ReasonCode::TRANSPORT_IDLE_TIMEOUT).await;
                return Ok(());
            }
            _ = keepalive_interval.tick() => {
                write_envelope(&mut control.send, messages::type_id::KEEP_ALIVE, &messages::encode(&messages::KeepAlive {})).await?;
            }
            control_msg = control.reader.read_envelope(StreamKind::Control.max_envelope_length()) => {
                let (type_raw, payload) = control_msg?;
                last_activity = tokio::time::Instant::now();
                match type_raw {
                    t if t == messages::type_id::SESSION_CLOSE => {
                        let close: SessionClose = messages::decode(&payload).unwrap_or(SessionClose { reason: ReasonCode::NONE });
                        eprintln!("server sent SessionClose (reason {:?}), closing", close.reason);
                        return Ok(());
                    }
                    t if t == messages::type_id::PERMISSION_UPDATE => {
                        let update: messages::PermissionUpdate = messages::decode(&payload).unwrap_or(messages::PermissionUpdate { granted_permissions: 0, immediate_revoke: 0 });
                        eprintln!("PermissionUpdate: granted={:#b} immediate_revoke={:#b}", update.granted_permissions, update.immediate_revoke);
                    }
                    t if t == messages::type_id::KEEP_ALIVE => {}
                    other => eprintln!("unhandled control message type 0x{other:04x}"),
                }
            }
            frame = frame_reader.read_next_frame() => {
                let (header, payload) = frame?;
                last_activity = tokio::time::Instant::now();
                process_frame(&mut feedback_send, timesync.offset_us, args.target_latency_us, header, payload).await?;
            }
            _ = audio_capture_interval.tick() => {
                if let Some(send) = audio_capture_send.as_mut() {
                    let capture_ts = clock::now_us();
                    let payload = audio_session::generate_silence_payload(audio_samples_per_frame);
                    let duration_us = u32::from(audio_config.frame_duration_ms) * 1000;
                    audio_session::send_audio_frame(send, audio_capture_sequence, capture_ts, duration_us, &payload).await?;
                    audio_capture_sequence += 1;
                }
            }
            accept_result = audio_session::accept_audio_stream(&connection, StreamKind::AudioPlayback),
                if audio_playback_reader.is_none() =>
            {
                let (config, reader) = accept_result?;
                eprintln!("accepted audio_playback stream: {config:?}");
                audio_playback_reader = Some(reader);
            }
            frame_result = async { audio_playback_reader.as_mut().unwrap().read_next_frame().await },
                if audio_playback_reader.is_some() =>
            {
                match frame_result {
                    Ok((header, payload)) => {
                        last_activity = tokio::time::Instant::now();
                        eprintln!("audio_playback frame sequence={} bytes={}", header.sequence, payload.len());
                    }
                    Err(e) => {
                        eprintln!("audio_playback stream ended: {e:?}");
                        audio_playback_reader = None;
                    }
                }
            }
            accept_result = clipboard_session::accept_clipboard_formats(&connection) => {
                let (mut send, mut reader, formats) = accept_result?;
                let request_id = formats.request_id;
                let Some(first_format) = formats.formats.into_iter().next() else {
                    eprintln!("received ClipboardFormats with no formats, nothing to request");
                    continue;
                };
                eprintln!(
                    "received clipboard formats, requesting {:?}/{}",
                    first_format.namespace, first_format.format_id
                );
                tokio::spawn(async move {
                    let request = messages::ClipboardRequest {
                        request_id,
                        namespace: first_format.namespace,
                        format_id: first_format.format_id,
                    };
                    match clipboard_session::request_clipboard_data(&mut send, &mut reader, &request).await {
                        Ok(Ok(data)) => eprintln!("clipboard data received: {} bytes", data.data.len()),
                        Ok(Err(error)) => eprintln!("clipboard request rejected: {:?}", error.reason),
                        Err(e) => eprintln!("clipboard request failed: {e:?}"),
                    }
                });
            }
        }
    }
}

async fn process_frame(
    feedback_send: &mut quinn::SendStream,
    offset_us: i64,
    target_latency_us: u32,
    header: messages::VideoFrameHeader,
    payload: Vec<u8>,
) -> Result<(), AppError> {
    let receive_ts = clock::now_us();
    let payload_len = payload.len();
    let (width, height) = (header.width, header.height);
    let decoded =
        tokio::task::spawn_blocking(move || decoder::decode_single_frame(&payload, width, height))
            .await??;
    let decode_done_ts = clock::now_us();

    let timecode = extract_timecode(&decoded);
    let display_ts = clock::now_us();

    println!(
        "frame generation={} frame_id={} idr={} timecode_us={} capture_ts={} bytes={}",
        header.generation,
        header.frame_id,
        header.is_idr(),
        timecode,
        header.capture_ts,
        payload_len
    );

    let feedback = feedback_session::build_transport_feedback(
        &header,
        payload_len,
        &FrameTimestamps {
            receive_ts,
            decode_done_ts,
            display_ts,
        },
        offset_us,
        target_latency_us,
    );
    feedback_session::send_transport_feedback(feedback_send, &feedback).await?;
    Ok(())
}
