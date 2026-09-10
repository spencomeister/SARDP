//! H.264 decoding via the `ffmpeg` CLI as a child process, mirroring
//! `encoder`'s subprocess pattern (PoC brief's recommended stack). This is
//! the client side of M4's "クライアント側デコード" requirement.
//!
//! Blocking, like `encoder::encode_single_frame_idr`; call from
//! `spawn_blocking` in async code that must stay responsive.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::timecode_frame::SyntheticFrame;

#[derive(Debug)]
pub enum DecodeError {
    Spawn(std::io::Error),
    WriteStdin(std::io::Error),
    Wait(std::io::Error),
    NonZeroExit {
        status: std::process::ExitStatus,
        stderr: Vec<u8>,
    },
    /// Decoded output wasn't an exact multiple of one `width`x`height`
    /// RGB24 frame -- ffmpeg produced something other than one frame.
    UnexpectedOutputSize {
        expected: usize,
        actual: usize,
    },
}

/// Decodes a single-frame H.264 Annex-B byte stream (as produced by
/// `encoder::encode_single_frame_idr`) back into an RGB24
/// [`SyntheticFrame`] of the given dimensions.
pub fn decode_single_frame(
    h264_bytes: &[u8],
    width: u32,
    height: u32,
) -> Result<SyntheticFrame, DecodeError> {
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "h264",
            "-i",
            "-",
            "-frames:v",
            "1",
            "-pix_fmt",
            "rgb24",
            "-f",
            "rawvideo",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(DecodeError::Spawn)?;

    let mut stdin = child.stdin.take().expect("stdin was piped");
    let input = h264_bytes.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));

    let output = child.wait_with_output().map_err(DecodeError::Wait)?;
    writer
        .join()
        .expect("stdin writer thread panicked")
        .map_err(DecodeError::WriteStdin)?;

    if !output.status.success() {
        return Err(DecodeError::NonZeroExit {
            status: output.status,
            stderr: output.stderr,
        });
    }

    let expected = (width * height * 3) as usize;
    if output.stdout.len() != expected {
        return Err(DecodeError::UnexpectedOutputSize {
            expected,
            actual: output.stdout.len(),
        });
    }

    Ok(SyntheticFrame {
        width,
        height,
        rgb: output.stdout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::{encode_single_frame_idr, ffmpeg_available};
    use crate::timecode_frame::{extract_timecode, generate_timecode_frame};

    #[test]
    fn decodes_back_to_the_expected_frame_size() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not found on PATH");
            return;
        }
        let original = generate_timecode_frame(640, 360, 1_000_000, [50, 50, 50]);
        let h264_bytes = encode_single_frame_idr(&original).expect("encode succeeds");
        let decoded = decode_single_frame(&h264_bytes, 640, 360).expect("decode succeeds");
        assert_eq!(decoded.width, 640);
        assert_eq!(decoded.height, 360);
        assert_eq!(decoded.rgb.len(), 640 * 360 * 3);
    }

    #[test]
    fn timecode_survives_encode_decode_round_trip() {
        // Answers the open question M3 flagged: does the naive
        // black/white bit-block timecode survive real H.264 compression?
        // With ultrafast/zerolatency/baseline and a big, high-contrast
        // block (8x8px, pure black/white) on a plain background, yes --
        // but this is exactly the kind of thing M6's real measurement
        // harness needs to keep verifying under harsher encoder settings.
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not found on PATH");
            return;
        }
        let timecode = 1_699_999_999_123;
        let original = generate_timecode_frame(640, 360, timecode, [40, 40, 40]);
        let h264_bytes = encode_single_frame_idr(&original).expect("encode succeeds");
        let decoded = decode_single_frame(&h264_bytes, 640, 360).expect("decode succeeds");
        assert_eq!(extract_timecode(&decoded), timecode);
    }

    #[test]
    fn wrong_dimensions_are_detected_as_a_size_mismatch() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not found on PATH");
            return;
        }
        let original = generate_timecode_frame(640, 360, 0, [0, 0, 0]);
        let h264_bytes = encode_single_frame_idr(&original).expect("encode succeeds");
        // Ask for the wrong height; the decoded byte count won't match.
        let result = decode_single_frame(&h264_bytes, 640, 480);
        assert!(matches!(
            result,
            Err(DecodeError::UnexpectedOutputSize { .. })
        ));
    }
}
