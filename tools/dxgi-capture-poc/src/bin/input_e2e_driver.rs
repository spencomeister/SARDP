//! 3W-1-d-4 E2E driver: exercises the whole input path on one machine.
//!
//! Setup (done by the E2E script): `sardp-server --capture desktop` and
//! `sardp-client --display window` running on this machine, LAN address.
//!
//! This driver then:
//! 1. finds the `sardp-client` window and reads the stream size from its
//!    title (`SARDP <addr> (WxH)`);
//! 2. launches Notepad, moves it to a fixed spot on the primary display;
//! 3. posts `WM_MOUSEMOVE`/`WM_LBUTTONDOWN`/`WM_LBUTTONUP` to the client
//!    window at the point where Notepad's edit area appears in the
//!    (scaled) mirror -> the client forwards `MouseMove`/`MouseButton`,
//!    the server injects a real click there -> Notepad gets focus. The
//!    real cursor position is then checked against the target;
//! 4. posts `WM_CHAR` per character plus a `WM_KEYDOWN/UP` Enter to the
//!    client window -> `TextInput`/`KeyEvent` -> `SendInput` into Notepad;
//! 5. reads Notepad's text back and compares.
//!
//! Messages are *posted* to the client window rather than sent with
//! `SendInput`, because on a single machine `SendInput` would go to the
//! foreground window -- which must be Notepad, the injection target.
//! Posted messages carry no `dwExtraInfo`, so the client treats them as
//! genuine user input (its echo filter only drops the injector's tag).

use std::process::Command;
use std::time::{Duration, Instant};

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_RETURN;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GetClassNameW, GetClientRect, GetCursorPos,
    GetForegroundWindow, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    PostMessageW,
    SendMessageW, SetForegroundWindow, SetWindowPos, SWP_SHOWWINDOW, WM_CHAR, WM_GETTEXT,
    WM_GETTEXTLENGTH, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
};

const DEFAULT_TEXT: &str = "SARDP d-4 input: Hello, 世界! 123";
/// `sardp_win::display`'s window class.
const CLIENT_WINDOW_CLASS: &str = "SardpDisplayWindow";

struct Args {
    text: String,
    /// Where Notepad is placed on the primary display (x, y, w, h).
    notepad_rect: (i32, i32, i32, i32),
}

fn parse_args() -> Args {
    let mut text = DEFAULT_TEXT.to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--text" => text = args.next().expect("--text requires a value"),
            other => panic!("unknown argument {other}"),
        }
    }
    Args {
        text,
        notepad_rect: (200, 150, 900, 600),
    }
}

fn main() {
    let args = parse_args();
    let mut failures = Vec::new();

    // 1. The client window (by window class -- a title prefix once matched
    //    a File Explorer window showing the repo folder) and its geometry.
    let Some(client) = find_window_by_class(CLIENT_WINDOW_CLASS) else {
        eprintln!("[input-e2e] FAIL: no sardp-client window (class {CLIENT_WINDOW_CLASS:?}) found");
        std::process::exit(2);
    };
    let title = window_text(client);
    let Some(stream_size) = parse_stream_size(&title) else {
        eprintln!("[input-e2e] FAIL: cannot parse stream size from client title {title:?}");
        std::process::exit(2);
    };
    let mut client_rect = RECT::default();
    unsafe { GetClientRect(client, &mut client_rect) }.expect("GetClientRect");
    let window_size = (client_rect.right - client_rect.left, client_rect.bottom - client_rect.top);
    println!(
        "[input-e2e] client window {client:?} title={title:?} client area {}x{} stream {}x{}",
        window_size.0, window_size.1, stream_size.0, stream_size.1
    );

    // 2. Notepad at a known place on the primary display.
    // Detached stdio: Notepad must not inherit this process's stdout pipe,
    // or whoever reads the driver's output waits until Notepad exits.
    let _child = Command::new("notepad.exe")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("launch notepad.exe");
    let Some(notepad) = wait_for_window(Duration::from_secs(5), |t| t.contains("Notepad") || t.contains("メモ帳"))
    else {
        eprintln!("[input-e2e] FAIL: notepad window did not appear");
        std::process::exit(2);
    };
    let (nx, ny, nw, nh) = args.notepad_rect;
    unsafe { SetWindowPos(notepad, None, nx, ny, nw, nh, SWP_SHOWWINDOW) }.expect("SetWindowPos");
    std::thread::sleep(Duration::from_millis(500));
    let mut notepad_rect = RECT::default();
    unsafe { GetWindowRect(notepad, &mut notepad_rect) }.expect("GetWindowRect");
    println!(
        "[input-e2e] notepad {notepad:?} at ({}, {})-({}, {})",
        notepad_rect.left, notepad_rect.top, notepad_rect.right, notepad_rect.bottom
    );
    if !ensure_foreground(notepad, Duration::from_secs(2)) {
        println!("[input-e2e] note: could not confirm notepad foreground before the click; the injected click should fix that");
    }

    // 3. Click into the edit area through the mirror.
    let target = (
        notepad_rect.left + (notepad_rect.right - notepad_rect.left) / 2,
        notepad_rect.top + (notepad_rect.bottom - notepad_rect.top) * 2 / 3,
    );
    let in_window = (
        (i64::from(target.0) * i64::from(window_size.0) / i64::from(stream_size.0)) as i32,
        (i64::from(target.1) * i64::from(window_size.1) / i64::from(stream_size.1)) as i32,
    );
    println!(
        "[input-e2e] clicking desktop ({}, {}) via client window point ({}, {})",
        target.0, target.1, in_window.0, in_window.1
    );
    let lparam = LPARAM(((in_window.1 as isize) << 16) | (in_window.0 as isize & 0xFFFF));
    post(client, WM_MOUSEMOVE, WPARAM(0), lparam);
    std::thread::sleep(Duration::from_millis(100));
    post(client, WM_LBUTTONDOWN, WPARAM(1), lparam);
    std::thread::sleep(Duration::from_millis(50));
    post(client, WM_LBUTTONUP, WPARAM(0), lparam);
    std::thread::sleep(Duration::from_millis(800));

    let mut cursor = POINT::default();
    unsafe { GetCursorPos(&mut cursor) }.expect("GetCursorPos");
    // The mirror is scaled (2560 -> 1280 => 2px per window px), so allow
    // the rounding of one window pixel each way.
    let tolerance = (stream_size.0 / window_size.0.max(1) as u32 + 2) as i32;
    let cursor_ok = (cursor.x - target.0).abs() <= tolerance && (cursor.y - target.1).abs() <= tolerance;
    println!(
        "[input-e2e] MOUSE: cursor now at ({}, {}), target ({}, {}), tolerance {tolerance}px -> {}",
        cursor.x, cursor.y, target.0, target.1, if cursor_ok { "PASS" } else { "FAIL" }
    );
    if !cursor_ok {
        failures.push("mouse position");
    }
    let foreground = unsafe { GetForegroundWindow() };
    let focus_ok = foreground == notepad;
    println!(
        "[input-e2e] FOCUS: foreground after injected click is {foreground:?} (notepad {notepad:?}) -> {}",
        if focus_ok { "PASS" } else { "FAIL" }
    );
    if !focus_ok {
        failures.push("focus via click");
        // 3W-1-c lesson: never type unless the intended window verifiably
        // has the focus -- the injected text would land in whatever else
        // is in front.
        if !ensure_foreground(notepad, Duration::from_secs(2)) {
            println!("[input-e2e] ABORT: notepad is not the foreground window; not typing anything");
            println!("[input-e2e] RESULT: FAIL ({})", failures.join(", "));
            std::process::exit(1);
        }
        println!("[input-e2e] note: notepad brought to the foreground directly for the typing step");
    }

    // 4. Type through the mirror: WM_CHAR per UTF-16 unit, then Enter as a key.
    println!("[input-e2e] typing {:?} + Enter via posted WM_CHAR/WM_KEYDOWN", args.text);
    for unit in args.text.encode_utf16() {
        post(client, WM_CHAR, WPARAM(unit as usize), LPARAM(1));
        std::thread::sleep(Duration::from_millis(20));
    }
    // Enter: scan code 0x1C in bits 16-23.
    let enter_down = LPARAM(1 | (0x1C << 16));
    let enter_up = LPARAM(1 | (0x1C << 16) | (1 << 30) | (1 << 31));
    post(client, WM_KEYDOWN, WPARAM(VK_RETURN.0 as usize), enter_down);
    std::thread::sleep(Duration::from_millis(30));
    post(client, WM_KEYUP, WPARAM(VK_RETURN.0 as usize), enter_up);

    // The server paces injected text (default 50ms/char); wait for it.
    let units = args.text.encode_utf16().count() as u64;
    std::thread::sleep(Duration::from_millis(units * 70 + 1500));

    // 5. Read back.
    let readback = find_edit_child(notepad).map(window_text_via_message);
    match &readback {
        Some(text) => {
            let text_ok = text.contains(&args.text);
            let newline_ok = text.contains("\r\n") || text.contains('\n');
            println!(
                "[input-e2e] TEXT: readback {:?} -> {}",
                text,
                if text_ok { "PASS" } else { "FAIL" }
            );
            println!(
                "[input-e2e] ENTER: newline present -> {}",
                if newline_ok { "PASS" } else { "FAIL" }
            );
            if !text_ok {
                failures.push("typed text");
            }
            if !newline_ok {
                failures.push("enter key");
            }
        }
        None => {
            println!("[input-e2e] TEXT: inconclusive (no readable edit control; check the notepad screenshot)");
        }
    }

    if failures.is_empty() {
        println!("[input-e2e] RESULT: PASS");
    } else {
        println!("[input-e2e] RESULT: FAIL ({})", failures.join(", "));
        std::process::exit(1);
    }
}

fn post(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) {
    unsafe { PostMessageW(Some(hwnd), msg, wparam, lparam) }.expect("PostMessageW");
}

fn parse_stream_size(title: &str) -> Option<(u32, u32)> {
    let open = title.rfind('(')?;
    let close = title[open..].find(')')? + open;
    let (w, h) = title[open + 1..close].split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// Same `AttachThreadInput` + verify approach as 3W-1-c.
fn ensure_foreground(hwnd: HWND, timeout: Duration) -> bool {
    unsafe {
        let current_foreground = GetForegroundWindow();
        let current_thread = GetCurrentThreadId();
        let foreground_thread = GetWindowThreadProcessId(current_foreground, None);
        let target_thread = GetWindowThreadProcessId(hwnd, None);
        let attached_fg = foreground_thread != current_thread
            && foreground_thread != 0
            && AttachThreadInput(current_thread, foreground_thread, true).as_bool();
        let attached_target = target_thread != current_thread
            && target_thread != 0
            && AttachThreadInput(current_thread, target_thread, true).as_bool();
        let _ = SetForegroundWindow(hwnd);
        if attached_target {
            let _ = AttachThreadInput(current_thread, target_thread, false);
        }
        if attached_fg {
            let _ = AttachThreadInput(current_thread, foreground_thread, false);
        }
    }
    let start = Instant::now();
    while start.elapsed() < timeout {
        if unsafe { GetForegroundWindow() } == hwnd {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

struct FindContext<'a> {
    /// `(class name, title)` -> match.
    predicate: &'a dyn Fn(&str, &str) -> bool,
    found: Option<HWND>,
}

fn find_window(predicate: &dyn Fn(&str, &str) -> bool) -> Option<HWND> {
    let mut ctx = FindContext {
        predicate,
        found: None,
    };
    unsafe {
        let _ = EnumWindows(Some(enum_windows_proc), LPARAM(&mut ctx as *mut FindContext as isize));
    }
    ctx.found
}

fn find_window_by_class(class: &str) -> Option<HWND> {
    find_window(&|c: &str, _t: &str| c == class)
}

fn window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..len.max(0) as usize])
}

fn wait_for_window(timeout: Duration, title_predicate: impl Fn(&str) -> bool) -> Option<HWND> {
    let start = Instant::now();
    let predicate = |_class: &str, title: &str| title_predicate(title);
    while start.elapsed() < timeout {
        if let Some(hwnd) = find_window(&predicate) {
            return Some(hwnd);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = unsafe { &mut *(lparam.0 as *mut FindContext) };
    if unsafe { IsWindowVisible(hwnd) }.as_bool()
        && (ctx.predicate)(&window_class(hwnd), &window_text(hwnd))
    {
        ctx.found = Some(hwnd);
        return BOOL(0);
    }
    BOOL(1)
}

fn window_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..len.max(0) as usize])
}

struct EditFindContext {
    found: Option<HWND>,
}

fn find_edit_child(parent: HWND) -> Option<HWND> {
    let mut ctx = EditFindContext { found: None };
    unsafe {
        let _ = EnumChildWindows(
            Some(parent),
            Some(enum_child_proc),
            LPARAM(&mut ctx as *mut EditFindContext as isize),
        );
    }
    ctx.found
}

unsafe extern "system" fn enum_child_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = unsafe { &mut *(lparam.0 as *mut EditFindContext) };
    let len = unsafe { SendMessageW(hwnd, WM_GETTEXTLENGTH, Some(WPARAM(0)), Some(LPARAM(0))) };
    if len.0 > 0 || ctx.found.is_none() {
        ctx.found = Some(hwnd);
    }
    BOOL(1)
}

fn window_text_via_message(hwnd: HWND) -> String {
    let len = unsafe { SendMessageW(hwnd, WM_GETTEXTLENGTH, Some(WPARAM(0)), Some(LPARAM(0))) }.0 as usize;
    if len == 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len + 1];
    unsafe {
        SendMessageW(
            hwnd,
            WM_GETTEXT,
            Some(WPARAM(buf.len())),
            Some(LPARAM(buf.as_mut_ptr() as isize)),
        )
    };
    String::from_utf16_lossy(&buf).trim_end_matches('\0').to_string()
}
