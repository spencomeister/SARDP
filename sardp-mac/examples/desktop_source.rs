//! End-to-end check of [`DesktopH264Source`]: the real thing `sardp-server`
//! will hold, driven from a tokio runtime the way the server does.
//!
//! Verifies what a unit test cannot: that readiness really means frames
//! flow, that `request_idr` produces a *self-contained* IDR mid-stream
//! (spec 2.10), and that the pipeline survives being restarted --
//! `--cycles N` runs the whole lifecycle N times and reports resident
//! memory, thread count and open files after each, so a source that
//! accumulated anything per restart would show a monotonic climb.
//!
//! What it does *not* prove: that each individual teardown call is load
//! bearing. Skipping `VTCompressionSessionInvalidate` produced no change
//! in any of these numbers on macOS 26.6.2 / M4, because the encode runs
//! out of process in `VTEncoderXPCService` (KNOWN_ISSUES #23).
//!
//! ```text
//! tools/sck-capture-poc/make-app.sh --package sardp-mac \
//!     --example desktop_source -- --frames 90 --out /tmp/source.h264
//! ```
//! Run it through `make-app.sh`: a bare binary is attributed to the
//! terminal for the Screen Recording grant (see that crate's README).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sardp::clock::now_us;
use sardp::h264;
use sardp_mac::{DesktopH264Config, DesktopH264Source};

#[tokio::main]
async fn main() {
    let mut frames_wanted = 90usize;
    let mut out_path = String::from("source.h264");
    let mut idr_at = 45usize;
    let mut cycles = 1usize;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--frames" => frames_wanted = args.next().unwrap().parse().unwrap(),
            "--out" => out_path = args.next().unwrap(),
            "--idr-at" => idr_at = args.next().unwrap().parse().unwrap(),
            "--cycles" => cycles = args.next().unwrap().parse().unwrap(),
            other => panic!("unknown argument: {other}"),
        }
    }

    if cycles > 1 {
        // Leak check: the file is written by the last cycle only.
        for cycle in 1..=cycles {
            let (frames, idrs) = run_once(frames_wanted, idr_at, None).await;
            println!(
                "cycle {cycle}/{cycles}: {frames} frames, {idrs} IDR(s); rss={} KiB threads={} open={}",
                rss_kib(),
                threads(),
                open_files()
            );
        }
        return;
    }

    run_once(frames_wanted, idr_at, Some(&out_path)).await;
}

/// One full lifecycle: start, pull `frames_wanted` frames (asking for an
/// IDR at `idr_at`), drop. Returns (frames, IDRs).
async fn run_once(frames_wanted: usize, idr_at: usize, out_path: Option<&str>) -> (usize, usize) {
    let config = DesktopH264Config::default();
    let started = Instant::now();
    let mut source = DesktopH264Source::start(config, Arc::new(now_us)).expect("start source");
    let info = source.info();
    println!(
        "ready in {:?}: {}x{} @{}fps origin=({}, {})",
        started.elapsed(),
        info.width,
        info.height,
        info.fps,
        info.origin_x,
        info.origin_y
    );

    let mut out = out_path.map(|p| BufWriter::new(File::create(p).expect("create output")));
    let mut received = 0usize;
    let mut idrs = 0usize;
    let mut not_self_contained = 0usize;
    let mut bytes = 0usize;
    let mut latencies: Vec<u64> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(60);

    while received < frames_wanted && Instant::now() < deadline {
        let Ok(Some(frame)) =
            tokio::time::timeout(Duration::from_secs(5), source.next_frame()).await
        else {
            println!("no frame in 5s (static screen?) or the source stopped");
            continue;
        };
        received += 1;
        bytes += frame.annex_b.len();
        latencies.push(frame.encode_done_ts.saturating_sub(frame.capture_ts));
        if frame.is_idr {
            idrs += 1;
            if !h264::is_self_contained_idr(&frame.annex_b) {
                not_self_contained += 1;
            }
        }
        if received <= 3 || received == idr_at + 1 {
            println!(
                "frame {received}: {} bytes idr={} nal_types={:?} capture->encode={}us",
                frame.annex_b.len(),
                frame.is_idr,
                h264::nal_unit_types(&frame.annex_b),
                frame.encode_done_ts.saturating_sub(frame.capture_ts),
            );
        }
        if let Some(out) = out.as_mut() {
            out.write_all(&frame.annex_b).expect("write");
        }
        if received == idr_at {
            println!("requesting an IDR");
            source.request_idr();
        }
    }
    if let Some(out) = out.as_mut() {
        out.flush().expect("flush");
    }

    latencies.sort_unstable();
    let at = |p: usize| latencies[(latencies.len().saturating_sub(1)) * p / 100];
    println!(
        "received {received} frames ({bytes} bytes) -> {}; \
         capture->encode p50={}us p95={}us",
        out_path.unwrap_or("(discarded)"),
        at(50),
        at(95)
    );
    println!("IDRs: {idrs} (not self-contained: {not_self_contained})");

    let stop = Instant::now();
    drop(source);
    println!(
        "source dropped (sessions torn down) in {:?}",
        stop.elapsed()
    );

    assert!(received > 0, "no frames");
    assert!(
        idrs >= 2,
        "expected the initial IDR and the requested one, got {idrs}"
    );
    assert_eq!(
        not_self_contained, 0,
        "every IDR must carry SPS+PPS (spec 2.10)"
    );
    (received, idrs)
}

/// Resident set size of this process, via `ps` -- reading it in-process
/// would need `mach_task_basic_info`, and this crate forbids `unsafe`.
fn rss_kib() -> u64 {
    std::process::Command::new("/bin/ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Open file descriptors -- a finer-grained signal than RSS for an OS
/// session that was dropped without being torn down.
fn open_files() -> usize {
    std::process::Command::new("/usr/sbin/lsof")
        .args(["-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.lines().count().saturating_sub(1))
        .unwrap_or(0)
}

/// Live thread count: ScreenCaptureKit's delivery queue and the shim's
/// own threads are in-process, so an unstopped SCStream would show here.
fn threads() -> usize {
    std::process::Command::new("/bin/ps")
        .args(["-M", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.lines().count().saturating_sub(2))
        .unwrap_or(0)
}
