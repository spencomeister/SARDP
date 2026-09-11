//! 診断ツール: send_input_poc.rsのtype_unicode_textで採用した「1文字あたり50ms待機」が
//! (a) Windowsの入力キュー全般に必要な間隔なのか、(b) 検証に使ったモダン化メモ帳の
//! WinUIベースのテキストコントロール固有の問題なのかを切り分ける。
//!
//! 自前のプロセス内に、モダンUIを一切介さない古典的なWin32 EDITコントロール
//! (システム標準の"EDIT"ウィンドウクラス)をトップレベルウィンドウとして作成し、
//! 複数の文字間隔(0ms/1ms/5ms/15ms/50ms)でSendInputし、それぞれ読み返しが
//! 完全一致するかを確認する。
//!
//! SARDPやサンプルバイナリの一部ではなく、調査後は削除してよい。

use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, SetFocus, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DispatchMessageW, PeekMessageW, SendMessageW, SetForegroundWindow,
    SetWindowTextW, ShowWindow, TranslateMessage, ES_AUTOVSCROLL, ES_MULTILINE, MSG,
    PM_REMOVE, SW_SHOW, WM_GETTEXT, WM_GETTEXTLENGTH, WS_EX_LEFT, WS_OVERLAPPEDWINDOW,
    WS_VISIBLE,
};

const DEMO_TEXT: &str = "SARDP 3W-1-c: SendInputによる入力注入テスト / Hello from Rust!";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn main() -> windows::core::Result<()> {
    let hinstance = unsafe { GetModuleHandleW(None)? };
    let class_name = wide("EDIT");
    let title = wide("send_input_timing_probe (classic Win32 EDIT control)");

    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LEFT,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW
                | WS_VISIBLE
                | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                    (ES_MULTILINE | ES_AUTOVSCROLL) as u32,
                ),
            100,
            100,
            900,
            300,
            None,
            None,
            Some(hinstance.into()),
            None,
        )?
    };
    println!("[timing-probe] created classic EDIT control window: {hwnd:?}");

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
    }
    pump_messages();
    std::thread::sleep(Duration::from_millis(200));

    for delay_ms in [0u64, 1, 5, 15, 50] {
        clear_text(hwnd)?;
        let start = Instant::now();
        type_unicode_text(DEMO_TEXT, Duration::from_millis(delay_ms))?;
        let elapsed = start.elapsed();
        let readback = window_text(hwnd);
        let pass = readback == DEMO_TEXT;
        println!(
            "[timing-probe] delay={delay_ms:>3}ms elapsed={:>6.1}ms pass={pass} readback={readback:?}",
            elapsed.as_secs_f64() * 1000.0,
        );
    }

    println!("[timing-probe] done. Leaving the EDIT control window open; close it manually.");
    Ok(())
}

fn pump_messages() {
    let mut msg = MSG::default();
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn clear_text(hwnd: HWND) -> windows::core::Result<()> {
    let empty = wide("");
    unsafe { SetWindowTextW(hwnd, PCWSTR(empty.as_ptr()))? };
    pump_messages();
    Ok(())
}

fn window_text(hwnd: HWND) -> String {
    let len =
        unsafe { SendMessageW(hwnd, WM_GETTEXTLENGTH, Some(WPARAM(0)), Some(LPARAM(0))) }.0 as usize;
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

fn type_unicode_text(text: &str, delay: Duration) -> windows::core::Result<()> {
    for unit in text.encode_utf16() {
        let down = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: unit,
                    dwFlags: KEYEVENTF_UNICODE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let up = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: unit,
                    dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let sent = unsafe { SendInput(&[down, up], std::mem::size_of::<INPUT>() as i32) };
        if sent != 2 {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                format!("SendInput sent {sent}/2 events"),
            ));
        }
        pump_messages();
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        pump_messages();
    }
    Ok(())
}
