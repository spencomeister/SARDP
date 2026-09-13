//! Desktop capture + hardware H.264 encode as a frame *source*.
//!
//! The macOS twin of `sardp_win::desktop_h264`, and deliberately the same
//! shape: [`DesktopH264Source::start`] spawns a worker thread that owns
//! the ScreenCaptureKit stream and the VideoToolbox session for their
//! whole lifetime, and finished [`EncodedFrame`]s reach the caller through
//! `sardp::frame_source`'s bounded channel with source-side drop (DR-007).
//!
//! Where the two platforms genuinely differ:
//!
//! - **Push, not pull.** DXGI is pulled (`AcquireNextFrame` blocks on the
//!   worker thread); ScreenCaptureKit *delivers* frames on a dispatch
//!   queue of its own, and VideoToolbox delivers encoded output on another
//!   thread again. So the worker thread here does not capture: it owns the
//!   two sessions, forwards what the callbacks produce, and tears both
//!   down on the way out. The callbacks reach it over a small bounded
//!   channel, which keeps every DR-007 decision (and its counting) in the
//!   one place the other platforms use, `FrameSender`.
//! - **No colour conversion step.** ScreenCaptureKit is asked for NV12
//!   directly, so the `ID3D11VideoProcessor` pass Windows needs has no
//!   counterpart -- the `CVPixelBuffer` goes straight into the encoder.
//! - **No "collect the previous frame's output" lag.** The Media
//!   Foundation path had to be changed (DR-036) to wait for the current
//!   input's own output; VideoToolbox pushes each frame out as soon as it
//!   is done, so the question does not arise.
//!
//! What is *not* different is the discipline: both OS sessions are
//! released on every path (`Drop` on the Rust wrappers, and the capture
//! stream is stopped before the encoder is flushed), and no API's account
//! of its own success is taken at face value -- the hardware-encoder flag
//! is read back from the session, the encoder's keyframe claim is checked
//! against the actual NAL units, and frames the encoder silently dropped
//! are counted rather than assumed not to exist.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::time::Duration;

use sardp::frame_source::{
    Clock, DEFAULT_CHANNEL_CAPACITY, DesktopH264Config, EncodedFrame, FrameWorker, SendOutcome,
    SourceError, SourceInfo, WorkerContext,
};
use sardp::h264::{self, ParameterSetCache};
use sck_capture_poc::shim::{self, FrameRef, FrameSink, FrameStatus, PixelFormat, Session};
use sck_capture_poc::vt::{EncodedRef, EncodedSink, Encoder};

/// Kept symmetrical with `sardp_win::WinCaptureError`.
pub type MacCaptureError = SourceError;

/// A running capture+encode pipeline. Dropping it stops the worker thread,
/// which stops the ScreenCaptureKit stream and invalidates the
/// VideoToolbox session.
pub struct DesktopH264Source {
    worker: FrameWorker<EncodedFrame, SourceInfo>,
    force_idr: Arc<AtomicBool>,
}

impl DesktopH264Source {
    /// Starts the worker and blocks (briefly) until the first frame has
    /// been captured *and* encoded -- readiness means the whole path
    /// works, not that the OS accepted our requests.
    pub fn start(config: DesktopH264Config, clock: Clock) -> Result<Self, SourceError> {
        let force_idr = Arc::new(AtomicBool::new(false));
        let worker = {
            let force_idr = force_idr.clone();
            FrameWorker::spawn(
                "sardp-mac-capture",
                DEFAULT_CHANNEL_CAPACITY,
                Duration::from_secs(15),
                move |ctx| run_worker(config, clock, ctx, force_idr),
            )?
        };
        Ok(Self { worker, force_idr })
    }

    pub fn info(&self) -> SourceInfo {
        *self.worker.info()
    }

    /// Next encoded frame, or `None` once the worker has stopped (an error
    /// after startup surfaces as the channel closing; the reason is logged
    /// by the worker).
    pub async fn next_frame(&mut self) -> Option<EncodedFrame> {
        self.worker.next().await
    }

    /// Ask the encoder to make the next frame an IDR (needed when a new
    /// video Instance/generation is opened and the stream must restart
    /// from a self-contained frame). A no-op in `all_idr` mode, where
    /// every frame is one already.
    pub fn request_idr(&self) {
        self.force_idr.store(true, Ordering::SeqCst);
    }
}

/// What the OS callbacks hand to the worker thread.
enum Event {
    Frame(EncodedFrame),
    /// ScreenCaptureKit stopped the stream on its own.
    Stopped(String),
}

fn run_worker(
    config: DesktopH264Config,
    clock: Clock,
    ctx: &mut WorkerContext<EncodedFrame, SourceInfo>,
    force_idr: Arc<AtomicBool>,
) -> Result<(), String> {
    if !shim::preflight_screen_capture_access() {
        // Deliberately not a prompt: macOS shows none for screen recording
        // (KNOWN_ISSUES #20). The caller is expected to put the user in
        // front of System Settings and start the source again -- a running
        // process never sees the grant appear (#22).
        return Err(
            "screen recording permission not granted (System Settings > Privacy & \
                    Security > Screen & System Audio Recording)"
                .to_string(),
        );
    }
    let display = shim::main_display_info().ok_or("no main display")?;
    let fps = display.refresh_hz.round().max(1.0) as u32;
    let info = SourceInfo {
        width: display.pixels_w,
        height: display.pixels_h,
        fps,
        origin_x: display.origin_x.round() as i32,
        origin_y: display.origin_y.round() as i32,
    };

    // Small and bounded: the worker forwards immediately, so this never
    // holds frames -- it exists only to get the callbacks' output onto the
    // thread that owns `ctx`.
    let (tx, rx): (SyncSender<Event>, Receiver<Event>) = sync_channel(DEFAULT_CHANNEL_CAPACITY);

    let encoder = Arc::new(
        Encoder::new(
            info.width,
            info.height,
            fps,
            config.bitrate_bps,
            config.all_idr,
            Forwarder {
                tx: tx.clone(),
                clock: clock.clone(),
                parameter_sets: ParameterSetCache::new(),
                handoff_drops: 0,
            },
        )
        .map_err(|e| e.to_string())?,
    );
    eprintln!(
        "[sardp-mac] VideoToolbox {}x{} @{fps}fps {} bps; hardware encoder: {}",
        info.width,
        info.height,
        config.bitrate_bps,
        // Asked for hardware; this is what the session says it actually got.
        encoder.uses_hardware()
    );

    let session = Session::start(
        fps,
        PixelFormat::Nv12,
        false, // DR-004: the client draws the cursor.
        15_000,
        false,
        CaptureSink {
            encoder: encoder.clone(),
            tx,
            clock: clock.clone(),
            force_idr,
            submitted: 0,
            no_image: 0,
            refused: 0,
        },
    )
    .map_err(|e| e.to_string())?;

    let result = pump(ctx, &rx, info, config.acquire_timeout);

    // Order matters: stop the capture first so nothing new can be
    // submitted, then flush what the encoder is still holding. Dropping
    // `encoder` afterwards invalidates the session.
    drop(session);
    encoder.complete();
    let stats = encoder.stats();
    eprintln!(
        "[sardp-mac] encoder: inputs={} outputs={} dropped_by_encoder={} callback_errors={} \
         dropped_by_consumer={}",
        stats.inputs,
        stats.outputs,
        stats.dropped_by_encoder,
        stats.callback_errors,
        ctx.frames.dropped()
    );
    result
}

/// Forwards encoded frames from the callbacks to the consumer until asked
/// to stop. Readiness is reported on the first frame that made it all the
/// way through, so a caller that got a `DesktopH264Source` back knows
/// capture and encode both work.
fn pump(
    ctx: &mut WorkerContext<EncodedFrame, SourceInfo>,
    rx: &Receiver<Event>,
    info: SourceInfo,
    poll_interval: Duration,
) -> Result<(), String> {
    while !ctx.should_stop() {
        // `acquire_timeout` has no capture call to bound on macOS; it is
        // reused as how long this may wait before re-checking the stop
        // flag, which is the same job it does on Windows.
        match rx.recv_timeout(poll_interval) {
            Ok(Event::Frame(frame)) => {
                ctx.report_ready(info);
                match ctx.frames.send(frame) {
                    SendOutcome::Sent | SendOutcome::Dropped => {}
                    SendOutcome::Closed => return Ok(()),
                }
            }
            Ok(Event::Stopped(message)) => {
                return Err(format!("ScreenCaptureKit stopped the stream: {message}"));
            }
            // A completely static screen produces no frames at all; that
            // is not an error, just nothing to forward.
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err("capture callbacks are gone".to_string());
            }
        }
    }
    Ok(())
}

/// Runs on ScreenCaptureKit's sample-handler queue.
struct CaptureSink {
    encoder: Arc<Encoder>,
    tx: SyncSender<Event>,
    clock: Clock,
    force_idr: Arc<AtomicBool>,
    submitted: u64,
    /// Sample buffers carrying no new image (SCK's `idle`/`blank`): the
    /// macOS analogue of DXGI's `LastPresentTime == 0`. Never encoded --
    /// on Windows doing so produced a black first frame (3W-1-d-3).
    no_image: u64,
    refused: u64,
}

impl FrameSink for CaptureSink {
    fn on_frame(&mut self, frame: FrameRef<'_>) {
        let Some(pixel_buffer) = frame
            .pixel_buffer
            .filter(|_| matches!(frame.status, FrameStatus::Complete | FrameStatus::Started))
        else {
            self.no_image += 1;
            return;
        };
        // Stamped here, on the same clock the consumer measures with, so
        // `capture_ts` means "when this pipeline first saw the frame".
        let capture_ts = (self.clock)();
        let force_idr = self.force_idr.swap(false, Ordering::SeqCst);
        if let Err(e) = self.encoder.encode(pixel_buffer, capture_ts, force_idr) {
            self.refused += 1;
            if self.refused.is_power_of_two() {
                eprintln!("[sardp-mac] encoder refused {} frame(s): {e}", self.refused);
            }
            return;
        }
        self.submitted += 1;
        if self.submitted.is_multiple_of(600) {
            let stats = self.encoder.stats();
            eprintln!(
                "[sardp-mac] capture: submitted={} no_image={} encoder_outputs={} \
                 dropped_by_encoder={}",
                self.submitted, self.no_image, stats.outputs, stats.dropped_by_encoder
            );
        }
    }

    fn on_stopped(&mut self, message: &str) {
        let _ = self.tx.try_send(Event::Stopped(message.to_string()));
    }
}

/// Runs on a VideoToolbox thread.
struct Forwarder {
    tx: SyncSender<Event>,
    clock: Clock,
    parameter_sets: ParameterSetCache,
    handoff_drops: u64,
}

impl EncodedSink for Forwarder {
    fn on_encoded(&mut self, frame: EncodedRef<'_>) {
        let encode_done_ts = (self.clock)();
        // VideoToolbox keeps SPS/PPS out of the bitstream entirely (they
        // live in the format description), so without this every IDR would
        // go out undecodable on its own, breaking spec 2.10. Same role
        // `MF_MT_MPEG_SEQUENCE_HEADER` plays on Windows.
        if let Some(sets) = frame.parameter_sets {
            self.parameter_sets.set_fallback(sets.to_vec());
        }
        let annex_b = frame.annex_b.to_vec();
        // The encoder's own keyframe claim is an input, not the decision.
        let is_idr = h264::is_idr_access_unit(&annex_b, frame.is_sync);
        self.parameter_sets.observe(&annex_b);
        let annex_b = if is_idr {
            self.parameter_sets.complete_idr(annex_b)
        } else {
            annex_b
        };
        let event = Event::Frame(EncodedFrame {
            annex_b,
            is_idr,
            capture_ts: frame.capture_ts_us,
            encode_done_ts,
        });
        // Never block a VideoToolbox thread. The worker forwards
        // immediately, so a full channel means it is gone or wedged; a
        // frame lost here is counted separately from the DR-007 drops
        // `FrameSender` reports, because it has a different cause.
        if let Err(TrySendError::Full(_)) = self.tx.try_send(event) {
            self.handoff_drops += 1;
            if self.handoff_drops.is_power_of_two() {
                eprintln!(
                    "[sardp-mac] worker not keeping up with the encoder; dropped {} frame(s)",
                    self.handoff_drops
                );
            }
        }
    }
}
