import Foundation
import RcCore

/// Thin Swift wrapper over the Rust engine's C API. Callbacks arrive on engine threads; video
/// frames go straight to the video pipeline, everything else hops to the main actor.
final class RcBridge {
    static let shared = RcBridge()

    let video = VideoPipeline()
    /// Set once by the app model; read from engine threads.
    weak var model: AppModel?

    private init() {
        video.requestKeyframe = { rc_request_keyframe() }
    }

    // MARK: - Calls into the engine

    /// Returns an error text, or nil on success.
    func start(filesDir: String, deviceName: String) -> String? {
        var cb = RcCallbacks()
        cb.ctx = Unmanaged.passUnretained(self).toOpaque()
        cb.on_devices_updated = { ctx, json in
            let s = RcBridge.str(json)
            RcBridge.main(ctx) { $0.onDevicesUpdated(s) }
        }
        cb.on_session_state = { ctx, state, message in
            let s = RcBridge.str(state), m = RcBridge.str(message)
            RcBridge.main(ctx) { $0.onSessionState(s, m) }
        }
        cb.on_stream_info = { ctx, w, h, encoder, fps, maxKbps in
            let e = RcBridge.str(encoder)
            RcBridge.main(ctx) { $0.onStreamInfo(width: Int(w), height: Int(h), encoder: e, fps: Int(fps), maxKbps: Int(maxKbps)) }
        }
        cb.on_monitors = { ctx, json, current in
            let s = RcBridge.str(json)
            RcBridge.main(ctx) { $0.onMonitors(s, current: Int(current)) }
        }
        cb.on_windows = { _, _ in }
        cb.on_app_icon = { _, _, _, _ in }
        cb.on_window_thumb = { _, _, _, _ in }
        cb.on_stats = { ctx, fps, kbps, rtt, pathKind in
            let p = RcBridge.str(pathKind)
            RcBridge.main(ctx) { $0.onStats(fps: Int(fps), kbps: Int(kbps), rttMs: Int(rtt), pathKind: p) }
        }
        cb.on_hint = { ctx, code, message in
            let m = RcBridge.str(message)
            RcBridge.main(ctx) { $0.onHint(code: Int(code), message: m) }
        }
        cb.on_notice = { ctx, message in
            let m = RcBridge.str(message)
            RcBridge.main(ctx) { $0.showToast(m) }
        }
        cb.on_cursor_shape = { ctx, id, w, h, hx, hy, rgba, len in
            guard let glyph = CursorGlyph.make(width: Int(w), height: Int(h), hotX: Int(hx), hotY: Int(hy), rgba: rgba, len: len) else { return }
            RcBridge.main(ctx) { $0.onCursorShape(id: Int(id), glyph: glyph) }
        }
        cb.on_cursor_use = { ctx, id in RcBridge.main(ctx) { $0.onCursorUse(Int(id)) } }
        cb.on_cursor_hidden = { ctx in RcBridge.main(ctx) { $0.onCursorHidden() } }
        cb.on_cursor_meta = { ctx, kind in RcBridge.main(ctx) { $0.cursorMetaKind = Int(kind) } }
        cb.on_text_focus = { ctx, editable in RcBridge.main(ctx) { $0.remoteEditable = editable } }
        cb.on_virtual_display = { _, _, _, _, _ in }
        cb.on_route_changed = { ctx, relayOnly in RcBridge.main(ctx) { $0.onRouteChanged(relayOnly) } }
        cb.on_video_frame = { ctx, codec, key, w, h, _, data, len in
            guard let ctx, let data, len > 0 else { return }
            let bridge = Unmanaged<RcBridge>.fromOpaque(ctx).takeUnretainedValue()
            bridge.video.submit(hevc: codec == 1, key: key, width: Int(w), height: Int(h), data: Data(bytes: data, count: len))
        }
        let err = RcBridge.take(rc_init(filesDir, deviceName, cb))
        return err.isEmpty ? nil : err
    }

    func setLanTargets(_ csv: String) { rc_set_broadcasts(csv) }
    func setDeviceName(_ name: String) { rc_set_device_name(name) }
    func triggerScan() { rc_trigger_scan() }

    func pasteCode(_ text: String) -> PasteResponse? {
        let json = RcBridge.take(rc_paste_code(text))
        return try? JSONDecoder().decode(PasteResponse.self, from: Data(json.utf8))
    }

    /// Returns an error text, or nil on success.
    func connect(id: String, fitW: Int, fitH: Int, fps: Int, relayOnly: Bool) -> String? {
        let err = RcBridge.take(rc_connect(id, Int32(fitW), Int32(fitH), Int32(fps), relayOnly))
        return err.isEmpty ? nil : err
    }

    func setRelayOnly(_ on: Bool) { rc_set_relay_only(on) }
    func disconnect() { rc_disconnect() }
    func selectMonitor(_ index: Int) { rc_select_monitor(Int32(index)) }

    func setQuality(fitW: Int, fitH: Int, shortSide: Int, fps: Int, kbps: Int, autoFps: Bool) {
        rc_set_quality(Int32(fitW), Int32(fitH), Int32(shortSide), Int32(fps), Int32(kbps), false, autoFps)
    }

    /// Normalized 0…1 position on the streamed monitor.
    func mouseMove(_ nx: CGFloat, _ ny: CGFloat) {
        rc_send_mouse_move(RcBridge.u16(nx), RcBridge.u16(ny))
    }

    func mouseButton(_ button: Int, down: Bool) { rc_send_mouse_button(Int32(button), down) }

    func touch(id: Int, phase: Int, _ nx: CGFloat, _ ny: CGFloat) {
        rc_send_touch(Int32(id), Int32(phase), RcBridge.u16(nx), RcBridge.u16(ny))
    }

    /// `phase`: 1 down, 2 move, 3 up. `pressure` 0…1.
    func pen(phase: Int, _ nx: CGFloat, _ ny: CGFloat, pressure: CGFloat, flags: Int = 0) {
        let p = Int32((min(max(pressure, 0), 1) * 1024).rounded())
        rc_send_pen(Int32(phase), RcBridge.u16(nx), RcBridge.u16(ny), p, 0, 0, Int32(flags))
    }

    func wheel(_ delta: Int, horizontal: Bool) { rc_send_wheel(Int32(delta), horizontal) }
    func key(_ scan: Int, down: Bool) { rc_send_key(Int32(scan), down) }
    func text(_ s: String) { if !s.isEmpty { rc_send_text(s) } }
    func releaseAll() { rc_release_all() }

    func remember(_ id: String) { rc_remember_device(id) }
    func forget(_ id: String) { rc_forget_device(id) }
    func agreeRemember(_ id: String) { rc_agree_remember(id) }
    func refuseRemember(_ id: String) { rc_refuse_remember(id) }
    func cancelRemember(_ id: String) { rc_cancel_remember(id) }
    func retry(_ id: String) { rc_retry_device(id) }

    // MARK: - Helpers

    private static func u16(_ v: CGFloat) -> Int32 {
        Int32((min(max(v, 0), 1) * 65535).rounded())
    }

    fileprivate static func str(_ p: UnsafePointer<CChar>?) -> String {
        guard let p else { return "" }
        return String(cString: p)
    }

    /// Copies and frees a string returned by the engine.
    private static func take(_ p: UnsafeMutablePointer<CChar>?) -> String {
        guard let p else { return "" }
        defer { rc_string_free(p) }
        return String(cString: p)
    }

    /// Runs `body` on the main queue, where the app model lives.
    fileprivate static func main(_ ctx: UnsafeMutableRawPointer?, _ body: @escaping (AppModel) -> Void) {
        guard let ctx else { return }
        let bridge = Unmanaged<RcBridge>.fromOpaque(ctx).takeUnretainedValue()
        DispatchQueue.main.async {
            if let m = bridge.model { body(m) }
        }
    }
}
