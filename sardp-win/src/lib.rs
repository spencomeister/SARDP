//! Windows OS integration for SARDP (Stage 3 roadmap, 3W-1-d).
//!
//! The core `sardp` crate forbids `unsafe`, but DXGI Desktop Duplication,
//! Direct3D 11 and Media Foundation are COM/FFI and inherently `unsafe`.
//! Everything of that nature lives here, behind a safe API; `sardp-server`
//! only ever sees [`DesktopH264Source`] and plain owned byte buffers.
//!
//! The capture/encode pipeline itself is the one validated standalone in
//! `tools/dxgi-capture-poc` (3W-1-a/b): DXGI `AcquireNextFrame` ->
//! GPU-side BGRA->NV12 via `ID3D11VideoProcessor` -> hardware H.264
//! encoder MFT driven directly through `IMFTransform`. The difference is
//! only in what happens to the encoded samples: instead of muxing them
//! into an mp4 file, their Annex-B bytes are handed to the caller.

pub mod desktop_h264;

pub use desktop_h264::{
    Clock, DesktopH264Config, DesktopH264Source, EncodedFrame, SourceInfo, WinCaptureError,
};
