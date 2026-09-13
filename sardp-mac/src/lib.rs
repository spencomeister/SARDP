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
//! Client side and input injection (`CGEvent`, 3M-1-c) are still to come.

pub mod desktop_h264;

pub use desktop_h264::{DesktopH264Source, MacCaptureError};
// Shared with the other platforms; re-exported so callers can name them
// without depending on the core crate directly, as `sardp-win` does.
pub use sardp::frame_source::{Clock, DesktopH264Config, EncodedFrame, SourceError, SourceInfo};
