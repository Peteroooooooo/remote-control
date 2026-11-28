import SwiftUI

/// Computer list: LAN computers appear by themselves, others are added with a code.
struct DevicesView: View {
    @ObservedObject var model: AppModel
    @State private var showAdd = false
    @State private var showSettings = false

    private var available: [DeviceItem] {
        model.devices.filter { $0.isOnline || $0.status == "checking" || $0.ask == "in" }
    }

    private var offline: [DeviceItem] {
        model.devices.filter { !$0.isOnline && $0.status != "checking" && $0.ask != "in" }
    }

    var body: some View {
        NavigationStack {
            List {
                if let err = model.initError {
                    Section {
                        Label("初始化失败：\(err)", systemImage: "exclamationmark.triangle.fill")
                            .foregroundStyle(.orange)
                    }
                }
                Section {
                    if available.isEmpty {
                        VStack(alignment: .leading, spacing: 6) {
                            HStack(spacing: 8) {
                                ProgressView()
                                Text("正在扫描局域网内的电脑…")
                            }
                            Text(emptyHint)
                                .font(.footnote)
                                .foregroundStyle(.secondary)
                        }
                        .padding(.vertical, 6)
                    }
                    ForEach(available) { d in
                        DeviceRow(model: model, device: d)
                    }
                } header: {
                    Text("可用电脑")
                }
                if !offline.isEmpty {
                    Section("离线") {
                        ForEach(offline) { d in
                            DeviceRow(model: model, device: d)
                        }
                    }
                }
            }
            .listStyle(.insetGrouped)
            .refreshable { model.refreshLan() }
            .navigationTitle("Remote Control")
            .toolbar {
                ToolbarItemGroup(placement: .navigationBarTrailing) {
                    Button { model.refreshLan() } label: { Image(systemName: "arrow.clockwise") }
                        .accessibilityLabel("扫描局域网")
                    Button { showAdd = true } label: { Image(systemName: "plus") }
                        .accessibilityLabel("粘贴连接码或中转码")
                    Button { showSettings = true } label: { Image(systemName: "gearshape") }
                        .accessibilityLabel("设置")
                }
            }
            .sheet(isPresented: $showAdd) { AddDeviceSheet(model: model) }
            .sheet(isPresented: $showSettings) { SettingsSheet(model: model) }
        }
    }

    private var emptyHint: String {
        let extra = model.ownRelayCount > 0 ? " · 已配置 \(model.ownRelayCount) 个自建中转" : ""
        return "同一局域网的电脑会自动出现；远程连接请点右上角 +\(extra)"
    }
}

private struct DeviceRow: View {
    @ObservedObject var model: AppModel
    let device: DeviceItem

    private var askIn: Bool { device.ask == "in" }
    private var askOut: Bool { device.ask == "out" }

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: "desktopcomputer")
                .font(.system(size: 20))
                .foregroundStyle(device.isOnline ? (device.remembered ? Color.accentColor : .primary) : .secondary)
                .frame(width: 28)
            VStack(alignment: .leading, spacing: 3) {
                Text(device.displayName)
                    .font(.body.weight(.medium))
                    .foregroundStyle(device.isOnline || device.status == "checking" || askIn ? Color.primary : Color.secondary)
                    .lineLimit(1)
                HStack(spacing: 6) {
                    Circle().fill(dotColor).frame(width: 6, height: 6)
                    Text(subtitle)
                        .font(.caption)
                        .foregroundStyle(warnText ? Color.orange : Color.secondary)
                        .lineLimit(warnText ? 2 : 1)
                }
            }
            Spacer(minLength: 8)
            trailing
        }
        .contentShape(Rectangle())
        .onTapGesture { tap() }
        .contextMenu { menuItems }
        .padding(.vertical, 2)
    }

    @ViewBuilder
    private var trailing: some View {
        if askIn {
            HStack(spacing: 6) {
                Button("拒绝") { model.refuseRemember(device) }.buttonStyle(.bordered)
                Button("同意") { model.agreeRemember(device) }.buttonStyle(.borderedProminent)
            }
        } else if askOut {
            Button("取消") { model.cancelRemember(device) }.buttonStyle(.bordered)
        } else {
            HStack(spacing: 4) {
                if !device.remembered && device.isOnline {
                    Button("记住") { model.remember(device) }
                        .buttonStyle(.bordered)
                }
                Menu { menuItems } label: {
                    Image(systemName: "ellipsis")
                        .frame(width: 30, height: 30)
                        .contentShape(Rectangle())
                }
                .accessibilityLabel("设备操作")
                if device.isOnline || device.status == "checking" {
                    Image(systemName: "chevron.right")
                        .font(.footnote.weight(.semibold))
                        .foregroundStyle(.tertiary)
                }
            }
        }
    }

    @ViewBuilder
    private var menuItems: some View {
        Text(device.remembered ? "已互相记住 · 电脑关掉大厅也能直接连" : "未记住 · 需要双方同意后才能长期保存")
        if !device.remembered && device.isOnline && device.ask == "none" {
            Button { model.remember(device) } label: { Label("记住这台电脑", systemImage: "star") }
        }
        if askOut {
            Button { model.cancelRemember(device) } label: { Label("取消记住请求", systemImage: "xmark") }
        }
        if device.status == "refused" || device.status == "offline" {
            Button { model.retry(device) } label: { Label("重新连接检测", systemImage: "arrow.clockwise") }
        }
        if device.remembered || device.status == "refused" || device.status == "offline" {
            Button(role: .destructive) { model.forget(device) } label: {
                Label(device.remembered ? "忘记并删除这台电脑" : "从列表移除", systemImage: "trash")
            }
        }
    }

    private func tap() {
        if askIn {
            model.agreeRemember(device)
        } else if device.isOnline || device.status == "checking" {
            model.connect(device)
        } else if device.status == "refused" {
            model.retry(device)
        }
    }

    private var warnText: Bool {
        askIn || (device.status == "refused" && !device.error.isEmpty)
    }

    private var dotColor: Color {
        if askIn || device.status == "checking" || device.status == "refused" { return .orange }
        return device.isOnline ? .green : Color(uiColor: .tertiaryLabel)
    }

    private var subtitle: String {
        if askIn { return "想记住这台设备 · 同意后可随时连接" }
        if askOut { return "已发送记住请求 · 等待电脑端同意" }
        if device.isOnline {
            var parts: [String] = []
            if !device.pathLabel.isEmpty { parts.append(device.pathLabel) }
            if device.rttMs > 0 { parts.append("\(device.rttMs) ms") }
            if device.remembered { parts.append("已记住") }
            return parts.isEmpty ? "在线" : parts.joined(separator: " · ")
        }
        switch device.status {
        case "checking": return "检测连接中…"
        case "refused": return "\(device.error.isEmpty ? "对方拒绝连接" : device.error)（点此重试）"
        default: return device.remembered ? "离线 · 已记住" : "离线"
        }
    }
}

private struct AddDeviceSheet: View {
    @ObservedObject var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @State private var code = ""

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("粘贴 rc1… 连接码或 rcr1… 中转码", text: $code, axis: .vertical)
                        .lineLimit(3...6)
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                        .font(.system(.body, design: .monospaced))
                    Button {
                        if let s = UIPasteboard.general.string { code = s }
                    } label: {
                        Label("从剪贴板粘贴", systemImage: "doc.on.clipboard")
                    }
                } footer: {
                    Text("电脑上点「复制本机连接码」，发到这台设备后粘贴到这里。自建中转码添加后，同一中转下的电脑都能连。")
                }
                if let fb = model.pasteFeedback {
                    Section {
                        Label(fb.message, systemImage: fb.ok ? "checkmark.circle.fill" : "exclamationmark.circle.fill")
                            .foregroundStyle(fb.ok ? .green : .orange)
                    }
                }
            }
            .navigationTitle("添加电脑或中转")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("关闭") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    if model.pasteBusy {
                        ProgressView()
                    } else {
                        Button("添加") { model.pasteCode(code) }
                            .disabled(code.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    }
                }
            }
            .onAppear { model.pasteFeedback = nil }
            .onChange(of: model.pasteFeedback) { fb in
                if fb?.ok == true {
                    DispatchQueue.main.asyncAfter(deadline: .now() + 0.8) { dismiss() }
                }
            }
        }
    }
}

private struct SettingsSheet: View {
    @ObservedObject var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @State private var name = ""
    @State private var saved = false

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField(model.modelName, text: $name)
                        .onSubmit(save)
                        .submitLabel(.done)
                } header: {
                    Text("设备名称")
                } footer: {
                    Text(saved ? "✓ 已保存" : "电脑上看到的这台设备的名称，留空则使用设备型号")
                }
                Section("连接") {
                    Toggle("只走中转", isOn: Binding(get: { model.relayOnly }, set: { model.setRelayOnly($0) }))
                    Picker("默认触控模式", selection: Binding(get: { model.touchMode }, set: { model.setTouchMode($0) })) {
                        Text("触控板").tag(TouchMode.trackpad)
                        Text("直触").tag(TouchMode.direct)
                    }
                }
                Section("关于") {
                    LabeledContent("版本", value: Self.version)
                    if !model.localDeviceId.isEmpty {
                        LabeledContent("本机 ID", value: String(model.localDeviceId.prefix(10)))
                    }
                }
            }
            .navigationTitle("设置")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("完成") {
                        save()
                        dismiss()
                    }
                }
            }
            .onAppear { name = model.customName }
        }
    }

    private func save() {
        model.setDeviceName(name)
        saved = true
    }

    private static var version: String {
        let v = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "?"
        let b = Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "?"
        return "\(v) (\(b))"
    }
}
