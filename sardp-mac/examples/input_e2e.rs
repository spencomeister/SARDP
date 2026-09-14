//! End-to-end check of [`InputInjector`] (3M-1-c): spec 2.12 commands in,
//! real `CGEvent`s out, read back from a session event tap.
//!
//! Posting an event tells you nothing about whether it arrived -- there is
//! no error path for "the system dropped it" -- so this does not trust
//! `CGEvent.post`. It observes the events coming back out of the window
//! server and checks the things that are easy to get wrong on macOS:
//! that a click carries the right click count, that a move with a button
//! held arrives as a *drag*, that a modifier is present in the flags of
//! the key that follows it, and that pixel coordinates landed on the right
//! point.
//!
//! **It is safe to run on a machine someone is using.** Everything except
//! the cursor moves is injected while the tap is in filtering mode, which
//! discards events carrying this process's own tag before any application
//! sees them. Real input is never touched. The cursor moves are let
//! through on purpose -- that is the only way to verify the pixel-to-point
//! conversion against the window server -- and the original position is
//! restored at the end.
//!
//! ```text
//! tools/sck-capture-poc/make-app.sh --package sardp-mac --example input_e2e
//! ```
//! Run it through `make-app.sh`: Accessibility is granted per code
//! identity, and a bare binary is attributed to the terminal.

use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;

use sardp_mac::inject::{
    INJECTED_USER_DATA, InjectCommand, InjectorConfig, InputInjector, button, cursor_position,
    flag, is_accessibility_trusted, request_accessibility_trust,
};
use sck_capture_poc::inject::{EventTap, TapSink, TappedEvent};
use sck_capture_poc::shim;

/// `CGEventType` raw values, named where this file uses them.
mod event_type {
    pub const LEFT_MOUSE_DOWN: u32 = 1;
    pub const LEFT_MOUSE_UP: u32 = 2;
    pub const MOUSE_MOVED: u32 = 5;
    pub const LEFT_MOUSE_DRAGGED: u32 = 6;
    pub const KEY_DOWN: u32 = 10;
    pub const KEY_UP: u32 = 11;
    pub const SCROLL_WHEEL: u32 = 22;

    pub fn name(t: u32) -> &'static str {
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
}

#[derive(Clone, Default)]
struct Collector(Arc<Mutex<Vec<TappedEvent>>>);

impl TapSink for Collector {
    fn on_event(&mut self, event: TappedEvent) {
        self.0.lock().expect("collector").push(event);
    }
}

impl Collector {
    /// Only the events this process injected; the user's own input is
    /// ignored so someone typing during the run cannot fail it.
    fn ours(&self) -> Vec<TappedEvent> {
        self.0
            .lock()
            .expect("collector")
            .iter()
            .filter(|e| e.user_data == INJECTED_USER_DATA)
            .copied()
            .collect()
    }

    fn foreign(&self) -> usize {
        self.0
            .lock()
            .expect("collector")
            .iter()
            .filter(|e| e.user_data != INJECTED_USER_DATA)
            .count()
    }

    fn clear(&self) {
        self.0.lock().expect("collector").clear();
    }
}

/// Gives the window server time to deliver what was just posted.
fn settle() {
    sleep(Duration::from_millis(120));
}

fn dump(label: &str, events: &[TappedEvent]) {
    println!("  {label}: {} event(s)", events.len());
    for e in events {
        println!(
            "    {:<18} key/button={:<3} click={} flags={:#010x} at ({:.1}, {:.1})",
            event_type::name(e.event_type),
            e.key_code,
            e.click_state,
            e.flags,
            e.x,
            e.y
        );
    }
}

fn main() {
    if !is_accessibility_trusted() {
        // Unlike Screen Recording (KNOWN_ISSUES #20), this really does put
        // a dialog up.
        let granted = request_accessibility_trust();
        eprintln!(
            "accessibility permission is not granted (request returned {granted}).\n\
             Approve this app in System Settings > Privacy & Security > Accessibility, \
             then run again. (Whether a *running* process ever sees the grant appear \
             has not been measured for Accessibility the way it was for Screen \
             Recording; starting again is known to work.)"
        );
        std::process::exit(3);
    }

    let display = shim::main_display_info().expect("main display");
    let scale = f64::from(display.pixels_w) / display.points_w;
    let config = InjectorConfig {
        output_origin: (display.origin_x, display.origin_y),
        scale,
        ..InjectorConfig::default()
    };
    println!(
        "display {}x{}px = {}x{}pt at ({}, {}); scale {scale}",
        display.pixels_w,
        display.pixels_h,
        display.points_w,
        display.points_h,
        display.origin_x,
        display.origin_y
    );

    let original = cursor_position().expect("cursor position");
    println!("cursor starts at {original:?}");
    let mut failures: Vec<String> = Vec::new();

    // --- phase 1: cursor moves, observed *and* let through ---------------
    //
    // Moves are the one thing worth letting reach the window server: the
    // cursor actually moving is how the pixel-to-point conversion gets
    // checked against something outside this program.
    let collector = Collector::default();
    {
        let tap = EventTap::start(collector.clone()).expect("start event tap");
        let injector = InputInjector::start(config).expect("start injector");

        // A pixel in the middle of the captured output. With scale 2 this
        // is only half as far in points, which is exactly the conversion
        // under test.
        let target_px = (display.pixels_w as i32 / 2, display.pixels_h as i32 / 2);
        let expected_pt = (
            display.origin_x + f64::from(target_px.0) / scale,
            display.origin_y + f64::from(target_px.1) / scale,
        );
        injector
            .inject(InjectCommand::MouseMove {
                x: target_px.0,
                y: target_px.1,
            })
            .expect("queue move");
        settle();

        let seen = cursor_position().expect("cursor position");
        println!(
            "\nmove to pixel {target_px:?} -> expected point ({:.1}, {:.1}); \
             window server reports ({:.1}, {:.1})",
            expected_pt.0, expected_pt.1, seen.0, seen.1
        );
        if (seen.0 - expected_pt.0).abs() > 1.0 || (seen.1 - expected_pt.1).abs() > 1.0 {
            failures.push(format!(
                "cursor landed at {seen:?}, expected {expected_pt:?}"
            ));
        }
        dump("tapped", &collector.ours());
        if !collector
            .ours()
            .iter()
            .any(|e| e.event_type == event_type::MOUSE_MOVED)
        {
            failures.push("no mouseMoved reached the session tap".into());
        }
        drop(injector);
        drop(tap);
    }

    // --- phase 2: everything else, injected and swallowed ----------------
    collector.clear();
    let tap = EventTap::start_filtering(collector.clone()).expect("start filtering tap");
    let injector = InputInjector::start(config).expect("start injector");
    println!("\nfiltering tap active: injected events are observed, then discarded");

    // Position first, as a client does: `MouseButton` reaches the sink as
    // button+down only, so where a click lands is whatever the last
    // `MouseMove` set. Without this the clicks below would be posted at
    // the injector's starting position, the top-left of the output.
    let click_px = (display.pixels_w as i32 / 2, display.pixels_h as i32 / 2);
    let click_pt = (
        display.origin_x + f64::from(click_px.0) / scale,
        display.origin_y + f64::from(click_px.1) / scale,
    );
    injector
        .inject(InjectCommand::MouseMove {
            x: click_px.0,
            y: click_px.1,
        })
        .unwrap();
    settle();
    collector.clear();

    // 2a. A click pair.
    injector
        .inject(InjectCommand::MouseButton {
            button: button::LEFT,
            down: true,
        })
        .unwrap();
    injector
        .inject(InjectCommand::MouseButton {
            button: button::LEFT,
            down: false,
        })
        .unwrap();
    settle();
    let click = collector.ours();
    dump("\nsingle click", &click);
    check(
        &mut failures,
        "a click arrives as a down/up pair with click count 1",
        click
            .iter()
            .any(|e| e.event_type == event_type::LEFT_MOUSE_DOWN && e.click_state == 1)
            && click
                .iter()
                .any(|e| e.event_type == event_type::LEFT_MOUSE_UP),
    );
    check(
        &mut failures,
        "and lands where the preceding move put the pointer",
        !click.is_empty()
            && click
                .iter()
                .all(|e| (e.x - click_pt.0).abs() < 1.0 && (e.y - click_pt.1).abs() < 1.0),
    );
    collector.clear();

    // 2b. A double click: the second press must say so, or apps see two
    // unrelated clicks.
    injector
        .inject(InjectCommand::MouseButton {
            button: button::LEFT,
            down: true,
        })
        .unwrap();
    injector
        .inject(InjectCommand::MouseButton {
            button: button::LEFT,
            down: false,
        })
        .unwrap();
    settle();
    let double = collector.ours();
    dump("\ndouble click", &double);
    check(
        &mut failures,
        "the second press carries click count 2",
        double
            .iter()
            .any(|e| e.event_type == event_type::LEFT_MOUSE_DOWN && e.click_state == 2),
    );
    collector.clear();

    // 2c. A drag: a move with the button held must not arrive as mouseMoved.
    sleep(Duration::from_millis(700)); // past the double-click interval
    injector
        .inject(InjectCommand::MouseMove {
            x: click_px.0,
            y: click_px.1,
        })
        .unwrap();
    settle();
    collector.clear();
    injector
        .inject(InjectCommand::MouseButton {
            button: button::LEFT,
            down: true,
        })
        .unwrap();
    for step in 1..=3 {
        injector
            .inject(InjectCommand::MouseMove {
                x: display.pixels_w as i32 / 2 + step * 20,
                y: display.pixels_h as i32 / 2 + step * 20,
            })
            .unwrap();
    }
    injector
        .inject(InjectCommand::MouseButton {
            button: button::LEFT,
            down: false,
        })
        .unwrap();
    settle();
    let drag = collector.ours();
    dump("\ndrag", &drag);
    check(
        &mut failures,
        "moves with the button held arrive as leftMouseDragged",
        drag.iter()
            .filter(|e| e.event_type == event_type::LEFT_MOUSE_DRAGGED)
            .count()
            == 3,
    );
    check(
        &mut failures,
        "and none of them as a plain mouseMoved",
        !drag.iter().any(|e| e.event_type == event_type::MOUSE_MOVED),
    );
    collector.clear();

    // 2d. Command-A: the modifier has to be in the flags of the `a`, not
    // just an earlier event, or the shortcut does not fire.
    injector
        .inject(InjectCommand::Key {
            hid_usage: 0xE3,
            down: true,
        })
        .unwrap(); // Left Command
    injector
        .inject(InjectCommand::Key {
            hid_usage: 0x04,
            down: true,
        })
        .unwrap(); // a
    injector
        .inject(InjectCommand::Key {
            hid_usage: 0x04,
            down: false,
        })
        .unwrap();
    injector
        .inject(InjectCommand::Key {
            hid_usage: 0xE3,
            down: false,
        })
        .unwrap();
    settle();
    let combo = collector.ours();
    dump("\nCommand-A", &combo);
    let a_down = combo
        .iter()
        .find(|e| e.event_type == event_type::KEY_DOWN && e.key_code == 0x00);
    // Note the masking: the window server ORs in its own bits (0x20000000,
    // `kCGEventFlagMaskNonCoalesced`) on the way through, so the flags that
    // come back are never equal to the ones that went in.
    check(
        &mut failures,
        "the `a` key-down carries the Command flag",
        a_down.is_some_and(|e| e.flags & flag::COMMAND != 0),
    );
    check(
        &mut failures,
        "and the left-Command device bit, so left/right are distinguishable",
        a_down.is_some_and(|e| e.flags & flag::DEVICE_LCMD != 0),
    );
    check(
        &mut failures,
        "a key-up follows the key-down",
        combo
            .iter()
            .any(|e| e.event_type == event_type::KEY_UP && e.key_code == 0x00),
    );
    println!(
        "  (modifier keys arrived as: {:?})",
        combo
            .iter()
            .filter(|e| e.key_code == 0x37)
            .map(|e| event_type::name(e.event_type))
            .collect::<Vec<_>>()
    );
    collector.clear();

    // 2e. Committed text (spec 2.12 `TextInput`), including a character
    // outside the BMP and one that needs a combining mark.
    let text = "SARDP か\u{3099}ぎ 🍣";
    injector
        .inject(InjectCommand::Text(text.to_string()))
        .unwrap();
    settle();
    let typed = collector.ours();
    println!(
        "\ntext {text:?}: {} keyDown event(s) reached the tap",
        typed
            .iter()
            .filter(|e| e.event_type == event_type::KEY_DOWN)
            .count()
    );
    check(
        &mut failures,
        "text arrives as keyboard events with no virtual key claimed",
        typed
            .iter()
            .filter(|e| e.event_type == event_type::KEY_DOWN)
            .count()
            > 0
            && typed
                .iter()
                .filter(|e| e.event_type == event_type::KEY_DOWN)
                .all(|e| e.key_code == 0),
    );
    check(
        &mut failures,
        "and carries no modifier flags (they would turn it into shortcuts)",
        typed
            .iter()
            .filter(|e| e.event_type == event_type::KEY_DOWN)
            .all(|e| e.flags & (flag::COMMAND | flag::CONTROL | flag::ALTERNATE) == 0),
    );
    collector.clear();

    // 2f. Scroll.
    injector
        .inject(InjectCommand::Wheel { dx: 0, dy: 240 })
        .unwrap();
    injector
        .inject(InjectCommand::Wheel { dx: 0, dy: -7 })
        .unwrap();
    settle();
    let scrolled = collector.ours();
    dump("\nscroll", &scrolled);
    check(
        &mut failures,
        "both a full notch and a partial one produce a scroll",
        scrolled
            .iter()
            .filter(|e| e.event_type == event_type::SCROLL_WHEEL)
            .count()
            == 2,
    );

    // Everything this process posts is tagged, which is what lets a
    // same-machine client tell its own events from the user's -- the
    // echo-loop problem `dwExtraInfo` solves on Windows. Untagged events
    // are the user's and were passed through, not swallowed.
    println!(
        "\nevents from outside this process seen while filtering: {} (passed through untouched)",
        collector.foreign()
    );

    drop(injector);
    drop(tap);

    // Put the cursor back where the person left it.
    let restore = InputInjector::start(InjectorConfig {
        output_origin: (0.0, 0.0),
        scale: 1.0,
        ..InjectorConfig::default()
    })
    .expect("start injector");
    restore
        .inject(InjectCommand::MouseMove {
            x: original.0.round() as i32,
            y: original.1.round() as i32,
        })
        .unwrap();
    drop(restore);
    settle();
    println!("cursor restored to {:?}", cursor_position());

    println!();
    if failures.is_empty() {
        println!("all input checks passed");
    } else {
        for f in &failures {
            eprintln!("FAIL: {f}");
        }
        std::process::exit(1);
    }
}

fn check(failures: &mut Vec<String>, what: &str, ok: bool) {
    println!("  [{}] {what}", if ok { "ok" } else { "FAIL" });
    if !ok {
        failures.push(what.to_string());
    }
}
