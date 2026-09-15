// On-screen window shim for SARDP (3M-1-e).
//
// Counterpart of the Win32 window in `sardp-win/src/display.rs`: shows
// whatever `VtDecoder.swift` decodes, with the same "no CPU pixel copy"
// property DXVA + `ID3D11VideoProcessor` gets on Windows -- a decoded
// frame's `CVPixelBuffer` is IOSurface-backed (`VtDecoder`'s
// `destAttrs`), and `CALayer.contents` accepts an `IOSurface` directly,
// so presenting a frame is one property assignment, not a blit.
//
// **`NSWindow` may only be instantiated on the process's real main
// thread.** This is enforced, not advisory: doing it from any other
// thread raises an uncaught `NSInternalInconsistencyException`
// ("NSWindow should only be instantiated on the main thread!"), which
// crosses the Swift/Rust FFI boundary as a foreign exception Rust cannot
// catch -- `fatal runtime error: Rust cannot catch foreign exceptions,
// aborting`, an immediate abort with no `Result` to have checked first.
// A standalone probe that first suggested otherwise (a plain window
// presenting a synthetic `CVPixelBuffer`, screenshotted with
// `SCScreenshotManager` to verify without asking a human to look) was
// itself run as a bare executable, where `main()` runs on thread 0 --
// it never actually exercised the case that matters, a window created
// from a *spawned* thread while thread 0 is doing something else, which
// is exactly `sardp-client`'s shape (`#[tokio::main]` occupies thread 0).
// Rerunning that probe with the window created on an explicitly spawned
// `Thread` reproduced the crash immediately and is what this rests on
// instead. Every function here therefore assumes it is being called on
// the real main thread; the caller (`sardp_capture_poc::main_thread`,
// `sardp-cli`'s restructured `main` on macOS with `--display window`) is
// what makes that actually true, not anything in this file.
//
// `.activationPolicy = .accessory` (no Dock icon, does not steal focus
// from whatever app the user is in) is the same choice every other
// bundle this project ships makes with `LSUIElement=true`.
// `NSApp.nextEvent(until: .distantPast)`, drained in a loop, is the
// non-blocking "service the window server, never block" shape
// `sardp-win`'s `pump_messages()` has for the same job, on the other
// platform's very different API for it.

import AppKit
import CoreVideo
import Foundation
import IOSurface
import QuartzCore

public let SARDP_WIN_ERR_NONE: Int32 = 0

/// One process needs at most one `NSApplication`; every window shares it.
/// `.accessory`: visible windows, no Dock icon, does not take over as the
/// frontmost app (so opening a SARDP display window does not yank focus
/// away from whatever the user was doing -- the same choice this
/// project's other bundles make with `LSUIElement=true`).
private func ensureApplication() {
    struct Once { static let run: Void = {
        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)
    }() }
    _ = Once.run
}

private final class CloseDelegate: NSObject, NSWindowDelegate {
    let shouldClose = NSLock()
    private var closed = false
    func windowShouldClose(_ sender: NSWindow) -> Bool {
        shouldClose.lock()
        closed = true
        shouldClose.unlock()
        return true
    }
    func isClosed() -> Bool {
        shouldClose.lock(); defer { shouldClose.unlock() }
        return closed
    }
}

final class DisplayWindow {
    private let window: NSWindow
    private let layer: CALayer
    private let delegate: CloseDelegate

    init(widthPt: Int, heightPt: Int, title: String) {
        ensureApplication()
        let rect = NSRect(x: 0, y: 0, width: widthPt, height: heightPt)
        let win = NSWindow(
            contentRect: rect,
            styleMask: [.titled, .closable, .miniaturizable],
            backing: .buffered, defer: false)
        win.title = title
        // The window (and its `CloseDelegate`) must survive independent
        // of any AppKit-side retain count guess; `destroy()` drops it.
        win.isReleasedWhenClosed = false
        let contentLayer = CALayer()
        contentLayer.backgroundColor = NSColor.black.cgColor
        // Matches `sardp-win`'s "frames are scaled to fit" (the video
        // processor's output size is the window's, independent of the
        // stream's): the layer scales whatever IOSurface it is given to
        // fit the window without this class doing any scale math itself.
        contentLayer.contentsGravity = .resizeAspect
        win.contentView?.wantsLayer = true
        win.contentView?.layer = contentLayer
        let delegate = CloseDelegate()
        win.delegate = delegate
        self.window = win
        self.layer = contentLayer
        self.delegate = delegate
        win.orderFrontRegardless()
        win.center()
        pump()
        if ProcessInfo.processInfo.environment["SARDP_WIN_TRACE"] != nil {
            FileHandle.standardError.write(Data("""
                [sardp-win trace] frame=\(win.frame) isVisible=\(win.isVisible) \
                screen=\(String(describing: win.screen?.frame)) \
                occlusionState=\(win.occlusionState.rawValue) \
                appIsActive=\(NSApp.isActive)\n
                """.utf8))
        }
    }

    /// Sets the layer's content to the given pixel buffer's IOSurface (no
    /// copy) and releases the caller's retained reference to it either
    /// way. Returns whether the buffer was actually IOSurface-backed --
    /// every buffer `VtDecoder.configure` produces should be, so `false`
    /// is worth logging, not silently swallowing.
    func present(_ pixelBuffer: CVPixelBuffer) -> Bool {
        guard let surface = CVPixelBufferGetIOSurface(pixelBuffer) else {
            return false
        }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        layer.contents = surface.takeUnretainedValue()
        CATransaction.commit()
        return true
    }

    /// Non-blocking: drains whatever the window server/AppKit has queued
    /// (close-box clicks, window-manager notifications, layer commits)
    /// without waiting for anything. Must be called reasonably often
    /// (the same job `sardp-win`'s `pump_messages()` does in its own
    /// bounded-wait loop) or the window stops responding and, per the
    /// file doc, may not even become visible in the first place.
    func pump() {
        let app = NSApplication.shared
        while let event = app.nextEvent(
            matching: .any, until: Date.distantPast, inMode: .default, dequeue: true)
        {
            app.sendEvent(event)
        }
    }

    /// Ground truth, not the caller's assumption: whether the window
    /// server currently considers this window on screen.
    func isVisible() -> Bool {
        window.isVisible
    }

    func shouldClose() -> Bool {
        delegate.isClosed()
    }

    func close() {
        window.close()
        pump()
    }
}

// MARK: - C ABI

/// Creates and shows a window (client-area size in points; frames are
/// scaled to fit, see `contentsGravity` above). Never fails -- if AppKit
/// itself is unusable in this process, that surfaces as `isVisible`
/// staying false forever, which the caller (`sardp_mac::display`) treats
/// as an init failure after one poll interval, not as a crash.
@_cdecl("sardp_win_create")
public func sardp_win_create(
    _ width_pt: Int32, _ height_pt: Int32, _ title: UnsafePointer<CChar>?
) -> UnsafeMutableRawPointer {
    let titleStr = title.map { String(cString: $0) } ?? "SARDP"
    let win = DisplayWindow(
        widthPt: Int(max(width_pt, 1)), heightPt: Int(max(height_pt, 1)), title: titleStr)
    return Unmanaged.passRetained(win).toOpaque()
}

/// Presents a decoded frame. `pixel_buffer` is a **retained**
/// `CVPixelBufferRef` (from `sardp_vtdec_decode`'s `out_pixel_buffer`);
/// this function always consumes (releases) it, whether or not it could
/// be shown. Returns whether it actually had an IOSurface to show.
@_cdecl("sardp_win_present")
public func sardp_win_present(
    _ handle: UnsafeMutableRawPointer?, _ pixel_buffer: UnsafeMutableRawPointer?
) -> Bool {
    guard let pixel_buffer = pixel_buffer else { return false }
    let buffer = Unmanaged<CVPixelBuffer>.fromOpaque(pixel_buffer).takeRetainedValue()
    guard let handle = handle else { return false }
    let win = Unmanaged<DisplayWindow>.fromOpaque(handle).takeUnretainedValue()
    return win.present(buffer)
}

/// Drains pending window-server/AppKit events without blocking. Call at
/// least as often as `sardp-win`'s `pump_messages()` -- every poll
/// iteration of the display worker's loop, not just when a frame arrives.
@_cdecl("sardp_win_pump")
public func sardp_win_pump(_ handle: UnsafeMutableRawPointer?) {
    guard let handle = handle else { return }
    Unmanaged<DisplayWindow>.fromOpaque(handle).takeUnretainedValue().pump()
}

/// Whether the window server currently shows this window (read back, not
/// assumed from `sardp_win_create` having returned).
@_cdecl("sardp_win_is_visible")
public func sardp_win_is_visible(_ handle: UnsafeMutableRawPointer?) -> Bool {
    guard let handle = handle else { return false }
    return Unmanaged<DisplayWindow>.fromOpaque(handle).takeUnretainedValue().isVisible()
}

/// Whether the user clicked the window's close box since creation (or
/// the last check -- this is sticky, it does not reset).
@_cdecl("sardp_win_should_close")
public func sardp_win_should_close(_ handle: UnsafeMutableRawPointer?) -> Bool {
    guard let handle = handle else { return true }
    return Unmanaged<DisplayWindow>.fromOpaque(handle).takeUnretainedValue().shouldClose()
}

/// Closes the window and releases it. Call exactly once per handle.
@_cdecl("sardp_win_destroy")
public func sardp_win_destroy(_ handle: UnsafeMutableRawPointer?) {
    guard let handle = handle else { return }
    let win = Unmanaged<DisplayWindow>.fromOpaque(handle).takeRetainedValue()
    win.close()
}

/// Drains AppKit's event queue without blocking, same as
/// `sardp_win_pump`, but callable before any window exists (the
/// `main_thread` run loop's very first iterations). Instantiates
/// `NSApplication.shared` on its own, same as `DisplayWindow.init`, so
/// calling this before the first window is created is exactly as safe as
/// calling it after.
@_cdecl("sardp_appkit_pump_main_thread")
public func sardp_appkit_pump_main_thread() {
    ensureApplication()
    let app = NSApplication.shared
    while let event = app.nextEvent(
        matching: .any, until: Date.distantPast, inMode: .default, dequeue: true)
    {
        app.sendEvent(event)
    }
}
