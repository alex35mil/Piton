import PitonKit
import SwiftUI

/// Every setting of the host this app is connected to, as the daemon reports them: each with its value, where it
/// lives (the host's state, or its global `config.toml` and whether shared or Local), whether it waits for a
/// restart, and a control when the apps may change it. Client preferences (fonts) are in Appearance, which also
/// handles their per-device Local choice.
public struct HostSettings: View {
    let store: Store

    public init(store: Store) {
        self.store = store
    }

    public var body: some View {
        Form {
            if let state = store.daemon {
                if let error = state.settingsError {
                    Section {
                        Label(error, systemImage: "exclamationmark.triangle")
                            .foregroundStyle(Theme.alert)
                            .textSelection(.enabled)
                    } footer: {
                        Text("The host keeps its last good values until the file is fixed.")
                    }
                }
                ForEach(HostSettingGroup.allCases) { group in
                    let settings = state.settings.filter { HostSettingGroup(key: $0.key) == group }
                    if !settings.isEmpty {
                        Section(group.title) {
                            ForEach(settings) { setting in
                                HostSettingRow(store: store, setting: setting)
                            }
                        }
                    }
                }
            } else {
                Text("Not connected to piton.").foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Host")
    }
}

/// How the host's settings are grouped, by key prefix.
enum HostSettingGroup: String, CaseIterable, Identifiable {
    case host, sandbox, tailnet, daemon

    var id: Self { self }

    /// A setting's group; client preferences are shown in Appearance, so they have none.
    init?(key: String) {
        let prefix = key.split(separator: ".").first.map(String.init) ?? key
        switch prefix {
        case "host", "keep_awake": self = .host
        case "sandbox", "workers": self = .sandbox
        case "tailnet": self = .tailnet
        case "client": return nil
        default: self = .daemon
        }
    }

    var title: String {
        switch self {
        case .host: "This host"
        case .sandbox: "Sandbox"
        case .tailnet: "Remote access"
        case .daemon: "Daemon"
        }
    }
}

/// One setting: its control (or value), where it lives, and the Local checkbox for configuration.
struct HostSettingRow: View {
    let store: Store
    let setting: Setting
    @State private var saving = false

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            control
            HStack(spacing: 8) {
                Text(origin).font(.caption).foregroundStyle(.secondary)
                if case .restart(pending: true) = setting.applies {
                    Text("Restart the daemon to apply").font(.caption).foregroundStyle(Theme.alert)
                }
                Spacer()
                // Local moves a value between the files; a key no file sets has nothing to move yet.
                if case .config(.some) = setting.kind, setting.editable {
                    Toggle("Local", isOn: local).font(.caption)
                        #if os(macOS)
                            .toggleStyle(.checkbox)
                            .help("Keep this value in the .local file: this host only, not shared through dotfiles")
                        #endif
                }
            }
        }
        .disabled(saving)
    }

    @ViewBuilder private var control: some View {
        let label = Text(setting.key).font(.callout.monospaced())
        switch (setting.editable, setting.valueType) {
        case (true, .bool):
            Toggle(isOn: Binding(get: { setting.value == .bool(true) }, set: { save(.bool($0)) })) { label }
        case (true, .choice(let options)):
            Picker(selection: Binding(get: { setting.value.string ?? "" }, set: { save(.string($0)) })) {
                ForEach(options, id: \.self) { Text($0).tag($0) }
            } label: {
                label
            }
        case (true, .text), (true, .integer), (true, .textList), (true, .integerList), (true, .networkProfile):
            TextSettingField(
                label: label, text: setting.valueType.text(setting.value), parse: setting.valueType.parse(_:),
                save: save(_:))
        case (false, _):
            LabeledContent {
                Text(setting.valueType.text(setting.value)).textSelection(.enabled)
            } label: {
                label
            }
        }
    }

    /// Where the value comes from.
    private var origin: String {
        switch setting.kind {
        case .state: "This host's state"
        case .config(.some(.local)): "config.local.toml"
        case .config(.some(.shared)): "config.toml"
        case .config(nil): "Default"
        }
    }

    private var local: Binding<Bool> {
        Binding(
            get: {
                switch setting.kind {
                case .config(.some(.local)): true
                case .config(.some(.shared)), .config(nil), .state: false
                }
            },
            set: { isLocal in
                saving = true
                Task {
                    _ = await store.setSetting(setting.key, to: setting.value, local: isLocal)
                    saving = false
                }
            })
    }

    private func save(_ value: JSON) {
        saving = true
        Task {
            _ = await store.setSetting(setting.key, to: value)
            saving = false
        }
    }
}

/// A text value edited in place and saved on Return (or the Save button), so a half-typed value never reaches the host.
struct TextSettingField: View {
    let label: Text
    let text: String
    let parse: (String) -> JSON?
    let save: (JSON) -> Void
    @State private var draft = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            label
            HStack(alignment: .top) {
                TextField("Default", text: $draft, axis: .vertical)
                    .font(.callout.monospaced())
                    .lineLimit(1...8)
                    .onSubmit(commit)
                Button("Save", action: commit)
                    .disabled(draft == text || parse(draft) == nil)
            }
        }
        .onAppear { draft = text }
        .onChange(of: text) { _, new in draft = new }
    }

    private func commit() {
        if draft != text, let value = parse(draft) { save(value) }
    }
}
