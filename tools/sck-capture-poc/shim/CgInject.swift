// CGEvent input injection + the Accessibility TCC gate, for SARDP 3M-1-c.
//
// Counterpart of `SendInput` in `sardp-win::inject`. This file is
// deliberately *dumb*: it posts exactly the event it is told to post, with
// the flags, click state and event kind the caller already decided. All
// the policy -- which CGEventFlags a modifier key sets, whether a move is
// a drag, what the click count is, how HID usages map to virtual keys --
// lives in `sardp-mac::inject`/`::keymap` in Rust, where it can be unit
// tested without a display or an Accessibility grant.
//
// Two macOS facts drive the shape of the C ABI:
//
//   - **Accessibility is a separate TCC gate from Screen Recording**, and
//     unlike screen recording it *does* get a prompt
//     (`AXIsProcessTrustedWithOptions`). Both checks are exposed so the
//     caller can distinguish "not granted" from "failed".
//   - **CGEvent works in points, not pixels.** Capture hands out a native
//     pixel buffer (3420x2224 here) while the display is 1710x1112 points,
//     so the caller converts; this file takes global point coordinates.
//
// The event tap at the bottom exists for verification, not for the
// product: it is how the PoC checks that a posted event actually reached
// the system rather than trusting `CGEvent.post` to be silent on failure.
// It can also *consume* the events it recognises as ours (by the
// `kCGEventSourceUserData` tag), which is what makes an end-to-end input
// test safe to run on a machine someone is using: the synthetic clicks and
// keystrokes are observed and then discarded before any application sees
// them, while real input passes through untouched.

import ApplicationServices
import CoreGraphics
import Foundation

public let SARDP_CG_OK: Int32 = 0
public let SARDP_CG_ERR_NOT_TRUSTED: Int32 = 1
public let SARDP_CG_ERR_EVENT: Int32 = 2
public let SARDP_CG_ERR_INVALID_HANDLE: Int32 = 3

/// Tag written into every posted event's `kCGEventSourceUserData` ("SARD"),
/// the macOS counterpart of `dwExtraInfo` on Windows. A `sardp-client`
/// running on the same machine as the server reads it back to tell its own
/// injected events apart from the user's and so avoid an echo loop.
public let SARDP_CG_USER_DATA: Int64 = 0x5341_5244

/// Whether this process is allowed to post events / read the UI, without
/// showing anything. The read-only half of the Accessibility gate.
@_cdecl("sardp_ax_is_trusted")
public func sardp_ax_is_trusted() -> Bool {
    return AXIsProcessTrusted()
}

/// Asks for Accessibility, showing the system prompt if the decision is
/// still pending for this code identity. Returns the state *now*, which
/// for a first request is false -- the grant is made in System Settings
/// and (like Screen Recording) is not observed by the running process.
@_cdecl("sardp_ax_request_trust")
public func sardp_ax_request_trust() -> Bool {
    let options = [kAXTrustedCheckOptionPrompt.takeUnretainedValue(): true] as CFDictionary
    return AXIsProcessTrustedWithOptions(options)
}

/// Owns the `CGEventSource` shared by every posted event.
final class Injector {
    let source: CGEventSource

    init?() {
        // `.privateState`, not `.combinedSessionState`: a combined source
        // inherits the *local* user's live modifier state, so a remote
        // click would pick up a Shift the person at the machine happens to
        // be holding. A private source starts empty and carries only the
        // flags we set, which is what makes the injected stream a function
        // of the protocol messages alone.
        guard let source = CGEventSource(stateID: .privateState) else { return nil }
        self.source = source
    }

    func stamp(_ event: CGEvent, flags: UInt64) {
        event.flags = CGEventFlags(rawValue: flags)
        event.setIntegerValueField(.eventSourceUserData, value: SARDP_CG_USER_DATA)
    }
}

/// Creates the event source. Fails if Accessibility is not granted, so the
/// caller finds out at start-up rather than on the first silently-dropped
/// event.
@_cdecl("sardp_cg_injector_create")
public func sardp_cg_injector_create(
    _ out_handle: UnsafeMutablePointer<UnsafeMutableRawPointer?>?,
    _ err: UnsafeMutablePointer<CChar>?,
    _ err_len: Int
) -> Int32 {
    guard let out_handle = out_handle else { return SARDP_CG_ERR_EVENT }
    out_handle.pointee = nil
    if !AXIsProcessTrusted() {
        putError(err, err_len, "accessibility permission not granted")
        return SARDP_CG_ERR_NOT_TRUSTED
    }
    guard let injector = Injector() else {
        putError(err, err_len, "CGEventSourceCreate failed")
        return SARDP_CG_ERR_EVENT
    }
    out_handle.pointee = Unmanaged.passRetained(injector).toOpaque()
    return SARDP_CG_OK
}

@_cdecl("sardp_cg_injector_destroy")
public func sardp_cg_injector_destroy(_ handle: UnsafeMutableRawPointer?) {
    guard let handle = handle else { return }
    _ = Unmanaged<Injector>.fromOpaque(handle).takeRetainedValue()
}

/// Posts one key down/up for a macOS virtual key code, with `flags` as the
/// complete modifier state the caller is tracking.
@_cdecl("sardp_cg_post_key")
public func sardp_cg_post_key(
    _ handle: UnsafeMutableRawPointer?,
    _ key_code: UInt16,
    _ down: Bool,
    _ flags: UInt64
) -> Int32 {
    guard let handle = handle else { return SARDP_CG_ERR_INVALID_HANDLE }
    let injector = Unmanaged<Injector>.fromOpaque(handle).takeUnretainedValue()
    guard let event = CGEvent(keyboardEventSource: injector.source,
                              virtualKey: key_code, keyDown: down) else {
        return SARDP_CG_ERR_EVENT
    }
    injector.stamp(event, flags: flags)
    event.post(tap: .cghidEventTap)
    return SARDP_CG_OK
}

/// Posts committed text as one keyboard event carrying a Unicode string
/// (the counterpart of `KEYEVENTF_UNICODE`). The virtual key is 0: no
/// physical key is claimed, only the characters.
@_cdecl("sardp_cg_post_text")
public func sardp_cg_post_text(
    _ handle: UnsafeMutableRawPointer?,
    _ utf8: UnsafePointer<UInt8>?,
    _ len: Int
) -> Int32 {
    guard let handle = handle else { return SARDP_CG_ERR_INVALID_HANDLE }
    guard let utf8 = utf8, len > 0 else { return SARDP_CG_OK }
    let injector = Unmanaged<Injector>.fromOpaque(handle).takeUnretainedValue()
    let bytes = UnsafeBufferPointer(start: utf8, count: len)
    guard let text = String(bytes: bytes, encoding: .utf8) else { return SARDP_CG_ERR_EVENT }
    var utf16 = Array(text.utf16)
    if utf16.isEmpty { return SARDP_CG_OK }

    // Down then up, as a real keystroke would be; an app that only watches
    // key-down still sees the text, and one that pairs them is not left
    // with an unbalanced press.
    for down in [true, false] {
        guard let event = CGEvent(keyboardEventSource: injector.source,
                                  virtualKey: 0, keyDown: down) else {
            return SARDP_CG_ERR_EVENT
        }
        // Text carries no modifier state: the characters are already
        // decided, and leaving Command set here would turn them into
        // shortcuts.
        injector.stamp(event, flags: 0)
        event.keyboardSetUnicodeString(stringLength: utf16.count, unicodeString: &utf16)
        event.post(tap: .cghidEventTap)
    }
    return SARDP_CG_OK
}

/// Posts a mouse event. `kind` is 0 = move, 1 = down, 2 = up, 3 = drag --
/// the caller decides, because whether a move is a drag depends on which
/// buttons it is holding, which is state it already tracks. `x`/`y` are
/// global display points. `click_state` is the click count (1, 2, 3) that
/// makes double-clicks register.
@_cdecl("sardp_cg_post_mouse")
public func sardp_cg_post_mouse(
    _ handle: UnsafeMutableRawPointer?,
    _ kind: UInt32,
    _ button: UInt32,
    _ x: Double,
    _ y: Double,
    _ click_state: Int64,
    _ flags: UInt64
) -> Int32 {
    guard let handle = handle else { return SARDP_CG_ERR_INVALID_HANDLE }
    let injector = Unmanaged<Injector>.fromOpaque(handle).takeUnretainedValue()

    // CGMouseButton: 0 left, 1 right, 2 centre, 3.. other.
    let cgButton: CGMouseButton
    switch button {
    case 1: cgButton = .right
    case 2: cgButton = .center
    case 3: cgButton = CGMouseButton(rawValue: 3) ?? .left
    case 4: cgButton = CGMouseButton(rawValue: 4) ?? .left
    default: cgButton = .left
    }
    let type: CGEventType
    switch (kind, button) {
    case (0, _): type = .mouseMoved
    case (1, 0): type = .leftMouseDown
    case (1, 1): type = .rightMouseDown
    case (1, _): type = .otherMouseDown
    case (2, 0): type = .leftMouseUp
    case (2, 1): type = .rightMouseUp
    case (2, _): type = .otherMouseUp
    case (3, 0): type = .leftMouseDragged
    case (3, 1): type = .rightMouseDragged
    case (3, _): type = .otherMouseDragged
    default: type = .mouseMoved
    }

    guard let event = CGEvent(mouseEventSource: injector.source, mouseType: type,
                              mouseCursorPosition: CGPoint(x: x, y: y),
                              mouseButton: cgButton) else {
        return SARDP_CG_ERR_EVENT
    }
    injector.stamp(event, flags: flags)
    if click_state > 0 {
        event.setIntegerValueField(.mouseEventClickState, value: click_state)
    }
    event.post(tap: .cghidEventTap)
    return SARDP_CG_OK
}

/// Posts a scroll. `lines_dy`/`lines_dx` are line units, positive = up /
/// right, matching the sign convention of spec 2.12's `Wheel`.
@_cdecl("sardp_cg_post_scroll")
public func sardp_cg_post_scroll(
    _ handle: UnsafeMutableRawPointer?,
    _ lines_dy: Int32,
    _ lines_dx: Int32,
    _ flags: UInt64
) -> Int32 {
    guard let handle = handle else { return SARDP_CG_ERR_INVALID_HANDLE }
    let injector = Unmanaged<Injector>.fromOpaque(handle).takeUnretainedValue()
    guard let event = CGEvent(scrollWheelEvent2Source: injector.source, units: .line,
                              wheelCount: 2, wheel1: lines_dy, wheel2: lines_dx, wheel3: 0) else {
        return SARDP_CG_ERR_EVENT
    }
    injector.stamp(event, flags: flags)
    event.post(tap: .cghidEventTap)
    return SARDP_CG_OK
}

/// Current cursor position in global points. Read back from the window
/// server, so a caller can check that a move actually took effect instead
/// of trusting that posting it was enough.
@_cdecl("sardp_cg_cursor_position")
public func sardp_cg_cursor_position(_ out: UnsafeMutablePointer<Double>?) -> Bool {
    guard let out = out, let event = CGEvent(source: nil) else { return false }
    out[0] = event.location.x
    out[1] = event.location.y
    return true
}

// MARK: - listen-only event tap (verification only)

/// Reports one observed event: `type` is the `CGEventType` raw value,
/// `key_code` the virtual key (keyboard events) or button number (mouse),
/// `click_state` the click count on a mouse event (0 otherwise), and
/// `user_data` the `kCGEventSourceUserData` field -- `SARDP_CG_USER_DATA`
/// for events this process injected.
public typealias SardpCgTapCallback = @convention(c) (
    _ ctx: UnsafeMutableRawPointer?,
    _ type: UInt32,
    _ key_code: Int64,
    _ flags: UInt64,
    _ x: Double,
    _ y: Double,
    _ click_state: Int64,
    _ user_data: Int64
) -> Void

final class Tap {
    let ctx: UnsafeMutableRawPointer?
    let callback: SardpCgTapCallback
    /// Swallow events carrying our own tag instead of letting them reach
    /// applications. Only ever applied to events we recognise as ours.
    let consumeTagged: Bool
    var machPort: CFMachPort?
    var runLoop: CFRunLoop?
    var thread: Thread?
    private let started = DispatchSemaphore(value: 0)
    private var ok = false

    init(ctx: UnsafeMutableRawPointer?, callback: @escaping SardpCgTapCallback,
         consumeTagged: Bool) {
        self.ctx = ctx
        self.callback = callback
        self.consumeTagged = consumeTagged
    }

    /// Runs the tap's run loop on its own thread; returns once the tap is
    /// installed (or failed).
    func start() -> Bool {
        let thread = Thread { [weak self] in self?.threadMain() }
        thread.name = "sardp-cg-tap"
        self.thread = thread
        thread.start()
        _ = started.wait(timeout: .now() + .seconds(5))
        return ok
    }

    private func threadMain() {
        let mask: CGEventMask =
            (1 << CGEventType.keyDown.rawValue) |
            (1 << CGEventType.keyUp.rawValue) |
            (1 << CGEventType.flagsChanged.rawValue) |
            (1 << CGEventType.mouseMoved.rawValue) |
            (1 << CGEventType.leftMouseDown.rawValue) |
            (1 << CGEventType.leftMouseUp.rawValue) |
            (1 << CGEventType.leftMouseDragged.rawValue) |
            (1 << CGEventType.rightMouseDown.rawValue) |
            (1 << CGEventType.rightMouseUp.rawValue) |
            (1 << CGEventType.otherMouseDown.rawValue) |
            (1 << CGEventType.otherMouseUp.rawValue) |
            (1 << CGEventType.scrollWheel.rawValue)
        guard let port = CGEvent.tapCreate(
            tap: .cgSessionEventTap,
            // Head-insert when filtering, so nothing else acts on a
            // synthetic event before it is discarded.
            place: consumeTagged ? .headInsertEventTap : .tailAppendEventTap,
            options: consumeTagged ? .defaultTap : .listenOnly,
            eventsOfInterest: mask,
            callback: tapCallback,
            userInfo: Unmanaged.passUnretained(self).toOpaque()
        ) else {
            ok = false
            started.signal()
            return
        }
        machPort = port
        let source = CFMachPortCreateRunLoopSource(kCFAllocatorDefault, port, 0)
        runLoop = CFRunLoopGetCurrent()
        CFRunLoopAddSource(runLoop, source, .commonModes)
        CGEvent.tapEnable(tap: port, enable: true)
        ok = true
        started.signal()
        CFRunLoopRun()
    }

    func stop() {
        if let port = machPort {
            CGEvent.tapEnable(tap: port, enable: false)
            CFMachPortInvalidate(port)
        }
        if let runLoop = runLoop {
            CFRunLoopStop(runLoop)
        }
        machPort = nil
        runLoop = nil
    }

    /// Reports the event and says whether it should be discarded.
    fileprivate func deliver(_ type: CGEventType, _ event: CGEvent) -> Bool {
        let keyCode: Int64
        var clickState: Int64 = 0
        switch type {
        case .keyDown, .keyUp, .flagsChanged:
            keyCode = event.getIntegerValueField(.keyboardEventKeycode)
        default:
            keyCode = event.getIntegerValueField(.mouseEventButtonNumber)
            clickState = event.getIntegerValueField(.mouseEventClickState)
        }
        let userData = event.getIntegerValueField(.eventSourceUserData)
        callback(ctx, type.rawValue, keyCode, event.flags.rawValue,
                 event.location.x, event.location.y, clickState, userData)
        // Never swallow anything that is not demonstrably ours: a bug here
        // would eat the real user's input.
        return consumeTagged && userData == SARDP_CG_USER_DATA
    }
}

private func tapCallback(
    proxy: CGEventTapProxy,
    type: CGEventType,
    event: CGEvent,
    userInfo: UnsafeMutableRawPointer?
) -> Unmanaged<CGEvent>? {
    guard let userInfo = userInfo else { return Unmanaged.passUnretained(event) }
    let tap = Unmanaged<Tap>.fromOpaque(userInfo).takeUnretainedValue()
    // The system disables a tap that takes too long or is interrupted; it
    // says so through these pseudo-types and stays dead until re-enabled.
    if type == .tapDisabledByTimeout || type == .tapDisabledByUserInput {
        if let port = tap.machPort {
            CGEvent.tapEnable(tap: port, enable: true)
        }
        return Unmanaged.passUnretained(event)
    }
    if tap.deliver(type, event) {
        return nil
    }
    return Unmanaged.passUnretained(event)
}

/// `consume_tagged`: discard events carrying `SARDP_CG_USER_DATA` instead
/// of letting them through. Everything else always passes.
@_cdecl("sardp_cg_tap_start")
public func sardp_cg_tap_start(
    _ ctx: UnsafeMutableRawPointer?,
    _ callback: SardpCgTapCallback,
    _ consume_tagged: Bool,
    _ out_handle: UnsafeMutablePointer<UnsafeMutableRawPointer?>?,
    _ err: UnsafeMutablePointer<CChar>?,
    _ err_len: Int
) -> Int32 {
    guard let out_handle = out_handle else { return SARDP_CG_ERR_EVENT }
    out_handle.pointee = nil
    if !AXIsProcessTrusted() {
        putError(err, err_len, "accessibility permission not granted (an event tap needs it too)")
        return SARDP_CG_ERR_NOT_TRUSTED
    }
    let tap = Tap(ctx: ctx, callback: callback, consumeTagged: consume_tagged)
    guard tap.start() else {
        putError(err, err_len, "CGEvent.tapCreate failed")
        return SARDP_CG_ERR_EVENT
    }
    out_handle.pointee = Unmanaged.passRetained(tap).toOpaque()
    return SARDP_CG_OK
}

@_cdecl("sardp_cg_tap_stop")
public func sardp_cg_tap_stop(_ handle: UnsafeMutableRawPointer?) {
    guard let handle = handle else { return }
    let tap = Unmanaged<Tap>.fromOpaque(handle).takeRetainedValue()
    tap.stop()
}
