import PitonKit
import PitonUI
import SwiftUI

/// The macOS app: a window with the session list and conversations, a Settings window (⌘,), and a menu bar item with
/// keep-awake. On first launch it asks what this Mac does (`Onboarding`): a host's app connects to its own daemon's
/// socket; a client's connects to another host over the tailnet, like the phone.
@main
struct PitonApp: App {
    /// The role and the host's address, and the one connection that follows them. Owned by the app, not a window:
    /// changing the role from Settings with the main window closed reconnects too.
    @State private var connection = Connection()

    var body: some Scene {
        WindowGroup(id: "main") {
            Group {
                if let store = connection.store {
                    RootView(store: store)
                } else {
                    Onboarding(role: connection.roleBinding, hostAddress: connection.hostAddressBinding)
                }
            }
            .frame(minWidth: 720, minHeight: 480)
            .pitonTheme()
        }
        Settings {
            if let store = connection.store {
                MacSettings(store: store, role: connection.roleBinding, hostAddress: connection.hostAddressBinding)
            } else {
                Onboarding(role: connection.roleBinding, hostAddress: connection.hostAddressBinding)
            }
        }
        // The window follows MacSettings' fixed width; only its height resizes.
        .windowResizability(.contentSize)
        MenuBarExtra {
            if let store = connection.store {
                MenuBarContent(store: store)
            } else {
                SetUpPrompt()
            }
        } label: {
            Image(systemName: connection.store?.daemon?.awake == true ? "cup.and.saucer.fill" : "cup.and.saucer")
        }
        .menuBarExtraStyle(.window)
    }
}

/// What this Mac does and where the app connects. The role and the host's address are kept in `UserDefaults`
/// (`MacRole.defaultsKey`, `hostAddress`, `hostPort`); setting either replaces the store: the old one is stopped
/// (it would otherwise keep reconnecting to the old host) and one for the new endpoint starts.
@MainActor
@Observable
final class Connection {
    private(set) var store: Store?
    private(set) var role: MacRole? {
        didSet { save() }
    }
    private(set) var hostAddress: String {
        didSet { save() }
    }
    private let hostPort: Int
    private let defaults = UserDefaults.standard

    init() {
        role = defaults.string(forKey: MacRole.defaultsKey).flatMap(MacRole.init(rawValue:))
        hostAddress = defaults.string(forKey: "hostAddress") ?? ""
        hostPort = defaults.object(forKey: "hostPort") as? Int ?? 7357
        connect()
    }

    var roleBinding: Binding<MacRole?> {
        Binding(get: { self.role }, set: { self.role = $0 })
    }

    var hostAddressBinding: Binding<String> {
        Binding(get: { self.hostAddress }, set: { self.hostAddress = $0 })
    }

    private func save() {
        defaults.set(role?.rawValue, forKey: MacRole.defaultsKey)
        defaults.set(hostAddress, forKey: "hostAddress")
        connect()
    }

    /// Where the app connects: this Mac's daemon (host), another host (client), or nowhere yet.
    private var endpoint: Endpoint? {
        switch role {
        case .host: .defaultLocal
        case .client:
            hostAddress.isEmpty ? nil : URL(string: "ws://\(hostAddress):\(hostPort)").map { .remote(url: $0) }
        case nil: nil
        }
    }

    private func connect() {
        let endpoint = endpoint
        guard store?.endpoint != endpoint else { return }
        store?.stop()
        store = endpoint.map { Store(endpoint: $0, client: "mac", device: .mac) }
        store?.start()
    }
}

/// The menu bar before setup: a way to the window that sets it up.
struct SetUpPrompt: View {
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Piton isn't set up yet.")
            Button("Open Piton") {
                openWindow(id: "main")
                NSApp.activate()
            }
        }
        .padding()
        .frame(width: 280, alignment: .leading)
    }
}

struct MenuBarContent: View {
    let store: Store

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            KeepAwakeToggle(store: store)
            Divider()
            HStack {
                Text("\(store.sessions.count) live session\(store.sessions.count == 1 ? "" : "s")")
                    .foregroundStyle(.secondary)
                Spacer()
                SettingsLink {
                    Text("Settings…")
                }
                Button("Open Piton") {
                    NSApp.activate()
                }
            }
            .font(.callout)
            if let version = store.daemon?.version {
                Text("Piton \(version)")
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
            }
        }
        .padding()
        .frame(width: 280)
    }
}
