//! H.264 encoding via the `ffmpeg` CLI as a child process (PoC brief:
//! "PoC初期段階ではffmpeg CLIを子プロセスとして呼び出す形で十分").
//!
//! This is a blocking wrapper (spawns and waits on a child process); call
//! it from a `spawn_blocking` task if used from async code that must
//! stay responsive.

use std::io::Write;
use std::process::{Command, ExitStatus, Stdio};

use crate::timecode_frame::SyntheticFrame;

#[derive(Debug)]
pub enum EncodeError {
    Spawn(std::io::Error),
    WriteStdin(std::io::Error),
    Wait(std::io::Error),
    NonZeroExit { status: ExitStatus, stderr: Vec<u8> },
    EmptyOutput,
}

/// `true` if an `ffmpeg` binary is on `PATH` and runs. Tests that need
/// `ffmpeg` should check this first and skip (rather than fail) when it's
/// absent, since it isn't guaranteed to be installed in every environment
/// this crate is built in.
pub fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Encodes a single RGB24 frame into a self-contained H.264 Annex-B
/// keyframe (SPS + PPS + IDR slice, spec 2.10's per-IDR self-containment
/// rule) using `libx264` via ffmpeg. `-x264-params repeat-headers=1`
/// makes this true even for later frames/generations, not just the first
/// encode of the process.
pub fn encode_single_frame_idr(frame: &SyntheticFrame) -> Result<Vec<u8>, EncodeError> {
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "rawvideo",
            "-pixel_format",
            "rgb24",
            "-video_size",
            &format!("{}x{}", frame.width, frame.height),
            "-i",
            "-",
            "-frames:v",
            "1",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-profile:v",
            "baseline",
            "-pix_fmt",
            "yuv420p",
            "-x264-params",
            "repeat-headers=1",
            "-f",
            "h264",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(EncodeError::Spawn)?;

    // Write stdin on a separate thread: ffmpeg can start producing stdout
    // before it has consumed all of stdin, and `wait_with_output` only
    // drains stdout/stderr concurrently for us, not stdin.
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let rgb = frame.rgb.clone();
    let writer = std::thread::spawn(move || stdin.write_all(&rgb));

    let output = child.wait_with_output().map_err(EncodeError::Wait)?;
    writer
        .join()
        .expect("stdin writer thread panicked")
        .map_err(EncodeError::WriteStdin)?;

    if !output.status.success() {
        return Err(EncodeError::NonZeroExit {
            status: output.status,
            stderr: output.stderr,
        });
    }
    if output.stdout.is_empty() {
        return Err(EncodeError::EmptyOutput);
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264;
    use crate::timecode_frame::generate_timecode_frame;

    #[test]
    fn encodes_a_self_contained_idr() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not found on PATH");
            return;
        }
        let frame = generate_timecode_frame(640, 360, 1_000_000, [30, 30, 30]);
        let h264_bytes = encode_single_frame_idr(&frame).expect("ffmpeg encode succeeds");

        assert!(!h264_bytes.is_empty());
        assert!(
            h264::is_self_contained_idr(&h264_bytes),
            "expected SPS+PPS to precede the IDR slice"
        );
    }

    #[test]
    fn different_dimensions_still_produce_a_self_contained_idr() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not found on PATH");
            return;
        }
        // libx264 requires even dimensions for 4:2:0 chroma subsampling.
        let frame = generate_timecode_frame(1024, 128, 42, [200, 0, 0]);
        let h264_bytes = encode_single_frame_idr(&frame).expect("ffmpeg encode succeeds");
        assert!(h264::is_self_contained_idr(&h264_bytes));
    }
}
