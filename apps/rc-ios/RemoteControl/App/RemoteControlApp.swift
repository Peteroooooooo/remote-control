import SwiftUI

@main
struct RemoteControlApp: App {
    @StateObject private var model = AppModel()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            Group {
                switch model.layer {
                case .devices: DevicesView(model: model)
                case .session: SessionView(model: model)
                }
            }
            .onAppear { model.start() }
        }
        .onChange(of: scenePhase) { phase in
            switch phase {
            case .active: model.didBecomeActive()
            case .background: model.didEnterBackground()
            default: break
            }
        }
    }
}
