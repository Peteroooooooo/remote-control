import Darwin
import Foundation

/// Reads this device's Wi-Fi / Ethernet IPv4 addresses and builds the LAN discovery targets.
enum LanTargets {
    static func csv() -> String {
        var seen = Set<UInt32>()
        var out: [String] = []
        var ifaddr: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&ifaddr) == 0, let first = ifaddr else { return "" }
        defer { freeifaddrs(ifaddr) }
        var cur: UnsafeMutablePointer<ifaddrs>? = first
        while let ifa = cur {
            defer { cur = ifa.pointee.ifa_next }
            let flags = Int32(ifa.pointee.ifa_flags)
            guard (flags & IFF_UP) != 0, (flags & IFF_LOOPBACK) == 0,
                  let addr = ifa.pointee.ifa_addr, addr.pointee.sa_family == UInt8(AF_INET),
                  let maskPtr = ifa.pointee.ifa_netmask else { continue }
            let name = String(cString: ifa.pointee.ifa_name)
            // en* = Wi-Fi and wired adapters; cellular (pdp_ip*) and VPN tunnels have no LAN peers.
            guard name.hasPrefix("en") else { continue }
            let ip = addr.withMemoryRebound(to: sockaddr_in.self, capacity: 1) { UInt32(bigEndian: $0.pointee.sin_addr.s_addr) }
            let mask = maskPtr.withMemoryRebound(to: sockaddr_in.self, capacity: 1) { UInt32(bigEndian: $0.pointee.sin_addr.s_addr) }
            // Link-local 169.254/16 means no DHCP lease, nothing to find there.
            if (ip >> 16) == 0xA9FE { continue }
            for t in Subnet.targets(address: ip, netmask: mask) where seen.insert(t).inserted {
                out.append(Subnet.dotted(t))
            }
        }
        return out.joined(separator: ",")
    }
}
