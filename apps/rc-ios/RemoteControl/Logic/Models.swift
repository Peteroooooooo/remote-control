import Foundation

/// Computer entry from the engine's `on_devices_updated` JSON.
struct DeviceItem: Identifiable, Equatable, Decodable {
    var id: String
    var shortId: String
    var name: String
    /// "online", "checking", "offline", "refused"
    var status: String
    var rttMs: Int
    /// "direct", "relay", "public", "none"
    var path: String
    var onLan: Bool
    var remembered: Bool
    var hasSelfHostedRelay: Bool
    /// "none", "in" (the computer asks to remember this device), "out" (we asked)
    var ask: String
    var error: String

    var isOnline: Bool { status == "online" }

    var pathLabel: String {
        switch path {
        case "direct": return onLan ? "局域网直连" : "直连"
        case "relay": return "自建中转"
        case "public": return "公共中转"
        default: return onLan ? "局域网" : ""
        }
    }

    var displayName: String { name.isEmpty ? shortId : name }

    enum CodingKeys: String, CodingKey {
        case id, shortId = "short_id", name, status, rttMs = "rtt_ms", path, onLan = "on_lan"
        case remembered, hasSelfHostedRelay = "has_self_hosted_relay", ask, error
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        shortId = try c.decodeIfPresent(String.self, forKey: .shortId) ?? ""
        name = try c.decodeIfPresent(String.self, forKey: .name) ?? ""
        status = try c.decodeIfPresent(String.self, forKey: .status) ?? "offline"
        rttMs = try c.decodeIfPresent(Int.self, forKey: .rttMs) ?? 0
        path = try c.decodeIfPresent(String.self, forKey: .path) ?? "none"
        onLan = try c.decodeIfPresent(Bool.self, forKey: .onLan) ?? false
        remembered = try c.decodeIfPresent(Bool.self, forKey: .remembered) ?? false
        hasSelfHostedRelay = try c.decodeIfPresent(Bool.self, forKey: .hasSelfHostedRelay) ?? false
        ask = try c.decodeIfPresent(String.self, forKey: .ask) ?? "none"
        error = try c.decodeIfPresent(String.self, forKey: .error) ?? ""
    }
}

struct DevicesPayload: Decodable {
    var deviceId: String
    var ownRelayCount: Int
    var devices: [DeviceItem]

    enum CodingKeys: String, CodingKey {
        case deviceId = "device_id", ownRelayCount = "own_relay_count", devices
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        deviceId = try c.decodeIfPresent(String.self, forKey: .deviceId) ?? ""
        ownRelayCount = try c.decodeIfPresent(Int.self, forKey: .ownRelayCount) ?? 0
        devices = try c.decodeIfPresent([DeviceItem].self, forKey: .devices) ?? []
    }
}

struct MonitorInfo: Identifiable, Equatable, Decodable {
    var index: Int
    var name: String
    var x: Int
    var y: Int
    var width: Int
    var height: Int
    var primary: Bool
    var rotation: Int

    var id: Int { index }

    var title: String { primary ? "主屏 \(index + 1)" : "副屏 \(index + 1)" }

    var shortRes: String {
        let s = min(width, height)
        if s >= 2160 { return "4K" }
        return s > 0 ? "\(s)p" : ""
    }

    enum CodingKeys: String, CodingKey { case index, name, x, y, width, height, primary, rotation }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        index = try c.decodeIfPresent(Int.self, forKey: .index) ?? 0
        name = try c.decodeIfPresent(String.self, forKey: .name) ?? ""
        x = try c.decodeIfPresent(Int.self, forKey: .x) ?? 0
        y = try c.decodeIfPresent(Int.self, forKey: .y) ?? 0
        width = try c.decodeIfPresent(Int.self, forKey: .width) ?? 0
        height = try c.decodeIfPresent(Int.self, forKey: .height) ?? 0
        primary = try c.decodeIfPresent(Bool.self, forKey: .primary) ?? false
        rotation = try c.decodeIfPresent(Int.self, forKey: .rotation) ?? 0
    }
}

struct PasteResponse: Decodable {
    var ok: Bool
    var message: String

    enum CodingKeys: String, CodingKey { case ok, message }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        ok = try c.decodeIfPresent(Bool.self, forKey: .ok) ?? false
        message = try c.decodeIfPresent(String.self, forKey: .message) ?? "解析失败"
    }
}

/// LAN discovery targets for one IPv4 interface. iOS drops broadcasts unless the app has the
/// multicast entitlement (which needs Apple's approval), so besides the broadcast address the
/// app asks every host of the subnet directly; unicast only needs the local network permission.
enum Subnet {
    /// Largest sweep, in hosts. Bigger subnets fall back to the /24 around this device.
    static let maxHosts = 510

    static func targets(address: UInt32, netmask: UInt32) -> [UInt32] {
        var mask = netmask
        var hostCount = Int(~mask)
        if mask == 0 || hostCount > maxHosts + 1 {
            mask = 0xFFFF_FF00
            hostCount = 255
        }
        guard hostCount >= 2 else { return [] }
        let network = address & mask
        let broadcast = network | ~mask
        var out: [UInt32] = [broadcast]
        out.reserveCapacity(hostCount)
        var h = network &+ 1
        while h < broadcast {
            if h != address { out.append(h) }
            h &+= 1
        }
        return out
    }

    static func dotted(_ a: UInt32) -> String {
        "\((a >> 24) & 0xFF).\((a >> 16) & 0xFF).\((a >> 8) & 0xFF).\(a & 0xFF)"
    }
}
