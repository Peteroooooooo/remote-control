import CoreGraphics
import XCTest

final class LogicTests: XCTestCase {
    // MARK: AnnexB

    func testSplitsH264AccessUnit() {
        // 4-byte start code SPS, 3-byte start code PPS, AUD, then an IDR slice.
        let sps: [UInt8] = [0x67, 0x42, 0x00, 0x1F]
        let pps: [UInt8] = [0x68, 0xCE, 0x3C, 0x80]
        let aud: [UInt8] = [0x09, 0xF0]
        let idr: [UInt8] = [0x65, 0x88, 0x84, 0x00, 0x33]
        var d = Data([0, 0, 0, 1] + sps + [0, 0, 1] + pps + [0, 0, 0, 1] + aud + [0, 0, 1] + idr)
        let s = AnnexB.split(d, hevc: false)
        XCTAssertEqual(s.parameterSets, [Data(sps), Data(pps)])
        XCTAssertEqual(s.sample, Data([0, 0, 0, 5] + idr))
        XCTAssertEqual(AnnexB.orderedParameterSets(s.parameterSets.reversed(), hevc: false), [Data(sps), Data(pps)])

        // Works on a slice whose indices do not start at 0.
        d = Data([0xAA]) + d
        let sliced = d.dropFirst()
        XCTAssertEqual(AnnexB.split(Data(sliced), hevc: false).sample, Data([0, 0, 0, 5] + idr))
        XCTAssertEqual(AnnexB.split(sliced, hevc: false).sample, Data([0, 0, 0, 5] + idr))
    }

    func testHevcParameterSetsNeedAllThree() {
        let vps = Data([0x40, 0x01, 0x0C])
        let sps = Data([0x42, 0x01, 0x01])
        let pps = Data([0x44, 0x01, 0xC1])
        XCTAssertEqual(AnnexB.orderedParameterSets([pps, sps, vps], hevc: true), [vps, sps, pps])
        XCTAssertNil(AnnexB.orderedParameterSets([sps, pps], hevc: true))
        XCTAssertTrue(AnnexB.isDelimiter(AnnexB.nalType(0x46, hevc: true), hevc: true))
    }

    // MARK: ViewMapping

    func testLetterboxedMappingRoundTrips() {
        let container = CGSize(width: 1000, height: 1000)
        let stream = CGSize(width: 1920, height: 1080)
        let fitted = ViewMapping.fittedSize(container: container, stream: stream)!
        XCTAssertEqual(fitted.width, 1000, accuracy: 0.001)
        XCTAssertEqual(fitted.height, 562.5, accuracy: 0.001)

        let n = CGPoint(x: 0.25, y: 0.75)
        for (scale, pan) in [(CGFloat(1), CGPoint.zero), (CGFloat(3), CGPoint(x: 120, y: -40))] {
            let p = ViewMapping.streamToScreen(n, container: container, stream: stream, scale: scale, pan: pan)!
            let back = ViewMapping.screenToStream(p, container: container, stream: stream, scale: scale, pan: pan)!
            XCTAssertEqual(back.x, n.x, accuracy: 1e-6)
            XCTAssertEqual(back.y, n.y, accuracy: 1e-6)
        }
        // Top-left of the letterboxed picture.
        let tl = ViewMapping.screenToStream(CGPoint(x: 0, y: 218.75), container: container, stream: stream, scale: 1, pan: .zero)!
        XCTAssertEqual(tl.x, 0, accuracy: 1e-6)
        XCTAssertEqual(tl.y, 0, accuracy: 1e-6)
    }

    func testPanIsClampedToPictureEdges() {
        let container = CGSize(width: 1000, height: 1000)
        let stream = CGSize(width: 1920, height: 1080)
        XCTAssertEqual(ViewMapping.clampPan(CGPoint(x: 500, y: 500), scale: 1, container: container, stream: stream), .zero)
        let c = ViewMapping.clampPan(CGPoint(x: 5000, y: -5000), scale: 2, container: container, stream: stream)
        XCTAssertEqual(c.x, 500, accuracy: 1e-6)
        XCTAssertEqual(c.y, -62.5, accuracy: 1e-6)
    }

    func testTrackpadCurveIsMonotonicAndCapped() {
        var last: CGFloat = 0
        for i in 0...40 {
            let g = TrackpadCurve.gain(speed: CGFloat(i) * 0.05, overInteractive: false)
            XCTAssertGreaterThanOrEqual(g, last)
            last = g
        }
        XCTAssertEqual(last, 2.45, accuracy: 1e-6)
        XCTAssertLessThan(TrackpadCurve.gain(speed: 0.1, overInteractive: true), TrackpadCurve.gain(speed: 0.1, overInteractive: false))
    }

    // MARK: Scan codes

    func testScanCodes() {
        XCTAssertEqual(Scan.forChar("c"), 0x2E)
        XCTAssertEqual(Scan.forChar("V"), 0x2F)
        XCTAssertEqual(Scan.forChar("1"), 0x02)
        XCTAssertEqual(Scan.forChar("0"), 0x0B)
        XCTAssertEqual(Scan.forChar("中"), 0)
        XCTAssertEqual(Scan.forHIDUsage(0x04), 0x1E) // a
        XCTAssertEqual(Scan.forHIDUsage(0x1D), 0x2C) // z
        XCTAssertEqual(Scan.forHIDUsage(0x27), 0x0B) // 0
        XCTAssertEqual(Scan.forHIDUsage(0x3A), 0x3B) // F1
        XCTAssertEqual(Scan.forHIDUsage(0x44), 0x57) // F11
        XCTAssertEqual(Scan.forHIDUsage(0x45), 0x58) // F12
        XCTAssertEqual(Scan.forHIDUsage(0x52), Scan.up)
        XCTAssertEqual(Scan.forHIDUsage(0xE3), Scan.win)
        XCTAssertEqual(Scan.forHIDUsage(0x99), 0)
    }

    // MARK: Models

    func testDecodesDevicesPayload() throws {
        let json = """
        {"device_id":"abc","own_relay_count":1,"devices":[
          {"id":"f00","short_id":"f0","name":"书房","status":"online","rtt_ms":4,"path":"direct","on_lan":true,"remembered":false,"ask":"in","error":""},
          {"id":"b4r","status":"offline"}
        ]}
        """
        let p = try JSONDecoder().decode(DevicesPayload.self, from: Data(json.utf8))
        XCTAssertEqual(p.deviceId, "abc")
        XCTAssertEqual(p.devices.count, 2)
        XCTAssertEqual(p.devices[0].pathLabel, "局域网直连")
        XCTAssertEqual(p.devices[0].ask, "in")
        XCTAssertEqual(p.devices[1].ask, "none")
        XCTAssertEqual(p.devices[1].displayName, "")
    }

    func testSubnetTargets() {
        // 192.168.1.23/24
        let ip: UInt32 = 0xC0A8_0117
        let t = Subnet.targets(address: ip, netmask: 0xFFFF_FF00)
        XCTAssertEqual(t.first, 0xC0A8_01FF) // broadcast first
        XCTAssertEqual(t.count, 254) // broadcast + 253 other hosts
        XCTAssertFalse(t.contains(ip))
        XCTAssertFalse(t.contains(0xC0A8_0100))
        // A /16 falls back to the /24 around the device.
        let big = Subnet.targets(address: 0x0A00_0505, netmask: 0xFFFF_0000)
        XCTAssertEqual(big.first, 0x0A00_05FF)
        XCTAssertEqual(big.count, 254)
        XCTAssertEqual(Subnet.dotted(ip), "192.168.1.23")
    }
}
