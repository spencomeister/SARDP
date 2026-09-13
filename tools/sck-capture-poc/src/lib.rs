//! The macOS OS boundary for SARDP's Stage 3 work, as a library so the
//! PoC binaries here and the `sardp-mac` crate share one copy.
//!
//! Everything inherently `unsafe` (the C ABI exported by `shim/*.swift`)
//! is behind [`shim`] (ScreenCaptureKit capture) and [`vt`] (VideoToolbox
//! H.264 encode); the core `sardp` crate keeps `unsafe_code = "forbid"`.
//! Same split as `tools/dxgi-capture-poc` + `sardp-win` on Windows.

pub mod shim;
pub mod vt;
