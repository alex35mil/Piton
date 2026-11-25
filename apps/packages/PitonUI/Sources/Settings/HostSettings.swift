import PitonKit
import SwiftUI

/// Every setting of the host this app is connected to, as the daemon reports them: each with its value, the file it
/// lives in (`config.host.toml` or the global `config.toml`, shared or Local), whether it waits for a restart, and a
/// control when the apps may change it. Client preferences (fonts) are in Appearance, which also handles their
/// per-device Local choice.
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
                    let settings = state.settings.filter { HostSettingGroup($0) == group }
                    if !settings.isEmpty {
                        Section(group.title) {
                            ForEach(settings) { setting in
                                HostSettingRow(store: store, setting: setting)
                            }
                        }
                    }
                }
            } else {
                Text("Not connected to Piton.").foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Host")
    }
}

/// How the host's settings are grouped: by file, and within the host file by table.
enum HostSettingGroup: String, CaseIterable, Identifiable {
    case host, tailnet, daemon, debugging, projects

    var id: Self { self }

    /// A setting's group; client preferences are shown in Appearance, so they have none.
    init?(_ setting: Setting) {
        switch setting.scope {
        case .client: return nil
        case .project: self = .projects
        case .host:
            let table = setting.key.split(separator: ".").count > 1 ? setting.key.split(separator: ".").first : nil
            switch table.map(String.init) {
            // `[integrations.tailscale]` is remote access; the other integrations are the daemon's.
            case "integrations": self = setting.key.hasPrefix("integrations.tailscale.") ? .tailnet : .daemon
            case "debugging": self = .debugging
            case "pi", "sandbox", "env", "stop": self = .daemon
            case .some, nil:
                switch setting.key {
                case "socket", "log": self = .daemon
                default: self = .host
                }
            }
        }
    }

    var title: String {
        switch self {
        case .host: "This host"
        case .tailnet: "Remote access"
        case .daemon: "Daemon"
        case .debugging: "Debugging"
        case .projects: "Every project"
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
                if case .file(placement: .some) = setting.location, setting.editable {
                    Toggle("Local", isOn: local).font(.caption)
                        #if os(macOS)
                            .toggleStyle(.checkbox)
                            .help("Keep this value in the .local file: this host only, not shared through dotfiles")
                        #endif
                }
            }
            if let warning = setting.warning {
                Label(warning, systemImage: "exclamationmark.triangle")
                    .font(.caption)
                    .foregroundStyle(Theme.alert)
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
        case (true, .text), (true, .integer), (true, .duration), (true, .textList), (true, .choiceList):
            TextSettingField(
                label: label, text: setting.valueType.text(setting.value), parse: setting.valueType.parse(_:),
                save: save(_:))
        case (true, .map(let values)):
            MapSettingField(
                label: label, rows: SettingValueType.mapRows(setting.value), values: values, save: save(_:))
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
        switch setting.placement {
        case .some(let placement): setting.scope.fileName(placement)
        case nil: "Default"
        }
    }

    private var local: Binding<Bool> {
        Binding(
            get: {
                switch setting.placement {
                case .local: true
                case .shared, nil: false
                }
            },
            set: { isLocal in
                saving = true
                Task {
                    _ = await store.setSetting(
                        setting.scope, setting.key, to: setting.value, target: .file(local: isLocal))
                    saving = false
                }
            })
    }

    private func save(_ value: JSON) {
        saving = true
        Task {
            _ = await store.setSetting(setting.scope, setting.key, to: value)
            saving = false
        }
    }
}

/// A map edited as rows, each a key field and a value picker, with Add and Remove; saved as a whole with the Save
/// button, so a half-typed key never reaches the host.
struct MapSettingField: View {
    let label: Text
    let rows: [MapRow]
    let values: [String]
    let save: (JSON) -> Void
    @State private var draft: [MapRow] = []

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            label
            ForEach($draft) { $row in
                HStack {
                    TextField("Path, host, or name", text: $row.key)
                        .font(.callout.monospaced())
                    Picker("", selection: $row.value) {
                        ForEach(values, id: \.self) { Text($0).tag($0) }
                    }
                    .labelsHidden()
                    .fixedSize()
                    Button(role: .destructive) {
                        draft.removeAll { $0.id == row.id }
                    } label: {
                        Image(systemName: "minus.circle")
                    }
                    .buttonStyle(.borderless)
                    .accessibilityLabel("Remove \(row.key)")
                }
            }
            HStack {
                Button("Add") { draft.append(MapRow(key: "", value: values.first ?? "")) }
                Spacer()
                Button("Save", action: commit).disabled(value == nil || draft == rows)
            }
        }
        .onAppear { draft = rows }
        .onChange(of: rows) { _, new in draft = new }
    }

    private var value: JSON? { SettingValueType.mapValue(draft, values: values) }

    private func commit() {
        if let value { save(value) }
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
