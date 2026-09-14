//! A safety net for running the input path on a machine someone is using.
//!
//! Holds a filtering session event tap that discards every event carrying
//! this project's `kCGEventSourceUserData` tag before any application sees
//! it, and prints what it swallowed. Events without the tag -- the real
//! user's input -- always pass through untouched.
//!
//! The tag travels in the event, not in the process, so this works across
//! processes: run the guard, then run `sardp-server --capture desktop`
//! and a client with `--input-script`, and the injected clicks and
//! keystrokes are observed here and go no further. Without it, a loopback
//! run drives whatever happens to have focus.
//!
//! ```text
//! tools/sck-capture-poc/make-app.sh --package sardp-mac --example input_guard
//! ```
//! Runs until interrupted, or for `--secs N`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sardp_mac::inject::{INJECTED_USER_DATA, is_accessibility_trusted};
use sck_capture_poc::inject::{EventTap, TapSink, TappedEvent};

fn name(t: u32) -> &'static str {
    match t {
        1 => "leftMouseDown",
        2 => "leftMouseUp",
        3 => "rightMouseDown",
        4 => "rightMouseUp",
        5 => "mouseMoved",
        6 => "leftMouseDragged",
        7 => "rightMouseDragged",
        10 => "keyDown",
        11 => "keyUp",
        12 => "flagsChanged",
        22 => "scrollWheel",
        25 => "otherMouseDown",
        26 => "otherMouseUp",
        27 => "otherMouseDragged",
        _ => "other",
    }
}

#[derive(Clone, Default)]
struct Guard {
    swallowed: Arc<Mutex<u64>>,
    passed: Arc<Mutex<u64>>,
}

impl TapSink for Guard {
    fn on_event(&mut self, e: TappedEvent) {
        if e.user_data == INJECTED_USER_DATA {
            let mut n = self.swallowed.lock().expect("counter");
            *n += 1;
            println!(
                "[guard] swallowed {:<18} key/button={:<3} click={} flags={:#010x} at ({:.1}, {:.1})",
                name(e.event_type),
                e.key_code,
                e.click_state,
                e.flags,
                e.x,
                e.y
            );
        } else {
            *self.passed.lock().expect("counter") += 1;
        }
    }
}

fn main() {
    let mut secs = 0u64;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--secs" => secs = args.next().unwrap().parse().unwrap(),
            other => panic!("unknown argument: {other}"),
        }
    }

    if !is_accessibility_trusted() {
        eprintln!(
            "accessibility permission is not granted; an event tap needs it too. \
             Approve this binary in System Settings > Privacy & Security > Accessibility."
        );
        std::process::exit(3);
    }

    let guard = Guard::default();
    let tap = EventTap::start_filtering(guard.clone()).expect("start filtering tap");
    println!(
        "[guard] filtering tap active: events tagged {INJECTED_USER_DATA:#x} are discarded, \
         everything else passes through"
    );

    // `--secs 0` (the default) means run until killed.
    let deadline = (secs > 0).then(|| Instant::now() + Duration::from_secs(secs));
    while deadline.is_none_or(|d| Instant::now() < d) {
        std::thread::sleep(Duration::from_millis(200));
    }
    drop(tap);
    println!(
        "[guard] done: swallowed {} injected event(s), passed {} other event(s) through",
        guard.swallowed.lock().expect("counter"),
        guard.passed.lock().expect("counter")
    );
}
