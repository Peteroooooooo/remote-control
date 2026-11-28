import Foundation

/// PC set 1 scan codes (0xE0xx = extended) that the host injects, as on Android (RemoteKeyboard.kt).
enum Scan {
    static let esc = 0x01
    static let backspace = 0x0E
    static let tab = 0x0F
    static let enter = 0x1C
    static let ctrl = 0x1D
    static let shift = 0x2A
    static let alt = 0x38
    static let space = 0x39
    static let win = 0xE05B
    static let home = 0xE047
    static let up = 0xE048
    static let pgUp = 0xE049
    static let left = 0xE04B
    static let right = 0xE04D
    static let end = 0xE04F
    static let down = 0xE050
    static let pgDn = 0xE051
    static let delete = 0xE053

    static func f(_ n: Int) -> Int {
        switch n {
        case 1...10: return 0x3A + n
        case 11: return 0x57
        default: return 0x58
        }
    }

    private static let letterRows: [(String, Int)] = [("qwertyuiop", 0x10), ("asdfghjkl", 0x1E), ("zxcvbnm", 0x2C)]

    /// US-layout key for a character typed while a sticky modifier is armed, so Ctrl + "c"
    /// becomes the real Ctrl+C. 0 when the character has no key.
    static func forChar(_ ch: Character) -> Int {
        let c = Character(ch.lowercased())
        for (row, base) in letterRows {
            if let i = row.firstIndex(of: c) {
                return base + row.distance(from: row.startIndex, to: i)
            }
        }
        if let d = c.wholeNumberValue, c.isASCII {
            return d == 0 ? 0x0B : 0x02 + (d - 1)
        }
        switch c {
        case "-": return 0x0C
        case "=": return 0x0D
        case "[": return 0x1A
        case "]": return 0x1B
        case ";": return 0x27
        case "'": return 0x28
        case "`": return 0x29
        case "\\": return 0x2B
        case ",": return 0x33
        case ".": return 0x34
        case "/": return 0x35
        case " ": return space
        case "\n": return enter
        case "\t": return tab
        default: return 0
        }
    }

    /// USB HID keyboard usage (UIKeyboardHIDUsage raw value) to a scan code; 0 when unmapped.
    /// Used for hardware keyboards on iPad.
    static func forHIDUsage(_ usage: Int) -> Int {
        if let s = hidTable[usage] { return s }
        return 0
    }

    private static let hidTable: [Int: Int] = {
        var t: [Int: Int] = [:]
        // a…z (HID 0x04…0x1D)
        let letters: [Int] = [
            0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32,
            0x31, 0x18, 0x19, 0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C,
        ]
        for (i, s) in letters.enumerated() { t[0x04 + i] = s }
        // 1…9, 0 (HID 0x1E…0x27)
        for i in 0..<10 { t[0x1E + i] = 0x02 + i }
        let rest: [(Int, Int)] = [
            (0x28, 0x1C), (0x29, 0x01), (0x2A, 0x0E), (0x2B, 0x0F), (0x2C, 0x39),
            (0x2D, 0x0C), (0x2E, 0x0D), (0x2F, 0x1A), (0x30, 0x1B), (0x31, 0x2B),
            (0x32, 0x2B), (0x33, 0x27), (0x34, 0x28), (0x35, 0x29), (0x36, 0x33),
            (0x37, 0x34), (0x38, 0x35), (0x39, 0x3A),
            (0x46, 0xE037), (0x47, 0x46), (0x49, 0xE052), (0x4A, 0xE047), (0x4B, 0xE049),
            (0x4C, 0xE053), (0x4D, 0xE04F), (0x4E, 0xE051), (0x4F, 0xE04D), (0x50, 0xE04B),
            (0x51, 0xE050), (0x52, 0xE048),
            (0x53, 0x45), (0x54, 0xE035), (0x55, 0x37), (0x56, 0x4A), (0x57, 0x4E),
            (0x58, 0xE01C), (0x59, 0x4F), (0x5A, 0x50), (0x5B, 0x51), (0x5C, 0x4B),
            (0x5D, 0x4C), (0x5E, 0x4D), (0x5F, 0x47), (0x60, 0x48), (0x61, 0x49),
            (0x62, 0x52), (0x63, 0x53), (0x64, 0x56), (0x65, 0xE05D),
            (0xE0, 0x1D), (0xE1, 0x2A), (0xE2, 0x38), (0xE3, 0xE05B),
            (0xE4, 0xE01D), (0xE5, 0x36), (0xE6, 0xE038), (0xE7, 0xE05C),
        ]
        for (h, s) in rest { t[h] = s }
        // F1…F12 (HID 0x3A…0x45)
        for i in 1...12 { t[0x39 + i] = f(i) }
        return t
    }()
}
