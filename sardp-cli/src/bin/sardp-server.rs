//! SARDP PoC reference server (Phase 1: "実バイナリ化"). Binds a QUIC
//! endpoint, accepts connections, drives each through the handshake
//! (spec 4.1, timeouts per 4.7) and a single-monitor `VideoChannel`
//! (spec 2.10/4.3), and streams synthetic timecode-embedded frames
//! continuously. Real OS capture/hardware encode are out of scope for
//! this PoC (see `docs/SARDP_PoC_Brief_for_ClaudeCode.md`).
//!
//! Typing `revoke-view` / `grant-view` on stdin toggles the connected
//! client's `VIEW` permission live (spec 2.5/4.5), sending a real
//! `PermissionUpdate` and gating the frame-send loop locally -- the
//! minimal admin action Part 4 asked the Permission SM wiring to prove.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::VerifyingKey;
use tokio::io::AsyncBufReadExt;
use tokio::sync::{Mutex, Notify};

use sardp::audio_session;
use sardp::backpressure::BackpressureDecision;
use sardp::channel_sm::ChannelState;
use sardp::clipboard_session;
use sardp::conn_error::{ConnError, is_transport_disconnect};
use sardp::connection_sm::defaults as timeouts;
use sardp::encoder;
use sardp::feedback_session::FeedbackReceiver;
use sardp::file_handle_store::FileHandleStore;
use sardp::file_transfer_session::{self as file_transfer};
use sardp::handshake::ControlChannel;
use sardp::input_session::{InputMessage, InputReceiver};
use sardp::input_state::{ImeModeSm, PressedInputs, Release, should_inject_key};
use sardp::messages::{
    self, AudioCodec, AudioConfig, ChromaFormat, ClipboardFormatEntry, ClipboardFormats, Codec,
    EncoderConfig, FileTransferAccept, FileTransferReject, FileTransferRequest, FormatNamespace,
    SessionClose,
};
use sardp::permission_set::bit;
use sardp::permission_sm::{self, PermissionSm};
use sardp::reason_code::ReasonCode;
use sardp::reconnection::{self, EstablishOutcome};
use sardp::session_store::SessionStore;
use sardp::stream_reader::{EnvelopeReader, write_envelope};
use sardp::timecode_frame;
use sardp::video_channel::VideoChannel;
use sardp::video_session;
use sardp::video_sm::defaults::VIDEO_CONFIGURING_TIMEOUT;
use sardp::{StreamKind, clock, dev_identity, net, pki};

struct Args {
    bind: SocketAddr,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
    cert_out: PathBuf,
    trusted_pubkey: VerifyingKey,
    width: u32,
    height: u32,
    fps: f64,
    server_name: String,
    capture: CaptureMode,
    /// Desktop capture only: encode every frame as an IDR. Off by default
    /// since 3W-1-d-3 (the client's persistent decoder handles P-frames);
    /// kept for debugging and for the ffmpeg-per-frame `--display log`
    /// client path, which can only decode self-contained frames.
    all_idr: bool,
    /// Pause between characters of injected `TextInput` (Windows desktop
    /// capture only; KNOWN_ISSUES.md #13).
    text_char_delay_ms: u64,
}

/// Per-connection copy of the capture/injection-related arguments.
#[derive(Debug, Clone, Copy)]
struct CaptureSettings {
    mode: CaptureMode,
    all_idr: bool,
    text_char_delay_ms: u64,
}

/// Where video frames come from (`--capture`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureMode {
    /// The M1-M6 timecode pattern, encoded with a per-frame `ffmpeg`
    /// subprocess. Runs anywhere; the default.
    Synthetic,
    /// The real desktop via DXGI Desktop Duplication + a hardware H.264
    /// encoder MFT (`sardp-win`, Stage 3 3W-1-d). Windows only.
    Desktop,
}

/// The per-connection video frame source. Held by `run_active_session`
/// for the session's lifetime (the desktop source owns a capture thread
/// and the encoder's state, so it must outlive every generation).
enum FrameSource {
    Synthetic {
        width: u32,
        height: u32,
    },
    #[cfg(windows)]
    Desktop(sardp_win::DesktopH264Source),
}

/// One frame ready to go on the wire, whatever produced it.
struct SourcedFrame {
    bytes: Vec<u8>,
    is_idr: bool,
    capture_ts: u64,
    encode_done_ts: u64,
}

/// Bounds how many non-IDR desktop frames `next_idr_frame` will discard
/// while waiting for the encoder to honor an IDR request.
const MAX_FRAMES_TO_SKIP_FOR_IDR: u32 = 120;

/// Next frame from `source`. For the synthetic source this is a fresh
/// timecode frame right now; for the desktop source it waits for the
/// capture thread's next encoded frame (`None` if that thread stopped).
async fn next_frame(source: &mut FrameSource) -> Result<Option<SourcedFrame>, ConnError> {
    match source {
        FrameSource::Synthetic { width, height } => {
            let capture_ts = clock::now_us();
            let frame =
                timecode_frame::generate_timecode_frame(*width, *height, capture_ts, [40, 40, 40]);
            let bytes =
                tokio::task::spawn_blocking(move || encoder::encode_single_frame_idr(&frame))
                    .await??;
            Ok(Some(SourcedFrame {
                bytes,
                is_idr: true,
                capture_ts,
                encode_done_ts: clock::now_us(),
            }))
        }
        #[cfg(windows)]
        FrameSource::Desktop(desktop) => Ok(desktop.next_frame().await.map(|f| SourcedFrame {
            bytes: f.annex_b,
            is_idr: f.is_idr,
            capture_ts: f.capture_ts,
            encode_done_ts: f.encode_done_ts,
        })),
    }
}

/// The desktop source's next frame, as a `select!` arm: never completes
/// for the synthetic source (whose frames are paced by the tick arm).
async fn next_desktop_frame(source: &mut FrameSource) -> Result<Option<SourcedFrame>, ConnError> {
    match source {
        FrameSource::Synthetic { .. } => std::future::pending().await,
        #[cfg(windows)]
        FrameSource::Desktop(_) => next_frame(source).await,
    }
}

/// `VideoFrameHeader` + payload for one sourced frame (DR-035 split done
/// by `video_session::send_video_frame`).
async fn send_sourced_frame(
    video_send: &mut quinn::SendStream,
    generation: u64,
    frame_id: u64,
    width: u32,
    height: u32,
    frame: &SourcedFrame,
) -> Result<(), ConnError> {
    let flags = if frame.is_idr {
        messages::VIDEO_FRAME_FLAG_IDR
    } else {
        0
    };
    video_session::send_video_frame(
        video_send,
        generation,
        frame_id,
        1,
        flags,
        frame.capture_ts,
        frame.encode_done_ts,
        width,
        height,
        &frame.bytes,
    )
    .await?;
    Ok(())
}

/// A frame that can open a new video Instance: asks the source for an IDR
/// and skips whatever non-IDR frames arrive first.
async fn next_idr_frame(source: &mut FrameSource) -> Result<SourcedFrame, ConnError> {
    #[cfg(windows)]
    if let FrameSource::Desktop(desktop) = source {
        desktop.request_idr();
    }
    for _ in 0..MAX_FRAMES_TO_SKIP_FOR_IDR {
        match next_frame(source).await? {
            Some(frame) if frame.is_idr => return Ok(frame),
            Some(_) => continue,
            None => return Err(ConnError::Capture("desktop capture stopped".into())),
        }
    }
    Err(ConnError::Capture(format!(
        "no IDR from the encoder within {MAX_FRAMES_TO_SKIP_FOR_IDR} frames"
    )))
}

/// State shared across every connection this server handles: `sessions`
/// (Phase 1) backs reconnection (spec 4.6), `file_handles` (this task)
/// backs file transfer's `file_handle` issuance and DR-037 ownership
/// checks (spec 2.6). Each store is independently `Arc`-wrapped (rather
/// than the whole struct) so a task that only needs one of them --
/// [`sardp::session_store::SessionStore::suspend_and_schedule_expiry`]'s
/// own background expiry task, in particular -- can hold just that.
#[derive(Default)]
struct ServerState {
    sessions: Arc<SessionStore>,
    file_handles: Arc<FileHandleStore>,
}

/// How long an issued `file_handle` stays valid if the `file` stream isn't
/// opened (spec 2.6's `expiry_ts`). Not spec-mandated; a PoC-reasonable
/// default.
const FILE_HANDLE_TTL: Duration = Duration::from_secs(300);

/// How many `file_handle`s this server allows outstanding (issued but not
/// yet completed/errored/expired) at once, across all connections.
/// KNOWN_ISSUES.md #3: without this, a client that keeps sending
/// `FileTransferRequest` faster than transfers finish can grow
/// `ServerState::file_handles` without bound. Not spec-mandated; a
/// PoC-reasonable default.
const MAX_CONCURRENT_FILE_TRANSFERS: usize = 64;

/// How often the server sweeps `ServerState::file_handles` for handles
/// whose `expiry_ts` passed without their `file` stream ever being opened
/// (KNOWN_ISSUES.md #3). Not spec-mandated.
const FILE_HANDLE_REAP_INTERVAL: Duration = Duration::from_secs(60);

fn print_help() {
    println!(
        "sardp-server -- SARDP PoC reference server\n\n\
USAGE:\n    sardp-server [OPTIONS]\n\n\
OPTIONS:\n\
    --bind <ADDR>           Bind address (default 127.0.0.1:4433)\n\
    --cert <PATH>           TLS certificate PEM (requires --key)\n\
    --key <PATH>            TLS private key PEM, PKCS8 (requires --cert)\n\
    --cert-out <PATH>       Where to write a generated self-signed cert if\n\
                            --cert/--key are omitted (default ./sardp-dev-cert.pem)\n\
    --trusted-pubkey <HEX>  Ed25519 public key (64 hex chars) trusted for\n\
                            client auth (default: PoC fixed dev key)\n\
    --width <N>             Synthetic frame width, >=512 (default 640)\n\
    --height <N>            Synthetic frame height (default 360)\n\
    --fps <N>               Frame send rate (default 4)\n\
    --capture <MODE>        synthetic (default): M1-M6 timecode pattern via ffmpeg\n\
                            desktop: real desktop via DXGI + hardware H.264 (Windows;\n\
                            --width/--height/--fps are then taken from the display)\n\
    --all-idr               With --capture desktop: encode every frame as an IDR\n\
                            (needed for a client running --display log; default off)\n\
    --text-char-delay-ms <N> With --capture desktop: pause between characters of\n\
                            injected TextInput (default 50; KNOWN_ISSUES #13)\n\
    --server-name <NAME>    Name announced in ServerHello (default sardp-server)\n\
    --help                  Show this message\n\n\
With --capture desktop, the client's input stream (spec 2.12) is injected\n\
into this desktop via SendInput; with synthetic capture it is only logged.\n\n\
Once a client is connected, typing one of the following (Enter) toggles\n\
the corresponding permission live, or triggers a one-shot action:\n\
    grant-view / revoke-view\n\
    grant-keyboard / revoke-keyboard\n\
    grant-mouse / revoke-mouse\n\
    grant-clip-read / revoke-clip-read\n\
    grant-audio-playback / revoke-audio-playback\n\
    grant-audio-capture / revoke-audio-capture\n\
    send-clipboard   (announces synthetic clipboard content once,\n\
                      if CLIP_READ is currently granted)"
    );
}

fn parse_args() -> Args {
    let mut bind: SocketAddr = "127.0.0.1:4433".parse().unwrap();
    let mut cert = None;
    let mut key = None;
    let mut cert_out = PathBuf::from("sardp-dev-cert.pem");
    let mut trusted_pubkey = dev_identity::dev_verifying_key();
    let mut width = 640u32;
    let mut height = 360u32;
    let mut fps = 4.0f64;
    let mut server_name = "sardp-server".to_string();
    let mut capture = CaptureMode::Synthetic;
    let mut all_idr = false;
    let mut text_char_delay_ms = 50u64;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => {
                bind = args
                    .next()
                    .expect("--bind requires a value")
                    .parse()
                    .expect("invalid --bind address")
            }
            "--cert" => cert = Some(PathBuf::from(args.next().expect("--cert requires a value"))),
            "--key" => key = Some(PathBuf::from(args.next().expect("--key requires a value"))),
            "--cert-out" => {
                cert_out = PathBuf::from(args.next().expect("--cert-out requires a value"))
            }
            "--trusted-pubkey" => {
                let hex = args.next().expect("--trusted-pubkey requires a value");
                let bytes = dev_identity::parse_hex32(&hex).expect("invalid --trusted-pubkey");
                trusted_pubkey =
                    VerifyingKey::from_bytes(&bytes).expect("invalid Ed25519 public key");
            }
            "--width" => {
                width = args
                    .next()
                    .expect("--width requires a value")
                    .parse()
                    .expect("invalid --width")
            }
            "--height" => {
                height = args
                    .next()
                    .expect("--height requires a value")
                    .parse()
                    .expect("invalid --height")
            }
            "--fps" => {
                fps = args
                    .next()
                    .expect("--fps requires a value")
                    .parse()
                    .expect("invalid --fps")
            }
            "--server-name" => server_name = args.next().expect("--server-name requires a value"),
            "--capture" => {
                capture = match args.next().expect("--capture requires a value").as_str() {
                    "synthetic" => CaptureMode::Synthetic,
                    "desktop" => {
                        if !cfg!(windows) {
                            eprintln!("--capture desktop is only available on Windows");
                            std::process::exit(2);
                        }
                        CaptureMode::Desktop
                    }
                    other => {
                        eprintln!("invalid --capture value: {other} (expected synthetic|desktop)");
                        std::process::exit(2);
                    }
                }
            }
            "--all-idr" => all_idr = true,
            "--text-char-delay-ms" => {
                text_char_delay_ms = args
                    .next()
                    .expect("--text-char-delay-ms requires a value")
                    .parse()
                    .expect("invalid --text-char-delay-ms")
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
    Args {
        bind,
        cert,
        key,
        cert_out,
        trusted_pubkey,
        width,
        height,
        fps,
        server_name,
        capture,
        all_idr,
        text_char_delay_ms,
    }
}

/// Shared queue of admin commands (stdin, see `permission_sm::AdminCommand`)
/// for the one connected client this PoC server handles interactively.
/// Drained by the connection's own loop.
type PermissionCommand = Arc<Mutex<Vec<permission_sm::AdminCommand>>>;

/// The `control` stream's send half, shared between `run_active_session`'s
/// own loop and any task it spawns that needs to write onto `control` too
/// (e.g. [`spawn_file_transfer`] reporting a `FileTransferError`, DR-038):
/// `quinn::SendStream` has exactly one owner, so concurrent writers need a
/// lock rather than each holding their own `&mut` to it. Locked only for
/// the duration of one `write_envelope` call, never held across an
/// `.await` that waits on the peer.
type SharedControlSend = Arc<Mutex<quinn::SendStream>>;

async fn write_control(
    control_send: &SharedControlSend,
    type_raw: u16,
    payload: &[u8],
) -> Result<(), quinn::WriteError> {
    let mut send = control_send.lock().await;
    write_envelope(&mut send, type_raw, payload).await
}

#[tokio::main]
async fn main() {
    let args = parse_args();

    let test_cert = match (&args.cert, &args.key) {
        (Some(cert_path), Some(key_path)) => pki::load_certificate_files(cert_path, key_path)
            .unwrap_or_else(|e| panic!("failed to load --cert/--key: {e}")),
        (None, None) => {
            let (test_cert, pem) = pki::generate_test_certificate_with_pem("localhost");
            std::fs::write(&args.cert_out, &pem).unwrap_or_else(|e| {
                panic!(
                    "failed to write generated certificate to {:?}: {e}",
                    args.cert_out
                )
            });
            eprintln!(
                "No --cert/--key given: generated a self-signed certificate at {:?}",
                args.cert_out
            );
            eprintln!(
                "Point sardp-client at it with: --trust-cert {:?}",
                args.cert_out
            );
            test_cert
        }
        _ => panic!("--cert and --key must be given together"),
    };

    let endpoint = net::server_endpoint(args.bind, &test_cert);
    let local_addr = endpoint
        .local_addr()
        .expect("bound socket has a local address");
    eprintln!("sardp-server listening on {local_addr}");

    let shutdown = Arc::new(Notify::new());
    let permission_command: PermissionCommand = Arc::new(Mutex::new(Vec::new()));
    let state = Arc::new(ServerState::default());
    // KNOWN_ISSUES.md #3: actively reclaims file_handles nobody ever opened
    // the `file` stream for, rather than relying solely on `validate`'s
    // passive expiry check. Runs for the server's whole lifetime; nothing
    // needs to join it.
    let _reaper = state.file_handles.spawn_reaper(FILE_HANDLE_REAP_INTERVAL);

    // stdin admin command reader (Part 4's minimal live-trigger).
    {
        let permission_command = permission_command.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
            loop {
                tokio::select! {
                    _ = shutdown.notified() => return,
                    line = lines.next_line() => {
                        match line {
                            Ok(Some(line)) => {
                                if line.trim().is_empty() {
                                    // nothing typed, just Enter
                                } else if let Some(command) = permission_sm::parse_admin_command(&line) {
                                    eprintln!("(admin) queued: {command:?}");
                                    permission_command.lock().await.push(command);
                                } else {
                                    eprintln!("(admin) unknown command: {line:?} (--help lists the recognized ones)");
                                }
                            }
                            _ => return,
                        }
                    }
                }
            }
        });
    }

    let mut connections = tokio::task::JoinSet::new();

    loop {
        tokio::select! {
            biased;
            () = shutdown_signal() => {
                eprintln!("shutdown requested, closing connections...");
                shutdown.notify_waiters();
                break;
            }
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let trusted_pubkey = args.trusted_pubkey;
                let server_name = args.server_name.clone();
                let (width, height, fps) = (args.width, args.height, args.fps);
                let capture = CaptureSettings {
                    mode: args.capture,
                    all_idr: args.all_idr,
                    text_char_delay_ms: args.text_char_delay_ms,
                };
                let shutdown = shutdown.clone();
                let permission_command = permission_command.clone();
                let state = state.clone();
                connections.spawn(async move {
                    let connection = match incoming.await {
                        Ok(c) => c,
                        Err(e) => {
                            eprintln!("failed to accept connection: {e}");
                            return;
                        }
                    };
                    let peer = connection.remote_address();
                    match handle_connection(
                        connection, &server_name, &trusted_pubkey, width, height, fps, capture,
                        shutdown, permission_command, state,
                    ).await {
                        Ok(()) => eprintln!("[{peer}] connection ended cleanly"),
                        Err(e) => eprintln!("[{peer}] connection ended: {e:?}"),
                    }
                });
            }
        }
    }

    endpoint.close(0u32.into(), b"server shutting down");
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    eprintln!("sardp-server stopped");
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

/// Sends `SessionClose{reason}` then closes the QUIC connection after the
/// spec 4.7 `CLOSING_GRACE_PERIOD`, mirroring the Connection SM's
/// `Closing` state (spec 4.1).
///
/// Known nuance (observed in manual testing, not fixed here): the peer
/// isn't guaranteed to actually read this `SessionClose` Envelope before
/// the hard `connection.close()` below tears down the transport -- in
/// practice the peer often instead sees the connection end via
/// `ConnectionError::ApplicationClosed` carrying this function's own
/// close reason bytes ("session closed"), which is still a clear,
/// intentional-shutdown signal rather than a crash, just not routed
/// through the application-level `SessionClose` message this PoC defines.
/// Closing this gap for real would need to wait for explicit delivery
/// confirmation (e.g. tracking the stream until the peer acknowledges it,
/// which quinn does not expose as a simple one-shot future) rather than
/// an unconditional sleep.
async fn close_gracefully(
    connection: &quinn::Connection,
    control_send: &SharedControlSend,
    reason: ReasonCode,
) {
    {
        let mut send = control_send.lock().await;
        let _ = write_envelope(
            &mut send,
            messages::type_id::SESSION_CLOSE,
            &messages::encode(&SessionClose { reason }),
        )
        .await;
        let _ = send.finish();
    }
    tokio::time::sleep(timeouts::CLOSING_GRACE_PERIOD).await;
    connection.close(0u32.into(), b"session closed");
}

#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    connection: quinn::Connection,
    server_name: &str,
    trusted_pubkey: &VerifyingKey,
    width: u32,
    height: u32,
    fps: f64,
    capture: CaptureSettings,
    shutdown: Arc<Notify>,
    permission_command: PermissionCommand,
    state: Arc<ServerState>,
) -> Result<(), ConnError> {
    let peer = connection.remote_address();

    // Spec 4.6: a new connection's first control message is either a fresh
    // `ClientHello` or a `SessionReauthenticate` resuming a `Suspended`
    // session; `establish_connection` dispatches between the two and
    // drives whichever applies to the Active-state threshold.
    let outcome = reconnection::establish_connection(
        &connection,
        server_name,
        trusted_pubkey,
        &state.sessions,
        timeouts::HANDSHAKE_TIMEOUT,
        timeouts::AUTH_TIMEOUT,
    )
    .await?;
    let mut ctx = match outcome {
        EstablishOutcome::Established(ctx) => ctx,
        EstablishOutcome::ReconnectRejected(reason) => {
            eprintln!("[{peer}] reconnect rejected: {reason:?}");
            return Ok(());
        }
    };
    if ctx.is_resumed {
        eprintln!(
            "[{peer}] reconnected, session_id={:x?}, user_id={:?}, resuming at generation {}",
            ctx.session_id, ctx.user_id, ctx.starting_generation
        );
    } else {
        eprintln!(
            "[{peer}] authenticated (fresh handshake), session_id={:x?}, user_id={:?}",
            ctx.session_id, ctx.user_id
        );
    }

    // The client runs several TimeSync rounds and keeps the best one
    // (`timesync::best_of`); answer them all before moving on.
    let time_sync_rounds = sardp::timesync::server_respond_time_sync_burst(
        &mut ctx.control,
        Duration::from_millis(200),
    )
    .await?;
    eprintln!("[{peer}] answered {time_sync_rounds} TimeSync round(s)");

    // The frame source outlives every generation of this session (the
    // desktop one owns the capture thread and the encoder's state).
    let (mut frame_source, width, height, fps, profile, tier) = match capture.mode {
        CaptureMode::Synthetic => (
            FrameSource::Synthetic { width, height },
            width,
            height,
            fps,
            66u16,
            4u8,
        ),
        #[cfg(windows)]
        CaptureMode::Desktop => {
            let clock: sardp_win::Clock = Arc::new(clock::now_us);
            let config = sardp_win::DesktopH264Config {
                all_idr: capture.all_idr,
                ..Default::default()
            };
            let source = sardp_win::DesktopH264Source::start(config, clock)
                .map_err(|e| ConnError::Capture(e.to_string()))?;
            let info = source.info();
            eprintln!(
                "[{peer}] desktop capture started: {}x{} @ {}Hz, hardware H.264, {}",
                info.width,
                info.height,
                info.fps,
                if capture.all_idr {
                    "all-IDR (--all-idr)"
                } else {
                    "IDR + P-frames"
                }
            );
            // Main profile (what the hardware MFT negotiates), Tier 3
            // (hardware 4:2:0, no QP map yet -- spec Part 6).
            (
                FrameSource::Desktop(source),
                info.width,
                info.height,
                f64::from(info.fps),
                77u16,
                3u8,
            )
        }
        #[cfg(not(windows))]
        CaptureMode::Desktop => {
            unreachable!("--capture desktop is rejected at argument parsing off Windows")
        }
    };

    // Input injection (spec 2.12, 3W-1-d-4) goes to the real desktop only
    // when the video comes from it; a synthetic-video session logs the
    // events instead, so a dev machine running the synthetic server never
    // gets its keyboard/mouse driven by a test client.
    let input_sink = match &frame_source {
        #[cfg(windows)]
        FrameSource::Desktop(source) => {
            let info = source.info();
            eprintln!(
                "[{peer}] input injection: SendInput, output origin ({}, {}), text char delay {}ms",
                info.origin_x, info.origin_y, capture.text_char_delay_ms
            );
            InputSink::Windows(sardp_win::InputInjector::start(sardp_win::InjectorConfig {
                text_unit_delay: Duration::from_millis(capture.text_char_delay_ms),
                output_origin: (info.origin_x, info.origin_y),
            }))
        }
        _ => {
            eprintln!("[{peer}] input injection: log only (synthetic capture)");
            InputSink::Log
        }
    };
    let mut input_injection = InputInjection::new(input_sink, peer);

    let encoder_config = EncoderConfig {
        codec: Codec::H264,
        profile,
        chroma_format: ChromaFormat::C420,
        bit_depth: 8,
        width,
        height,
        max_fps: fps.round() as u16,
        tier,
        b_frames: 0,
        server_cursor_excludable: false,
    };

    let mut video_channel = VideoChannel::new(ctx.starting_generation);
    let video_send = tokio::time::timeout(
        timeouts::SESSION_SETUP_TIMEOUT,
        open_generation(
            &connection,
            ctx.starting_generation,
            0,
            encoder_config,
            &mut frame_source,
        ),
    )
    .await
    .map_err(|_elapsed| ConnError::Violation(ReasonCode::PROTOCOL_SESSION_SETUP_TIMEOUT))??;
    video_channel.mark_instance_streaming()?;
    if !ctx.is_resumed {
        // A resumed connection_sm is already `Active` (spec 4.6:
        // `resume()` skips straight there); only a fresh handshake needs
        // this Authenticated -> Active transition.
        ctx.connection_sm.on_channel_live()?;
    }
    eprintln!(
        "[{peer}] video channel Live, connection {:?}",
        ctx.connection_sm.state()
    );

    let feedback_receiver = FeedbackReceiver::accept(&connection).await?;
    let permission_sm = PermissionSm::new(ctx.granted_permissions);

    // Split into a shared, lockable send half (spawned per-transfer tasks
    // need to write FileTransferError onto control too, DR-038) and the
    // read half this loop keeps exclusively.
    let ControlChannel {
        send: control_send,
        reader: mut control_reader,
    } = ctx.control;
    let control_send: SharedControlSend = Arc::new(Mutex::new(control_send));

    let result = run_active_session(
        &connection,
        &control_send,
        &mut control_reader,
        &mut video_channel,
        video_send,
        feedback_receiver,
        permission_sm,
        encoder_config,
        &mut frame_source,
        &mut input_injection,
        fps,
        peer,
        &shutdown,
        &permission_command,
        ctx.granted_permissions,
        &state,
        ctx.session_id,
        &ctx.user_id,
    )
    .await;
    // Spec 4.4.2: whatever the session left pressed is released now,
    // whichever way the session ended.
    input_injection.release_all();

    match result {
        Ok(()) => Ok(()),
        Err(e) if is_transport_disconnect(&e) => {
            eprintln!("[{peer}] connection lost unexpectedly ({e:?}); suspending session");
            suspend_and_store(
                &state,
                ctx.session_id,
                ctx.user_id,
                ctx.connection_sm,
                ctx.reconnect_token,
                ctx.granted_permissions,
                video_channel.generation(),
                peer,
            )
        }
        Err(e) => Err(e),
    }
}

/// Transitions `connection_sm` `Active -> Suspended` and registers the
/// session in `state.sessions` so a new connection presenting
/// `reconnect_token` can resume it within `RECONNECT_GRACE_PERIOD` (spec
/// 4.6), via [`SessionStore::suspend_and_schedule_expiry`].
#[allow(clippy::too_many_arguments)]
fn suspend_and_store(
    state: &Arc<ServerState>,
    session_id: [u8; 16],
    user_id: String,
    connection_sm: sardp::ConnectionSm,
    reconnect_token: [u8; 32],
    granted_permissions: u32,
    last_generation: u64,
    peer: SocketAddr,
) -> Result<(), ConnError> {
    state.sessions.suspend_and_schedule_expiry(
        session_id,
        user_id,
        connection_sm,
        reconnect_token,
        granted_permissions,
        last_generation,
        timeouts::RECONNECT_GRACE_PERIOD,
    )?;
    eprintln!(
        "[{peer}] session {session_id:x?} suspended, reconnectable for {:?}",
        timeouts::RECONNECT_GRACE_PERIOD
    );
    Ok(())
}

/// The Active-state session loop: control-stream messages (`SessionClose`,
/// `FileTransferRequest`, ...), keepalives, frame send, and feedback-driven
/// backpressure. Split out of `handle_connection` so its caller can tell a
/// real transport disconnect apart from a graceful end and, on the former,
/// suspend the session (see [`is_transport_disconnect`]) instead of just
/// dropping it.
#[allow(clippy::too_many_arguments)]
async fn run_active_session(
    connection: &quinn::Connection,
    control_send: &SharedControlSend,
    control_reader: &mut EnvelopeReader,
    video_channel: &mut VideoChannel,
    mut video_send: quinn::SendStream,
    mut feedback_receiver: FeedbackReceiver,
    mut permission_sm: PermissionSm,
    encoder_config: EncoderConfig,
    frame_source: &mut FrameSource,
    input_injection: &mut InputInjection,
    fps: f64,
    peer: SocketAddr,
    shutdown: &Arc<Notify>,
    permission_command: &PermissionCommand,
    granted_permissions: u32,
    state: &Arc<ServerState>,
    session_id: [u8; 16],
    user_id: &str,
) -> Result<(), ConnError> {
    let (width, height) = (encoder_config.width, encoder_config.height);
    // The client's `input` stream (spec 2.12), once it opens one.
    let mut input_receiver: Option<InputReceiver> = None;
    // For the synthetic source this paces frame generation; for the
    // desktop source frames arrive on their own arm and this tick only
    // services admin commands.
    let mut frame_interval = tokio::time::interval(Duration::from_secs_f64(1.0 / fps));
    frame_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut keepalive_interval = tokio::time::interval(timeouts::KEEPALIVE_INTERVAL);
    let mut frame_id = 1u64;
    let mut last_activity = tokio::time::Instant::now();

    // KNOWN_ISSUES.md #12: audio_playback (server -> client), gated live on
    // AUDIO_PLAYBACK the same way video frame send is gated on VIEW.
    // Lazily opened on the first tick it's granted, rather than
    // unconditionally at session start, since a fresh handshake doesn't
    // grant it by default (see `handshake.rs`'s doc comment on
    // `granted_permissions`).
    let audio_config = AudioConfig {
        codec: AudioCodec::Opus,
        sample_rate: 48_000,
        channels: 1,
        frame_duration_ms: 20,
    };
    let audio_samples_per_frame = (u64::from(audio_config.sample_rate)
        * u64::from(audio_config.frame_duration_ms)
        / 1000) as usize;
    let mut audio_interval = tokio::time::interval(Duration::from_millis(u64::from(
        audio_config.frame_duration_ms,
    )));
    audio_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut audio_playback_send: Option<quinn::SendStream> = None;
    let mut audio_sequence = 0u64;

    loop {
        let idle_deadline = last_activity + timeouts::IDLE_TIMEOUT;
        tokio::select! {
            biased;
            () = shutdown.notified() => {
                eprintln!("[{peer}] server shutting down, closing session");
                close_gracefully(connection, control_send, ReasonCode::NONE).await;
                return Ok(());
            }
            () = tokio::time::sleep_until(idle_deadline) => {
                // Spec 4.1: Active -(IDLE_TIMEOUT)-> Suspended, not Closing
                // -- unlike a graceful SessionClose, this keeps the session
                // reconnectable for RECONNECT_GRACE_PERIOD rather than
                // ending it. The transport itself has nothing left to do
                // (a reconnect always uses a brand new QUIC connection),
                // so it's fine to drop after storing state.
                eprintln!("[{peer}] IDLE_TIMEOUT ({:?} since last activity)", timeouts::IDLE_TIMEOUT);
                return Err(ConnError::IdleTimeout);
            }
            control_msg = control_reader.read_envelope(sardp::StreamKind::Control.max_envelope_length()) => {
                let (type_raw, payload) = control_msg?;
                last_activity = tokio::time::Instant::now();
                if type_raw == messages::type_id::TIME_SYNC_REQUEST {
                    // Spec 2.9: TimeSync can run at any time (and SHOULD be
                    // answered ahead of other control traffic).
                    let response = sardp::timesync::answer_time_sync_request(type_raw, &payload)?;
                    write_control(control_send, messages::type_id::TIME_SYNC_RESPONSE, &messages::encode(&response)).await?;
                    continue;
                }
                if type_raw == messages::type_id::SESSION_CLOSE {
                    let close: SessionClose = messages::decode(&payload).unwrap_or(SessionClose { reason: ReasonCode::NONE });
                    eprintln!("[{peer}] client sent SessionClose (reason {:?}), closing", close.reason);
                    return Ok(());
                }
                if type_raw == messages::type_id::FILE_TRANSFER_REQUEST {
                    let request: FileTransferRequest = messages::decode(&payload)
                        .map_err(|_| ConnError::Violation(ReasonCode::PROTOCOL_UNEXPECTED_MESSAGE))?;

                    // Spec 4.5: a new operation needs the relevant bit
                    // Granted -- NotGranted and Draining (a revoke still
                    // waiting on in-progress operations to finish) both
                    // block starting a *new* one, same as the VIEW gate
                    // below for frame sending.
                    let required_bit = file_transfer::required_permission_bit(request.direction);
                    if let Err(reason) = permission_sm.check_gate(required_bit) {
                        let reject = FileTransferReject {
                            request_id: request.request_id,
                            reason,
                        };
                        write_control(control_send, messages::type_id::FILE_TRANSFER_REJECT, &messages::encode(&reject)).await?;
                        eprintln!(
                            "[{peer}] rejected FileTransferRequest ({:?}): permission not granted ({reason:?})",
                            request.direction
                        );
                        continue;
                    }

                    let Some((file_handle, expiry_ts)) = state.file_handles.try_issue(
                        session_id, user_id.to_string(), request.direction, request.declared_size,
                        FILE_HANDLE_TTL, MAX_CONCURRENT_FILE_TRANSFERS,
                    ) else {
                        // KNOWN_ISSUES.md #3: refuse rather than growing
                        // file_handles without bound. Spec 2.6 has no
                        // dedicated "server busy" code for file transfer;
                        // POLICY_FILE_POLICY_REJECTED is the closest fit
                        // among the defined ReasonCode table (spec 4.8.1).
                        let reject = FileTransferReject {
                            request_id: request.request_id,
                            reason: ReasonCode::POLICY_FILE_POLICY_REJECTED,
                        };
                        write_control(control_send, messages::type_id::FILE_TRANSFER_REJECT, &messages::encode(&reject)).await?;
                        eprintln!(
                            "[{peer}] rejected FileTransferRequest ({:?}): at the concurrent transfer limit ({MAX_CONCURRENT_FILE_TRANSFERS})",
                            request.direction
                        );
                        continue;
                    };
                    let accept = FileTransferAccept {
                        request_id: request.request_id,
                        file_handle,
                        // No real filesystem in this PoC (see
                        // docs/SARDP_PoC_Brief_for_ClaudeCode.md): the
                        // declared size is trusted as-is rather than
                        // resolved against anything real.
                        resolved_size: request.declared_size,
                        expiry_ts,
                    };
                    write_control(control_send, messages::type_id::FILE_TRANSFER_ACCEPT, &messages::encode(&accept)).await?;
                    eprintln!(
                        "[{peer}] issued file_handle {file_handle:#x} for {:?} of {:?} ({} bytes)",
                        request.direction, request.virtual_path, request.declared_size
                    );
                    spawn_file_transfer(connection.clone(), control_send.clone(), state.clone(), session_id, user_id.to_string(), request, file_handle, peer);
                    continue;
                }
                // Other control-stream message types aren't produced by
                // this PoC's client yet; ignore (matches the ignorable-flag
                // spirit of spec 2.1.1 rather than a hard protocol error,
                // since nothing observed here is a core message this PoC
                // hasn't implemented on purpose).
            }
            _ = keepalive_interval.tick() => {
                write_control(control_send, messages::type_id::KEEP_ALIVE, &messages::encode(&messages::KeepAlive {})).await?;
            }
            _ = frame_interval.tick() => {
                let mut commands = permission_command.lock().await;
                for admin_command in commands.drain(..) {
                    match admin_command {
                        permission_sm::AdminCommand::TogglePermission { bit: toggled_bit, grant } => {
                            let update = permission_sm::build_permission_toggle(granted_permissions, toggled_bit, grant);
                            permission_sm.apply_update(&update);
                            write_control(control_send, messages::type_id::PERMISSION_UPDATE, &messages::encode(&update)).await?;
                            eprintln!(
                                "[{peer}] {} is now {}",
                                permission_bit_name(toggled_bit),
                                if grant { "granted" } else { "revoked" }
                            );
                        }
                        permission_sm::AdminCommand::SendClipboard => {
                            if let Err(reason) = permission_sm.check_gate(bit::CLIP_READ) {
                                eprintln!("[{peer}] cannot send clipboard: CLIP_READ not granted ({reason:?})");
                            } else {
                                spawn_clipboard_announce(connection.clone(), peer);
                            }
                        }
                    }
                }
                drop(commands);

                if !permission_sm.is_granted(bit::VIEW) {
                    continue;
                }
                // Desktop frames arrive through their own arm below.
                if !matches!(frame_source, FrameSource::Synthetic { .. }) {
                    continue;
                }
                let Some(frame) = next_frame(frame_source).await? else { continue };
                send_sourced_frame(&mut video_send, video_channel.generation(), frame_id, width, height, &frame).await?;
                frame_id += 1;
            }
            desktop = next_desktop_frame(frame_source) => {
                let frame = desktop?.ok_or_else(|| ConnError::Capture("desktop capture stopped".into()))?;
                // Same VIEW gate as the synthetic path; a revoked VIEW just
                // drops captured frames on the floor (the capture thread
                // keeps running so re-granting resumes instantly).
                if !permission_sm.is_granted(bit::VIEW) {
                    continue;
                }
                send_sourced_frame(&mut video_send, video_channel.generation(), frame_id, width, height, &frame).await?;
                if frame_id <= 3 || frame_id.is_multiple_of(60) {
                    eprintln!(
                        "[{peer}] desktop frame {frame_id}: {} bytes, idr={}, encode {}us",
                        frame.bytes.len(), frame.is_idr, frame.encode_done_ts.saturating_sub(frame.capture_ts)
                    );
                }
                frame_id += 1;
            }
            feedback = feedback_receiver.read_one() => {
                let feedback = feedback?;
                last_activity = tokio::time::Instant::now();
                let now_us = clock::now_us();
                let decision = video_channel.on_feedback(now_us, feedback.client_queue_delay_us, 0)?;
                match decision {
                    BackpressureDecision::Continue | BackpressureDecision::EnterCongested | BackpressureDecision::ExitCongested => {}
                    BackpressureDecision::ResetStream => {
                        eprintln!("[{peer}] backpressure hard threshold exceeded, resetting video stream");
                        let _ = video_send.reset(quinn::VarInt::from_u32(0));
                        let new_generation = video_channel.prepare_reopen();
                        video_send = tokio::time::timeout(
                            VIDEO_CONFIGURING_TIMEOUT,
                            open_generation(connection, new_generation, 1, encoder_config, frame_source),
                        )
                        .await
                        .map_err(|_elapsed| ConnError::Violation(ReasonCode::PROTOCOL_VIDEO_CONFIGURING_TIMEOUT))??;
                        video_channel.mark_instance_streaming()?;
                        frame_id = 1;
                        eprintln!(
                            "[{peer}] reopened at generation {new_generation}, channel {:?}",
                            video_channel.channel_state()
                        );
                        debug_assert_eq!(video_channel.channel_state(), ChannelState::Live);
                    }
                }
            }
            _ = audio_interval.tick() => {
                if permission_sm.is_granted(bit::AUDIO_PLAYBACK) {
                    if audio_playback_send.is_none() {
                        let send = audio_session::open_audio_stream(connection, StreamKind::AudioPlayback, &audio_config).await?;
                        eprintln!("[{peer}] opened audio_playback stream");
                        audio_playback_send = Some(send);
                    }
                    let send = audio_playback_send.as_mut().expect("just ensured Some above");
                    let capture_ts = clock::now_us();
                    let payload = audio_session::generate_sine_wave_payload(audio_samples_per_frame, audio_config.sample_rate, 440.0);
                    let duration_us = u32::from(audio_config.frame_duration_ms) * 1000;
                    audio_session::send_audio_frame(send, audio_sequence, capture_ts, duration_us, &payload).await?;
                    audio_sequence += 1;
                }
            }
            // Every client-initiated unidirectional stream (audio_capture,
            // input) is accepted here and dispatched on its prologue's
            // `kind`: two `accept_uni()` arms would race for the same
            // stream and each reject the other's kind.
            accepted = accept_incoming_uni(connection) => {
                let (kind, mut reader) = accepted?;
                match kind {
                    StreamKind::AudioCapture => {
                        match audio_session::accept_audio_capture_from_reader(reader, permission_sm.is_granted(bit::AUDIO_CAPTURE)).await? {
                            Some((_config, mut frame_reader)) => {
                                eprintln!("[{peer}] accepted audio_capture stream");
                                tokio::spawn(async move {
                                    loop {
                                        match frame_reader.read_next_frame().await {
                                            Ok((header, payload)) => {
                                                eprintln!("[{peer}] audio_capture frame sequence={} bytes={}", header.sequence, payload.len());
                                            }
                                            Err(e) => {
                                                eprintln!("[{peer}] audio_capture stream ended: {e:?}");
                                                return;
                                            }
                                        }
                                    }
                                });
                            }
                            None => {
                                eprintln!("[{peer}] refused audio_capture stream: AUDIO_CAPTURE not granted");
                            }
                        }
                    }
                    StreamKind::Input => {
                        if input_receiver.is_some() {
                            eprintln!("[{peer}] refused a second input stream");
                            reader.stop(quinn::VarInt::from_u32(0));
                        } else {
                            eprintln!("[{peer}] accepted input stream");
                            input_receiver = Some(InputReceiver::from_reader(reader));
                        }
                    }
                    other => {
                        eprintln!("[{peer}] refused unexpected client-initiated stream kind {other:?}");
                        reader.stop(quinn::VarInt::from_u32(0));
                    }
                }
            }
            input = async { input_receiver.as_mut().expect("guarded by the arm precondition").read_message().await },
                if input_receiver.is_some() =>
            {
                match input {
                    Ok(message) => {
                        last_activity = tokio::time::Instant::now();
                        input_injection.handle(message, &permission_sm)?;
                    }
                    Err(e) => {
                        // Spec 4.4.2: the stream closing releases whatever
                        // it left pressed. The client may open a new one.
                        eprintln!("[{peer}] input stream ended: {e:?}");
                        input_injection.release_all();
                        input_receiver = None;
                    }
                }
            }
            accept_result = clipboard_session::accept_clipboard_formats(connection) => {
                // CLIP_WRITE (client -> server announce): safe to accept
                // here now that `file` is unidirectional (DR-038) and no
                // longer touches `accept_bi()` -- this is the only
                // `accept_bi()` caller left on the server, so there's
                // nothing left to race with (KNOWN_ISSUES.md).
                let (mut send, mut reader, formats) = accept_result?;
                if !permission_sm.is_granted(bit::CLIP_WRITE) {
                    // Spec 2.7 doesn't define a way to refuse an announce
                    // outright; simply never requesting anything is
                    // already a valid response to `ClipboardFormats`.
                    eprintln!("[{peer}] received clipboard formats but CLIP_WRITE not granted; ignoring");
                    continue;
                }
                let request_id = formats.request_id;
                let Some(first_format) = formats.formats.into_iter().next() else {
                    eprintln!("[{peer}] received ClipboardFormats with no formats, nothing to request");
                    continue;
                };
                eprintln!(
                    "[{peer}] received clipboard formats, requesting {:?}/{}",
                    first_format.namespace, first_format.format_id
                );
                tokio::spawn(async move {
                    let request = messages::ClipboardRequest {
                        request_id,
                        namespace: first_format.namespace,
                        format_id: first_format.format_id,
                    };
                    match clipboard_session::request_clipboard_data(&mut send, &mut reader, &request).await {
                        Ok(Ok(data)) => eprintln!("[{peer}] clipboard data received: {} bytes", data.data.len()),
                        Ok(Err(error)) => eprintln!("[{peer}] clipboard request rejected: {:?}", error.reason),
                        Err(e) => eprintln!("[{peer}] clipboard request failed: {e:?}"),
                    }
                });
            }
        }
    }
}

/// Accepts the next client-initiated unidirectional stream and reads its
/// `StreamPrologue`, leaving the dispatch on `kind` to the caller.
async fn accept_incoming_uni(
    connection: &quinn::Connection,
) -> Result<(StreamKind, EnvelopeReader), ConnError> {
    let recv = connection.accept_uni().await?;
    let mut reader = EnvelopeReader::new(recv);
    let prologue = reader.read_prologue().await?;
    Ok((prologue.kind, reader))
}

/// Where a session's input events go (spec 2.12 -> OS).
enum InputSink {
    /// Log only (synthetic video: nothing to drive).
    Log,
    #[cfg(windows)]
    Windows(sardp_win::InputInjector),
}

/// Per-session input state (spec 4.4): IME mode SM, the pressed-key
/// invariant, permission gating, and the sink events are delivered to.
struct InputInjection {
    sink: InputSink,
    ime: ImeModeSm,
    pressed: PressedInputs,
    peer: SocketAddr,
    /// Counters for the log.
    injected: u64,
    dropped_not_granted: u64,
    skipped_character_keys: u64,
    mouse_moves: u64,
}

impl InputInjection {
    fn new(sink: InputSink, peer: SocketAddr) -> Self {
        Self {
            sink,
            ime: ImeModeSm::new(),
            pressed: PressedInputs::new(),
            peer,
            injected: 0,
            dropped_not_granted: 0,
            skipped_character_keys: 0,
            mouse_moves: 0,
        }
    }

    /// Applies one `input` stream message. Permission checks use the live
    /// `PermissionSm` (a revoke drops events from that moment on, spec
    /// 4.5); `Err` only for protocol violations (spec 4.4.1's forbidden
    /// messages), which end the session.
    fn handle(
        &mut self,
        message: InputMessage,
        permission_sm: &PermissionSm,
    ) -> Result<(), ConnError> {
        let peer = self.peer;
        let keyboard = permission_sm.is_granted(bit::INPUT_KEYBOARD);
        let mouse = permission_sm.is_granted(bit::INPUT_MOUSE);
        let mut not_granted = |what: &str, id: u64| {
            self.dropped_not_granted += 1;
            if self.dropped_not_granted.is_power_of_two() {
                eprintln!(
                    "[{peer}] dropped {what} event {id}: permission not granted ({} so far)",
                    self.dropped_not_granted
                );
            }
        };
        match message {
            InputMessage::ImeModeChange(change) => {
                eprintln!(
                    "[{peer}] IME mode -> {:?} after event {}",
                    change.mode, change.effective_after_event_id
                );
                self.ime.on_mode_change(&change);
            }
            InputMessage::Key(key) => {
                let mode = self.ime.mode_for(key.header.event_id);
                if !keyboard {
                    not_granted("key", key.header.event_id);
                    return Ok(());
                }
                if !should_inject_key(mode, key.scancode, key.modifiers) {
                    // Spec 2.12: the character comes via TextInput.
                    self.skipped_character_keys += 1;
                    return Ok(());
                }
                self.pressed.on_key(key.scancode, key.down);
                eprintln!(
                    "[{peer}] key event {}: hid={:#04x} down={} modifiers={:#06b}",
                    key.header.event_id, key.scancode, key.down, key.modifiers
                );
                self.emit(SinkEvent::Key {
                    hid_usage: key.scancode,
                    down: key.down,
                });
            }
            InputMessage::Text(text) => {
                let mode = self.ime.mode_for(text.header.event_id);
                ImeModeSm::check_text_allowed(mode).map_err(ConnError::Violation)?;
                if !keyboard {
                    not_granted("text", text.header.event_id);
                    return Ok(());
                }
                eprintln!(
                    "[{peer}] text event {}: {:?}",
                    text.header.event_id, text.text
                );
                self.emit(SinkEvent::Text(text.text));
            }
            InputMessage::ImeComposition(composition) => {
                let mode = self.ime.mode_for(composition.header.event_id);
                ImeModeSm::check_text_allowed(mode).map_err(ConnError::Violation)?;
                if !keyboard {
                    not_granted("ime composition", composition.header.event_id);
                    return Ok(());
                }
                // Nothing to inject: the committed text follows as
                // TextInput. Logged so the client-side IME path is visible.
                eprintln!(
                    "[{peer}] ime composition event {}: {:?} caret={}",
                    composition.header.event_id, composition.text, composition.caret
                );
            }
            InputMessage::MouseMove(m) => {
                if !mouse {
                    not_granted("mouse move", m.header.event_id);
                    return Ok(());
                }
                self.mouse_moves += 1;
                if self.mouse_moves <= 3 || self.mouse_moves.is_multiple_of(100) {
                    eprintln!(
                        "[{peer}] mouse move event {}: ({}, {})",
                        m.header.event_id, m.x, m.y
                    );
                }
                self.emit(SinkEvent::MouseMove { x: m.x, y: m.y });
            }
            InputMessage::MouseButton(b) => {
                if !mouse {
                    not_granted("mouse button", b.header.event_id);
                    return Ok(());
                }
                self.pressed.on_button(b.button, b.down);
                eprintln!(
                    "[{peer}] mouse button event {}: button={} down={} at ({}, {})",
                    b.header.event_id, b.button, b.down, b.x, b.y
                );
                // Position first, so the click lands where the client saw it.
                self.emit(SinkEvent::MouseMove { x: b.x, y: b.y });
                self.emit(SinkEvent::MouseButton {
                    button: b.button,
                    down: b.down,
                });
            }
            InputMessage::Wheel(w) => {
                if !mouse {
                    not_granted("wheel", w.header.event_id);
                    return Ok(());
                }
                eprintln!(
                    "[{peer}] wheel event {}: dx={} dy={}",
                    w.header.event_id, w.dx, w.dy
                );
                self.emit(SinkEvent::Wheel { dx: w.dx, dy: w.dy });
            }
        }
        Ok(())
    }

    /// Spec 4.4.2: synthesize releases for everything still pressed.
    fn release_all(&mut self) {
        let releases = self.pressed.take_releases();
        if releases.is_empty() {
            return;
        }
        eprintln!(
            "[{}] releasing {} pressed key(s)/button(s)",
            self.peer,
            releases.len()
        );
        for release in releases {
            match release {
                Release::Key(hid_usage) => self.emit(SinkEvent::Key {
                    hid_usage,
                    down: false,
                }),
                Release::Button(button) => self.emit(SinkEvent::MouseButton {
                    button,
                    down: false,
                }),
            }
        }
    }

    fn emit(&mut self, event: SinkEvent) {
        self.injected += 1;
        match &self.sink {
            InputSink::Log => {
                if !matches!(event, SinkEvent::MouseMove { .. }) || self.mouse_moves <= 3 {
                    eprintln!("[{}] (log-only sink) {event:?}", self.peer);
                }
            }
            #[cfg(windows)]
            InputSink::Windows(injector) => {
                let command = match event {
                    SinkEvent::Key { hid_usage, down } => {
                        sardp_win::InjectCommand::Key { hid_usage, down }
                    }
                    SinkEvent::Text(text) => sardp_win::InjectCommand::Text(text),
                    SinkEvent::MouseMove { x, y } => sardp_win::InjectCommand::MouseMove { x, y },
                    SinkEvent::MouseButton { button, down } => {
                        sardp_win::InjectCommand::MouseButton { button, down }
                    }
                    SinkEvent::Wheel { dx, dy } => sardp_win::InjectCommand::Wheel { dx, dy },
                };
                if let Err(e) = injector.inject(command) {
                    eprintln!("[{}] input injection failed: {e}", self.peer);
                }
            }
        }
    }
}

impl Drop for InputInjection {
    fn drop(&mut self) {
        self.release_all();
        eprintln!(
            "[{}] input summary: injected={} skipped_character_keys={} dropped_not_granted={} mouse_moves={}",
            self.peer,
            self.injected,
            self.skipped_character_keys,
            self.dropped_not_granted,
            self.mouse_moves
        );
    }
}

/// Sink-level event, after permission/IME/character-key decisions.
#[derive(Debug)]
enum SinkEvent {
    Key { hid_usage: u32, down: bool },
    Text(String),
    MouseMove { x: i32, y: i32 },
    MouseButton { button: u8, down: bool },
    Wheel { dx: i16, dy: i16 },
}

/// A human-readable name for one of this server's admin-togglable
/// `PermissionSet` bits, for the stdin admin log line -- falls back to the
/// raw bitmask for anything not in that list (there shouldn't be any,
/// since `permission_sm::parse_admin_command` is the only source of these
/// values).
fn permission_bit_name(toggled_bit: u32) -> String {
    match toggled_bit {
        b if b == bit::VIEW => "VIEW".to_string(),
        b if b == bit::INPUT_KEYBOARD => "INPUT_KEYBOARD".to_string(),
        b if b == bit::INPUT_MOUSE => "INPUT_MOUSE".to_string(),
        b if b == bit::CLIP_READ => "CLIP_READ".to_string(),
        b if b == bit::CLIP_WRITE => "CLIP_WRITE".to_string(),
        b if b == bit::AUDIO_PLAYBACK => "AUDIO_PLAYBACK".to_string(),
        b if b == bit::AUDIO_CAPTURE => "AUDIO_CAPTURE".to_string(),
        other => format!("permission bit {other:#x}"),
    }
}

/// Spawns a task that announces synthetic clipboard content (spec 2.7,
/// KNOWN_ISSUES.md #12) to whichever peer is on `connection` -- triggered
/// by the `send-clipboard` admin command -- and, if a `ClipboardRequest`
/// for it arrives, responds with fixed pseudo text. Independent of
/// `run_active_session`'s own select loop for the same reason
/// `spawn_file_transfer` is: `read_clipboard_request` blocks until the
/// peer actually asks, which must not stall video/control/keepalive on
/// the same connection. This is the `CLIP_READ` (server-announces)
/// direction; see `run_active_session`'s own `accept_clipboard_formats`
/// arm for the reverse (`CLIP_WRITE`, client-announces) direction.
fn spawn_clipboard_announce(connection: quinn::Connection, peer: SocketAddr) {
    tokio::spawn(async move {
        let formats = ClipboardFormats {
            request_id: 1,
            formats: vec![ClipboardFormatEntry {
                namespace: FormatNamespace::Mime,
                format_id: "text/plain".to_string(),
            }],
        };
        let (mut send, mut reader) =
            match clipboard_session::announce_clipboard_formats(&connection, &formats).await {
                Ok(pair) => pair,
                Err(e) => {
                    eprintln!("[{peer}] failed to announce clipboard formats: {e:?}");
                    return;
                }
            };
        match clipboard_session::read_clipboard_request(&mut reader).await {
            Ok(request) => {
                let pseudo_data = b"hello from sardp-server's synthetic clipboard".to_vec();
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
                    Ok(()) => eprintln!("[{peer}] responded to ClipboardRequest {request_id}"),
                    Err(e) => eprintln!("[{peer}] failed to respond to ClipboardRequest: {e:?}"),
                }
            }
            Err(e) => {
                eprintln!("[{peer}] clipboard announce: never received a ClipboardRequest ({e:?})");
            }
        }
    });
}

/// Spawns a task that drives `request`'s transfer via
/// [`file_transfer::run_file_transfer`] to completion, independently of
/// `handle_connection`'s own select loop so a slow or stalled transfer
/// doesn't block keepalives, video frames, or control messages on the same
/// connection.
#[allow(clippy::too_many_arguments)]
fn spawn_file_transfer(
    connection: quinn::Connection,
    control_send: SharedControlSend,
    state: Arc<ServerState>,
    session_id: [u8; 16],
    user_id: String,
    request: FileTransferRequest,
    file_handle: u64,
    peer: SocketAddr,
) {
    tokio::spawn(async move {
        match file_transfer::run_file_transfer(
            &connection,
            &state.file_handles,
            session_id,
            &user_id,
            &request,
            file_handle,
        )
        .await
        {
            Ok(file_transfer::FileTransferOutcome::Done) => {
                eprintln!("[{peer}] file transfer for handle {file_handle:#x} completed");
            }
            Ok(file_transfer::FileTransferOutcome::ReportError(error)) => {
                // DR-038: FileTransferError travels on `control`, not
                // `file` -- `run_file_transfer` only detected it.
                let result = write_control(
                    &control_send,
                    messages::type_id::FILE_TRANSFER_ERROR,
                    &messages::encode(&error),
                )
                .await;
                eprintln!(
                    "[{peer}] file transfer for handle {file_handle:#x} ended with {:?}{}",
                    error.reason,
                    if let Err(e) = result {
                        format!(" (failed to report it on control: {e:?})")
                    } else {
                        String::new()
                    }
                );
            }
            Err(e) => {
                eprintln!("[{peer}] file transfer for handle {file_handle:#x} failed: {e:?}");
            }
        }
    });
}

/// Opens a fresh video Instance at `generation` (spec 2.10/4.3.2): a
/// self-contained IDR from `source` plus setup messages. Shared by the
/// initial open and every backpressure-triggered reopen.
async fn open_generation(
    connection: &quinn::Connection,
    generation: u64,
    config_id: u64,
    encoder_config: EncoderConfig,
    source: &mut FrameSource,
) -> Result<quinn::SendStream, ConnError> {
    let idr = next_idr_frame(source).await?;
    if !sardp::h264::is_self_contained_idr(&idr.bytes) {
        // Spec 2.10 requires every IDR to carry its own SPS/PPS. The
        // synthetic encoder guarantees it (repeat-headers=1); the desktop
        // encoder prepends the negotiated sequence header when needed, so
        // this firing means that fallback broke -- worth a loud log rather
        // than a silent bad stream.
        eprintln!(
            "warning: IDR opening generation {generation} is not self-contained (no SPS/PPS before the IDR slice)"
        );
    }
    let (send, _sm) = video_session::open_video_instance(
        connection,
        0,
        generation,
        config_id,
        encoder_config,
        idr.bytes,
        idr.capture_ts,
        idr.encode_done_ts,
    )
    .await?;
    Ok(send)
}
