//! macOS OS integration for SARDP (Stage 3 roadmap, 3M-1).
//!
//! The mirror of `sardp-win`. The core `sardp` crate forbids `unsafe`, and
//! so does this one: everything of that nature -- the C ABI exported by
//! the Swift shim over ScreenCaptureKit and VideoToolbox -- lives in
//! `sck-capture-poc` (`tools/sck-capture-poc`, the same role
//! `dxgi-capture-poc` plays on Windows). `sardp-server` only ever sees
//! [`DesktopH264Source`] and plain owned byte buffers.
//!
//! What is *not* macOS-specific is in the core crate and only used from
//! here: the frame/config types and the worker-thread + source-side-drop
//! pattern (`sardp::frame_source`), and the H.264 NAL unit handling that
//! keeps IDRs self-contained despite unreliable encoder metadata
//! (`sardp::h264`).
//!
//! Server side ([`desktop_h264`], 3M-1-b): the capture/encode pipeline
//! validated standalone in `tools/sck-capture-poc` (3M-1-a/b) --
//! ScreenCaptureKit delivering NV12 `CVPixelBuffer`s straight into a
//! VideoToolbox H.264 compression session, with the encoded access units'
//! Annex-B bytes handed to the caller.
//!
//! Input injection ([`inject`], 3M-1-c): spec 2.12 messages -> `CGEvent`
//! on a dedicated thread, with the HID usage <-> macOS virtual key table
//! in [`keymap`]. Gated by Accessibility, which is a TCC permission
//! independent of the Screen Recording one capture needs.
//!
//! Client side ([`display`], 3M-1-e): a persistent VideoToolbox H.264
//! decoder behind an on-screen `NSWindow`, the same shape as
//! `sardp_win::display::H264DisplayWindow`.

pub mod desktop_h264;
pub mod display;
pub mod inject;
pub mod keymap;

pub use desktop_h264::{DesktopH264Source, MacCaptureError};
pub use display::{DisplayConfig, FrameTiming, H264DisplayWindow, MacDisplayError, SubmittedFrame};
pub use inject::{
    InjectCommand, InjectorClosed, InjectorConfig, InputInjector, InputState, Post,
    is_accessibility_trusted, request_accessibility_trust,
};
/// Services AppKit's event queue for the life of the process; **must be
/// called on the process's real main thread**, before the first
/// [`H264DisplayWindow`] is created from any other thread (see
/// `sck_capture_poc::main_thread`'s doc for why this exists at all --
/// `NSWindow`'s main-thread requirement is enforced with an uncaught
/// exception Rust cannot recover from, not a `Result`). `sardp-cli`'s
/// `main`, on macOS with `--display window`, restructures itself around
/// this: its real thread 0 runs this loop while the tokio runtime (and
/// with it `display`'s worker thread) runs on a spawned thread instead.
pub use sck_capture_poc::main_thread::run_main_thread_loop;
// Shared with the other platforms; re-exported so callers can name them
// without depending on the core crate directly, as `sardp-win` does.
pub use sardp::frame_source::{Clock, DesktopH264Config, EncodedFrame, SourceError, SourceInfo};
