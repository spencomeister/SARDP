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

use sardp::backpressure::BackpressureDecision;
use sardp::channel_sm::ChannelState;
use sardp::conn_error::{ConnError, is_transport_disconnect};
use sardp::connection_sm::defaults as timeouts;
use sardp::encoder;
use sardp::feedback_session::FeedbackReceiver;
use sardp::file_handle_store::FileHandleStore;
use sardp::file_transfer_session::{self as file_transfer};
use sardp::handshake::ControlChannel;
use sardp::messages::{
    self, ChromaFormat, Codec, EncoderConfig, FileTransferAccept, FileTransferReject,
    FileTransferRequest, SessionClose,
};
use sardp::permission_set::bit;
use sardp::permission_sm::{self, PermissionSm};
use sardp::reason_code::ReasonCode;
use sardp::reconnection::{self, EstablishOutcome};
use sardp::session_store::SessionStore;
use sardp::stream_reader::write_envelope;
use sardp::timecode_frame;
use sardp::video_channel::VideoChannel;
use sardp::video_session;
use sardp::video_sm::defaults::VIDEO_CONFIGURING_TIMEOUT;
use sardp::{clock, dev_identity, net, pki};

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
    --server-name <NAME>    Name announced in ServerHello (default sardp-server)\n\
    --help                  Show this message\n\n\
Once a client is connected, typing `revoke-view` or `grant-view` (Enter)\n\
toggles its VIEW permission live."
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
    }
}

/// Shared, admin-triggered permission command for the one connected
/// client this PoC server handles interactively (stdin `revoke-view` /
/// `grant-view`). `None` once consumed by the connection's own loop.
type PermissionCommand = Arc<Mutex<Option<bool>>>; // Some(true)=grant VIEW, Some(false)=revoke VIEW

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
    let permission_command: PermissionCommand = Arc::new(Mutex::new(None));
    let state = Arc::new(ServerState::default());

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
                            Ok(Some(line)) => match line.trim() {
                                "revoke-view" => {
                                    *permission_command.lock().await = Some(false);
                                    eprintln!("(admin) queued: revoke VIEW");
                                }
                                "grant-view" => {
                                    *permission_command.lock().await = Some(true);
                                    eprintln!("(admin) queued: grant VIEW");
                                }
                                "" => {}
                                other => eprintln!("(admin) unknown command: {other:?} (try revoke-view / grant-view)"),
                            },
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
                        connection, &server_name, &trusted_pubkey, width, height, fps,
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

#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    connection: quinn::Connection,
    server_name: &str,
    trusted_pubkey: &VerifyingKey,
    width: u32,
    height: u32,
    fps: f64,
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

    sardp::timesync::server_respond_time_sync(&mut ctx.control).await?;

    let encoder_config = EncoderConfig {
        codec: Codec::H264,
        profile: 66,
        chroma_format: ChromaFormat::C420,
        bit_depth: 8,
        width,
        height,
        max_fps: fps.round() as u16,
        tier: 4,
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
            width,
            height,
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

    let result = run_active_session(
        &connection,
        &mut ctx.control,
        &mut video_channel,
        video_send,
        feedback_receiver,
        permission_sm,
        encoder_config,
        width,
        height,
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
    control: &mut ControlChannel,
    video_channel: &mut VideoChannel,
    mut video_send: quinn::SendStream,
    mut feedback_receiver: FeedbackReceiver,
    mut permission_sm: PermissionSm,
    encoder_config: EncoderConfig,
    width: u32,
    height: u32,
    fps: f64,
    peer: SocketAddr,
    shutdown: &Arc<Notify>,
    permission_command: &PermissionCommand,
    granted_permissions: u32,
    state: &Arc<ServerState>,
    session_id: [u8; 16],
    user_id: &str,
) -> Result<(), ConnError> {
    let mut frame_interval = tokio::time::interval(Duration::from_secs_f64(1.0 / fps));
    frame_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut keepalive_interval = tokio::time::interval(timeouts::KEEPALIVE_INTERVAL);
    let mut frame_id = 1u64;
    let mut last_activity = tokio::time::Instant::now();

    loop {
        let idle_deadline = last_activity + timeouts::IDLE_TIMEOUT;
        tokio::select! {
            biased;
            () = shutdown.notified() => {
                eprintln!("[{peer}] server shutting down, closing session");
                close_gracefully(connection, &mut control.send, ReasonCode::NONE).await;
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
            control_msg = control.reader.read_envelope(sardp::StreamKind::Control.max_envelope_length()) => {
                let (type_raw, payload) = control_msg?;
                last_activity = tokio::time::Instant::now();
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
                        write_envelope(&mut control.send, messages::type_id::FILE_TRANSFER_REJECT, &messages::encode(&reject)).await?;
                        eprintln!(
                            "[{peer}] rejected FileTransferRequest ({:?}): permission not granted ({reason:?})",
                            request.direction
                        );
                        continue;
                    }

                    let (file_handle, expiry_ts) = state.file_handles.issue(
                        session_id, user_id.to_string(), request.direction, request.declared_size, FILE_HANDLE_TTL,
                    );
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
                    write_envelope(&mut control.send, messages::type_id::FILE_TRANSFER_ACCEPT, &messages::encode(&accept)).await?;
                    eprintln!(
                        "[{peer}] issued file_handle {file_handle:#x} for {:?} of {:?} ({} bytes)",
                        request.direction, request.virtual_path, request.declared_size
                    );
                    spawn_file_transfer(connection.clone(), state.clone(), session_id, user_id.to_string(), request, file_handle, peer);
                    continue;
                }
                // Other control-stream message types aren't produced by
                // this PoC's client yet; ignore (matches the ignorable-flag
                // spirit of spec 2.1.1 rather than a hard protocol error,
                // since nothing observed here is a core message this PoC
                // hasn't implemented on purpose).
            }
            _ = keepalive_interval.tick() => {
                write_envelope(&mut control.send, messages::type_id::KEEP_ALIVE, &messages::encode(&messages::KeepAlive {})).await?;
            }
            _ = frame_interval.tick() => {
                let mut command = permission_command.lock().await;
                if let Some(grant) = command.take() {
                    let update = permission_sm::build_view_toggle(granted_permissions, grant);
                    permission_sm.apply_update(&update);
                    write_envelope(&mut control.send, messages::type_id::PERMISSION_UPDATE, &messages::encode(&update)).await?;
                    eprintln!("[{peer}] VIEW is now {}", if grant { "granted" } else { "revoked" });
                }
                drop(command);

                if !permission_sm.is_granted(bit::VIEW) {
                    continue;
                }

                let capture_ts = clock::now_us();
                let frame = timecode_frame::generate_timecode_frame(width, height, capture_ts, [40, 40, 40]);
                let bytes = tokio::task::spawn_blocking(move || encoder::encode_single_frame_idr(&frame)).await??;
                let encode_done_ts = clock::now_us();
                video_session::send_video_frame(
                    &mut video_send, video_channel.generation(), frame_id, 1,
                    messages::VIDEO_FRAME_FLAG_IDR, capture_ts, encode_done_ts, width, height, &bytes,
                ).await?;
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
                            open_generation(connection, new_generation, 1, encoder_config, width, height),
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
        }
    }
}

/// Spawns a task that drives `request`'s transfer via
/// [`file_transfer::run_file_transfer`] to completion, independently of
/// `handle_connection`'s own select loop so a slow or stalled transfer
/// doesn't block keepalives, video frames, or control messages on the same
/// connection.
fn spawn_file_transfer(
    connection: quinn::Connection,
    state: Arc<ServerState>,
    session_id: [u8; 16],
    user_id: String,
    request: FileTransferRequest,
    file_handle: u64,
    peer: SocketAddr,
) {
    tokio::spawn(async move {
        if let Err(e) = file_transfer::run_file_transfer(
            &connection,
            &state.file_handles,
            session_id,
            &user_id,
            &request,
        )
        .await
        {
            eprintln!("[{peer}] file transfer for handle {file_handle:#x} failed: {e:?}");
        }
    });
}

/// Opens a fresh video Instance at `generation` (spec 2.10/4.3.2): a
/// synthetic self-contained IDR plus setup messages. Shared by the
/// initial open and every backpressure-triggered reopen.
async fn open_generation(
    connection: &quinn::Connection,
    generation: u64,
    config_id: u64,
    encoder_config: EncoderConfig,
    width: u32,
    height: u32,
) -> Result<quinn::SendStream, ConnError> {
    let capture_ts = clock::now_us();
    let frame = timecode_frame::generate_timecode_frame(width, height, capture_ts, [40, 40, 40]);
    let idr =
        tokio::task::spawn_blocking(move || encoder::encode_single_frame_idr(&frame)).await??;
    let encode_done_ts = clock::now_us();
    let (send, _sm) = video_session::open_video_instance(
        connection,
        0,
        generation,
        config_id,
        encoder_config,
        idr,
        capture_ts,
        encode_done_ts,
    )
    .await?;
    Ok(send)
}
