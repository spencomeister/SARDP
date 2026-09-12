//! 3W-1-c: SendInput()による入力注入の単体疎通確認。
//!
//! SARDP本体(sardp-server/sardp-client)とはまだ接続しない、3W-1-a/bと同じ位置づけの
//! 独立したサンプルバイナリ。以下を確認する:
//! - マウスカーソルを対象ウィンドウ内の座標へ`SendInput`(MOUSEEVENTF_MOVE|ABSOLUTE)で動かす
//! - 自前で起動したメモ帳ウィンドウへ`SendInput`(KEYEVENTF_UNICODE)で文字列を打ち込む
//!
//! 重要: `SetForegroundWindow`は戻り値`TRUE`でも実際にフォーカスが移っているとは
//! 限らない(初回実装でこれを信用した結果、キー入力が全く別の前面ウィンドウへ
//! 入ってしまう事故が起きた)。そのため`AttachThreadInput`で入力キューを結合した上で
//! `SetForegroundWindow`を呼び、その後`GetForegroundWindow()`が実際に対象ウィンドウを
//! 指しているかを確認してから初めてキー入力を送る。確認できなければキー入力は
//! 一切送らずに中断する。
//!
//! メモ帳のエディットコントロールを`EnumChildWindows`+`GetWindowTextW`で読み返し、
//! 実際に入力どおりの文字列が入ったかをベストエフォートで自動検証する
//! (Windowsのバージョンによってエディットコントロールの実装が変わり得るため、
//! 読み返しに失敗してもエラー終了はせず診断ログを出すに留める)。

use std::process::Command;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentProcessId, GetCurrentThreadId,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT, SendInput,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GetCursorPos, GetForegroundWindow, GetSystemMetrics,
    GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SendMessageW, SetForegroundWindow,
    WM_GETTEXT, WM_GETTEXTLENGTH,
};

const DEMO_TEXT: &str = "SARDP 3W-1-c: SendInputによる入力注入テスト / Hello from Rust!";

fn main() -> windows::core::Result<()> {
    println!("[send-input-poc] launching notepad.exe");
    let child = Command::new("notepad.exe")
        .spawn()
        .expect("failed to launch notepad.exe");
    let launcher_pid = child.id();

    // Windows 11のメモ帳はMSIXパッケージ経由で起動し、notepad.exeが起動した
    // プロセス(PID)と実際にウィンドウを持つプロセスのPIDが一致しないため、
    // PIDではなくウィンドウタイトルで探す。
    let hwnd = wait_for_notepad_window(Duration::from_secs(5))
        .expect("notepad window did not appear within timeout");
    let title = window_text(hwnd);
    println!(
        "[send-input-poc] found notepad window: hwnd={hwnd:?} launcher_pid={launcher_pid} title=\"{title}\""
    );

    // --- マウス: 対象ウィンドウの矩形内の座標へSendInputで移動し、クリックする ---
    // スクリーン中央ではなく対象ウィンドウの内側を狙うことで、ウィンドウの位置に
    // 関わらず「意図した相手」への操作になるようにする。
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect)? };
    let target_x = (rect.left + rect.right) / 2;
    let target_y = (rect.top + rect.bottom) / 2;
    println!(
        "[send-input-poc] moving cursor to ({target_x}, {target_y}) (center of target window) via SendInput"
    );
    move_cursor_absolute(target_x, target_y)?;
    click_left()?;
    std::thread::sleep(Duration::from_millis(200));

    let mut pos = POINT::default();
    unsafe { GetCursorPos(&mut pos)? };
    println!(
        "[send-input-poc] cursor position after move+click: ({}, {})",
        pos.x, pos.y
    );

    // --- フォーカス確認: SetForegroundWindowの戻り値は信用せず、実際に
    // GetForegroundWindow()が対象ウィンドウを指すまで確認する。確認できなければ
    // キー入力は一切送らずに中断する(別ウィンドウへの誤入力を防ぐため)。
    if !ensure_foreground(hwnd, Duration::from_secs(2)) {
        eprintln!(
            "[send-input-poc] ABORT: could not confirm hwnd={hwnd:?} actually has OS foreground \
             focus (GetForegroundWindow() never matched). Not sending any keyboard input to avoid \
             typing into the wrong window."
        );
        return Err(windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            "could not confirm target window has foreground focus",
        ));
    }
    println!("[send-input-poc] confirmed: GetForegroundWindow() == target hwnd");

    // --- キーボード: SendInput(KEYEVENTF_UNICODE)でウィンドウへ文字列を打ち込む ---
    println!("[send-input-poc] typing text via SendInput: {DEMO_TEXT:?}");
    type_unicode_text(DEMO_TEXT)?;
    std::thread::sleep(Duration::from_millis(300));

    // 打ち込んだ後もまだ対象ウィンドウがフォアグラウンドのままかを再確認しておく
    // (途中でフォーカスが奪われていれば、後半の文字が別の場所に入った可能性がある)。
    let still_foreground = unsafe { GetForegroundWindow() } == hwnd;
    println!("[send-input-poc] still foreground after typing: {still_foreground}");

    // --- ベストエフォート検証: メモ帳の子ウィンドウ(エディットコントロール)を読み返す ---
    match find_edit_child(hwnd) {
        Some(edit_hwnd) => {
            let readback = window_text_via_message(edit_hwnd);
            println!("[send-input-poc] readback from edit control: {readback:?}");
            if readback.contains(DEMO_TEXT) {
                println!("[send-input-poc] VERIFY: PASS (typed text found in edit control)");
            } else {
                println!(
                    "[send-input-poc] VERIFY: inconclusive (readback does not exactly contain the demo text; \
                     check the notepad window visually)"
                );
            }
        }
        None => {
            println!(
                "[send-input-poc] VERIFY: could not locate an edit control child window; \
                 check the notepad window visually"
            );
        }
    }

    println!(
        "[send-input-poc] done. notepad window left open (launcher_pid={launcher_pid}) for visual confirmation."
    );
    let _ = child.id(); // プロセスは終了させず、目視確認用に開いたままにする
    Ok(())
}

/// `SetForegroundWindow`単体は戻り値`TRUE`でも実際のフォーカス移動を保証しない
/// (前面ウィンドウの奪取にはWindows側の制限が複数ある)。`AttachThreadInput`で
/// 現在のフォアグラウンドスレッドと入力キューを結合した状態で呼ぶことで成功率を上げ、
/// その上で`GetForegroundWindow()`が実際に`hwnd`を指すまで確認する。
/// 確認できた場合のみ`true`を返す(呼び出し側はこれを見てからキー入力を送ること)。
fn ensure_foreground(hwnd: HWND, timeout: Duration) -> bool {
    unsafe {
        let current_foreground = GetForegroundWindow();
        let current_thread = GetCurrentThreadId();
        let foreground_thread = GetWindowThreadProcessId(current_foreground, None);
        let target_thread = GetWindowThreadProcessId(hwnd, None);

        let attached_to_foreground =
            if foreground_thread != current_thread && foreground_thread != 0 {
                AttachThreadInput(current_thread, foreground_thread, true).as_bool()
            } else {
                false
            };
        let attached_to_target = if target_thread != current_thread && target_thread != 0 {
            AttachThreadInput(current_thread, target_thread, true).as_bool()
        } else {
            false
        };

        let _ = SetForegroundWindow(hwnd);

        if attached_to_target {
            let _ = AttachThreadInput(current_thread, target_thread, false);
        }
        if attached_to_foreground {
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

fn move_cursor_absolute(x: i32, y: i32) -> windows::core::Result<()> {
    // MOUSEEVENTF_VIRTUALDESK指定時、MOUSEEVENTF_ABSOLUTEの座標系は仮想デスクトップ全体
    // (全モニター分、SM_XVIRTUALSCREEN起点)を0..65535に正規化したもの。
    // プライマリモニターだけを基準にすると、対象ウィンドウが別モニターにある場合に
    // 正規化がずれる(このバイナリ自身の以前の実装で実際にそうなっていた)。
    let vs_x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let vs_y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let vs_w = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) }.max(1);
    let vs_h = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) }.max(1);
    let abs_x = ((x - vs_x) as i64 * 65535 / vs_w as i64) as i32;
    let abs_y = ((y - vs_y) as i64 * 65535 / vs_h as i64) as i32;
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: abs_x,
                dy: abs_y,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    send_inputs(&[input])
}

fn click_left() -> windows::core::Result<()> {
    let down = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_LEFTDOWN,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let up = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_LEFTUP,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    send_inputs(&[down, up])
}

/// 文字列をUTF-16コード単位ごとに、KEYEVENTF_UNICODEのkeydown+keyupペアで送る。
/// 仮想キーコード変換(キーボードレイアウト依存)を経由しないため、日本語等の
/// レイアウト外文字も含めてそのまま打ち込める。
///
/// 全文字分のイベントを1回の`SendInput`にまとめて送ると、モダン化されたメモ帳の
/// (WinUI/XAMLベースと見られる)テキストコントロールが処理しきれず、文字の脱落・
/// 直前の文字の繰り返しといった文字化けが実際に発生した。1文字ずつ個別に
/// `SendInput`し、間に短い待機を挟むことで解決している。
///
/// この待機(50ms/文字)は「Windowsの入力キュー全般に必要な間隔」ではなく、
/// 検証に使ったモダン化メモ帳のWinUIコントロール固有の問題であることを、別途
/// `send_input_timing_probe`で確認済み: 自前で作った古典的なWin32 EDITコントロール
/// (システム標準の"EDIT"ウィンドウクラス)へは待機0msでも文字化けせず全文字が
/// 正しく入った。一方このメモ帳では15msでもまだ文字化けし、50msで初めて安定した。
/// つまり安全な間隔は相手のコントロール実装依存であり、単一の定数では一般化できない。
/// SARDP本体(仕様2.12節TextInput)で任意長の文章を打ち込む際は、相手アプリの種類を
/// 事前に知る手段がない以上、固定の保守的な間隔を使うか、エラー/読み返し結果に応じて
/// 動的に間隔を調整する設計を検討する必要がある(KNOWN_ISSUES.md参照)。
fn type_unicode_text(text: &str) -> windows::core::Result<()> {
    for unit in text.encode_utf16() {
        let down = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(0),
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
                    wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(0),
                    wScan: unit,
                    dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        send_inputs(&[down, up])?;
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
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

/// EnumWindowsで、可視トップレベルウィンドウの中からタイトルに"Notepad"
/// (英語UI)または"メモ帳"(日本語UI)を含むものが見つかるまで待つ。
fn wait_for_notepad_window(timeout: Duration) -> Option<HWND> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Some(hwnd) = find_notepad_window() {
            return Some(hwnd);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

struct FindContext {
    found: Option<HWND>,
}

fn find_notepad_window() -> Option<HWND> {
    let mut ctx = FindContext { found: None };
    unsafe {
        let _ = EnumWindows(
            Some(enum_windows_proc),
            LPARAM(&mut ctx as *mut FindContext as isize),
        );
    }
    ctx.found
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    let ctx = unsafe { &mut *(lparam.0 as *mut FindContext) };
    if unsafe { IsWindowVisible(hwnd) }.as_bool() {
        let title = window_text(hwnd);
        if title.contains("Notepad") || title.contains("メモ帳") {
            ctx.found = Some(hwnd);
            return windows::core::BOOL(0); // 見つかったので列挙を打ち切る
        }
    }
    windows::core::BOOL(1)
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

unsafe extern "system" fn enum_child_proc(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    let ctx = unsafe { &mut *(lparam.0 as *mut EditFindContext) };
    let len = unsafe { SendMessageW(hwnd, WM_GETTEXTLENGTH, Some(WPARAM(0)), Some(LPARAM(0))) };
    if len.0 > 0 || ctx.found.is_none() {
        // 最初に見つかった(かつテキストを持ちうる)子ウィンドウを候補にする。
        // メモ帳は通常エディットコントロール1つだけを子に持つ。
        ctx.found = Some(hwnd);
    }
    windows::core::BOOL(1)
}

fn window_text_via_message(hwnd: HWND) -> String {
    let len = unsafe { SendMessageW(hwnd, WM_GETTEXTLENGTH, Some(WPARAM(0)), Some(LPARAM(0))) }.0
        as usize;
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
    String::from_utf16_lossy(&buf)
        .trim_end_matches('\0')
        .to_string()
}

#[allow(dead_code)]
fn current_pid() -> u32 {
    unsafe { GetCurrentProcessId() }
}
