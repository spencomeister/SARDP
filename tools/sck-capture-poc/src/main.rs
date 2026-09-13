//! 3M-1-a: ScreenCaptureKit capture PoC.
//!
//! 1. Reports who this process is (path, pid, parent) -- TCC attributes a
//!    prompt to the *responsible process*, so a binary run straight from
//!    a terminal/IDE shell shows the dialog in that app's name. Run it via
//!    `make-app.sh` (an `.app` bundle launched with `open`) to be its own
//!    responsible process.
//! 2. Checks Screen Recording permission (read-only), optionally requests
//!    it (system dialog), and then waits/polls in a way a user can see
//!    ("権限待ち"), re-trying capture once the grant becomes visible.
//! 3. Captures N frames of the main display through the Swift shim and
//!    writes them as sequential BMPs plus a `frames.log` with the SCK
//!    per-frame metadata (status, dirty rects, display time).

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sck_capture_poc::shim::{
    self, DirtyRect, FrameRef, FrameSink, FrameStatus, PixelFormat, Session, StartError,
};

struct Args {
    frames: usize,
    fps: u32,
    out: Option<PathBuf>,
    request: bool,
    wait_secs: u64,
    cursor: bool,
    start_timeout_ms: u32,
    /// Skip the CG preflight/request/wait and go straight to SCK, so SCK's
    /// own permission path (SCShareableContent) is what prompts/fails.
    force_sck: bool,
}

fn parse_args() -> Args {
    let mut a = Args {
        frames: 10,
        fps: 10,
        out: None,
        request: true,
        wait_secs: 180,
        cursor: false,
        start_timeout_ms: 15_000,
        force_sck: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--frames" => a.frames = it.next().expect("--frames N").parse().expect("frames"),
            "--fps" => a.fps = it.next().expect("--fps N").parse().expect("fps"),
            "--out" => a.out = Some(PathBuf::from(it.next().expect("--out DIR"))),
            "--no-request" => a.request = false,
            "--wait-secs" => a.wait_secs = it.next().expect("--wait-secs N").parse().expect("secs"),
            "--cursor" => a.cursor = true,
            "--force-sck" => a.force_sck = true,
            "--start-timeout-ms" => {
                a.start_timeout_ms = it
                    .next()
                    .expect("--start-timeout-ms N")
                    .parse()
                    .expect("ms")
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: sck-capture-poc [--frames N] [--fps N] [--out DIR] [--no-request] \
                     [--wait-secs N] [--cursor] [--start-timeout-ms N]"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }
    a
}

fn now_str() -> String {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    format!("{}.{:03}", t.as_secs(), t.subsec_millis())
}

fn log(msg: &str) {
    println!("[{}] {msg}", now_str());
    let _ = std::io::stdout().flush();
}

/// Frame as copied out of the callback, for the main thread to write.
struct Captured {
    seq: u64,
    status: FrameStatus,
    display_time_ns: u64,
    recv_at: Instant,
    width: u32,
    height: u32,
    stride: u32,
    dirty: Vec<DirtyRect>,
    content_scale: f64,
    scale_factor: f64,
    /// BGRA rows (stride-packed), only for frames with a new image.
    image: Option<Vec<u8>>,
}

struct ChannelSink {
    tx: mpsc::SyncSender<Captured>,
    seq: u64,
    want_images: usize,
    saved: usize,
}

impl FrameSink for ChannelSink {
    fn on_frame(&mut self, f: FrameRef<'_>) {
        self.seq += 1;
        let has_new_image = matches!(f.status, FrameStatus::Complete | FrameStatus::Started);
        let image = if has_new_image && self.saved < self.want_images {
            self.saved += 1;
            f.bgra.map(|b| b.to_vec())
        } else {
            None
        };
        let c = Captured {
            seq: self.seq,
            status: f.status,
            display_time_ns: f.display_time_ns,
            recv_at: Instant::now(),
            width: f.width,
            height: f.height,
            stride: f.stride,
            dirty: f.dirty.to_vec(),
            content_scale: f.content_scale,
            scale_factor: f.scale_factor,
            image,
        };
        // Never block SCK's queue: if the writer is behind, drop (the
        // same source-side-drop rule as DR-007).
        let _ = self.tx.try_send(c);
    }
    fn on_stopped(&mut self, message: &str) {
        eprintln!("[sck] stream stopped with error: {message}");
    }
}

fn write_bmp(
    path: &PathBuf,
    width: u32,
    height: u32,
    stride: u32,
    bgra: &[u8],
) -> std::io::Result<()> {
    let row_bytes = (width as usize * 3 + 3) & !3;
    let image_size = row_bytes * height as usize;
    let file_size = 14 + 40 + image_size;
    let mut w = BufWriter::new(File::create(path)?);
    w.write_all(b"BM")?;
    w.write_all(&(file_size as u32).to_le_bytes())?;
    w.write_all(&0u32.to_le_bytes())?;
    w.write_all(&54u32.to_le_bytes())?;
    w.write_all(&40u32.to_le_bytes())?;
    w.write_all(&(width as i32).to_le_bytes())?;
    w.write_all(&(height as i32).to_le_bytes())?; // positive = bottom-up
    w.write_all(&1u16.to_le_bytes())?;
    w.write_all(&24u16.to_le_bytes())?;
    w.write_all(&0u32.to_le_bytes())?;
    w.write_all(&(image_size as u32).to_le_bytes())?;
    w.write_all(&2835i32.to_le_bytes())?;
    w.write_all(&2835i32.to_le_bytes())?;
    w.write_all(&0u32.to_le_bytes())?;
    w.write_all(&0u32.to_le_bytes())?;
    let pad = vec![0u8; row_bytes - width as usize * 3];
    let mut row = Vec::with_capacity(row_bytes);
    for y in (0..height as usize).rev() {
        row.clear();
        let src = &bgra[y * stride as usize..y * stride as usize + width as usize * 4];
        for px in src.as_chunks::<4>().0 {
            row.extend_from_slice(&px[..3]);
        }
        row.extend_from_slice(&pad);
        w.write_all(&row)?;
    }
    w.flush()
}

fn report_identity() {
    let exe = std::env::current_exe().unwrap_or_default();
    let ppid = unsafe { libc_getppid() };
    log(&format!(
        "process: pid={} ppid={} exe={}",
        std::process::id(),
        ppid,
        exe.display()
    ));
    // Bundle or bare binary? TCC keys the grant on the code signature
    // (identifier + cdhash for ad-hoc signing).
    let in_bundle = exe
        .ancestors()
        .any(|p| p.extension().is_some_and(|e| e == "app"));
    log(&format!("running inside an .app bundle: {in_bundle}"));
}

unsafe extern "C" {
    #[link_name = "getppid"]
    fn libc_getppid() -> i32;
}

fn main() {
    let args = parse_args();
    report_identity();

    if let Some(d) = shim::main_display_info() {
        log(&format!(
            "main display: id={} points={}x{} pixels={}x{} origin=({}, {}) refresh={}Hz",
            d.display_id,
            d.points_w,
            d.points_h,
            d.pixels_w,
            d.pixels_h,
            d.origin_x,
            d.origin_y,
            d.refresh_hz
        ));
    }

    // --- TCC: Screen Recording -------------------------------------------
    let mut granted = shim::preflight_screen_capture_access();
    log(&format!("CGPreflightScreenCaptureAccess = {granted}"));
    if args.force_sck {
        log("--force-sck: skipping CGRequest/wait; letting ScreenCaptureKit itself prompt or fail");
        granted = true;
    }
    if !granted && args.request {
        let r = shim::request_screen_capture_access();
        log(&format!(
            "CGRequestScreenCaptureAccess returned {r} (a first request shows the system dialog and \
             returns false; the grant is made in System Settings > Privacy & Security > Screen Recording)"
        ));
        granted = r;
    }

    // Wait, visibly, for the grant to become observable -- and record
    // whether a running process ever sees it (or must be relaunched).
    let deadline = Instant::now() + Duration::from_secs(args.wait_secs);
    let mut polls = 0u32;
    while !granted && Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(2));
        polls += 1;
        granted = shim::preflight_screen_capture_access();
        if polls.is_multiple_of(5) || granted {
            log(&format!(
                "waiting for screen recording permission... preflight={granted} ({}s left)",
                deadline.saturating_duration_since(Instant::now()).as_secs()
            ));
        }
    }
    if !granted {
        log("giving up: screen recording permission not granted within the wait window");
        std::process::exit(3);
    }

    // --- capture -----------------------------------------------------------
    let out = args.out.clone().unwrap_or_else(|| {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("captures")
            .join(format!("session_{ts}"))
    });
    fs::create_dir_all(&out).expect("create output dir");
    log(&format!("output dir: {}", out.display()));
    let mut frame_log = BufWriter::new(File::create(out.join("frames.log")).expect("frames.log"));

    let (tx, rx) = mpsc::sync_channel::<Captured>(8);
    let sink = ChannelSink {
        tx,
        seq: 0,
        want_images: args.frames,
        saved: 0,
    };

    let start_at = Instant::now();
    // Retry loop: even with preflight=true, the first SCK start after a
    // fresh grant has been seen to fail until relaunch; make that visible
    // rather than crashing.
    let session = match Session::start(
        args.fps,
        PixelFormat::Bgra,
        args.cursor,
        args.start_timeout_ms,
        args.force_sck,
        sink,
    ) {
        Ok(s) => s,
        Err(StartError::NoPermission(m)) => {
            log(&format!("SCK start refused: {m}"));
            std::process::exit(3);
        }
        Err(e) => {
            log(&format!("SCK start failed: {e}"));
            std::process::exit(4);
        }
    };
    log(&format!("SCK stream started in {:?}", start_at.elapsed()));

    let mut saved = 0usize;
    let mut total = 0u64;
    let mut by_status = std::collections::BTreeMap::<String, u64>::new();
    let mut first_frame_at: Option<Instant> = None;
    let mut last_display_time: Option<u64> = None;
    let overall_deadline = Instant::now() + Duration::from_secs(60);
    while saved < args.frames && Instant::now() < overall_deadline {
        let c = match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(c) => c,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                log(
                    "no frame from SCK in 5s (nothing changing on screen? move the mouse / open a window)",
                );
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        total += 1;
        *by_status.entry(format!("{:?}", c.status)).or_default() += 1;
        if first_frame_at.is_none() {
            first_frame_at = Some(c.recv_at);
            log(&format!(
                "first sample buffer {:?} after start: status={:?} {}x{} stride={} content_scale={} scale_factor={}",
                c.recv_at.duration_since(start_at),
                c.status,
                c.width,
                c.height,
                c.stride,
                c.content_scale,
                c.scale_factor
            ));
        }
        let delta_ms = last_display_time
            .map(|p| (c.display_time_ns as f64 - p as f64) / 1e6)
            .unwrap_or(0.0);
        last_display_time = Some(c.display_time_ns);
        let dirty_area: f64 = c.dirty.iter().map(|r| r.w * r.h).sum();
        let dirty_desc: Vec<String> = c
            .dirty
            .iter()
            .take(6)
            .map(|r| format!("({:.0},{:.0} {:.0}x{:.0})", r.x, r.y, r.w, r.h))
            .collect();
        let line = format!(
            "seq={} status={:?} display_time_ns={} delta_ms={:.1} size={}x{} dirty_rects={} dirty_area_pts={:.0} {}{}",
            c.seq,
            c.status,
            c.display_time_ns,
            delta_ms,
            c.width,
            c.height,
            c.dirty.len(),
            dirty_area,
            dirty_desc.join(" "),
            if c.dirty.len() > 6 { " ..." } else { "" }
        );
        writeln!(frame_log, "{line}").unwrap();
        if let Some(img) = &c.image {
            let path = out.join(format!("frame_{saved:04}.bmp"));
            let t = Instant::now();
            write_bmp(&path, c.width, c.height, c.stride, img).expect("write bmp");
            saved += 1;
            log(&format!(
                "{line} -> {} ({:?})",
                path.file_name().unwrap().to_string_lossy(),
                t.elapsed()
            ));
        } else if total <= 20 || total.is_multiple_of(50) {
            log(&line);
        }
    }
    frame_log.flush().unwrap();
    log(&format!(
        "done: saved {saved} image(s) out of {total} sample buffers; by status: {by_status:?}"
    ));
    let t = Instant::now();
    drop(session);
    log(&format!("SCK stream stopped in {:?}", t.elapsed()));
}
