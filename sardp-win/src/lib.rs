//! Windows OS integration for SARDP (Stage 3 roadmap, 3W-1-d).
//!
//! The core `sardp` crate forbids `unsafe`, but DXGI Desktop Duplication,
//! Direct3D 11 and Media Foundation are COM/FFI and inherently `unsafe`.
//! Everything of that nature lives here, behind a safe API; `sardp-server`
//! only ever sees [`DesktopH264Source`] and plain owned byte buffers, and
//! `sardp-client` only [`H264DisplayWindow`] and timing reports.
//!
//! What is *not* Windows-specific is in the core crate and only used from
//! here: the frame/config types and the worker-thread + source-side-drop
//! pattern (`sardp::frame_source`), and the H.264 NAL unit handling that
//! keeps IDRs self-contained despite unreliable encoder metadata
//! (`sardp::h264`). A macOS/Linux crate implements the same shape.
//!
//! Server side ([`desktop_h264`]): the capture/encode pipeline validated
//! standalone in `tools/dxgi-capture-poc` (3W-1-a/b) -- DXGI
//! `AcquireNextFrame` -> GPU-side BGRA->NV12 via `ID3D11VideoProcessor` ->
//! hardware H.264 encoder MFT driven directly through `IMFTransform` --
//! paced to the display refresh, with the encoded samples' Annex-B bytes
//! handed to the caller instead of muxed into a file.
//!
//! Client side ([`display`], 3W-1-d-3): the mirror image -- Annex-B in,
//! in-box H.264 decoder MFT with DXVA (D3D11 device manager) out to NV12
//! textures, `ID3D11VideoProcessor` NV12->BGRA into a flip-model swap
//! chain on a Win32 window. Persistent decoder, so P-frame streams work.
//! The same window reports the user's keyboard/mouse input
//! ([`WindowInput`]) for the client to forward as spec 2.12 messages.
//!
//! Input injection ([`inject`], 3W-1-d-4): spec 2.12 messages -> `SendInput`
//! on a dedicated thread, with the HID usage <-> scan code table in
//! [`keymap`].

pub mod desktop_h264;
pub mod display;
pub mod inject;
pub mod keymap;

pub use desktop_h264::{DesktopH264Source, WinCaptureError};
pub use display::{
    DisplayConfig, FrameTiming, H264DisplayWindow, SubmittedFrame, WinDisplayError, WindowInput,
};
pub use inject::{INJECTED_EXTRA_INFO, InjectCommand, InjectorConfig, InputInjector};
// Shared with the other platforms; re-exported so 3W-1-d-2-era callers
// (`sardp_win::Clock` etc.) keep working.
pub use sardp::frame_source::{Clock, DesktopH264Config, EncodedFrame, SourceError, SourceInfo};
