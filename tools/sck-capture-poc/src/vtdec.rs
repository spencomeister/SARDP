//! Safe wrapper over the C ABI exported by `shim/VtDecoder.swift`.
//!
//! Same shape as [`crate::vt::Encoder`]: one opaque handle, invalidated in
//! `Drop` so an early return or a panic still tears the VideoToolbox
//! session down (3W-1 lesson 1). Unlike the encoder, output is *pulled*:
//! [`Decoder::decode`] returns the pixel buffer synchronously (see the
//! Swift file's doc comment for why passing empty `VTDecodeFrameFlags`
//! makes that true rather than just usually true), so there is no
//! callback/sink trait here at all.

use std::ffi::{CStr, c_char, c_void};
use std::fmt;

#[derive(Debug)]
pub enum DecodeError {
    /// SPS/PPS rejected, or `decode` called before the first `configure`.
    Format(String),
    Create(String),
    /// A single `decode` call failed; the session is still usable.
    Decode(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format(m) => write!(f, "decoder format description rejected: {m}"),
            Self::Create(m) => write!(f, "VTDecompressionSessionCreate failed: {m}"),
            Self::Decode(m) => write!(f, "decode failed: {m}"),
        }
    }
}

impl std::error::Error for DecodeError {}

unsafe extern "C" {
    fn sardp_vtdec_create() -> *mut c_void;
    fn sardp_vtdec_configure(
        handle: *mut c_void,
        width: i32,
        height: i32,
        sps: *const u8,
        sps_len: usize,
        pps: *const u8,
        pps_len: usize,
        err: *mut c_char,
        err_len: usize,
    ) -> i32;
    fn sardp_vtdec_decode(
        handle: *mut c_void,
        avcc: *const u8,
        avcc_len: usize,
        pts_us: u64,
        out_pixel_buffer: *mut *mut c_void,
        err: *mut c_char,
        err_len: usize,
    ) -> i32;
    fn sardp_vtdec_is_configured(handle: *mut c_void) -> bool;
    fn sardp_vtdec_release_pixelbuffer(pixel_buffer: *mut c_void);
    fn sardp_vtdec_destroy(handle: *mut c_void);
}

fn read_err(buf: &[c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// A persistent VideoToolbox H.264 decompression session. No session
/// exists until the first successful [`Self::configure`] (VideoToolbox
/// needs a `CMFormatDescription`, built from SPS/PPS, before it will
/// create one at all).
pub struct Decoder {
    handle: *mut c_void,
}

// The handle is driven from whichever thread owns the display worker (one
// thread at a time, never concurrently -- there is no `Sync` impl).
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Self {
        Self {
            handle: unsafe { sardp_vtdec_create() },
        }
    }

    /// (Re)creates the session for `width`x`height` from an IDR's SPS/PPS
    /// (Annex-B NAL bytes, start codes and header-length prefix both
    /// excluded -- exactly `sardp::h264::NalUnit::bytes`). Tearing down a
    /// prior session, if any, happens on the Swift side before the new
    /// one is created, so there is never a moment with two live sessions.
    pub fn configure(
        &mut self,
        width: u32,
        height: u32,
        sps: &[u8],
        pps: &[u8],
    ) -> Result<(), DecodeError> {
        let mut err = [0 as c_char; 512];
        let rc = unsafe {
            sardp_vtdec_configure(
                self.handle,
                width as i32,
                height as i32,
                sps.as_ptr(),
                sps.len(),
                pps.as_ptr(),
                pps.len(),
                err.as_mut_ptr(),
                err.len(),
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            let msg = read_err(&err);
            Err(match rc {
                2 => DecodeError::Create(msg),
                _ => DecodeError::Format(msg),
            })
        }
    }

    pub fn is_configured(&self) -> bool {
        unsafe { sardp_vtdec_is_configured(self.handle) }
    }

    /// Decodes one access unit's non-parameter-set NAL units, AVCC-framed
    /// (4-byte big-endian length prefixes, no start codes -- the caller's
    /// job, `sardp::h264`). `pts_us` round-trips through the sample's
    /// presentation stamp unchanged (same microsecond clock convention as
    /// [`crate::vt::Encoder::encode`]'s `capture_ts_us`).
    ///
    /// `Ok(None)` is not an error: it means this call produced no output
    /// (should not happen for this protocol's B-frame-free streams, but
    /// nothing here assumes it can't).
    pub fn decode(
        &mut self,
        avcc: &[u8],
        pts_us: u64,
    ) -> Result<Option<PixelBufferOwned>, DecodeError> {
        let mut err = [0 as c_char; 512];
        let mut out: *mut c_void = std::ptr::null_mut();
        let rc = unsafe {
            sardp_vtdec_decode(
                self.handle,
                avcc.as_ptr(),
                avcc.len(),
                pts_us,
                &mut out,
                err.as_mut_ptr(),
                err.len(),
            )
        };
        if rc != 0 {
            return Err(DecodeError::Decode(read_err(&err)));
        }
        Ok((!out.is_null()).then_some(PixelBufferOwned { ptr: out }))
    }
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { sardp_vtdec_destroy(self.handle) };
    }
}

/// A retained `CVPixelBufferRef` this crate owns. Normally handed
/// straight to [`crate::window::DisplayWindow::present`] (which consumes
/// it, [`Self::into_raw`]); if that never happens -- an error path
/// discarding the frame, say -- `Drop` releases it instead, so nothing
/// leaks either way (3W-1 lesson 1 applies to this handle too, not just
/// to the session it came from).
pub struct PixelBufferOwned {
    ptr: *mut c_void,
}

impl PixelBufferOwned {
    /// The raw retained `CVPixelBufferRef`, for `sardp_win_present`
    /// (which takes ownership -- do not use `self` again after this).
    pub fn into_raw(self) -> *mut c_void {
        let ptr = self.ptr;
        std::mem::forget(self);
        ptr
    }
}

impl fmt::Debug for PixelBufferOwned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PixelBufferOwned({:p})", self.ptr)
    }
}

impl Drop for PixelBufferOwned {
    fn drop(&mut self) {
        unsafe { sardp_vtdec_release_pixelbuffer(self.ptr) };
    }
}
