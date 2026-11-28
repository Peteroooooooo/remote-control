import Foundation

/// Annex B (start-code delimited) H.264 / HEVC parsing for VideoToolbox, which wants
/// parameter sets separately and NAL units with 4-byte big-endian length prefixes.
enum AnnexB {
    /// Byte ranges of the NAL units in `data`, start codes removed.
    static func nalRanges(_ data: Data) -> [Range<Int>] {
        let n = data.count
        guard n >= 4 else { return [] }
        var starts: [(code: Int, payload: Int)] = []
        data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            guard let p = raw.bindMemory(to: UInt8.self).baseAddress else { return }
            var i = 0
            while i + 2 < n {
                if p[i + 2] > 1 {
                    i += 3
                } else if p[i] == 0, p[i + 1] == 0, p[i + 2] == 1 {
                    starts.append((i, i + 3))
                    i += 3
                } else {
                    i += 1
                }
            }
        }
        var out: [Range<Int>] = []
        out.reserveCapacity(starts.count)
        for (k, s) in starts.enumerated() {
            var end = k + 1 < starts.count ? starts[k + 1].code : n
            // Zero bytes before the next start code belong to it (4-byte start codes,
            // trailing_zero_8bits), not to this NAL.
            while end > s.payload, data[data.startIndex + end - 1] == 0 {
                end -= 1
            }
            if end > s.payload {
                out.append(s.payload..<end)
            }
        }
        return out
    }

    static func nalType(_ firstByte: UInt8, hevc: Bool) -> UInt8 {
        hevc ? (firstByte >> 1) & 0x3F : firstByte & 0x1F
    }

    /// H.264 SPS / PPS, HEVC VPS / SPS / PPS.
    static func isParameterSet(_ type: UInt8, hevc: Bool) -> Bool {
        hevc ? (type >= 32 && type <= 34) : (type == 7 || type == 8)
    }

    /// Access unit delimiters carry nothing the decoder needs.
    static func isDelimiter(_ type: UInt8, hevc: Bool) -> Bool {
        hevc ? type == 35 : type == 9
    }

    struct Split {
        /// Parameter sets in stream order (H.264: SPS, PPS; HEVC: VPS, SPS, PPS).
        var parameterSets: [Data] = []
        /// The remaining NAL units, each with a 4-byte length prefix.
        var sample = Data()
    }

    static func split(_ data: Data, hevc: Bool) -> Split {
        var s = Split()
        let base = data.startIndex
        for r in nalRanges(data) {
            let first = data[base + r.lowerBound]
            let type = nalType(first, hevc: hevc)
            let nal = data.subdata(in: (base + r.lowerBound)..<(base + r.upperBound))
            if isParameterSet(type, hevc: hevc) {
                s.parameterSets.append(nal)
            } else if !isDelimiter(type, hevc: hevc) {
                var len = UInt32(nal.count).bigEndian
                withUnsafeBytes(of: &len) { s.sample.append(contentsOf: $0) }
                s.sample.append(nal)
            }
        }
        return s
    }

    /// Orders parameter sets the way VideoToolbox expects and keeps the newest of each kind.
    static func orderedParameterSets(_ sets: [Data], hevc: Bool) -> [Data]? {
        var byType: [UInt8: Data] = [:]
        for d in sets where !d.isEmpty {
            byType[nalType(d[d.startIndex], hevc: hevc)] = d
        }
        let order: [UInt8] = hevc ? [32, 33, 34] : [7, 8]
        let out = order.compactMap { byType[$0] }
        return out.count == order.count ? out : nil
    }
}
