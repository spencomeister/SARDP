//! SARDP PoC reference client (Phase 1: "実バイナリ化"). Connects to
//! `sardp-server`, completes the handshake and TimeSync, receives the
//! single-monitor video Instance and every generation that follows it,
//! and hands each frame to one of two sinks (`--display`):
//!
//! - `log` (default, every platform): decodes each frame via a fresh
//!   ffmpeg process and logs the decoded frame's embedded timecode,
//!   proving the wire protocol end-to-end the same way M4-M6's tests did.
//!   Only self-contained frames decode this way (`sardp-server --all-idr`
//!   for desktop capture).
//! - `window` (Windows, Stage 3 3W-1-d-3): a persistent hardware H.264
//!   decoder behind an on-screen window (`sardp_win::H264DisplayWindow`),
//!   so P-frame streams display at the server's frame rate.
//!
//! Either way a real `TransportFeedback` is sent for every frame (spec
//! 2.14), which is what lets `sardp-server`'s backpressure mechanism
//! (spec 2.10, DR-029) do anything when the two binaries run against
//! each other -- including resetting the stream, which this client
//! recovers from by accepting the next generation's Instance.

#[cfg(windows)]
use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use ed25519_dalek::SigningKey;

use sardp::audio_session;
use sardp::client_display::{ClientDisplay, SubmitOutcome};
use sardp::clipboard_session;
use sardp::connection_sm::{ConnectionSm, defaults as timeouts};
use sardp::decoder;
use sardp::feedback_session::{self, FrameTimestamps};
use sardp::handshake::client_handshake;
use sardp::messages::{self, AudioCodec, AudioConfig, SessionClose, VideoFrameHeader};
use sardp::permission_set::bit;
use sardp::reason_code::ReasonCode;
use sardp::reconnection::client_reconnect;
use sardp::session_file::{SavedSession, read_saved_session, write_saved_session};
use sardp::stream_reader::{StreamReadError, write_envelope};
use sardp::timecode_frame::extract_timecode;
use sardp::timesync::client_time_sync;
use sardp::video_session::{VideoError, VideoFrameReader, accept_video_instance};
use sardp::{StreamKind, clock, dev_identity, net, pki};

/// `--display`: where decoded frames go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayMode {
    Log,
    Window,
}

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
    display: DisplayMode,
    /// `--display window` only: client-area size of the window.
    window_size: (u32, u32),
    /// Write the first few frames of generation 0 as raw Annex-B files
    /// here (diagnostics: what did the server's first IDR actually contain?).
    dump_frames: Option<PathBuf>,
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
    --display <log|window>    log: decode each frame with ffmpeg and log its\n\
                              timecode (default, any platform; needs\n\
                              self-contained frames). window: Windows only,\n\
                              persistent hardware decoder + on-screen window\n\
    --window-size <WxH>       Window client size for --display window\n\
                              (default 1280x720; frames are scaled to fit)\n\
    --dump-frames <DIR>       Save generation 0's first 3 frames as Annex-B\n\
                              .h264 files in DIR (diagnostics)\n\
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
    let mut display = DisplayMode::Log;
    let mut window_size = (1280u32, 720u32);
    let mut dump_frames: Option<PathBuf> = None;

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
            "--display" => {
                let value = args.next().expect("--display requires a value");
                display = match value.as_str() {
                    "log" => DisplayMode::Log,
                    "window" => {
                        if !cfg!(windows) {
                            eprintln!("--display window is only available on Windows");
                            std::process::exit(2);
                        }
                        DisplayMode::Window
                    }
                    other => {
                        eprintln!("invalid --display {other:?} (expected log or window)");
                        std::process::exit(2);
                    }
                };
            }
            "--window-size" => {
                let value = args.next().expect("--window-size requires a value");
                let parsed = value
                    .split_once('x')
                    .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
                    .filter(|(w, h)| *w > 0 && *h > 0);
                let Some(parsed) = parsed else {
                    eprintln!("invalid --window-size {value:?} (expected WxH, e.g. 1280x720)");
                    std::process::exit(2);
                };
                window_size = parsed;
            }
            "--dump-frames" => {
                dump_frames = Some(PathBuf::from(
                    args.next().expect("--dump-frames requires a value"),
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
        display,
        window_size,
        dump_frames,
    }
}

/// `--dump-frames`: generation 0's first three frames, each as its own
/// Annex-B file plus one cumulative file (a P-frame only decodes behind
/// its references, so `ffmpeg -i gen0_first3.h264` is the way to look at
/// frames 1 and 2).
const DUMP_FRAME_COUNT: u64 = 3;

fn dump_frame(dir: &std::path::Path, header: &VideoFrameHeader, payload: &[u8]) {
    if header.generation != 0 || header.frame_id >= DUMP_FRAME_COUNT {
        return;
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("--dump-frames: cannot create {dir:?}: {e}");
        return;
    }
    let single = dir.join(format!("gen0_frame{}.h264", header.frame_id));
    if let Err(e) = std::fs::write(&single, payload) {
        eprintln!("--dump-frames: cannot write {single:?}: {e}");
    }
    let cumulative = dir.join("gen0_first3.h264");
    let appended = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&cumulative)
        .and_then(|mut f| std::io::Write::write_all(&mut f, payload));
    if let Err(e) = appended {
        eprintln!("--dump-frames: cannot append to {cumulative:?}: {e}");
    }
    eprintln!(
        "--dump-frames: wrote generation 0 frame {} ({} bytes, idr={}) to {single:?}",
        header.frame_id,
        payload.len(),
        header.is_idr()
    );
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
    /// `--display window` could not be set up (no decoder MFT, no D3D11
    /// device, ...). Not a transport error.
    Display(String),
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

    let mut sink = VideoSink::open(args, &intro.encoder_config)?;
    let mut display: ClientDisplay<DisplayedFrame> = ClientDisplay::new();
    let mut stats = FrameStats::default();
    if let Some(dir) = &args.dump_frames {
        // Fresh cumulative file per run.
        let _ = std::fs::remove_file(dir.join("gen0_first3.h264"));
    }

    process_frame(
        &mut sink,
        &mut display,
        &mut stats,
        &mut feedback_send,
        timesync.offset_us,
        args.target_latency_us,
        intro.first_frame_header,
        intro.first_frame_payload,
        args.dump_frames.as_deref(),
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

    // CLIP_WRITE (client -> server announce): like AUDIO_CAPTURE, only
    // acted on if granted at handshake/reconnect time -- this client has
    // no interactive control surface to react to a later grant with.
    // Fire-and-forget: this connection's own `accept_bi()` isn't used for
    // anything else, so a spawned announce here can't race with anything.
    if granted_permissions & bit::CLIP_WRITE != 0 {
        tokio::spawn(clipboard_announce_once(connection.clone()));
    }

    loop {
        let idle_deadline = last_activity + timeouts::IDLE_TIMEOUT;
        tokio::select! {
            biased;
            () = shutdown_signal() => {
                eprintln!("shutting down, closing session");
                stats.print_summary();
                close_gracefully(&connection, &mut control.send, ReasonCode::NONE).await;
                return Ok(());
            }
            () = tokio::time::sleep_until(idle_deadline) => {
                eprintln!("IDLE_TIMEOUT ({:?} since last activity)", timeouts::IDLE_TIMEOUT);
                stats.print_summary();
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
                        stats.print_summary();
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
                let (header, payload) = match frame {
                    Ok(frame) => frame,
                    Err(VideoError::Read(StreamReadError::Read(quinn::ReadError::Reset(code)))) => {
                        // Spec 2.10 / 4.3.1: the server reset the video
                        // stream (backpressure) and opens a new Instance
                        // with generation+1; accept it and carry on.
                        // The generation gate (`display`) discards
                        // anything older that could still show up.
                        eprintln!(
                            "video stream reset by server (code {code}) after generation {:?}; waiting for the next Instance",
                            display.current_generation()
                        );
                        stats.resets += 1;
                        let (intro, reader) = accept_next_generation(&connection).await?;
                        frame_reader = reader;
                        last_activity = tokio::time::Instant::now();
                        eprintln!(
                            "new video Instance: generation {} ({}x{})",
                            intro.first_frame_header.generation,
                            intro.encoder_config.width,
                            intro.encoder_config.height
                        );
                        process_frame(
                            &mut sink, &mut display, &mut stats, &mut feedback_send,
                            timesync.offset_us, args.target_latency_us,
                            intro.first_frame_header, intro.first_frame_payload,
                            args.dump_frames.as_deref(),
                        ).await?;
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                };
                last_activity = tokio::time::Instant::now();
                process_frame(
                    &mut sink, &mut display, &mut stats, &mut feedback_send,
                    timesync.offset_us, args.target_latency_us, header, payload,
                    args.dump_frames.as_deref(),
                ).await?;
            }
            timing = sink.next_window_timing() => {
                let Some(timing) = timing else {
                    eprintln!("display window closed by the user, closing session");
                    stats.print_summary();
                    close_gracefully(&connection, &mut control.send, ReasonCode::NONE).await;
                    return Ok(());
                };
                sink.on_window_timing(timing, &mut stats, &mut feedback_send, timesync.offset_us, args.target_latency_us).await?;
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

/// CLIP_WRITE (client -> server announce, spec 2.7): announces synthetic
/// clipboard content once, then responds to a `ClipboardRequest` for it if
/// one arrives. Mirrors `sardp-server`'s `spawn_clipboard_announce`
/// (`CLIP_READ` direction) exactly, just from the other side.
async fn clipboard_announce_once(connection: quinn::Connection) {
    let formats = messages::ClipboardFormats {
        request_id: 1,
        formats: vec![messages::ClipboardFormatEntry {
            namespace: messages::FormatNamespace::Mime,
            format_id: "text/plain".to_string(),
        }],
    };
    let (mut send, mut reader) =
        match clipboard_session::announce_clipboard_formats(&connection, &formats).await {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("failed to announce clipboard formats: {e:?}");
                return;
            }
        };
    match clipboard_session::read_clipboard_request(&mut reader).await {
        Ok(request) => {
            let pseudo_data = b"hello from sardp-client's synthetic clipboard".to_vec();
            let request_id = request.request_id;
            let result = clipboard_session::respond_to_clipboard_request(
                &mut send,
                request.request_id,
                request.namespace,
                request.format_id,
                pseudo_data,
                None,
            )
            .await;
            match result {
                Ok(()) => eprintln!("responded to ClipboardRequest {request_id}"),
                Err(e) => eprintln!("failed to respond to ClipboardRequest: {e:?}"),
            }
        }
        Err(e) => {
            eprintln!("clipboard announce: never received a ClipboardRequest ({e:?})");
        }
    }
}

/// Accepts the video Instance the server opens after resetting the
/// previous one. Spec 4.3.1 gives the server `VIDEO_RECOVERY_TIMEOUT`
/// (5s) per attempt with backoff between attempts; the client-side wait
/// here is bounded by the (longer) session-setup timeout, and by the
/// control stream's `IDLE_TIMEOUT` beyond that -- a server that gives up
/// on recovery closes the session, which surfaces here as a read error.
async fn accept_next_generation(
    connection: &quinn::Connection,
) -> Result<(sardp::video_session::VideoInstanceIntro, VideoFrameReader), AppError> {
    let deadline = tokio::time::Instant::now() + timeouts::SESSION_SETUP_TIMEOUT;
    loop {
        match tokio::time::timeout_at(deadline, accept_video_instance(connection)).await {
            Ok(Ok(accepted)) => return Ok(accepted),
            // The server can reset the *new* Instance too, before its intro
            // has been fully read (a slow decoder keeps the backpressure
            // tripping); that just means yet another generation follows.
            Ok(Err(VideoError::Read(StreamReadError::Read(quinn::ReadError::Reset(code))))) => {
                eprintln!("new video Instance was reset (code {code}) before it started; waiting for the next one");
                continue;
            }
            Ok(Err(e)) => return Err(e.into()),
            Err(_elapsed) => {
                eprintln!(
                    "no new video Instance within {:?} after the reset",
                    timeouts::SESSION_SETUP_TIMEOUT
                );
                return Err(AppError::Violation(ReasonCode::PROTOCOL_VIDEO_CONFIGURING_TIMEOUT));
            }
        }
    }
}

/// What `ClientDisplay` tracks per displayed frame. In `log` mode the
/// decoded pixels' embedded timecode; in `window` mode nothing beyond the
/// identity `ClientDisplay` itself records (the pixels live on the GPU).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisplayedFrame {
    timecode_us: Option<u64>,
}

/// A frame handed to the window whose decode/present timing hasn't come
/// back yet (`window` mode only).
#[cfg(windows)]
struct PendingFeedback {
    header: VideoFrameHeader,
    payload_len: usize,
}

enum VideoSink {
    Log,
    #[cfg(windows)]
    Window {
        window: sardp_win::H264DisplayWindow,
        pending: VecDeque<PendingFeedback>,
    },
}

impl VideoSink {
    fn open(args: &Args, encoder_config: &messages::EncoderConfig) -> Result<Self, AppError> {
        match args.display {
            DisplayMode::Log => {
                eprintln!("display: log (per-frame ffmpeg decode)");
                Ok(Self::Log)
            }
            #[cfg(windows)]
            DisplayMode::Window => {
                let config = sardp_win::DisplayConfig {
                    title: format!(
                        "SARDP {} ({}x{})",
                        args.server, encoder_config.width, encoder_config.height
                    ),
                    width: args.window_size.0,
                    height: args.window_size.1,
                };
                let window = sardp_win::H264DisplayWindow::open(config, std::sync::Arc::new(clock::now_us))
                    .map_err(|e| AppError::Display(e.to_string()))?;
                eprintln!(
                    "display: window {}x{} (hardware H.264 decode, stream {}x{})",
                    args.window_size.0, args.window_size.1, encoder_config.width, encoder_config.height
                );
                Ok(Self::Window {
                    window,
                    pending: VecDeque::new(),
                })
            }
            #[cfg(not(windows))]
            DisplayMode::Window => {
                let _ = encoder_config;
                unreachable!("--display window is rejected at argument parsing off Windows")
            }
        }
    }

    /// `select!` arm: the window's next decode/present timing report.
    /// Never completes in `log` mode; `Some(None)` once the window is gone.
    async fn next_window_timing(&mut self) -> Option<WindowTiming> {
        match self {
            Self::Log => std::future::pending().await,
            #[cfg(windows)]
            Self::Window { window, .. } => window.next_timing().await.map(|t| WindowTiming {
                generation: t.generation,
                frame_id: t.frame_id,
                receive_ts: t.receive_ts,
                dequeue_ts: t.dequeue_ts,
                decode_done_ts: t.decode_done_ts,
                display_ts: t.display_ts,
                presented: t.presented,
            }),
        }
    }

    /// Turns a window timing report into the frame's `TransportFeedback`.
    async fn on_window_timing(
        &mut self,
        timing: WindowTiming,
        stats: &mut FrameStats,
        feedback_send: &mut quinn::SendStream,
        offset_us: i64,
        target_latency_us: u32,
    ) -> Result<(), AppError> {
        #[cfg(windows)]
        if let Self::Window { pending, .. } = self {
            // Frames complete in submission order; anything ahead of the
            // reported one in the queue never produced output (the decoder
            // needed more input, or it was dropped inside the window) and
            // gets no feedback.
            let position = pending
                .iter()
                .position(|p| p.header.generation == timing.generation && p.header.frame_id == timing.frame_id);
            let Some(position) = position else {
                eprintln!(
                    "timing for unknown frame generation={} frame_id={}",
                    timing.generation, timing.frame_id
                );
                return Ok(());
            };
            let skipped = pending.drain(..position).count();
            stats.no_output += skipped as u64;
            let entry = pending.pop_front().expect("position is in range");
            let timestamps = FrameTimestamps {
                receive_ts: timing.receive_ts,
                decode_done_ts: timing.decode_done_ts,
                display_ts: timing.display_ts,
            };
            stats.record_displayed(
                &entry.header,
                entry.payload_len,
                &timestamps,
                Some(timing.dequeue_ts),
                timing.presented,
            );
            let feedback = feedback_session::build_transport_feedback(
                &entry.header,
                entry.payload_len,
                &timestamps,
                offset_us,
                target_latency_us,
            );
            feedback_session::send_transport_feedback(feedback_send, &feedback).await?;
        }
        let _ = (&timing, &*stats, &*feedback_send, offset_us, target_latency_us);
        Ok(())
    }
}

/// Window-mode decode/present timestamps, mirrored from `sardp_win` so the
/// `select!` arm has a type on every platform.
#[derive(Debug, Clone, Copy)]
struct WindowTiming {
    generation: u64,
    frame_id: u64,
    receive_ts: u64,
    dequeue_ts: u64,
    decode_done_ts: u64,
    display_ts: u64,
    presented: bool,
}

/// Running counters printed on shutdown (and every 300 displayed frames),
/// so a long run leaves a one-line verdict in the log.
#[derive(Default)]
struct FrameStats {
    received: u64,
    displayed: u64,
    stale_generation: u64,
    /// Window mode: frames handed to the display thread that never
    /// produced a decode/present timing -- skipped as a stale generation
    /// there, or absorbed by the decoder without output.
    no_output: u64,
    /// Window mode: decoded but not shown (swap chain still busy with the
    /// previous frame -- the decoder got ahead of the display refresh).
    not_presented: u64,
    resets: u64,
    bytes: u64,
    /// receive -> decode done (what `TransportFeedback` reports as the
    /// decode delay; includes queueing in window mode).
    decode_us_total: u64,
    /// Window mode: receive -> picked up by the display thread.
    queue_us_total: u64,
    display_us_total: u64,
    max_decode_us: u64,
    idr: u64,
}

impl FrameStats {
    fn record_displayed(
        &mut self,
        header: &VideoFrameHeader,
        payload_len: usize,
        t: &FrameTimestamps,
        dequeue_ts: Option<u64>,
        presented: bool,
    ) {
        self.displayed += 1;
        self.bytes += payload_len as u64;
        if header.is_idr() {
            self.idr += 1;
        }
        if !presented {
            self.not_presented += 1;
        }
        let decode_us = t.decode_done_ts.saturating_sub(t.receive_ts);
        let queue_us = dequeue_ts.map(|d| d.saturating_sub(t.receive_ts));
        let display_us = t.display_ts.saturating_sub(t.decode_done_ts);
        self.decode_us_total += decode_us;
        self.queue_us_total += queue_us.unwrap_or(0);
        self.display_us_total += display_us;
        self.max_decode_us = self.max_decode_us.max(decode_us);
        match queue_us {
            Some(queue_us) => println!(
                "frame generation={} frame_id={} idr={} bytes={} capture_ts={} queue_us={} decode_us={} present_us={} presented={}",
                header.generation, header.frame_id, header.is_idr(), payload_len, header.capture_ts,
                queue_us, decode_us - queue_us, display_us, presented
            ),
            None => println!(
                "frame generation={} frame_id={} idr={} bytes={} capture_ts={} decode_us={} present_us={}",
                header.generation, header.frame_id, header.is_idr(), payload_len, header.capture_ts, decode_us, display_us
            ),
        }
        if self.displayed.is_multiple_of(300) {
            self.print_summary();
        }
    }

    fn print_summary(&self) {
        let avg = |total: u64| total.checked_div(self.displayed).unwrap_or(0);
        eprintln!(
            "stats: received={} displayed={} (idr={}, not_presented={}) stale_generation={} no_output={} resets={} bytes={} avg_queue_us={} avg_decode_us={} max_decode_us={} avg_present_us={}",
            self.received,
            self.displayed,
            self.idr,
            self.not_presented,
            self.stale_generation,
            self.no_output,
            self.resets,
            self.bytes,
            avg(self.queue_us_total),
            avg(self.decode_us_total),
            self.max_decode_us,
            avg(self.display_us_total),
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn process_frame(
    sink: &mut VideoSink,
    display: &mut ClientDisplay<DisplayedFrame>,
    stats: &mut FrameStats,
    feedback_send: &mut quinn::SendStream,
    offset_us: i64,
    target_latency_us: u32,
    header: VideoFrameHeader,
    payload: Vec<u8>,
    dump_dir: Option<&std::path::Path>,
) -> Result<(), AppError> {
    let receive_ts = clock::now_us();
    stats.received += 1;
    if let Some(dir) = dump_dir {
        dump_frame(dir, &header, &payload);
    }

    // Spec 2.10 client MUST: frames of a generation older than the one on
    // screen are discarded (without decoding).
    if !display.would_display(&header) {
        stats.stale_generation += 1;
        eprintln!(
            "discarding frame of stale generation {} (frame_id {}), displaying generation {:?}",
            header.generation,
            header.frame_id,
            display.current_generation()
        );
        return Ok(());
    }

    match sink {
        VideoSink::Log => {
            let payload_len = payload.len();
            let (width, height) = (header.width, header.height);
            let decoded = tokio::task::spawn_blocking(move || {
                decoder::decode_single_frame(&payload, width, height)
            })
            .await??;
            let decode_done_ts = clock::now_us();

            let timecode = extract_timecode(&decoded);
            let outcome = display.submit_frame(
                &header,
                DisplayedFrame {
                    timecode_us: Some(timecode),
                },
            );
            debug_assert_eq!(outcome, SubmitOutcome::Displayed, "gated by would_display above");
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
            let timestamps = FrameTimestamps {
                receive_ts,
                decode_done_ts,
                display_ts,
            };
            stats.record_displayed(&header, payload_len, &timestamps, None, true);
            let feedback = feedback_session::build_transport_feedback(
                &header,
                payload_len,
                &timestamps,
                offset_us,
                target_latency_us,
            );
            feedback_session::send_transport_feedback(feedback_send, &feedback).await?;
        }
        #[cfg(windows)]
        VideoSink::Window { window, pending } => {
            let payload_len = payload.len();
            window
                .submit(sardp_win::SubmittedFrame {
                    generation: header.generation,
                    frame_id: header.frame_id,
                    is_idr: header.is_idr(),
                    width: header.width,
                    height: header.height,
                    annex_b: payload,
                    receive_ts,
                })
                .map_err(|e| AppError::Display(e.to_string()))?;
            // "Submitted for display" is the point the generation gate
            // tracks; decode/present timing arrives via
            // `next_window_timing` and is what the feedback reports.
            let outcome = display.submit_frame(&header, DisplayedFrame { timecode_us: None });
            debug_assert_eq!(outcome, SubmitOutcome::Displayed, "gated by would_display above");
            pending.push_back(PendingFeedback { header, payload_len });
            if pending.len() > 64 && pending.len().is_power_of_two() {
                eprintln!(
                    "window decoder behind: {} frames queued (newest generation={} frame_id={})",
                    pending.len(),
                    header.generation,
                    header.frame_id
                );
            }
        }
    }
    Ok(())
}
