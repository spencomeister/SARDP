// VideoToolbox H.264 encoder shim for SARDP (3M-1-b).
//
// Counterpart of the Media Foundation encoder in `sardp-win`: takes the
// `CVPixelBuffer` ScreenCaptureKit already produced (no CPU copy, no
// colour conversion step -- SCK is asked for NV12 directly, which is what
// the encoder wants) and hands finished **Annex-B** access units back
// through a C callback.
//
// Design rules carried over from 3W-1 and restated for VideoToolbox:
//
//   - Every OS object is released on every path, not just the happy one.
//     `VTCompressionSessionCreate` succeeding but a later property set
//     failing still has to invalidate the session, and `deinit` is the
//     backstop. `VTCompressionSessionInvalidate` is the documented
//     teardown and is called unconditionally -- though measured, skipping
//     it made no observable difference on macOS 26.6.2 / M4 (see
//     KNOWN_ISSUES #23): ARC's release appears to tear the session down
//     too. The discipline stands because the contract says so, not
//     because this machine punishes breaking it.
//   - Never trust an API's own account of what it did. Specifically:
//     * `EnableHardwareAcceleratedVideoEncoder` is a *request*; whether
//       hardware is actually in use is read back from
//       `kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder`.
//     * `VTCompressionSessionEncodeFrame` returning `noErr` does not mean
//       a frame comes out: `infoFlagsOut` may say `frameDropped`, and the
//       asynchronous callback carries its own `status`. Both are counted.
//     * The `NotSync` sample attachment is passed to the caller as a hint
//       only; whether an access unit is really an IDR is decided from the
//       NAL units by `sardp::h264` (3W-1-b/d-2 found the MFT's keyframe
//       flag unreliable, and VideoToolbox has no better claim to trust).
//     * `CMBlockBuffer`s are not necessarily contiguous; the range is
//       checked and made contiguous rather than assumed.
//
// VideoToolbox emits length-prefixed (AVCC) NAL units and keeps SPS/PPS
// *only* in the format description -- an IDR's bytes do not contain them.
// This file converts the length prefixes to Annex-B start codes and
// reports the parameter sets separately (whenever they change) so that
// `sardp::h264::ParameterSetCache` can prepend them and keep every IDR
// self-contained as spec 2.10 requires. That is exactly the role
// `read_sequence_header` plays on Windows.

import CoreMedia
import CoreVideo
import Foundation
import VideoToolbox

/// Called for every encoded access unit, on a VideoToolbox thread (not the
/// caller's). Calls for one session are serialized.
///
/// `annex_b` / `len` are only valid for the duration of the call. `is_sync`
/// is VideoToolbox's own keyframe claim -- a hint, not the decision.
/// `capture_ts_us` is the value the caller passed to
/// `sardp_vt_encoder_encode`, carried through as the presentation stamp.
/// `param_sets` is non-NULL only when the parameter sets changed (first
/// frame, or a format change): SPS and PPS as Annex-B, ready to prepend.
public typealias SardpVtEncodedCallback = @convention(c) (
    _ ctx: UnsafeMutableRawPointer?,
    _ annex_b: UnsafePointer<UInt8>?,
    _ len: Int,
    _ is_sync: Bool,
    _ capture_ts_us: UInt64,
    _ param_sets: UnsafePointer<UInt8>?,
    _ param_sets_len: Int
) -> Void

public let SARDP_VT_OK: Int32 = 0
public let SARDP_VT_ERR_CREATE: Int32 = 1
public let SARDP_VT_ERR_PROPERTY: Int32 = 2
public let SARDP_VT_ERR_ENCODE: Int32 = 3
public let SARDP_VT_ERR_INVALID_HANDLE: Int32 = 4

/// Timescale of the presentation stamps: the caller's clock is in
/// microseconds (`sardp::clock::now_us`), so the stamp round-trips
/// exactly and comes back out of the sample buffer unchanged.
private let ptsTimescale: CMTimeScale = 1_000_000

final class VtEncoder {
    private var session: VTCompressionSession?
    private let ctx: UnsafeMutableRawPointer?
    private let onEncoded: SardpVtEncodedCallback
    private let fps: UInt32
    private let allIdr: Bool

    /// Serializes output handling. VideoToolbox delivers callbacks in
    /// order, but the scratch buffers below are shared state and the Rust
    /// callback must not be re-entered concurrently.
    private let lock = NSLock()
    /// Counters are read from the caller's thread while the callback
    /// thread writes them, so they need their own lock -- a separate one,
    /// because `lock` is held across the call into the caller's callback
    /// and a caller asking for stats from there would otherwise deadlock.
    private let statsLock = NSLock()
    private var annexB: [UInt8] = []
    private var paramSets: [UInt8] = []
    private var lastParamSets: [UInt8] = []

    private(set) var usesHardware = false
    private var inputCount: UInt64 = 0
    private var outputCount: UInt64 = 0
    /// Frames the encoder itself threw away (`kVTEncodeInfo_FrameDropped`).
    private var droppedByEncoder: UInt64 = 0
    /// Output callbacks that arrived with a non-zero `status`.
    private var callbackErrors: UInt64 = 0
    private var loggedCallbackError = false

    /// [inputs, outputs, dropped_by_encoder, callback_errors].
    func stats() -> (UInt64, UInt64, UInt64, UInt64) {
        statsLock.lock(); defer { statsLock.unlock() }
        return (inputCount, outputCount, droppedByEncoder, callbackErrors)
    }

    init(ctx: UnsafeMutableRawPointer?, onEncoded: @escaping SardpVtEncodedCallback,
         fps: UInt32, allIdr: Bool) {
        self.ctx = ctx
        self.onEncoded = onEncoded
        self.fps = max(fps, 1)
        self.allIdr = allIdr
    }

    /// Creates the session and applies every property. On any failure the
    /// session created so far is invalidated before returning.
    func start(width: Int32, height: Int32, bitrateBps: Int32,
               err: UnsafeMutablePointer<CChar>?, errLen: Int) -> Int32 {
        let spec: [CFString: Any] = [
            kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: true
        ]
        var created: VTCompressionSession?
        let status = VTCompressionSessionCreate(
            allocator: kCFAllocatorDefault,
            width: width,
            height: height,
            codecType: kCMVideoCodecType_H264,
            encoderSpecification: spec as CFDictionary,
            imageBufferAttributes: nil,
            compressedDataAllocator: nil,
            outputCallback: vtOutputCallback,
            refcon: Unmanaged.passUnretained(self).toOpaque(),
            compressionSessionOut: &created)
        guard status == noErr, let session = created else {
            putError(err, errLen, "VTCompressionSessionCreate: OSStatus \(status)")
            // `created` is nil on failure; nothing to invalidate.
            return SARDP_VT_ERR_CREATE
        }
        self.session = session

        // Low latency first: real time, and no frame reordering (B-frames
        // would make the encoder hold frames back, which is exactly the
        // one-frame lag DR-036 went after on Windows).
        var props: [(CFString, CFTypeRef)] = [
            (kVTCompressionPropertyKey_RealTime, kCFBooleanTrue),
            (kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse),
            (kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_High_AutoLevel),
            (kVTCompressionPropertyKey_H264EntropyMode, kVTH264EntropyMode_CABAC),
            (kVTCompressionPropertyKey_AverageBitRate, NSNumber(value: bitrateBps)),
            (kVTCompressionPropertyKey_ExpectedFrameRate, NSNumber(value: self.fps)),
        ]
        if allIdr {
            // GOP size 1: every frame independently decodable. Both keys,
            // because an encoder may honour one and ignore the other (the
            // same belt-and-braces the MFT needed).
            props.append((kVTCompressionPropertyKey_MaxKeyFrameInterval, NSNumber(value: 1)))
            props.append((kVTCompressionPropertyKey_AllowTemporalCompression, kCFBooleanFalse))
        } else {
            // IDR on request, plus a safety net so a client joining late
            // (or a lost IDR) recovers within a bounded time.
            props.append((kVTCompressionPropertyKey_MaxKeyFrameInterval,
                          NSNumber(value: self.fps * 10)))
            props.append((kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                          NSNumber(value: 10.0)))
        }
        for (key, value) in props {
            let s = VTSessionSetProperty(session, key: key, value: value)
            if s != noErr {
                // Not every property is supported by every encoder. The
                // ones that decide correctness (no reordering, GOP size in
                // all-IDR mode) are fatal; the rest are reported and the
                // session keeps its default.
                let fatal = (key == kVTCompressionPropertyKey_AllowFrameReordering)
                    || (allIdr && key == kVTCompressionPropertyKey_MaxKeyFrameInterval)
                if fatal {
                    putError(err, errLen, "VTSessionSetProperty(\(key)): OSStatus \(s)")
                    invalidate()
                    return SARDP_VT_ERR_PROPERTY
                }
                FileHandle.standardError.write(
                    Data("[sardp-vt] property \(key) rejected (OSStatus \(s)); using the encoder default\n".utf8))
            }
        }

        let prepared = VTCompressionSessionPrepareToEncodeFrames(session)
        if prepared != noErr {
            putError(err, errLen, "VTCompressionSessionPrepareToEncodeFrames: OSStatus \(prepared)")
            invalidate()
            return SARDP_VT_ERR_CREATE
        }

        // Asked for hardware above; find out whether we got it rather than
        // assuming the request was honoured.
        var value: CFTypeRef?
        if VTSessionCopyProperty(
            session,
            key: kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
            allocator: kCFAllocatorDefault,
            valueOut: &value) == noErr,
           let number = value as? NSNumber {
            usesHardware = number.boolValue
        }
        return SARDP_VT_OK
    }

    func encode(pixelBuffer: CVPixelBuffer, captureTsUs: UInt64, forceIdr: Bool) -> Int32 {
        guard let session = session else { return SARDP_VT_ERR_INVALID_HANDLE }
        let pts = CMTime(value: CMTimeValue(captureTsUs), timescale: ptsTimescale)
        let duration = CMTime(value: 1, timescale: CMTimeScale(fps))
        var frameProperties: CFDictionary?
        if forceIdr || allIdr {
            frameProperties = [kVTEncodeFrameOptionKey_ForceKeyFrame: kCFBooleanTrue!] as CFDictionary
        }
        var infoFlags = VTEncodeInfoFlags()
        let status = VTCompressionSessionEncodeFrame(
            session,
            imageBuffer: pixelBuffer,
            presentationTimeStamp: pts,
            duration: duration,
            frameProperties: frameProperties,
            sourceFrameRefcon: nil,
            infoFlagsOut: &infoFlags)
        if status != noErr { return SARDP_VT_ERR_ENCODE }
        statsLock.lock()
        inputCount += 1
        // noErr does not mean a frame is coming: the encoder may have
        // dropped this one outright.
        if infoFlags.contains(.frameDropped) { droppedByEncoder += 1 }
        statsLock.unlock()
        return SARDP_VT_OK
    }

    /// Flushes frames the encoder is still holding; their callbacks run
    /// before this returns.
    func complete() {
        guard let session = session else { return }
        VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: .invalid)
    }

    /// Idempotent: flush, then invalidate, then drop the reference.
    func invalidate() {
        guard let session = session else { return }
        self.session = nil
        VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: .invalid)
        VTCompressionSessionInvalidate(session)
    }

    deinit {
        invalidate()
    }

    // MARK: - output

    fileprivate func handleOutput(status: OSStatus, infoFlags: VTEncodeInfoFlags,
                                  sampleBuffer: CMSampleBuffer?) {
        if status != noErr {
            statsLock.lock()
            callbackErrors += 1
            let first = !loggedCallbackError
            loggedCallbackError = true
            statsLock.unlock()
            if first {
                FileHandle.standardError.write(
                    Data("[sardp-vt] encode callback failed: OSStatus \(status)\n".utf8))
            }
            return
        }
        if infoFlags.contains(.frameDropped) {
            statsLock.lock(); droppedByEncoder += 1; statsLock.unlock()
            return
        }
        guard let sampleBuffer = sampleBuffer,
              CMSampleBufferDataIsReady(sampleBuffer) else { return }

        lock.lock()
        defer { lock.unlock() }

        guard let format = CMSampleBufferGetFormatDescription(sampleBuffer) else { return }
        let nalHeaderLength = collectParameterSets(format)
        guard nalHeaderLength > 0 else { return }
        guard convertToAnnexB(sampleBuffer, nalHeaderLength: nalHeaderLength) else { return }

        // VideoToolbox's own keyframe claim: `NotSync` absent means sync.
        // Passed on as a hint; `sardp::h264` decides from the bytes.
        var isSync = true
        if let attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer,
                                                                    createIfNecessary: false)
            as? [[CFString: Any]], let first = attachments.first,
           let notSync = first[kCMSampleAttachmentKey_NotSync] as? Bool {
            isSync = !notSync
        }
        let ptsUs = UInt64(max(0, CMTimeConvertScale(
            CMSampleBufferGetPresentationTimeStamp(sampleBuffer),
            timescale: ptsTimescale,
            method: .roundHalfAwayFromZero).value))

        statsLock.lock(); outputCount += 1; statsLock.unlock()
        let sendSets = paramSets != lastParamSets
        if sendSets { lastParamSets = paramSets }
        annexB.withUnsafeBufferPointer { body in
            paramSets.withUnsafeBufferPointer { sets in
                onEncoded(ctx, body.baseAddress, body.count, isSync, ptsUs,
                          sendSets ? sets.baseAddress : nil,
                          sendSets ? sets.count : 0)
            }
        }
    }

    /// Fills `paramSets` with SPS/PPS as Annex-B and returns the AVCC
    /// length-prefix size (0 if the format description can't be read).
    private func collectParameterSets(_ format: CMFormatDescription) -> Int {
        var count = 0
        var nalHeaderLength: Int32 = 0
        guard CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                format, parameterSetIndex: 0,
                parameterSetPointerOut: nil, parameterSetSizeOut: nil,
                parameterSetCountOut: &count,
                nalUnitHeaderLengthOut: &nalHeaderLength) == noErr else {
            return 0
        }
        paramSets.removeAll(keepingCapacity: true)
        for i in 0..<count {
            var pointer: UnsafePointer<UInt8>?
            var size = 0
            guard CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    format, parameterSetIndex: i,
                    parameterSetPointerOut: &pointer, parameterSetSizeOut: &size,
                    parameterSetCountOut: nil, nalUnitHeaderLengthOut: nil) == noErr,
                  let pointer = pointer, size > 0 else { continue }
            paramSets.append(contentsOf: [0, 0, 0, 1])
            paramSets.append(contentsOf: UnsafeBufferPointer(start: pointer, count: size))
        }
        return Int(nalHeaderLength)
    }

    /// Rewrites the sample's AVCC length prefixes as Annex-B start codes
    /// into `annexB`. Returns false if the buffer can't be read.
    private func convertToAnnexB(_ sampleBuffer: CMSampleBuffer, nalHeaderLength: Int) -> Bool {
        guard var block = CMSampleBufferGetDataBuffer(sampleBuffer) else { return false }
        // A CMBlockBuffer may be a chain of non-contiguous segments; only
        // then is a copy needed.
        if !CMBlockBufferIsRangeContiguous(block, atOffset: 0, length: 0) {
            var contiguous: CMBlockBuffer?
            guard CMBlockBufferCreateContiguous(
                    allocator: kCFAllocatorDefault, sourceBuffer: block,
                    blockAllocator: kCFAllocatorDefault, customBlockSource: nil,
                    offsetToData: 0, dataLength: 0, flags: 0,
                    blockBufferOut: &contiguous) == kCMBlockBufferNoErr,
                  let contiguous = contiguous else { return false }
            block = contiguous
        }
        var totalLength = 0
        var dataPointer: UnsafeMutablePointer<CChar>?
        guard CMBlockBufferGetDataPointer(block, atOffset: 0, lengthAtOffsetOut: nil,
                                          totalLengthOut: &totalLength,
                                          dataPointerOut: &dataPointer) == kCMBlockBufferNoErr,
              let base = dataPointer else { return false }
        let bytes = UnsafeRawPointer(base).assumingMemoryBound(to: UInt8.self)

        annexB.removeAll(keepingCapacity: true)
        annexB.reserveCapacity(totalLength + 16)
        var offset = 0
        while offset + nalHeaderLength <= totalLength {
            var nalLength = 0
            for i in 0..<nalHeaderLength {
                nalLength = (nalLength << 8) | Int(bytes[offset + i])
            }
            offset += nalHeaderLength
            // A truncated or bogus length would otherwise read past the
            // buffer; stop rather than trust it.
            if nalLength <= 0 || offset + nalLength > totalLength { break }
            annexB.append(contentsOf: [0, 0, 0, 1])
            annexB.append(contentsOf: UnsafeBufferPointer(start: bytes + offset, count: nalLength))
            offset += nalLength
        }
        return !annexB.isEmpty
    }
}

private func vtOutputCallback(
    _ outputCallbackRefCon: UnsafeMutableRawPointer?,
    _ sourceFrameRefCon: UnsafeMutableRawPointer?,
    _ status: OSStatus,
    _ infoFlags: VTEncodeInfoFlags,
    _ sampleBuffer: CMSampleBuffer?
) {
    guard let refcon = outputCallbackRefCon else { return }
    Unmanaged<VtEncoder>.fromOpaque(refcon).takeUnretainedValue()
        .handleOutput(status: status, infoFlags: infoFlags, sampleBuffer: sampleBuffer)
}

// MARK: - C ABI

/// Creates an encoder. On success writes an opaque handle to `out_handle`
/// (release it with `sardp_vt_encoder_destroy`, which is safe to call
/// exactly once).
@_cdecl("sardp_vt_encoder_create")
public func sardp_vt_encoder_create(
    _ width: Int32,
    _ height: Int32,
    _ fps: UInt32,
    _ bitrate_bps: Int32,
    _ all_idr: Bool,
    _ ctx: UnsafeMutableRawPointer?,
    _ on_encoded: SardpVtEncodedCallback,
    _ out_handle: UnsafeMutablePointer<UnsafeMutableRawPointer?>?,
    _ err: UnsafeMutablePointer<CChar>?,
    _ err_len: Int
) -> Int32 {
    guard let out_handle = out_handle else { return SARDP_VT_ERR_CREATE }
    out_handle.pointee = nil
    let encoder = VtEncoder(ctx: ctx, onEncoded: on_encoded, fps: fps, allIdr: all_idr)
    let result = encoder.start(width: width, height: height, bitrateBps: bitrate_bps,
                               err: err, errLen: err_len)
    if result != SARDP_VT_OK { return result }
    out_handle.pointee = Unmanaged.passRetained(encoder).toOpaque()
    return SARDP_VT_OK
}

/// Encodes one frame. `pixel_buffer` is a `CVPixelBufferRef` -- typically
/// the one handed to a `sardp_sck_start` frame callback, which stays valid
/// for the duration of that callback. The encoded output arrives later on
/// the `on_encoded` callback.
@_cdecl("sardp_vt_encoder_encode")
public func sardp_vt_encoder_encode(
    _ handle: UnsafeMutableRawPointer?,
    _ pixel_buffer: UnsafeMutableRawPointer?,
    _ capture_ts_us: UInt64,
    _ force_idr: Bool
) -> Int32 {
    guard let handle = handle, let pixel_buffer = pixel_buffer else {
        return SARDP_VT_ERR_INVALID_HANDLE
    }
    let encoder = Unmanaged<VtEncoder>.fromOpaque(handle).takeUnretainedValue()
    let buffer = Unmanaged<CVPixelBuffer>.fromOpaque(pixel_buffer).takeUnretainedValue()
    return encoder.encode(pixelBuffer: buffer, captureTsUs: capture_ts_us, forceIdr: force_idr)
}

/// Whether the session actually ended up on the hardware encoder (read
/// back from the session, not the flag we asked for).
@_cdecl("sardp_vt_encoder_uses_hardware")
public func sardp_vt_encoder_uses_hardware(_ handle: UnsafeMutableRawPointer?) -> Bool {
    guard let handle = handle else { return false }
    return Unmanaged<VtEncoder>.fromOpaque(handle).takeUnretainedValue().usesHardware
}

/// Fills `out` with [inputs, outputs, dropped_by_encoder, callback_errors].
@_cdecl("sardp_vt_encoder_stats")
public func sardp_vt_encoder_stats(
    _ handle: UnsafeMutableRawPointer?,
    _ out: UnsafeMutablePointer<UInt64>?
) {
    guard let handle = handle, let out = out else { return }
    let e = Unmanaged<VtEncoder>.fromOpaque(handle).takeUnretainedValue()
    let (inputs, outputs, dropped, errors) = e.stats()
    out[0] = inputs
    out[1] = outputs
    out[2] = dropped
    out[3] = errors
}

/// Flushes frames still inside the encoder; their `on_encoded` callbacks
/// run before this returns.
@_cdecl("sardp_vt_encoder_complete")
public func sardp_vt_encoder_complete(_ handle: UnsafeMutableRawPointer?) {
    guard let handle = handle else { return }
    Unmanaged<VtEncoder>.fromOpaque(handle).takeUnretainedValue().complete()
}

/// Flushes, invalidates and releases. Call exactly once per handle.
@_cdecl("sardp_vt_encoder_destroy")
public func sardp_vt_encoder_destroy(_ handle: UnsafeMutableRawPointer?) {
    guard let handle = handle else { return }
    let encoder = Unmanaged<VtEncoder>.fromOpaque(handle).takeRetainedValue()
    encoder.invalidate()
}
