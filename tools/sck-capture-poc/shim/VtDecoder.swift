// VideoToolbox H.264 decoder shim for SARDP (3M-1-e).
//
// Counterpart of the Media Foundation H.264 decoder MFT in
// `sardp-win/src/display.rs`: a persistent `VTDecompressionSession` fed
// one access unit at a time, producing `CVPixelBuffer`s an IOSurface
// backs (so `DisplayWindow.swift` can hand them to the window server
// without a CPU copy, the same "no pixel copy" property the Windows path
// gets from DXVA + `ID3D11VideoProcessor`).
//
// Design rules carried over from `VtEncoder.swift`/3W-1:
//
//   - The session is torn down (`VTDecompressionSessionInvalidate`) on
//     every path, including the one just before a new session replaces
//     it on a resolution change -- never left to `deinit` alone to find
//     out about an error path.
//   - Synchronous by construction, not by polling: passing an empty
//     `VTDecodeFrameFlags` (neither `EnableAsynchronousDecompression`
//     nor `EnableTemporalProcessing`) means "the output callback runs
//     before `VTDecompressionSessionDecodeFrame` returns" per Apple's own
//     doc comment on the flag, so `decode()` can hand the caller a
//     pixel buffer (or `nil`) synchronously without a `WaitFor...` poll
//     loop -- the same one-call-in, one-call-out shape the Windows MFT
//     path has to build with a manual `ProcessOutput` loop because MF
//     doesn't offer a synchronous mode.
//   - `sardp::h264::ParameterSetCache` on the server already guarantees
//     every IDR access unit is self-contained (spec 2.10), so this shim
//     never has to go looking for parameter sets on its own: the caller
//     extracts SPS/PPS from the IDR bytes (`sardp::h264`) and passes them
//     to `configure`, once per resolution rather than per frame --
//     VideoToolbox needs a `CMFormatDescription` built *before* decoding
//     can start at all, unlike Media Foundation's decoder MFT, which
//     parses SPS/PPS out of the Annex-B bitstream itself.
//   - Only the non-parameter-set NAL units are decoded (AVCC, 4-byte
//     length prefixes -- the caller's job, `sardp::h264` again): handing
//     an in-band SPS/PPS to a session that already has them from its
//     format description is unsupported by this shim and would just be
//     redundant given the point above.

import CoreMedia
import CoreVideo
import Foundation
import VideoToolbox

public let SARDP_VTDEC_OK: Int32 = 0
public let SARDP_VTDEC_ERR_FORMAT: Int32 = 1
public let SARDP_VTDEC_ERR_CREATE: Int32 = 2
public let SARDP_VTDEC_ERR_DECODE: Int32 = 3
public let SARDP_VTDEC_ERR_INVALID_HANDLE: Int32 = 4

/// Same timescale `VtEncoder.swift` uses, so presentation stamps round-trip
/// exactly through the microsecond clock (`sardp::clock::now_us`).
private let ptsTimescale: CMTimeScale = 1_000_000

final class VtDecoder {
    private var session: VTDecompressionSession?
    private var format: CMFormatDescription?

    // Filled in by the output callback for the one `decode()` call
    // currently in flight; read back once `VTDecompressionSessionDecodeFrame`
    // returns (empty decode flags make that synchronous -- see file doc).
    private var lastOutput: CVPixelBuffer?
    private var lastStatus: OSStatus = noErr
    private var lastDropped = false

    private(set) var framesDecoded: UInt64 = 0

    /// (Re)configures the session from an IDR's SPS/PPS. Tears down any
    /// existing session first -- called whenever the stream's resolution
    /// changes (`sardp_mac::display` decides when that is), so there is
    /// always at most one live session, never two overlapping ones.
    func configure(
        width: Int32, height: Int32,
        sps: UnsafePointer<UInt8>?, spsLen: Int,
        pps: UnsafePointer<UInt8>?, ppsLen: Int,
        err: UnsafeMutablePointer<CChar>?, errLen: Int
    ) -> Int32 {
        invalidate()
        guard let sps = sps, let pps = pps, spsLen > 0, ppsLen > 0 else {
            putError(err, errLen, "configure: SPS/PPS missing (IDR was not self-contained)")
            return SARDP_VTDEC_ERR_FORMAT
        }
        var newFormat: CMFormatDescription?
        let status = [sps, pps].withUnsafeBufferPointer { pointerBuf -> OSStatus in
            [spsLen, ppsLen].withUnsafeBufferPointer { sizeBuf -> OSStatus in
                CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    allocator: kCFAllocatorDefault,
                    parameterSetCount: 2,
                    parameterSetPointers: pointerBuf.baseAddress!,
                    parameterSetSizes: sizeBuf.baseAddress!,
                    nalUnitHeaderLength: 4,
                    formatDescriptionOut: &newFormat)
            }
        }
        guard status == noErr, let newFormat = newFormat else {
            putError(
                err, errLen,
                "CMVideoFormatDescriptionCreateFromH264ParameterSets: OSStatus \(status)")
            return SARDP_VTDEC_ERR_FORMAT
        }

        // IOSurface-backed NV12 output: what `DisplayWindow.swift` needs
        // to set a `CALayer`'s `contents` directly, no CPU pixel copy.
        let destAttrs: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary,
            kCVPixelBufferMetalCompatibilityKey: true,
            kCVPixelBufferWidthKey: Int(width),
            kCVPixelBufferHeightKey: Int(height),
        ]
        var record = VTDecompressionOutputCallbackRecord(
            decompressionOutputCallback: vtDecOutputCallback,
            decompressionOutputRefCon: Unmanaged.passUnretained(self).toOpaque())
        var created: VTDecompressionSession?
        let createStatus = VTDecompressionSessionCreate(
            allocator: kCFAllocatorDefault,
            formatDescription: newFormat,
            decoderSpecification: nil,
            imageBufferAttributes: destAttrs as CFDictionary,
            outputCallback: &record,
            decompressionSessionOut: &created)
        guard createStatus == noErr, let session = created else {
            putError(err, errLen, "VTDecompressionSessionCreate: OSStatus \(createStatus)")
            return SARDP_VTDEC_ERR_CREATE
        }
        self.session = session
        self.format = newFormat
        return SARDP_VTDEC_OK
    }

    var isConfigured: Bool { session != nil }

    /// Decodes one access unit's non-parameter-set NAL units (AVCC,
    /// 4-byte length prefixes). Synchronous: the returned pixel buffer
    /// (if any -- there is no B-frame reordering in this protocol, so
    /// every call should produce exactly one, but nothing here assumes
    /// that) is already the result of this call's own output callback,
    /// not a leftover from a previous one.
    func decode(
        avcc: UnsafePointer<UInt8>, len: Int, ptsUs: UInt64,
        err: UnsafeMutablePointer<CChar>?, errLen: Int
    ) -> (Int32, CVPixelBuffer?) {
        guard let session = session, let format = format else {
            putError(err, errLen, "decode called before configure")
            return (SARDP_VTDEC_ERR_INVALID_HANDLE, nil)
        }
        var blockBuffer: CMBlockBuffer?
        let blockStatus = CMBlockBufferCreateWithMemoryBlock(
            allocator: kCFAllocatorDefault, memoryBlock: nil, blockLength: len,
            blockAllocator: kCFAllocatorDefault, customBlockSource: nil,
            offsetToData: 0, dataLength: len, flags: 0, blockBufferOut: &blockBuffer)
        guard blockStatus == kCMBlockBufferNoErr, let blockBuffer = blockBuffer else {
            putError(err, errLen, "CMBlockBufferCreateWithMemoryBlock: \(blockStatus)")
            return (SARDP_VTDEC_ERR_DECODE, nil)
        }
        let copyStatus = CMBlockBufferReplaceDataBytes(
            with: avcc, blockBuffer: blockBuffer, offsetIntoDestination: 0, dataLength: len)
        guard copyStatus == kCMBlockBufferNoErr else {
            putError(err, errLen, "CMBlockBufferReplaceDataBytes: \(copyStatus)")
            return (SARDP_VTDEC_ERR_DECODE, nil)
        }

        var sampleBuffer: CMSampleBuffer?
        let pts = CMTime(value: CMTimeValue(ptsUs), timescale: ptsTimescale)
        var timing = CMSampleTimingInfo(
            duration: .invalid, presentationTimeStamp: pts, decodeTimeStamp: .invalid)
        var sampleSize = len
        let sbStatus = CMSampleBufferCreateReady(
            allocator: kCFAllocatorDefault, dataBuffer: blockBuffer,
            formatDescription: format, sampleCount: 1,
            sampleTimingEntryCount: 1, sampleTimingArray: &timing,
            sampleSizeEntryCount: 1, sampleSizeArray: &sampleSize,
            sampleBufferOut: &sampleBuffer)
        guard sbStatus == noErr, let sampleBuffer = sampleBuffer else {
            putError(err, errLen, "CMSampleBufferCreateReady: OSStatus \(sbStatus)")
            return (SARDP_VTDEC_ERR_DECODE, nil)
        }

        lastOutput = nil
        lastStatus = noErr
        lastDropped = false
        // Empty flags: neither async nor temporal-reorder bit set, so the
        // output callback has already run (with this call's result) by
        // the time this returns -- see the file doc's rationale.
        let decodeStatus = VTDecompressionSessionDecodeFrame(
            session, sampleBuffer: sampleBuffer, flags: [],
            frameRefcon: nil, infoFlagsOut: nil)
        guard decodeStatus == noErr else {
            putError(err, errLen, "VTDecompressionSessionDecodeFrame: OSStatus \(decodeStatus)")
            return (SARDP_VTDEC_ERR_DECODE, nil)
        }
        guard lastStatus == noErr else {
            putError(err, errLen, "decompression callback: OSStatus \(lastStatus)")
            return (SARDP_VTDEC_ERR_DECODE, nil)
        }
        if lastDropped {
            // Not an error -- the decoder discarded the frame on its own
            // (e.g. a decoder-internal resource limit); nothing to present.
            return (SARDP_VTDEC_OK, nil)
        }
        framesDecoded += 1
        let output = lastOutput
        lastOutput = nil
        return (SARDP_VTDEC_OK, output)
    }

    fileprivate func handleOutput(status: OSStatus, dropped: Bool, imageBuffer: CVImageBuffer?) {
        lastStatus = status
        lastDropped = dropped
        lastOutput = imageBuffer
    }

    /// Idempotent: safe to call from `configure` (before replacing the
    /// session) and again from `deinit`.
    func invalidate() {
        guard let session = session else { return }
        self.session = nil
        self.format = nil
        VTDecompressionSessionInvalidate(session)
    }

    deinit {
        invalidate()
    }
}

private func vtDecOutputCallback(
    _ decompressionOutputRefCon: UnsafeMutableRawPointer?,
    _ sourceFrameRefCon: UnsafeMutableRawPointer?,
    _ status: OSStatus,
    _ infoFlags: VTDecodeInfoFlags,
    _ imageBuffer: CVImageBuffer?,
    _ presentationTimeStamp: CMTime,
    _ presentationDuration: CMTime
) {
    guard let refcon = decompressionOutputRefCon else { return }
    Unmanaged<VtDecoder>.fromOpaque(refcon).takeUnretainedValue()
        .handleOutput(
            status: status, dropped: infoFlags.contains(.frameDropped), imageBuffer: imageBuffer)
}

// MARK: - C ABI

/// Allocates a decoder with no session yet (call `sardp_vtdec_configure`
/// before the first `sardp_vtdec_decode`). Never fails.
@_cdecl("sardp_vtdec_create")
public func sardp_vtdec_create() -> UnsafeMutableRawPointer {
    Unmanaged.passRetained(VtDecoder()).toOpaque()
}

/// (Re)configures the session for `width`x`height` from an IDR's SPS/PPS
/// (Annex-B NAL bytes, start codes excluded -- `sardp::h264::NalUnit`'s
/// own slicing). Tears down any previous session first.
@_cdecl("sardp_vtdec_configure")
public func sardp_vtdec_configure(
    _ handle: UnsafeMutableRawPointer?,
    _ width: Int32, _ height: Int32,
    _ sps: UnsafePointer<UInt8>?, _ sps_len: Int,
    _ pps: UnsafePointer<UInt8>?, _ pps_len: Int,
    _ err: UnsafeMutablePointer<CChar>?, _ err_len: Int
) -> Int32 {
    guard let handle = handle else { return SARDP_VTDEC_ERR_INVALID_HANDLE }
    let decoder = Unmanaged<VtDecoder>.fromOpaque(handle).takeUnretainedValue()
    return decoder.configure(
        width: width, height: height, sps: sps, spsLen: sps_len, pps: pps, ppsLen: pps_len,
        err: err, errLen: err_len)
}

/// Decodes one access unit's slice NAL units (AVCC, 4-byte length
/// prefixes; SPS/PPS excluded -- they are already in the format
/// description from `sardp_vtdec_configure`). On success writes a
/// retained `CVPixelBufferRef` to `out_pixel_buffer` (`nil` if nothing
/// came out this call, which is not itself an error) -- ownership passes
/// to the caller, who must release it exactly once (`sardp_win_present`
/// does, taking a retained reference).
@_cdecl("sardp_vtdec_decode")
public func sardp_vtdec_decode(
    _ handle: UnsafeMutableRawPointer?,
    _ avcc: UnsafePointer<UInt8>?, _ avcc_len: Int,
    _ pts_us: UInt64,
    _ out_pixel_buffer: UnsafeMutablePointer<UnsafeMutableRawPointer?>?,
    _ err: UnsafeMutablePointer<CChar>?, _ err_len: Int
) -> Int32 {
    guard let handle = handle, let out_pixel_buffer = out_pixel_buffer else {
        return SARDP_VTDEC_ERR_INVALID_HANDLE
    }
    out_pixel_buffer.pointee = nil
    guard let avcc = avcc, avcc_len > 0 else {
        putError(err, err_len, "decode: empty input")
        return SARDP_VTDEC_ERR_DECODE
    }
    let decoder = Unmanaged<VtDecoder>.fromOpaque(handle).takeUnretainedValue()
    let (status, output) = decoder.decode(
        avcc: avcc, len: avcc_len, ptsUs: pts_us, err: err, errLen: err_len)
    if let output = output {
        out_pixel_buffer.pointee = Unmanaged.passRetained(output).toOpaque()
    }
    return status
}

/// Whether a session currently exists (i.e. `configure` has succeeded at
/// least once since the last `configure` failure or `destroy`).
@_cdecl("sardp_vtdec_is_configured")
public func sardp_vtdec_is_configured(_ handle: UnsafeMutableRawPointer?) -> Bool {
    guard let handle = handle else { return false }
    return Unmanaged<VtDecoder>.fromOpaque(handle).takeUnretainedValue().isConfigured
}

/// Releases a retained `CVPixelBufferRef` that was never handed to
/// `sardp_win_present` (an error path on the Rust side, say). Every
/// buffer `sardp_vtdec_decode` produces must go through exactly one of
/// this or `sardp_win_present` -- never both, never neither.
@_cdecl("sardp_vtdec_release_pixelbuffer")
public func sardp_vtdec_release_pixelbuffer(_ pixel_buffer: UnsafeMutableRawPointer?) {
    guard let pixel_buffer = pixel_buffer else { return }
    _ = Unmanaged<CVPixelBuffer>.fromOpaque(pixel_buffer).takeRetainedValue()
}

/// Invalidates the session (if any) and releases the decoder. Call
/// exactly once per handle.
@_cdecl("sardp_vtdec_destroy")
public func sardp_vtdec_destroy(_ handle: UnsafeMutableRawPointer?) {
    guard let handle = handle else { return }
    let decoder = Unmanaged<VtDecoder>.fromOpaque(handle).takeRetainedValue()
    decoder.invalidate()
}

// `putError` itself is `SckShim.swift`'s (module-internal, so visible
// here too -- this file must not redeclare it).
