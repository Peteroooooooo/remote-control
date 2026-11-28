import AVFoundation
import CoreMedia
import UIKit

/// Hardware decode and display of the host's H.264 / HEVC stream. AVSampleBufferDisplayLayer
/// runs VideoToolbox internally; every frame is marked "display immediately" so nothing is
/// buffered for smooth playback, which keeps latency at one decode.
final class VideoPipeline {
    let displayLayer = AVSampleBufferDisplayLayer()

    /// Asks the host for a new keyframe (set by the bridge).
    var requestKeyframe: () -> Void = {}
    /// Main queue: the first picture of a session is on screen.
    var onFirstFrame: () -> Void = {}
    /// Main queue: the picture size in pixels changed.
    var onSizeChange: (CGSize) -> Void = { _ in }

    private let queue = DispatchQueue(label: "rc.video", qos: .userInteractive)
    private var format: CMVideoFormatDescription?
    private var formatHevc = false
    private var formatSets: [Data] = []
    private var waitingForKey = true
    private var firstFrameReported = false
    private var lastSize = CGSize.zero
    private var lastKeyRequest = Date.distantPast

    init() {
        displayLayer.videoGravity = .resize
        displayLayer.backgroundColor = UIColor.black.cgColor
    }

    /// New session: forget the old stream's format and wait for a keyframe.
    func reset() {
        queue.async {
            self.format = nil
            self.formatSets = []
            self.waitingForKey = true
            self.firstFrameReported = false
            self.lastSize = .zero
            DispatchQueue.main.async { self.displayLayer.flushAndRemoveImage() }
        }
    }

    /// The layer stops decoding while the app is in the background; resume with a keyframe.
    func recover() {
        queue.async {
            self.displayLayer.flush()
            self.waitingForKey = true
            self.askKeyframe(force: true)
        }
    }

    func submit(hevc: Bool, key: Bool, width: Int, height: Int, data: Data) {
        queue.async { self.decode(hevc: hevc, key: key, width: width, height: height, data: data) }
    }

    private func decode(hevc: Bool, key: Bool, width: Int, height: Int, data: Data) {
        if displayLayer.status == .failed {
            displayLayer.flush()
            waitingForKey = true
            askKeyframe(force: false)
        }
        if waitingForKey && !key {
            askKeyframe(force: false)
            return
        }

        let split = AnnexB.split(data, hevc: hevc)
        if !split.parameterSets.isEmpty,
           let sets = AnnexB.orderedParameterSets(split.parameterSets, hevc: hevc),
           sets != formatSets || hevc != formatHevc || format == nil {
            if let f = Self.makeFormat(sets, hevc: hevc) {
                format = f
                formatSets = sets
                formatHevc = hevc
            }
        }
        guard let format, formatHevc == hevc, !split.sample.isEmpty else {
            if key { askKeyframe(force: false) }
            return
        }
        guard let sample = Self.makeSample(split.sample, format: format, key: key) else { return }
        displayLayer.enqueue(sample)
        waitingForKey = false

        let dims = CMVideoFormatDescriptionGetDimensions(format)
        let size = width > 0 && height > 0
            ? CGSize(width: width, height: height)
            : CGSize(width: Int(dims.width), height: Int(dims.height))
        if size != lastSize {
            lastSize = size
            DispatchQueue.main.async { self.onSizeChange(size) }
        }
        if !firstFrameReported {
            firstFrameReported = true
            DispatchQueue.main.async { self.onFirstFrame() }
        }
    }

    private func askKeyframe(force: Bool) {
        let now = Date()
        if force || now.timeIntervalSince(lastKeyRequest) > 0.5 {
            lastKeyRequest = now
            requestKeyframe()
        }
    }

    private static func makeFormat(_ sets: [Data], hevc: Bool) -> CMVideoFormatDescription? {
        // VideoToolbox needs stable pointers to every set for the duration of the call.
        let buffers = sets.map { d -> UnsafeMutableBufferPointer<UInt8> in
            let b = UnsafeMutableBufferPointer<UInt8>.allocate(capacity: d.count)
            _ = b.initialize(from: d)
            return b
        }
        defer { buffers.forEach { $0.deallocate() } }
        let pointers = buffers.map { UnsafePointer($0.baseAddress!) }
        let sizes = buffers.map { $0.count }
        var out: CMFormatDescription?
        let status: OSStatus
        if hevc {
            status = CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                allocator: kCFAllocatorDefault,
                parameterSetCount: pointers.count,
                parameterSetPointers: pointers,
                parameterSetSizes: sizes,
                nalUnitHeaderLength: 4,
                extensions: nil,
                formatDescriptionOut: &out
            )
        } else {
            status = CMVideoFormatDescriptionCreateFromH264ParameterSets(
                allocator: kCFAllocatorDefault,
                parameterSetCount: pointers.count,
                parameterSetPointers: pointers,
                parameterSetSizes: sizes,
                nalUnitHeaderLength: 4,
                formatDescriptionOut: &out
            )
        }
        return status == noErr ? out : nil
    }

    private static func makeSample(_ avcc: Data, format: CMVideoFormatDescription, key: Bool) -> CMSampleBuffer? {
        var block: CMBlockBuffer?
        guard CMBlockBufferCreateWithMemoryBlock(
            allocator: kCFAllocatorDefault,
            memoryBlock: nil,
            blockLength: avcc.count,
            blockAllocator: kCFAllocatorDefault,
            customBlockSource: nil,
            offsetToData: 0,
            dataLength: avcc.count,
            flags: kCMBlockBufferAssureMemoryNowFlag,
            blockBufferOut: &block
        ) == kCMBlockBufferNoErr, let block else { return nil }
        let copied = avcc.withUnsafeBytes { raw -> OSStatus in
            guard let base = raw.baseAddress else { return -1 }
            return CMBlockBufferReplaceDataBytes(with: base, blockBuffer: block, offsetIntoDestination: 0, dataLength: avcc.count)
        }
        guard copied == kCMBlockBufferNoErr else { return nil }

        var sample: CMSampleBuffer?
        var size = avcc.count
        guard CMSampleBufferCreateReady(
            allocator: kCFAllocatorDefault,
            dataBuffer: block,
            formatDescription: format,
            sampleCount: 1,
            sampleTimingEntryCount: 0,
            sampleTimingArray: nil,
            sampleSizeEntryCount: 1,
            sampleSizeArray: &size,
            sampleBufferOut: &sample
        ) == noErr, let sample else { return nil }

        if let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: true),
           CFArrayGetCount(attachments) > 0 {
            let dict = unsafeBitCast(CFArrayGetValueAtIndex(attachments, 0), to: CFMutableDictionary.self)
            CFDictionarySetValue(
                dict,
                Unmanaged.passUnretained(kCMSampleAttachmentKey_DisplayImmediately).toOpaque(),
                Unmanaged.passUnretained(kCFBooleanTrue).toOpaque()
            )
            if !key {
                CFDictionarySetValue(
                    dict,
                    Unmanaged.passUnretained(kCMSampleAttachmentKey_NotSync).toOpaque(),
                    Unmanaged.passUnretained(kCFBooleanTrue).toOpaque()
                )
            }
        }
        return sample
    }
}

/// A host cursor image ready for a CALayer.
struct CursorGlyph {
    let image: CGImage
    let width: Int
    let height: Int
    let hotX: Int
    let hotY: Int

    /// `rgba` is premultiplied RGBA8888 from the engine; copied here.
    static func make(width: Int, height: Int, hotX: Int, hotY: Int, rgba: UnsafePointer<UInt8>?, len: Int) -> CursorGlyph? {
        guard width > 0, height > 0, let rgba, len >= width * height * 4 else { return nil }
        let data = Data(bytes: rgba, count: width * height * 4) as CFData
        guard let provider = CGDataProvider(data: data),
              let image = CGImage(
                  width: width,
                  height: height,
                  bitsPerComponent: 8,
                  bitsPerPixel: 32,
                  bytesPerRow: width * 4,
                  space: CGColorSpaceCreateDeviceRGB(),
                  bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedLast.rawValue),
                  provider: provider,
                  decode: nil,
                  shouldInterpolate: true,
                  intent: .defaultIntent
              ) else { return nil }
        return CursorGlyph(
            image: image,
            width: width,
            height: height,
            hotX: min(max(hotX, 0), width - 1),
            hotY: min(max(hotY, 0), height - 1)
        )
    }
}
