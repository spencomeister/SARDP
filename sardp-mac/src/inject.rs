//! Input injection via `CGEvent` (3M-1-c), driven by spec 2.12 messages.
//!
//! [`InputInjector::start`] spawns a thread that injects in submission
//! order, for the same reason the Windows one does: text goes in with a
//! configurable pause between units (KNOWN_ISSUES.md #13), which must not
//! block the server's async loop.
//!
//! What is macOS-specific, and why this is not a transliteration of
//! `sardp_win::inject`:
//!
//! - **A separate TCC gate.** Posting events needs Accessibility, which is
//!   independent of the Screen Recording grant capture needs. Unlike
//!   screen recording there *is* a prompt for it (KNOWN_ISSUES #20 vs
//!   #26), so [`InputInjector::start`] can fail with something the user
//!   can act on -- and it fails at start-up rather than on the first
//!   silently dropped event.
//! - **Modifiers are state, not keystrokes.** macOS carries the modifier
//!   state in *every* event's flags rather than inferring it from earlier
//!   key-downs, so a Command-C that arrives as three spec 2.12 messages
//!   only works if the `c` event itself says Command is held. [`InputState`]
//!   tracks the pressed modifiers and stamps the flags on everything.
//! - **A move with a button held is a drag.** Posting `mouseMoved` while
//!   the left button is down does not drag in most apps; it has to be
//!   `leftMouseDragged`. Which one to send is a function of the buttons
//!   this module is tracking.
//! - **Double clicks need a click count.** Two `leftMouseDown`s do not
//!   make a double click unless the second carries
//!   `kCGMouseEventClickState = 2`.
//! - **Points, not pixels.** Capture reports pixels (3420x2224 here) while
//!   `CGEvent` works in points (1710x1112), so coordinates are divided by
//!   the backing scale factor on the way in.
//! - **The flags that come back are not the flags that went in.** The
//!   window server ORs in its own bits (`kCGEventFlagMaskNonCoalesced`,
//!   0x20000000), so anything comparing flags has to mask rather than
//!   test for equality.
//!
//! All of that is decided in [`InputState`], which is ordinary Rust with
//! unit tests; the Swift shim only posts what it is told.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use sck_capture_poc::inject::{InjectError, Injector, MouseKind};

use crate::keymap;
use sardp::worker_handle::WorkerHandle;

pub use sck_capture_poc::inject::{
    INJECTED_USER_DATA, cursor_position, is_accessibility_trusted, request_accessibility_trust,
};

/// `MouseButton.button` numbering, mirrored from
/// `sardp::messages::mouse_button` so this module matches `sardp_win::inject`.
pub mod button {
    pub const LEFT: u8 = 1;
    pub const RIGHT: u8 = 2;
    pub const MIDDLE: u8 = 3;
    pub const X1: u8 = 4;
    pub const X2: u8 = 5;
}

/// `CGEventFlags` bits.
///
/// The first group is the documented `kCGEventFlagMask*` set. The second
/// is the undocumented-but-stable device-dependent group from IOKit's
/// `IOLLEvent.h` (`NX_DEVICELSHIFTKEYMASK` and friends), which is how
/// macOS tells left Shift from right Shift; an app that distinguishes the
/// two reads these, and a synthesised event without them looks like a
/// keyboard with only left-hand modifiers.
pub mod flag {
    pub const ALPHA_SHIFT: u64 = 0x0001_0000;
    pub const SHIFT: u64 = 0x0002_0000;
    pub const CONTROL: u64 = 0x0004_0000;
    pub const ALTERNATE: u64 = 0x0008_0000;
    pub const COMMAND: u64 = 0x0010_0000;
    pub const NUMERIC_PAD: u64 = 0x0020_0000;

    pub const DEVICE_LCTRL: u64 = 0x0000_0001;
    pub const DEVICE_LSHIFT: u64 = 0x0000_0002;
    pub const DEVICE_RSHIFT: u64 = 0x0000_0004;
    pub const DEVICE_LCMD: u64 = 0x0000_0008;
    pub const DEVICE_RCMD: u64 = 0x0000_0010;
    pub const DEVICE_LALT: u64 = 0x0000_0020;
    pub const DEVICE_RALT: u64 = 0x0000_0040;
    pub const DEVICE_RCTRL: u64 = 0x0000_2000;
}

/// `(hid_usage, group_mask, device_mask)` for the eight modifier keys, in
/// HID order 0xE0..=0xE7.
const MODIFIERS: [(u32, u64, u64); 8] = [
    (0xE0, flag::CONTROL, flag::DEVICE_LCTRL),
    (0xE1, flag::SHIFT, flag::DEVICE_LSHIFT),
    (0xE2, flag::ALTERNATE, flag::DEVICE_LALT),
    (0xE3, flag::COMMAND, flag::DEVICE_LCMD),
    (0xE4, flag::CONTROL, flag::DEVICE_RCTRL),
    (0xE5, flag::SHIFT, flag::DEVICE_RSHIFT),
    (0xE6, flag::ALTERNATE, flag::DEVICE_RALT),
    (0xE7, flag::COMMAND, flag::DEVICE_RCMD),
];

/// Same shape as `sardp_win::InjectCommand`, so `sardp-server`'s
/// `SinkEvent` maps onto either platform the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InjectCommand {
    /// A physical key by USB HID usage.
    Key {
        hid_usage: u32,
        down: bool,
    },
    /// Committed text (`TextInput`).
    Text(String),
    /// Pointer position in *pixels* relative to the captured output's
    /// top-left corner.
    MouseMove {
        x: i32,
        y: i32,
    },
    MouseButton {
        button: u8,
        down: bool,
    },
    /// `WHEEL_DELTA` (120) units, positive = up / right.
    Wheel {
        dx: i16,
        dy: i16,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct InjectorConfig {
    /// Pause between units of injected text (KNOWN_ISSUES.md #13).
    pub text_unit_delay: Duration,
    /// Top-left of the captured output in global display *points*
    /// (`SourceInfo.origin_x/y` are already points; `CGDisplayBounds`).
    pub output_origin: (f64, f64),
    /// Pixels per point of the captured output: `SourceInfo.width` divided
    /// by the display's width in points. 2.0 on a Retina display.
    pub scale: f64,
    /// How close together two clicks must be to count as a double click.
    /// macOS's own default is 500ms; the system value lives in
    /// `NSEvent.doubleClickInterval`, which this crate does not read
    /// because it would pull in AppKit.
    pub double_click_interval: Duration,
    /// How far apart, in points, two clicks may be and still be a double
    /// click.
    pub double_click_slop: f64,
}

impl Default for InjectorConfig {
    fn default() -> Self {
        Self {
            text_unit_delay: Duration::from_millis(0),
            output_origin: (0.0, 0.0),
            scale: 1.0,
            double_click_interval: Duration::from_millis(500),
            double_click_slop: 4.0,
        }
    }
}

/// One thing to post. Produced by [`InputState`], consumed by the shim.
#[derive(Debug, Clone, PartialEq)]
pub enum Post {
    Key {
        key_code: u16,
        down: bool,
        flags: u64,
    },
    Text(String),
    Mouse {
        kind: MouseKind,
        /// `CGMouseButton`: 0 left, 1 right, 2 centre, 3.. other.
        button: u32,
        x: f64,
        y: f64,
        click_state: i64,
        flags: u64,
    },
    Scroll {
        lines_dy: i32,
        lines_dx: i32,
        flags: u64,
    },
    /// Nothing to post: a key macOS has no code for, or a zero scroll.
    Nothing,
}

#[derive(Debug, Clone, Copy)]
struct LastClick {
    button: u8,
    at_us: u64,
    position: (f64, f64),
    click_state: i64,
}

/// The injector's view of the input device it is pretending to be.
///
/// Pure logic on purpose: every macOS rule that is easy to get wrong --
/// modifier flags, drag vs move, click counting, pixel-to-point scaling --
/// is decided here so it can be tested without a display or a TCC grant.
#[derive(Debug)]
pub struct InputState {
    config: InjectorConfig,
    /// Bit `i` set = HID usage `0xE0 + i` is down.
    modifiers: u8,
    /// Bit `b` set = mouse button `b` (1-based, spec numbering) is down.
    buttons: u8,
    position: (f64, f64),
    last_click: Option<LastClick>,
}

impl InputState {
    pub fn new(config: InjectorConfig) -> Self {
        Self {
            config,
            modifiers: 0,
            buttons: 0,
            position: config.output_origin,
            last_click: None,
        }
    }

    /// The modifier flags as they stand, without any per-key extras.
    pub fn flags(&self) -> u64 {
        let mut flags = 0u64;
        for (i, (_, group, device)) in MODIFIERS.iter().enumerate() {
            if self.modifiers & (1 << i) != 0 {
                flags |= group | device;
            }
        }
        flags
    }

    /// Buttons currently held, as a bitmask over spec button numbers.
    pub fn pressed_buttons(&self) -> u8 {
        self.buttons
    }

    /// Modifier HID usages currently held.
    pub fn pressed_modifiers(&self) -> Vec<u32> {
        MODIFIERS
            .iter()
            .enumerate()
            .filter(|(i, _)| self.modifiers & (1 << i) != 0)
            .map(|(_, (hid, _, _))| *hid)
            .collect()
    }

    pub fn position(&self) -> (f64, f64) {
        self.position
    }

    pub fn key(&mut self, hid_usage: u32, down: bool) -> Post {
        let Some(key_code) = keymap::hid_to_virtual_key(hid_usage) else {
            return Post::Nothing;
        };
        if keymap::is_modifier(hid_usage) {
            // Update first, then read: a modifier's own event carries the
            // state *after* the change, which is what macOS reports for a
            // real keyboard's flagsChanged.
            let bit = 1u8 << (hid_usage - 0xE0);
            if down {
                self.modifiers |= bit;
            } else {
                self.modifiers &= !bit;
            }
        }
        let mut flags = self.flags();
        if keymap::is_numeric_pad(hid_usage) {
            flags |= flag::NUMERIC_PAD;
        }
        Post::Key {
            key_code,
            down,
            flags,
        }
    }

    pub fn text(&mut self, text: String) -> Post {
        if text.is_empty() {
            Post::Nothing
        } else {
            Post::Text(text)
        }
    }

    pub fn mouse_move(&mut self, x_px: i32, y_px: i32) -> Post {
        self.position = self.to_points(x_px, y_px);
        // A move while a button is held has to be a drag, or apps that
        // implement dragging by watching for `*MouseDragged` see nothing.
        // The event also has to name the button being dragged.
        let (kind, button) = match self.lowest_pressed_button() {
            Some(b) => (MouseKind::Drag, cg_button(b)),
            None => (MouseKind::Move, 0),
        };
        Post::Mouse {
            kind,
            button,
            x: self.position.0,
            y: self.position.1,
            // Dragging keeps the click state of the press it belongs to,
            // so a double-click-drag (select by word) still reads as one.
            click_state: if kind == MouseKind::Drag {
                self.last_click.map(|c| c.click_state).unwrap_or(1)
            } else {
                0
            },
            flags: self.flags(),
        }
    }

    pub fn mouse_button(&mut self, button: u8, down: bool, now_us: u64) -> Post {
        let bit = 1u8 << (button.min(7));
        if down {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        let click_state = if down {
            let state = match self.last_click {
                Some(last)
                    if last.button == button
                        && now_us.saturating_sub(last.at_us)
                            <= self.config.double_click_interval.as_micros() as u64
                        && distance(last.position, self.position)
                            <= self.config.double_click_slop =>
                {
                    (last.click_state + 1).min(3)
                }
                _ => 1,
            };
            self.last_click = Some(LastClick {
                button,
                at_us: now_us,
                position: self.position,
                click_state: state,
            });
            state
        } else {
            // The release must repeat the press's click state, or the
            // pair does not read as one click.
            self.last_click
                .filter(|c| c.button == button)
                .map(|c| c.click_state)
                .unwrap_or(1)
        };
        Post::Mouse {
            kind: if down { MouseKind::Down } else { MouseKind::Up },
            button: cg_button(button),
            x: self.position.0,
            y: self.position.1,
            click_state,
            flags: self.flags(),
        }
    }

    pub fn wheel(&mut self, dx: i16, dy: i16) -> Post {
        let (lines_dy, lines_dx) = (wheel_to_lines(dy), wheel_to_lines(dx));
        if lines_dy == 0 && lines_dx == 0 {
            return Post::Nothing;
        }
        Post::Scroll {
            lines_dy,
            lines_dx,
            flags: self.flags(),
        }
    }

    /// Pixels relative to the captured output -> global display points.
    fn to_points(&self, x_px: i32, y_px: i32) -> (f64, f64) {
        let scale = if self.config.scale > 0.0 {
            self.config.scale
        } else {
            1.0
        };
        (
            self.config.output_origin.0 + f64::from(x_px) / scale,
            self.config.output_origin.1 + f64::from(y_px) / scale,
        )
    }

    fn lowest_pressed_button(&self) -> Option<u8> {
        (1..=7u8).find(|b| self.buttons & (1 << b) != 0)
    }
}

/// Spec button number -> `CGMouseButton`.
fn cg_button(button: u8) -> u32 {
    match button {
        button::RIGHT => 1,
        button::MIDDLE => 2,
        button::X1 => 3,
        button::X2 => 4,
        _ => 0,
    }
}

/// `WHEEL_DELTA` (120) units -> line units, keeping a partial notch from
/// vanishing: anything non-zero moves at least one line.
fn wheel_to_lines(delta: i16) -> i32 {
    let lines = i32::from(delta) / 120;
    if lines != 0 {
        lines
    } else if delta > 0 {
        1
    } else if delta < 0 {
        -1
    } else {
        0
    }
}

fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

#[derive(Debug)]
pub struct InjectorClosed;

impl std::fmt::Display for InjectorClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "input injector thread is gone")
    }
}

impl std::error::Error for InjectorClosed {}

/// A running injector. Dropping it lets the thread finish what is queued
/// (the caller's spec 4.4.2 releases are typically the last commands) and
/// then releases the event source.
///
/// The "feed a worker thread, then join on drop" boilerplate this used to
/// carry as its own `impl Drop` is [`WorkerHandle`] (core crate,
/// KNOWN_ISSUES #29) now -- the same replacement `sardp_win::InputInjector`
/// got; this one is macOS's turn (KNOWN_ISSUES #29's "held for a macOS
/// session" note, #30).
pub struct InputInjector {
    handle: WorkerHandle<InjectCommand>,
}

impl InputInjector {
    /// Fails immediately if Accessibility is not granted, rather than
    /// posting events the system silently drops. Unlike the Windows
    /// injector this is fallible for exactly that reason -- `SendInput`
    /// needs no permission.
    pub fn start(config: InjectorConfig) -> Result<Self, InjectError> {
        // Created here, on the caller's thread, so the permission failure
        // is reported to the caller instead of logged from a thread.
        let injector = Injector::new()?;
        let (tx, rx) = mpsc::channel::<InjectCommand>();
        let worker = std::thread::Builder::new()
            .name("sardp-mac-inject".into())
            .spawn(move || {
                let start = Instant::now();
                let mut state = InputState::new(config);
                for command in rx {
                    let now_us = start.elapsed().as_micros() as u64;
                    let post = match command {
                        InjectCommand::Key { hid_usage, down } => {
                            let post = state.key(hid_usage, down);
                            if post == Post::Nothing {
                                eprintln!(
                                    "[sardp-mac] no macOS virtual key for HID usage \
                                     {hid_usage:#x}; ignored"
                                );
                            }
                            post
                        }
                        InjectCommand::Text(text) => state.text(text),
                        InjectCommand::MouseMove { x, y } => state.mouse_move(x, y),
                        InjectCommand::MouseButton { button, down } => {
                            state.mouse_button(button, down, now_us)
                        }
                        InjectCommand::Wheel { dx, dy } => state.wheel(dx, dy),
                    };
                    if let Err(e) = perform(&injector, &config, post) {
                        eprintln!("[sardp-mac] input injection failed: {e}");
                    }
                }
            })
            .expect("spawn inject thread");
        Ok(Self {
            handle: WorkerHandle::new(tx, worker),
        })
    }

    /// Queues one injection; never blocks.
    pub fn inject(&self, command: InjectCommand) -> Result<(), InjectorClosed> {
        self.handle.send(command).map_err(|_| InjectorClosed)
    }
}

fn perform(injector: &Injector, config: &InjectorConfig, post: Post) -> Result<(), InjectError> {
    match post {
        Post::Nothing => Ok(()),
        Post::Key {
            key_code,
            down,
            flags,
        } => injector.key(key_code, down, flags),
        Post::Text(text) => {
            // One grapheme cluster at a time: a receiving control that
            // cannot keep up drops whole characters rather than half of a
            // surrogate pair or a combining sequence (KNOWN_ISSUES #13 --
            // the safe pacing depends on the control, so it is configurable
            // and defaults to none).
            let clusters: Vec<&str> = split_graphemes(&text);
            for (i, cluster) in clusters.iter().enumerate() {
                injector.text(cluster)?;
                if i + 1 < clusters.len() && !config.text_unit_delay.is_zero() {
                    std::thread::sleep(config.text_unit_delay);
                }
            }
            Ok(())
        }
        Post::Mouse {
            kind,
            button,
            x,
            y,
            click_state,
            flags,
        } => injector.mouse(kind, button, x, y, click_state, flags),
        Post::Scroll {
            lines_dy,
            lines_dx,
            flags,
        } => injector.scroll(lines_dy, lines_dx, flags),
    }
}

/// Splits text into units that are safe to send separately.
///
/// Not a full grapheme segmenter (that would need a Unicode table this
/// crate has no reason to carry): it keeps a base character together with
/// any following combining marks and never splits a surrogate pair, which
/// covers what an IME commits. Anything it gets wrong costs pacing
/// granularity, not correctness -- the text itself is unchanged.
fn split_graphemes(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if i > start && !is_combining(c) {
            out.push(&text[start..i]);
            start = i;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

fn is_combining(c: char) -> bool {
    matches!(c as u32,
        0x0300..=0x036F      // combining diacritical marks
        | 0x1AB0..=0x1AFF
        | 0x1DC0..=0x1DFF
        | 0x20D0..=0x20FF
        | 0xFE20..=0xFE2F
        | 0x3099..=0x309A    // Japanese dakuten / handakuten
        | 0x200D             // zero-width joiner (emoji sequences)
        | 0xFE0E..=0xFE0F    // variation selectors
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> InputState {
        InputState::new(InjectorConfig {
            output_origin: (100.0, 50.0),
            scale: 2.0,
            ..InjectorConfig::default()
        })
    }

    #[test]
    fn a_plain_key_carries_no_modifiers() {
        let mut s = state();
        assert_eq!(
            s.key(0x04, true), // a
            Post::Key {
                key_code: 0x00,
                down: true,
                flags: 0
            }
        );
    }

    #[test]
    fn a_modifier_is_state_the_next_key_carries() {
        let mut s = state();
        // Command down: its own event already says Command is held.
        let Post::Key { flags, .. } = s.key(0xE3, true) else {
            panic!("expected a key post")
        };
        assert_eq!(flags, flag::COMMAND | flag::DEVICE_LCMD);
        // ...and so does the `c` that follows, which is what makes
        // Command-C work at all.
        let Post::Key {
            key_code, flags, ..
        } = s.key(0x06, true)
        else {
            panic!("expected a key post")
        };
        assert_eq!(key_code, 0x08);
        assert_eq!(flags, flag::COMMAND | flag::DEVICE_LCMD);
        // Releasing Command clears it from the release event itself.
        let Post::Key { flags, .. } = s.key(0xE3, false) else {
            panic!("expected a key post")
        };
        assert_eq!(flags, 0);
    }

    #[test]
    fn left_and_right_modifiers_are_distinguishable() {
        let mut s = state();
        let Post::Key { flags: left, .. } = s.key(0xE1, true) else {
            panic!()
        };
        s.key(0xE1, false);
        let Post::Key { flags: right, .. } = s.key(0xE5, true) else {
            panic!()
        };
        assert_eq!(left & flag::SHIFT, flag::SHIFT);
        assert_eq!(right & flag::SHIFT, flag::SHIFT);
        assert_ne!(left, right, "the device-dependent bits differ");
    }

    #[test]
    fn arrows_and_keypad_carry_the_numeric_pad_flag() {
        let mut s = state();
        let Post::Key { flags, .. } = s.key(0x50, true) else {
            panic!()
        }; // Left arrow
        assert_eq!(flags & flag::NUMERIC_PAD, flag::NUMERIC_PAD);
        let Post::Key { flags, .. } = s.key(0x04, true) else {
            panic!()
        }; // a
        assert_eq!(flags & flag::NUMERIC_PAD, 0);
    }

    #[test]
    fn an_unmapped_key_posts_nothing() {
        let mut s = state();
        assert_eq!(s.key(0x46, true), Post::Nothing); // Print Screen
    }

    #[test]
    fn pixels_become_points_relative_to_the_output_origin() {
        let mut s = state(); // origin (100, 50), scale 2
        let Post::Mouse { x, y, kind, .. } = s.mouse_move(200, 100) else {
            panic!()
        };
        assert_eq!((x, y), (200.0, 100.0));
        assert_eq!(kind, MouseKind::Move);
    }

    #[test]
    fn a_move_with_a_button_held_is_a_drag_naming_that_button() {
        let mut s = state();
        s.mouse_button(button::LEFT, true, 0);
        let Post::Mouse { kind, button, .. } = s.mouse_move(10, 10) else {
            panic!()
        };
        assert_eq!(kind, MouseKind::Drag);
        assert_eq!(button, 0); // CGMouseButton::left
        s.mouse_button(button::LEFT, false, 1_000);
        let Post::Mouse { kind, .. } = s.mouse_move(20, 20) else {
            panic!()
        };
        assert_eq!(kind, MouseKind::Move);
    }

    #[test]
    fn a_right_drag_names_the_right_button() {
        let mut s = state();
        s.mouse_button(button::RIGHT, true, 0);
        let Post::Mouse { kind, button, .. } = s.mouse_move(10, 10) else {
            panic!()
        };
        assert_eq!((kind, button), (MouseKind::Drag, 1));
    }

    #[test]
    fn a_second_click_soon_enough_and_close_enough_is_a_double_click() {
        let mut s = state();
        let Post::Mouse { click_state, .. } = s.mouse_button(button::LEFT, true, 0) else {
            panic!()
        };
        assert_eq!(click_state, 1);
        // The release repeats the press's count.
        let Post::Mouse { click_state, .. } = s.mouse_button(button::LEFT, false, 1_000) else {
            panic!()
        };
        assert_eq!(click_state, 1);
        let Post::Mouse { click_state, .. } = s.mouse_button(button::LEFT, true, 200_000) else {
            panic!()
        };
        assert_eq!(click_state, 2);
        let Post::Mouse { click_state, .. } = s.mouse_button(button::LEFT, true, 300_000) else {
            panic!()
        };
        assert_eq!(click_state, 3, "triple click");
        // Capped: macOS has no quadruple click.
        let Post::Mouse { click_state, .. } = s.mouse_button(button::LEFT, true, 400_000) else {
            panic!()
        };
        assert_eq!(click_state, 3);
    }

    #[test]
    fn a_slow_or_distant_second_click_is_a_fresh_single_click() {
        let mut s = state();
        s.mouse_button(button::LEFT, true, 0);
        let Post::Mouse { click_state, .. } = s.mouse_button(button::LEFT, true, 900_000) else {
            panic!()
        };
        assert_eq!(click_state, 1, "past the double click interval");

        let mut s = state();
        s.mouse_button(button::LEFT, true, 0);
        s.mouse_move(200, 200); // far away in points
        let Post::Mouse { click_state, .. } = s.mouse_button(button::LEFT, true, 10_000) else {
            panic!()
        };
        assert_eq!(click_state, 1, "outside the slop");
    }

    #[test]
    fn a_different_button_does_not_continue_a_double_click() {
        let mut s = state();
        s.mouse_button(button::LEFT, true, 0);
        let Post::Mouse { click_state, .. } = s.mouse_button(button::RIGHT, true, 10_000) else {
            panic!()
        };
        assert_eq!(click_state, 1);
    }

    #[test]
    fn a_drag_keeps_the_click_state_of_its_press() {
        let mut s = state();
        s.mouse_button(button::LEFT, true, 0);
        s.mouse_button(button::LEFT, false, 1_000);
        s.mouse_button(button::LEFT, true, 100_000); // double click...
        let Post::Mouse { click_state, .. } = s.mouse_move(4, 4) else {
            panic!()
        };
        assert_eq!(click_state, 2, "...then drag: select by word");
    }

    #[test]
    fn modifiers_ride_along_on_mouse_events() {
        let mut s = state();
        s.key(0xE1, true); // Shift
        let Post::Mouse { flags, .. } = s.mouse_button(button::LEFT, true, 0) else {
            panic!()
        };
        assert_eq!(flags & flag::SHIFT, flag::SHIFT);
    }

    #[test]
    fn wheel_deltas_become_lines_without_losing_a_partial_notch() {
        let mut s = state();
        assert_eq!(
            s.wheel(0, 120),
            Post::Scroll {
                lines_dy: 1,
                lines_dx: 0,
                flags: 0
            }
        );
        assert_eq!(
            s.wheel(0, -360),
            Post::Scroll {
                lines_dy: -3,
                lines_dx: 0,
                flags: 0
            }
        );
        // A trackpad's fine-grained delta must still move something.
        assert_eq!(
            s.wheel(0, 7),
            Post::Scroll {
                lines_dy: 1,
                lines_dx: 0,
                flags: 0
            }
        );
        assert_eq!(s.wheel(0, 0), Post::Nothing);
    }

    #[test]
    fn pressed_state_is_readable_for_the_release_all_invariant() {
        // spec 4.4.2: whatever is held has to be released when a session
        // ends, so the injector has to be able to say what it is holding.
        let mut s = state();
        s.key(0xE3, true);
        s.key(0xE5, true);
        s.mouse_button(button::LEFT, true, 0);
        assert_eq!(s.pressed_modifiers(), vec![0xE3, 0xE5]);
        assert_eq!(s.pressed_buttons() & (1 << button::LEFT), 1 << button::LEFT);
        s.key(0xE3, false);
        s.mouse_button(button::LEFT, false, 1);
        assert_eq!(s.pressed_modifiers(), vec![0xE5]);
        assert_eq!(s.pressed_buttons(), 0);
    }

    #[test]
    fn text_splits_without_breaking_combining_sequences() {
        assert_eq!(split_graphemes("abc"), vec!["a", "b", "c"]);
        // が as base + dakuten stays one unit.
        assert_eq!(split_graphemes("か\u{3099}き"), vec!["か\u{3099}", "き"]);
        // A surrogate pair is one `char` in Rust and is never split.
        assert_eq!(split_graphemes("🍣x"), vec!["🍣", "x"]);
        assert_eq!(split_graphemes(""), Vec::<&str>::new());
    }

    #[test]
    fn empty_text_posts_nothing() {
        let mut s = state();
        assert_eq!(s.text(String::new()), Post::Nothing);
    }
}
