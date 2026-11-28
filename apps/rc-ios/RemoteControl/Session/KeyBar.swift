import SwiftUI

/// Shortcut keys plus a draft box under the capsule. Text is typed into the draft (so Chinese
/// input methods can finish composing) and sent on 发送 or Return; keys go out right away.
/// Ctrl / Alt / Shift / Win are one-shot: tap one, then a key or a typed letter, and the chord
/// is sent with the modifier released afterwards.
struct KeyBar: View {
    @ObservedObject var model: AppModel
    let onClose: () -> Void

    @State private var draft = ""
    @State private var armed: [Int] = []
    @FocusState private var focused: Bool

    private struct Key: Identifiable {
        let label: String
        let scan: Int
        var chord: Int? = nil
        var mod = false
        var id: String { label }
    }

    private let keys: [Key] = [
        Key(label: "Esc", scan: Scan.esc),
        Key(label: "Tab", scan: Scan.tab),
        Key(label: "Ctrl", scan: Scan.ctrl, mod: true),
        Key(label: "Alt", scan: Scan.alt, mod: true),
        Key(label: "Shift", scan: Scan.shift, mod: true),
        Key(label: "Win", scan: Scan.win, mod: true),
        Key(label: "←", scan: Scan.left),
        Key(label: "↑", scan: Scan.up),
        Key(label: "↓", scan: Scan.down),
        Key(label: "→", scan: Scan.right),
        Key(label: "Home", scan: Scan.home),
        Key(label: "End", scan: Scan.end),
        Key(label: "PgUp", scan: Scan.pgUp),
        Key(label: "PgDn", scan: Scan.pgDn),
        Key(label: "⌫", scan: Scan.backspace),
        Key(label: "Del", scan: Scan.delete),
        Key(label: "Enter", scan: Scan.enter),
        Key(label: "复制", scan: 0x2E, chord: Scan.ctrl),
        Key(label: "粘贴", scan: 0x2F, chord: Scan.ctrl),
        Key(label: "撤销", scan: 0x2C, chord: Scan.ctrl),
        Key(label: "全选", scan: 0x1E, chord: Scan.ctrl),
    ] + (1...12).map { Key(label: "F\($0)", scan: Scan.f($0)) }

    var body: some View {
        VStack(spacing: 8) {
            HStack(spacing: 8) {
                TextField("输入文字，回车发送并换行", text: $draft)
                    .textFieldStyle(.plain)
                    .autocorrectionDisabled()
                    .textInputAutocapitalization(.never)
                    .focused($focused)
                    .submitLabel(.send)
                    .onSubmit { sendDraft(withEnter: true) }
                    .padding(.horizontal, 12)
                    .frame(height: 36)
                    .background(Color.white.opacity(0.1), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                Button("发送") { sendDraft(withEnter: false) }
                    .buttonStyle(.borderedProminent)
                    .disabled(draft.isEmpty)
                Button { onClose() } label: {
                    Image(systemName: "xmark").frame(width: 30, height: 30)
                }
                .accessibilityLabel("收起键盘")
            }
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 6) {
                    ForEach(keys) { k in
                        Button { tap(k) } label: {
                            Text(k.label)
                                .font(.system(size: 14, weight: .medium))
                                .frame(minWidth: 40, minHeight: 34)
                                .padding(.horizontal, 6)
                                .background(
                                    armed.contains(k.scan) && k.mod ? Color.accentColor.opacity(0.55) : Color.white.opacity(0.12),
                                    in: RoundedRectangle(cornerRadius: 8, style: .continuous)
                                )
                        }
                    }
                }
            }
        }
        .foregroundStyle(.white)
        .padding(10)
        .frame(maxWidth: 760)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
        .environment(\.colorScheme, .dark)
        .onAppear { focused = true }
    }

    private func tap(_ k: Key) {
        if k.mod {
            if let i = armed.firstIndex(of: k.scan) { armed.remove(at: i) } else { armed.append(k.scan) }
            return
        }
        if k.scan == Scan.backspace && !draft.isEmpty {
            draft.removeLast()
            return
        }
        var mods = armed
        if let c = k.chord, !mods.contains(c) { mods.insert(c, at: 0) }
        if mods.isEmpty { model.tapKey(k.scan) } else { model.chord(mods, k.scan) }
        armed = []
    }

    private func sendDraft(withEnter: Bool) {
        let text = draft
        draft = ""
        if !armed.isEmpty {
            // Ctrl + "c" from the soft keyboard means the real Ctrl+C.
            for ch in text {
                let s = Scan.forChar(ch)
                if s != 0 { model.chord(armed, s) } else { model.sendText(String(ch)) }
            }
            armed = []
        } else {
            // Newlines and tabs go as real Enter / Tab keys, everything else as text.
            var run = ""
            for ch in text {
                let key = ch == "\n" ? Scan.enter : ch == "\t" ? Scan.tab : 0
                if key == 0 {
                    run.append(ch)
                } else {
                    model.sendText(run)
                    run = ""
                    model.tapKey(key)
                }
            }
            model.sendText(run)
        }
        if withEnter { model.tapKey(Scan.enter) }
        focused = true
    }
}

/// Bottom-right four-way wheel: a tap is one notch, holding scrolls at a steady speed.
struct ScrollWheel: View {
    let onStart: () -> Void
    let onStep: (_ delta: Int, _ horizontal: Bool) -> Void

    private enum Dir { case up, down, left, right }

    @State private var dir: Dir?
    @State private var repeater = WheelRepeater()

    var body: some View {
        GeometryReader { geo in
            let size = min(geo.size.width, geo.size.height)
            ZStack {
                Circle().fill(Color.black.opacity(0.34))
                Circle().stroke(Color.white.opacity(0.24), lineWidth: 1)
                Circle().fill(Color.black.opacity(0.0)).frame(width: size * 0.45, height: size * 0.45)
                    .overlay(Circle().stroke(Color.white.opacity(0.24), lineWidth: 1))
                arrow("chevron.up", active: dir == .up).offset(y: -size * 0.36)
                arrow("chevron.down", active: dir == .down).offset(y: size * 0.36)
                arrow("chevron.left", active: dir == .left).offset(x: -size * 0.36)
                arrow("chevron.right", active: dir == .right).offset(x: size * 0.36)
            }
            .opacity(dir == nil ? 0.62 : 0.95)
            .contentShape(Circle())
            .gesture(
                DragGesture(minimumDistance: 0)
                    .onChanged { v in
                        let next = resolve(v.location, size: size)
                        if next != dir { change(to: next) }
                    }
                    .onEnded { _ in change(to: nil) }
            )
        }
        .accessibilityLabel("滚轮")
    }

    private func arrow(_ name: String, active: Bool) -> some View {
        Image(systemName: name)
            .font(.system(size: 13, weight: .bold))
            .foregroundStyle(active ? Color.accentColor : Color.white.opacity(0.85))
    }

    private func resolve(_ p: CGPoint, size: CGFloat) -> Dir? {
        let dx = p.x - size / 2, dy = p.y - size / 2
        let d = hypot(dx, dy)
        let outer = size / 2
        if d < outer * 0.45 * 0.6 || d > outer * 1.18 { return nil }
        if abs(dy) >= abs(dx) { return dy < 0 ? .up : .down }
        return dx < 0 ? .left : .right
    }

    private func change(to next: Dir?) {
        repeater.stop()
        let wasIdle = dir == nil
        dir = next
        guard let next else { return }
        if wasIdle { onStart() }
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
        let step = onStep
        let (delta, horizontal): (Int, Bool)
        switch next {
        case .up: (delta, horizontal) = (120, false)
        case .down: (delta, horizontal) = (-120, false)
        case .left: (delta, horizontal) = (-120, true)
        case .right: (delta, horizontal) = (120, true)
        }
        repeater.start { step(delta, horizontal) }
    }
}

/// Fires once now, again after 230 ms, then every 75 ms until stopped (as on Android).
final class WheelRepeater: NSObject {
    private var timer: Timer?
    private var step: () -> Void = {}

    func start(_ step: @escaping () -> Void) {
        stop()
        self.step = step
        step()
        timer = Timer.scheduledTimer(timeInterval: 0.23, target: self, selector: #selector(first), userInfo: nil, repeats: false)
    }

    func stop() {
        timer?.invalidate()
        timer = nil
    }

    @objc private func first() {
        step()
        timer = Timer.scheduledTimer(timeInterval: 0.075, target: self, selector: #selector(tick), userInfo: nil, repeats: true)
    }

    @objc private func tick() { step() }
}
