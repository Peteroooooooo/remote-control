import SwiftUI
import UIKit

enum AppLayer { case devices, session }

enum TouchMode { case trackpad, direct }

enum SessionPhase { case idle, connecting, waitingConsent, connected, paused, disconnected, error }

struct LiveStats: Equatable {
    var fps = 0
    var kbps = 0
    var rttMs = 0
    var pathKind = ""

    var pathLabel: String {
        switch pathKind {
        case "direct": return "直连"
        case "relay": return "中转"
        case "": return ""
        default: return pathKind
        }
    }
}

struct Feedback: Equatable {
    var ok: Bool
    var message: String
}

/// App state, the iOS counterpart of Android's RcViewModel. Used on the main queue only;
/// engine calls that may block go to a background queue.
final class AppModel: ObservableObject {
    @Published var layer: AppLayer = .devices
    @Published var localDeviceId = ""
    @Published var ownRelayCount = 0
    @Published var devices: [DeviceItem] = []
    @Published var activeDevice: DeviceItem?
    @Published var phase: SessionPhase = .idle
    @Published var sessionMessage = ""
    @Published var streamSize = CGSize(width: 1920, height: 1080)
    @Published var encoder = ""
    @Published var stats = LiveStats()
    @Published var monitors: [MonitorInfo] = []
    @Published var currentMonitor = 0
    @Published var hintCode = 0
    @Published var hintMessage = ""
    @Published var toast: String?
    @Published var pasteFeedback: Feedback?
    @Published var pasteBusy = false
    @Published var hasFirstFrame = false
    @Published var touchMode: TouchMode = .trackpad
    @Published var cursor: CursorGlyph?
    @Published var cursorVisible = true
    @Published var cursorMetaKind = 0
    @Published var remoteEditable = false
    @Published var initError: String?
    @Published var customName = ""
    @Published var relayOnly = false
    @Published var showStatsHud = false

    let modelName: String
    let bridge = RcBridge.shared

    private var cursorGlyphs: [Int: CursorGlyph] = [:]
    private var cursorOrder: [Int] = []
    private var currentCursorId = 0
    private var started = false
    private var toastWork: DispatchWorkItem?
    private let io = DispatchQueue(label: "rc.io", qos: .userInitiated)
    private let defaults = UserDefaults.standard

    static let maxNameLength = 32

    init() {
        modelName = UIDevice.current.model // "iPad" / "iPhone"
        customName = defaults.string(forKey: "device_name") ?? ""
        relayOnly = defaults.bool(forKey: "relay_only")
        showStatsHud = defaults.bool(forKey: "show_stats_hud")
        if let raw = defaults.string(forKey: "touch_mode") {
            touchMode = raw == "direct" ? .direct : .trackpad
        }
        bridge.model = self
        bridge.video.onFirstFrame = { [weak self] in self?.hasFirstFrame = true }
        bridge.video.onSizeChange = { [weak self] size in self?.streamSize = size }
    }

    var deviceName: String { customName.isEmpty ? modelName : customName }

    // MARK: - Lifecycle

    func start() {
        if started {
            refreshLan()
            return
        }
        started = true
        let dir = Self.filesDir()
        let name = deviceName
        let lan = LanTargets.csv()
        io.async {
            if let err = self.bridge.start(filesDir: dir, deviceName: name) {
                DispatchQueue.main.async { self.initError = err }
                return
            }
            if !lan.isEmpty { self.bridge.setLanTargets(lan) }
            self.bridge.triggerScan()
        }
    }

    func refreshLan() {
        let lan = LanTargets.csv()
        io.async {
            if !lan.isEmpty { self.bridge.setLanTargets(lan) }
            self.bridge.triggerScan()
        }
    }

    func didEnterBackground() {
        if layer == .session { io.async { self.bridge.releaseAll() } }
    }

    func didBecomeActive() {
        refreshLan()
        if layer == .session { bridge.video.recover() }
    }

    private static func filesDir() -> String {
        let fm = FileManager.default
        let base = (try? fm.url(for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true))
            ?? fm.temporaryDirectory
        let dir = base.appendingPathComponent("rc", isDirectory: true)
        try? fm.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.path
    }

    // MARK: - Devices

    func pasteCode(_ raw: String) {
        let text = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else {
            pasteFeedback = Feedback(ok: false, message: "请先粘贴连接码或自建中转码")
            return
        }
        pasteBusy = true
        pasteFeedback = nil
        io.async {
            let res = self.bridge.pasteCode(text)
            DispatchQueue.main.async {
                self.pasteBusy = false
                self.pasteFeedback = Feedback(ok: res?.ok ?? false, message: res?.message ?? "解析失败")
            }
        }
    }

    func setDeviceName(_ raw: String) {
        let cleaned = raw.replacingOccurrences(of: "[\\t\\r\\n]", with: " ", options: .regularExpression)
            .trimmingCharacters(in: .whitespaces)
        let name = String(cleaned.prefix(Self.maxNameLength))
        let custom = name == modelName ? "" : name
        guard custom != customName else { return }
        customName = custom
        defaults.set(custom, forKey: "device_name")
        let shown = deviceName
        io.async { self.bridge.setDeviceName(shown) }
    }

    func remember(_ d: DeviceItem) { io.async { self.bridge.remember(d.id) } }
    func agreeRemember(_ d: DeviceItem) { io.async { self.bridge.agreeRemember(d.id) } }
    func refuseRemember(_ d: DeviceItem) { io.async { self.bridge.refuseRemember(d.id) } }
    func cancelRemember(_ d: DeviceItem) { io.async { self.bridge.cancelRemember(d.id) } }
    func forget(_ d: DeviceItem) { io.async { self.bridge.forget(d.id) } }
    func retry(_ d: DeviceItem) { io.async { self.bridge.retry(d.id) } }

    // MARK: - Session

    func connect(_ device: DeviceItem) {
        bridge.video.reset()
        layer = .session
        activeDevice = device
        phase = .connecting
        sessionMessage = "正在连接 \(device.displayName)…"
        monitors = []
        currentMonitor = 0
        stats = LiveStats()
        hintCode = 0
        hintMessage = ""
        hasFirstFrame = false
        cursorGlyphs = [:]
        cursorOrder = []
        cursor = nil
        cursorVisible = true
        cursorMetaKind = 0
        remoteEditable = false
        UIApplication.shared.isIdleTimerDisabled = true
        let relay = relayOnly
        io.async {
            // Start at native resolution up to 4K: people zoom in to read desktop text, and the
            // host can still step down on slow links because fit_w / fit_h are set.
            if let err = self.bridge.connect(id: device.id, fitW: 3840, fitH: 2160, fps: 60, relayOnly: relay) {
                DispatchQueue.main.async {
                    self.phase = .error
                    self.sessionMessage = err
                }
            } else {
                self.sendQuality()
            }
        }
    }

    func reconnect() {
        if let d = activeDevice { connect(d) }
    }

    func disconnect() {
        bridge.video.reset()
        io.async {
            self.bridge.releaseAll()
            self.bridge.disconnect()
        }
        layer = .devices
        activeDevice = nil
        phase = .idle
        sessionMessage = ""
        monitors = []
        hasFirstFrame = false
        cursor = nil
        UIApplication.shared.isIdleTimerDisabled = false
    }

    func selectMonitor(_ index: Int) {
        currentMonitor = index
        io.async { self.bridge.selectMonitor(index) }
    }

    func setTouchMode(_ mode: TouchMode) {
        touchMode = mode
        defaults.set(mode == .direct ? "direct" : "trackpad", forKey: "touch_mode")
    }

    func setRelayOnly(_ on: Bool) {
        guard on != relayOnly else { return }
        relayOnly = on
        defaults.set(on, forKey: "relay_only")
        io.async { self.bridge.setRelayOnly(on) }
    }

    func setShowStatsHud(_ on: Bool) {
        showStatsHud = on
        defaults.set(on, forKey: "show_stats_hud")
    }

    /// Same as Android's "auto" picture: native resolution up to 4K, 60 fps with automatic
    /// frame rate, no bitrate cap; the host steps down on weak links.
    private func sendQuality() {
        bridge.setQuality(fitW: 3840, fitH: 2160, shortSide: 0, fps: 60, kbps: 0, autoFps: true)
    }

    // MARK: - Input helpers (cheap, called from touch handling on the main thread)

    func tapKey(_ scan: Int) {
        io.async {
            self.bridge.key(scan, down: true)
            self.bridge.key(scan, down: false)
        }
    }

    /// Presses `mods`, taps `scan`, then releases the mods in reverse order.
    func chord(_ mods: [Int], _ scan: Int) {
        io.async {
            for m in mods { self.bridge.key(m, down: true) }
            self.bridge.key(scan, down: true)
            self.bridge.key(scan, down: false)
            for m in mods.reversed() { self.bridge.key(m, down: false) }
        }
    }

    func sendText(_ s: String) {
        guard !s.isEmpty else { return }
        io.async { self.bridge.text(s) }
    }

    func showToast(_ message: String) {
        guard !message.isEmpty else { return }
        toast = message
        toastWork?.cancel()
        let work = DispatchWorkItem { [weak self] in self?.toast = nil }
        toastWork = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 2.4, execute: work)
    }

    // MARK: - Engine callbacks (main queue)

    func onDevicesUpdated(_ json: String) {
        guard let payload = try? JSONDecoder().decode(DevicesPayload.self, from: Data(json.utf8)) else { return }
        localDeviceId = payload.deviceId
        ownRelayCount = payload.ownRelayCount
        devices = payload.devices
        if let cur = activeDevice, let fresh = payload.devices.first(where: { $0.id == cur.id }) {
            activeDevice = fresh
        }
    }

    func onSessionState(_ state: String, _ message: String) {
        let p: SessionPhase
        switch state {
        case "connecting": p = .connecting
        case "waiting_consent": p = .waitingConsent
        case "connected": p = .connected
        case "paused": p = .paused
        case "disconnected": p = .disconnected
        case "error": p = .error
        default: p = .idle
        }
        phase = p
        sessionMessage = message
        if p == .connected {
            io.async { self.sendQuality() }
        }
    }

    func onStreamInfo(width: Int, height: Int, encoder: String, fps: Int, maxKbps: Int) {
        self.encoder = encoder
        // Once video is live the decoder reports the real picture size; before that, use this.
        if !hasFirstFrame, width > 0, height > 0 {
            streamSize = CGSize(width: width, height: height)
        }
    }

    func onMonitors(_ json: String, current: Int) {
        guard let list = try? JSONDecoder().decode([MonitorInfo].self, from: Data(json.utf8)) else { return }
        monitors = list
        currentMonitor = min(max(current, 0), max(list.count - 1, 0))
    }

    func onStats(fps: Int, kbps: Int, rttMs: Int, pathKind: String) {
        stats = LiveStats(fps: fps, kbps: kbps, rttMs: rttMs, pathKind: pathKind)
    }

    func onHint(code: Int, message: String) {
        hintCode = code
        hintMessage = message
    }

    func onCursorShape(id: Int, glyph: CursorGlyph) {
        if cursorGlyphs[id] == nil {
            cursorOrder.append(id)
            if cursorOrder.count > 64 {
                cursorGlyphs.removeValue(forKey: cursorOrder.removeFirst())
            }
        }
        cursorGlyphs[id] = glyph
        currentCursorId = id
        cursor = glyph
        cursorVisible = true
    }

    func onCursorUse(_ id: Int) {
        currentCursorId = id
        if let g = cursorGlyphs[id] { cursor = g }
        cursorVisible = true
    }

    func onCursorHidden() {
        cursorVisible = false
        cursorMetaKind = 0
    }

    func onRouteChanged(_ relayOnly: Bool) {
        if self.relayOnly != relayOnly {
            self.relayOnly = relayOnly
            defaults.set(relayOnly, forKey: "relay_only")
        }
    }
}
