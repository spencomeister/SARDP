//! Input injection via `SendInput` (3W-1-c's validated approach, now
//! driven by spec 2.12 messages instead of a demo script).
//!
//! [`InputInjector::start`] spawns a thread that performs injections in
//! submission order. Text goes in one UTF-16 unit at a time with a
//! configurable pause between units (KNOWN_ISSUES.md #13: the safe pacing
//! depends on the receiving control; 3W-1-c's Notepad needed 50ms), so
//! the pause must not block the server's async loop -- hence the thread.
//!
//! Every injected event carries [`INJECTED_EXTRA_INFO`] in
//! `dwExtraInfo`. A `sardp-client` window running on the *same* machine
//! as the server (the loopback E2E setup) sees the injected events come
//! back through its own message queue; it checks `GetMessageExtraInfo()`
//! for this tag and doesn't forward them, which is what breaks the echo
//! loop.

use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    MOUSE_EVENT_FLAGS, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    XBUTTON1, XBUTTON2,
};

use crate::keymap;

/// `dwExtraInfo` tag on everything this module injects ("SARD").
pub const INJECTED_EXTRA_INFO: usize = 0x5341_5244;

/// `MouseButton.button` numbering, mirrored from
/// `sardp::messages::mouse_button` (this crate can't depend on `sardp`).
pub mod button {
    pub const LEFT: u8 = 1;
    pub const RIGHT: u8 = 2;
    pub const MIDDLE: u8 = 3;
    pub const X1: u8 = 4;
    pub const X2: u8 = 5;
}

/// `KeyEvent.modifiers` bits, mirrored from `sardp::messages::key_modifier`.
pub mod modifier {
    pub const SHIFT: u16 = 1 << 0;
    pub const CTRL: u16 = 1 << 1;
    pub const ALT: u16 = 1 << 2;
    pub const META: u16 = 1 << 3;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InjectCommand {
    /// A physical key by USB HID usage.
    Key { hid_usage: u32, down: bool },
    /// Committed text (`TextInput`), typed as Unicode.
    Text(String),
    /// Pointer position in pixels relative to the captured output's
    /// top-left corner.
    MouseMove { x: i32, y: i32 },
    MouseButton { button: u8, down: bool },
    /// `WHEEL_DELTA` (120) units, positive = up / right.
    Wheel { dx: i16, dy: i16 },
}

#[derive(Debug, Clone)]
pub struct InjectorConfig {
    /// Pause between UTF-16 units of injected text (KNOWN_ISSUES.md #13).
    pub text_unit_delay: Duration,
    /// Top-left of the captured output in virtual-desktop coordinates
    /// (`dxgi_capture_poc::capture::duplicated_output_desktop_rect`).
    pub output_origin: (i32, i32),
}

#[derive(Debug)]
pub struct InjectorClosed;

impl std::fmt::Display for InjectorClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "input injector thread is gone")
    }
}

pub struct InputInjector {
    tx: Option<mpsc::Sender<InjectCommand>>,
    worker: Option<JoinHandle<()>>,
}

impl InputInjector {
    pub fn start(config: InjectorConfig) -> Self {
        let (tx, rx) = mpsc::channel::<InjectCommand>();
        let worker = std::thread::Builder::new()
            .name("sardp-win-inject".into())
            .spawn(move || {
                for command in rx {
                    if let Err(err) = perform(&config, &command) {
                        eprintln!("[sardp-win] inject {command:?} failed: {err}");
                    }
                }
            })
            .expect("spawn inject thread");
        Self {
            tx: Some(tx),
            worker: Some(worker),
        }
    }

    /// Queues one injection; never blocks.
    pub fn inject(&self, command: InjectCommand) -> Result<(), InjectorClosed> {
        self.tx
            .as_ref()
            .ok_or(InjectorClosed)?
            .send(command)
            .map_err(|_| InjectorClosed)
    }
}

impl Drop for InputInjector {
    fn drop(&mut self) {
        // Close the queue, then let the thread finish what's queued (the
        // caller's spec 4.4.2 releases are typically the last commands).
        drop(self.tx.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn perform(config: &InjectorConfig, command: &InjectCommand) -> windows::core::Result<()> {
    match command {
        InjectCommand::Key { hid_usage, down } => {
            let Some((scancode, extended)) = keymap::hid_to_scancode(*hid_usage) else {
                eprintln!("[sardp-win] no Windows scan code for HID usage {hid_usage:#x}; ignored");
                return Ok(());
            };
            let mut flags = KEYEVENTF_SCANCODE;
            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if !down {
                flags |= KEYEVENTF_KEYUP;
            }
            send_inputs(&[keyboard_input(scancode, flags)])
        }
        InjectCommand::Text(text) => {
            let units: Vec<u16> = text.encode_utf16().collect();
            for (i, unit) in units.iter().enumerate() {
                send_inputs(&[
                    keyboard_input(*unit, KEYEVENTF_UNICODE),
                    keyboard_input(*unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
                ])?;
                if i + 1 < units.len() && !config.text_unit_delay.is_zero() {
                    std::thread::sleep(config.text_unit_delay);
                }
            }
            Ok(())
        }
        InjectCommand::MouseMove { x, y } => {
            // Same virtual-desktop normalization as 3W-1-c.
            let screen_x = config.output_origin.0 + x;
            let screen_y = config.output_origin.1 + y;
            let vs_x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
            let vs_y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
            let vs_w = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) }.max(1);
            let vs_h = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) }.max(1);
            let abs_x = ((screen_x - vs_x) as i64 * 65535 / vs_w as i64) as i32;
            let abs_y = ((screen_y - vs_y) as i64 * 65535 / vs_h as i64) as i32;
            send_inputs(&[mouse_input(
                abs_x,
                abs_y,
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            )])
        }
        InjectCommand::MouseButton { button, down } => {
            let (flags, data) = match (*button, *down) {
                (button::LEFT, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                (button::LEFT, false) => (MOUSEEVENTF_LEFTUP, 0),
                (button::RIGHT, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                (button::RIGHT, false) => (MOUSEEVENTF_RIGHTUP, 0),
                (button::MIDDLE, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                (button::MIDDLE, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                (button::X1, true) => (MOUSEEVENTF_XDOWN, u32::from(XBUTTON1)),
                (button::X1, false) => (MOUSEEVENTF_XUP, u32::from(XBUTTON1)),
                (button::X2, true) => (MOUSEEVENTF_XDOWN, u32::from(XBUTTON2)),
                (button::X2, false) => (MOUSEEVENTF_XUP, u32::from(XBUTTON2)),
                (other, _) => {
                    eprintln!("[sardp-win] unknown mouse button {other}; ignored");
                    return Ok(());
                }
            };
            send_inputs(&[mouse_input(0, 0, data, flags)])
        }
        InjectCommand::Wheel { dx, dy } => {
            let mut inputs = Vec::with_capacity(2);
            if *dy != 0 {
                inputs.push(mouse_input(0, 0, *dy as i32 as u32, MOUSEEVENTF_WHEEL));
            }
            if *dx != 0 {
                inputs.push(mouse_input(0, 0, *dx as i32 as u32, MOUSEEVENTF_HWHEEL));
            }
            if inputs.is_empty() {
                return Ok(());
            }
            send_inputs(&inputs)
        }
    }
}

fn keyboard_input(
    scan: u16,
    flags: windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS,
) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECTED_EXTRA_INFO,
            },
        },
    }
}

fn mouse_input(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECTED_EXTRA_INFO,
            },
        },
    }
}

fn send_inputs(inputs: &[INPUT]) -> windows::core::Result<()> {
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        return Err(windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            format!("SendInput sent {sent}/{} events", inputs.len()),
        ));
    }
    Ok(())
}
