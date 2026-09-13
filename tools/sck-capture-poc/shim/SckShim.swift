// ScreenCaptureKit shim for SARDP (3M-1-a).
//
// ScreenCaptureKit is a Swift/Objective-C-first API (async methods,
// delegate protocols, CMSampleBuffer attachments keyed by NSString). This
// file is the *only* place that talks to it; everything it exports is a
// plain C ABI (`@_cdecl`) so the Rust side (`src/shim.rs`) sees nothing
// but functions, scalars and callbacks.
//
// Design rules (carried over from 3W-1):
//   - Every OS object lives inside `Session`; `sardp_sck_stop` (or the
//     Rust `Drop`) releases it on every path, not only the happy one.
//   - We never trust "started OK": `startCapture()` returning without an
//     error is not the same as frames arriving, so the caller waits for
//     the first frame with a timeout (`SCFrameStatus.started`/`complete`).
//   - Frames are handed to the callback synchronously while the pixel
//     buffer is locked; the callback must copy what it needs and return.

import CoreGraphics
import CoreMedia
import CoreVideo
import Foundation
import ScreenCaptureKit

/// Called on the stream's sample-handler queue for every sample buffer
/// SCK delivers (including `idle`/`blank` frames that carry no new image;
/// then `bgra` is NULL and width/height/stride are 0).
///
/// `dirty` points at `dirty_count * 4` doubles: x, y, w, h of each dirty
/// rect in *points* (the stream's content coordinate space, origin
/// top-left of the captured display). `content_scale`/`scale_factor` are
/// the SCK attachments of the same name (points -> pixels multipliers).
public typealias SardpSckFrameCallback = @convention(c) (
    _ ctx: UnsafeMutableRawPointer?,
    _ bgra: UnsafePointer<UInt8>?,
    _ width: UInt32,
    _ height: UInt32,
    _ stride: UInt32,
    _ status: Int32,
    _ display_time_ns: UInt64,
    _ dirty: UnsafePointer<Double>?,
    _ dirty_count: UInt32,
    _ content_scale: Double,
    _ scale_factor: Double
) -> Void

/// Called (once) if the stream stops on its own with an error.
public typealias SardpSckStoppedCallback = @convention(c) (
    _ ctx: UnsafeMutableRawPointer?,
    _ message: UnsafePointer<CChar>?
) -> Void

/// Status codes returned by `sardp_sck_start`.
public let SARDP_SCK_OK: Int32 = 0
public let SARDP_SCK_ERR_NO_PERMISSION: Int32 = 1
public let SARDP_SCK_ERR_NO_DISPLAY: Int32 = 2
public let SARDP_SCK_ERR_START_FAILED: Int32 = 3
public let SARDP_SCK_ERR_TIMEOUT: Int32 = 4

/// mach_absolute_time ticks -> nanoseconds. The timebase is constant for
/// the life of the process, so it is read once.
private let machTimebase: mach_timebase_info_data_t = {
    var tb = mach_timebase_info_data_t()
    mach_timebase_info(&tb)
    if tb.numer == 0 || tb.denom == 0 { tb.numer = 1; tb.denom = 1 }
    return tb
}()

private func machTicksToNanos(_ ticks: UInt64) -> UInt64 {
    let numer = UInt64(machTimebase.numer)
    let denom = UInt64(machTimebase.denom)
    if numer == denom { return ticks }
    // Split to keep the multiply from overflowing on long uptimes.
    let whole = ticks / denom
    let rem = ticks % denom
    return whole &* numer &+ (rem &* numer) / denom
}

final class Session: NSObject, SCStreamOutput, SCStreamDelegate {
    let stream: SCStream
    let queue = DispatchQueue(label: "sardp.sck.frames", qos: .userInteractive)
    let ctx: UnsafeMutableRawPointer?
    let onFrame: SardpSckFrameCallback
    let onStopped: SardpSckStoppedCallback?
    // Reused across frames so we do not allocate per dirty rect.
    var dirtyScratch: [Double] = []

    init(stream: SCStream, ctx: UnsafeMutableRawPointer?,
         onFrame: @escaping SardpSckFrameCallback,
         onStopped: SardpSckStoppedCallback?) {
        self.stream = stream
        self.ctx = ctx
        self.onFrame = onFrame
        self.onStopped = onStopped
    }

    func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer,
                of type: SCStreamOutputType) {
        guard type == .screen else { return }
        guard let attachments = CMSampleBufferGetSampleAttachmentsArray(
                sampleBuffer, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]],
              let first = attachments.first else {
            return
        }
        let statusRaw = first[.status] as? Int ?? -1
        // `.displayTime` is a raw mach_absolute_time *tick* count, not
        // seconds and not nanoseconds: on Apple silicon the timebase is
        // 125/3 (24 MHz), so treating it as seconds and scaling by 1e9
        // overflows UInt64 and traps. Convert through mach_timebase_info.
        let displayTimeTicks = (first[.displayTime] as? NSNumber)?.uint64Value ?? 0
        let contentScale = first[.contentScale] as? Double ?? 0
        let scaleFactor = first[.scaleFactor] as? Double ?? 0

        dirtyScratch.removeAll(keepingCapacity: true)
        if let rects = first[.dirtyRects] as? [Any] {
            for r in rects {
                if let dict = r as? NSDictionary,
                   let rect = CGRect(dictionaryRepresentation: dict) {
                    dirtyScratch.append(rect.origin.x)
                    dirtyScratch.append(rect.origin.y)
                    dirtyScratch.append(rect.size.width)
                    dirtyScratch.append(rect.size.height)
                }
            }
        }
        let dirtyCount = UInt32(dirtyScratch.count / 4)
        let displayTimeNs = machTicksToNanos(displayTimeTicks)

        // `.complete` / `.started` carry a new image. SCK still delivers a
        // sample buffer for `.idle` (no change) with the previous image
        // attached; the caller decides what to do with it via `status`.
        guard let pixelBuffer = CMSampleBufferGetImageBuffer(sampleBuffer) else {
            dirtyScratch.withUnsafeBufferPointer { d in
                onFrame(ctx, nil, 0, 0, 0, Int32(statusRaw), displayTimeNs,
                        d.baseAddress, dirtyCount, contentScale, scaleFactor)
            }
            return
        }
        CVPixelBufferLockBaseAddress(pixelBuffer, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(pixelBuffer, .readOnly) }
        let width = UInt32(CVPixelBufferGetWidth(pixelBuffer))
        let height = UInt32(CVPixelBufferGetHeight(pixelBuffer))
        let stride = UInt32(CVPixelBufferGetBytesPerRow(pixelBuffer))
        let base = CVPixelBufferGetBaseAddress(pixelBuffer)?
            .assumingMemoryBound(to: UInt8.self)
        dirtyScratch.withUnsafeBufferPointer { d in
            onFrame(ctx, base, width, height, stride, Int32(statusRaw), displayTimeNs,
                    d.baseAddress, dirtyCount, contentScale, scaleFactor)
        }
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        let msg = "\(error)"
        msg.withCString { onStopped?(ctx, $0) }
    }
}

private func putError(_ buf: UnsafeMutablePointer<CChar>?, _ len: Int, _ msg: String) {
    guard let buf = buf, len > 0 else { return }
    let bytes = Array(msg.utf8.prefix(len - 1))
    for (i, b) in bytes.enumerated() { buf[i] = CChar(bitPattern: b) }
    buf[bytes.count] = 0
}

/// Read-only: whether this process may capture the screen. Never prompts.
@_cdecl("sardp_sck_preflight")
public func sardp_sck_preflight() -> Bool {
    return CGPreflightScreenCaptureAccess()
}

/// Prompts (once per TCC decision) via the system Screen Recording dialog;
/// returns the *current* state, which for a first request is `false` --
/// the grant only becomes visible after the user acts in System Settings.
@_cdecl("sardp_sck_request_access")
public func sardp_sck_request_access() -> Bool {
    return CGRequestScreenCaptureAccess()
}

/// Display geometry: fills `out` with [display_id, points_w, points_h,
/// pixels_w, pixels_h, origin_x, origin_y, refresh_hz] for the main
/// display. Returns false if no display.
@_cdecl("sardp_sck_main_display_info")
public func sardp_sck_main_display_info(_ out: UnsafeMutablePointer<Double>?) -> Bool {
    guard let out = out else { return false }
    let id = CGMainDisplayID()
    let bounds = CGDisplayBounds(id)
    var pw = 0, ph = 0, hz = 0.0
    if let mode = CGDisplayCopyDisplayMode(id) {
        pw = mode.pixelWidth
        ph = mode.pixelHeight
        hz = mode.refreshRate
    }
    out[0] = Double(id)
    out[1] = Double(bounds.width)
    out[2] = Double(bounds.height)
    out[3] = Double(pw)
    out[4] = Double(ph)
    out[5] = Double(bounds.origin.x)
    out[6] = Double(bounds.origin.y)
    out[7] = hz
    return true
}

/// Starts capturing the main display. Blocks until SCK reports the stream
/// started (or fails / times out after `timeout_ms`). On success writes an
/// opaque session pointer to `out_handle` and returns SARDP_SCK_OK.
///
/// `fps`: minimum frame interval = 1/fps. `pixel_format`: 0 = 32BGRA,
/// 1 = NV12 (420v, for VideoToolbox in 3M-1-b). `shows_cursor`: DR-004
/// says the client draws the cursor, so normally false.
@_cdecl("sardp_sck_start")
public func sardp_sck_start(
    _ fps: UInt32,
    _ pixel_format: UInt32,
    _ shows_cursor: Bool,
    _ timeout_ms: UInt32,
    _ skip_preflight: Bool,
    _ ctx: UnsafeMutableRawPointer?,
    _ on_frame: SardpSckFrameCallback,
    _ on_stopped: SardpSckStoppedCallback?,
    _ out_handle: UnsafeMutablePointer<UnsafeMutableRawPointer?>?,
    _ err: UnsafeMutablePointer<CChar>?,
    _ err_len: Int
) -> Int32 {
    guard let out_handle = out_handle else { return SARDP_SCK_ERR_START_FAILED }
    out_handle.pointee = nil

    // Belt and braces: SCK would prompt on its own, but we want the
    // no-permission path to be explicit and fast (roadmap: "権限待ちで
    // あることが分かる形で待機・再試行できること"). `skip_preflight` lets
    // the PoC exercise SCK's own prompt path (SCShareableContent).
    if !skip_preflight && !CGPreflightScreenCaptureAccess() {
        putError(err, err_len, "screen recording permission not granted (CGPreflightScreenCaptureAccess=false)")
        return SARDP_SCK_ERR_NO_PERMISSION
    }

    let box = StartBox()

    Task {
        var result: Int32 = SARDP_SCK_ERR_START_FAILED
        var errMsg = ""
        var session: Session? = nil
        defer {
            // If the caller already gave up (timeout), the session we just
            // built must not leak a running capture: stop it here.
            if let abandoned = box.finish(result: result, error: errMsg, session: session) {
                Task { try? await abandoned.stream.stopCapture() }
            }
        }
        do {
            let content = try await SCShareableContent.excludingDesktopWindows(
                false, onScreenWindowsOnly: true)
            let mainID = CGMainDisplayID()
            guard let display = content.displays.first(where: { $0.displayID == mainID })
                    ?? content.displays.first else {
                errMsg = "SCShareableContent returned no displays"
                result = SARDP_SCK_ERR_NO_DISPLAY
                return
            }
            let filter = SCContentFilter(display: display, excludingWindows: [])
            let config = SCStreamConfiguration()
            // Full native resolution: SCDisplay.width/height are points.
            var pw = display.width, ph = display.height
            if let mode = CGDisplayCopyDisplayMode(display.displayID) {
                pw = mode.pixelWidth
                ph = mode.pixelHeight
            }
            config.width = pw
            config.height = ph
            config.minimumFrameInterval = CMTime(value: 1, timescale: CMTimeScale(max(fps, 1)))
            config.pixelFormat = pixel_format == 1
                ? kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
                : kCVPixelFormatType_32BGRA
            config.showsCursor = shows_cursor
            config.queueDepth = 3
            if #available(macOS 14.0, *) {
                config.captureResolution = .best
            }
            let stream = SCStream(filter: filter, configuration: config, delegate: nil)
            let s = Session(stream: stream, ctx: ctx, onFrame: on_frame, onStopped: on_stopped)
            try stream.addStreamOutput(s, type: .screen, sampleHandlerQueue: s.queue)
            try await stream.startCapture()
            session = s
            result = SARDP_SCK_OK
        } catch {
            errMsg = "\(error)"
            result = SARDP_SCK_ERR_START_FAILED
        }
    }

    guard let (result, errMsg, session) = box.wait(ms: Int(timeout_ms)) else {
        putError(err, err_len, "SCK start did not complete within \(timeout_ms) ms")
        return SARDP_SCK_ERR_TIMEOUT
    }
    if result != SARDP_SCK_OK {
        putError(err, err_len, errMsg)
        return result
    }
    guard let s = session else {
        putError(err, err_len, "internal: no session after successful start")
        return SARDP_SCK_ERR_START_FAILED
    }
    out_handle.pointee = Unmanaged.passRetained(s).toOpaque()
    return SARDP_SCK_OK
}

/// Hand-off between the synchronous C entry point and the async start
/// Task, with the "caller timed out" case made explicit so a late
/// success is stopped rather than leaked.
final class StartBox: @unchecked Sendable {
    private let lock = NSLock()
    private let sema = DispatchSemaphore(value: 0)
    private var outcome: (Int32, String, Session?)? = nil
    private var abandoned = false

    /// Called by the Task when done. Returns the session back if the
    /// caller has already given up, so the Task can stop it.
    func finish(result: Int32, error: String, session: Session?) -> Session? {
        lock.lock(); defer { lock.unlock() }
        if abandoned { return session }
        outcome = (result, error, session)
        sema.signal()
        return nil
    }

    /// Called by the C entry point. `nil` = timed out (and marks the box
    /// abandoned so a late `finish` stops its session).
    func wait(ms: Int) -> (Int32, String, Session?)? {
        if sema.wait(timeout: .now() + .milliseconds(ms)) == .timedOut {
            lock.lock(); defer { lock.unlock() }
            if let o = outcome { return o } // raced: finished just now
            abandoned = true
            return nil
        }
        lock.lock(); defer { lock.unlock() }
        return outcome
    }
}

/// Stops the stream and releases everything. Safe to call once per
/// handle; blocks until SCK acknowledges the stop (bounded).
@_cdecl("sardp_sck_stop")
public func sardp_sck_stop(_ handle: UnsafeMutableRawPointer?) {
    guard let handle = handle else { return }
    let s = Unmanaged<Session>.fromOpaque(handle).takeRetainedValue()
    let sema = DispatchSemaphore(value: 0)
    Task {
        defer { sema.signal() }
        do { try await s.stream.stopCapture() } catch { /* already stopped */ }
    }
    _ = sema.wait(timeout: .now() + .seconds(5))
    // After stopCapture no more callbacks arrive; the output can be
    // detached and the object released with `s`.
    try? s.stream.removeStreamOutput(s, type: .screen)
}
