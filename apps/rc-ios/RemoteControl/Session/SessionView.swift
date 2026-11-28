import SwiftUI

/// Full-screen remote desktop with the floating capsule, key bar and scroll wheel.
struct SessionView: View {
    @ObservedObject var model: AppModel
    @StateObject private var screen = ScreenController()
    @State private var keyBarOpen = false

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()

            RemoteScreen(model: model, controller: screen) { toggleKeyBar() }
                .ignoresSafeArea()

            if !(model.phase == .connected && model.hasFirstFrame) {
                ConnectingOverlay(model: model)
            }

            VStack(spacing: 8) {
                SessionCapsule(model: model, screen: screen, keyBarOpen: keyBarOpen) { toggleKeyBar() }
                if keyBarOpen {
                    KeyBar(model: model) { closeKeyBar() }
                        .transition(.move(edge: .top).combined(with: .opacity))
                }
                Spacer()
            }
            .padding(.top, 8)
            .padding(.horizontal, 12)

            if model.showStatsHud && model.phase == .connected {
                VStack {
                    HStack {
                        StatsHud(stats: model.stats)
                        Spacer()
                    }
                    Spacer()
                }
                .padding(.leading, 12)
                .padding(.top, 60)
                .allowsHitTesting(false)
            }

            if model.phase == .connected && model.hasFirstFrame {
                VStack {
                    Spacer()
                    HStack {
                        Spacer()
                        ScrollWheel(
                            onStart: { screen.prepareWheel() },
                            onStep: { delta, horizontal in model.bridge.wheel(delta, horizontal: horizontal) }
                        )
                        .frame(width: 104, height: 104)
                    }
                }
                .padding(.trailing, 20)
                .padding(.bottom, 24)
            }

            if let t = model.toast {
                VStack {
                    Spacer()
                    Text(t)
                        .font(.footnote)
                        .foregroundStyle(.white)
                        .padding(.horizontal, 14)
                        .padding(.vertical, 9)
                        .background(.ultraThinMaterial, in: Capsule())
                        .environment(\.colorScheme, .dark)
                        .padding(.bottom, 40)
                }
                .allowsHitTesting(false)
            }
        }
        .statusBarHidden(true)
        .persistentSystemOverlays(.hidden)
        .defersSystemGestures(on: .all)
        .animation(.easeOut(duration: 0.15), value: keyBarOpen)
    }

    private func toggleKeyBar() {
        if keyBarOpen { closeKeyBar() } else { keyBarOpen = true }
    }

    private func closeKeyBar() {
        keyBarOpen = false
        screen.focusScreen()
    }
}

// MARK: - Connecting / consent / error card

private struct ConnectingOverlay: View {
    @ObservedObject var model: AppModel

    var body: some View {
        VStack(spacing: 14) {
            switch model.phase {
            case .error, .disconnected:
                Image(systemName: "exclamationmark.triangle.fill")
                    .font(.system(size: 30))
                    .foregroundStyle(.orange)
            case .waitingConsent:
                Image(systemName: "hand.raised.fill")
                    .font(.system(size: 30))
                    .foregroundStyle(.yellow)
            default:
                ProgressView().tint(.white).scaleEffect(1.2)
            }
            Text(title)
                .font(.headline)
                .foregroundStyle(.white)
                .multilineTextAlignment(.center)
            if !detail.isEmpty {
                Text(detail)
                    .font(.subheadline)
                    .foregroundStyle(.white.opacity(0.7))
                    .multilineTextAlignment(.center)
            }
            HStack(spacing: 12) {
                if model.phase == .error || model.phase == .disconnected {
                    Button("重新连接") { model.reconnect() }
                        .buttonStyle(.borderedProminent)
                }
                Button(model.phase == .error || model.phase == .disconnected ? "返回" : "取消") { model.disconnect() }
                    .buttonStyle(.bordered)
                    .tint(.white)
            }
            .padding(.top, 4)
        }
        .padding(24)
        .frame(maxWidth: 360)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 20, style: .continuous))
        .environment(\.colorScheme, .dark)
        .padding(24)
    }

    private var title: String {
        switch model.phase {
        case .waitingConsent: return "等待电脑端同意…"
        case .connected: return "正在接收画面…"
        case .paused: return "画面已暂停"
        case .disconnected: return "连接已断开"
        case .error: return "连接失败"
        default: return model.sessionMessage.isEmpty ? "正在连接…" : model.sessionMessage
        }
    }

    private var detail: String {
        switch model.phase {
        case .waitingConsent: return "请在电脑上点「允许」"
        case .connecting: return model.hintMessage
        case .idle: return ""
        default:
            if !model.hintMessage.isEmpty && model.phase == .connected { return model.hintMessage }
            return model.phase == .connected ? "" : model.sessionMessage
        }
    }
}

// MARK: - Top capsule

private struct SessionCapsule: View {
    @ObservedObject var model: AppModel
    @ObservedObject var screen: ScreenController
    let keyBarOpen: Bool
    let onKeyboard: () -> Void

    var body: some View {
        HStack(spacing: 2) {
            capsuleButton("chevron.left", label: "断开") { model.disconnect() }
            divider
            Button {
                model.setTouchMode(model.touchMode == .trackpad ? .direct : .trackpad)
            } label: {
                Text(model.touchMode == .trackpad ? "触控板" : "直触")
                    .font(.system(size: 14, weight: .medium))
                    .frame(minWidth: 54, minHeight: 36)
            }
            if screen.zoomed {
                divider
                Button { screen.resetZoom() } label: {
                    Image(systemName: "arrow.down.right.and.arrow.up.left")
                        .frame(width: 40, height: 36)
                }
                .accessibilityLabel("恢复原大小")
            }
            divider
            capsuleButton(keyBarOpen ? "keyboard.chevron.compact.down" : "keyboard", label: "键盘") { onKeyboard() }
            divider
            Menu {
                if !infoLine.isEmpty {
                    Text(infoLine)
                }
                if model.monitors.count > 1 {
                    Section("显示器") {
                        ForEach(model.monitors) { m in
                            Button {
                                model.selectMonitor(m.index)
                            } label: {
                                if m.index == model.currentMonitor {
                                    Label("\(m.title) · \(m.shortRes)", systemImage: "checkmark")
                                } else {
                                    Text("\(m.title) · \(m.shortRes)")
                                }
                            }
                        }
                    }
                }
                Toggle("只走中转", isOn: Binding(get: { model.relayOnly }, set: { model.setRelayOnly($0) }))
                Toggle("显示帧率与延迟", isOn: Binding(get: { model.showStatsHud }, set: { model.setShowStatsHud($0) }))
                Button(role: .destructive) { model.disconnect() } label: {
                    Label("断开连接", systemImage: "xmark.circle")
                }
            } label: {
                Image(systemName: "ellipsis")
                    .frame(width: 40, height: 36)
            }
        }
        .foregroundStyle(.white)
        .padding(.horizontal, 6)
        .background(.ultraThinMaterial, in: Capsule())
        .environment(\.colorScheme, .dark)
    }

    private var infoLine: String {
        let s = model.stats
        var parts: [String] = []
        if let name = model.activeDevice?.displayName { parts.append(name) }
        if !s.pathLabel.isEmpty { parts.append(s.pathLabel) }
        if s.rttMs > 0 { parts.append("\(s.rttMs) ms") }
        if s.fps > 0 { parts.append("\(s.fps) fps") }
        return parts.joined(separator: " · ")
    }

    private var divider: some View {
        Rectangle().fill(.white.opacity(0.18)).frame(width: 0.5, height: 20)
    }

    private func capsuleButton(_ icon: String, label: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Image(systemName: icon).frame(width: 40, height: 36)
        }
        .accessibilityLabel(label)
    }
}

private struct StatsHud: View {
    let stats: LiveStats

    var body: some View {
        Text("\(stats.fps) fps · \(stats.rttMs) ms\(stats.pathLabel.isEmpty ? "" : " · \(stats.pathLabel)")")
            .font(.system(size: 11, weight: .medium, design: .monospaced))
            .foregroundStyle(.white)
            .padding(.horizontal, 8)
            .padding(.vertical, 4)
            .background(Color.black.opacity(0.45), in: Capsule())
    }
}
