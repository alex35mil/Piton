import PitonKit
import SwiftUI

/// Starting a session: pick a workspace under the daemon's roots, the launch mode, and an optional first prompt.
struct NewSessionView: View {
    let store: Store
    let done: (SessionKey?) -> Void
    @State private var workspace = ""
    @State private var prompt = ""
    @State private var host = false
    @State private var busy = false
    /// The repository's trust question, while it's being asked.
    @State private var trust: TrustNeeded?

    var body: some View {
        NavigationStack {
            Form {
                Section("Workspace") {
                    NavigationLink {
                        WorkspaceBrowser(store: store, path: nil) { picked in workspace = picked }
                    } label: {
                        if workspace.isEmpty {
                            Text("Choose…").foregroundStyle(.secondary)
                        } else {
                            Text(URL(fileURLWithPath: workspace).lastPathComponent)
                        }
                    }
                }
                Toggle("Run on the host (no sandbox)", isOn: $host)
                TextField("First prompt (optional)", text: $prompt, axis: .vertical)
                    .lineLimit(3...8)
                if let error = store.lastError {
                    Text(error).foregroundStyle(.red).font(.caption)
                }
            }
            .navigationTitle("New session")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { done(nil) }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Start") { Task { await start() } }
                        .disabled(workspace.isEmpty || busy)
                }
            }
            .sheet(item: $trust) { needed in
                TrustQuestion(needed: needed) { decision in
                    trust = nil
                    guard let decision else { return }
                    Task {
                        busy = true
                        _ = await store.decideTrust(workspace: workspace, decision, grants: needed.grants)
                        // Recorded: this starts the session. Refused (the grants changed since it was shown): this
                        // asks again with the current grants.
                        await start()
                    }
                }
            }
        }
    }

    private func start() async {
        busy = true
        let outcome = await store.startSession(
            workspace: workspace, mode: host ? .host : .sandbox, prompt: prompt.isEmpty ? nil : prompt)
        busy = false
        switch outcome {
        case .started(let info): done(info.key)
        case .trustNeeded(let needed): trust = needed
        case nil: break
        }
    }
}

extension TrustNeeded: Identifiable {
    public var id: String { source + grants.joined(separator: "\n") }
}

/// Asks whether to trust a repository before its first session: its committed grants (new ones marked) and what pi
/// would load from it. `answer` gets the decision, or nil to cancel.
struct TrustQuestion: View {
    let needed: TrustNeeded
    let answer: (TrustDecision?) -> Void

    var body: some View {
        NavigationStack {
            List {
                Section {
                    Text(
                        "Trusting it applies its committed grants and lets pi load its project files. Its restrictions apply either way."
                    )
                    .font(.callout)
                }
                if !needed.grants.isEmpty {
                    Section("Committed grants") {
                        ForEach(needed.grants, id: \.self) { grant in
                            HStack {
                                Text(grant).font(.caption.monospaced())
                                if needed.isNew(grant) {
                                    Spacer()
                                    Text("new").font(.caption2).foregroundStyle(.orange)
                                }
                            }
                        }
                    }
                }
                if !needed.resources.pi.isEmpty {
                    Section {
                        ForEach(needed.resources.pi, id: \.self) { Text($0).font(.caption.monospaced()) }
                    } header: {
                        Text("pi's project files")
                    } footer: {
                        Text("Extensions run code in pi's process.")
                    }
                }
                if !needed.resources.mcpServers.isEmpty {
                    Section("MCP servers") {
                        ForEach(needed.resources.mcpServers, id: \.self) { Text($0) }
                    }
                }
            }
            .navigationTitle("Trust \(URL(fileURLWithPath: needed.source).lastPathComponent)?")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { answer(nil) }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Trust") { answer(.trusted) }
                }
                ToolbarItem(placement: .destructiveAction) {
                    Button("Don't Trust") { answer(.declined) }
                }
            }
        }
    }
}

/// Browses the daemon's workspace roots one level at a time; picking a checkout pops back to the sheet.
struct WorkspaceBrowser: View {
    let store: Store
    let path: String?
    let pick: (String) -> Void
    @State private var load = Load.loading
    @Environment(\.dismiss) private var dismiss

    enum Load {
        case loading
        case loaded([WorkspaceEntry])
        /// The daemon refused (for example, no roots configured) or the connection failed.
        case failed(String)
    }

    var body: some View {
        List {
            switch load {
            case .loading:
                ProgressView()
            case .failed(let message):
                Label(message, systemImage: "exclamationmark.triangle")
                    .foregroundStyle(.secondary)
            case .loaded(let entries):
                if entries.isEmpty {
                    Text("Nothing here").foregroundStyle(.secondary)
                }
                ForEach(entries) { entry in
                    switch entry {
                    case .group(let name, let path):
                        NavigationLink {
                            WorkspaceBrowser(store: store, path: path) { picked in
                                pick(picked)
                                dismiss()
                            }
                        } label: {
                            Label(name, systemImage: "folder")
                        }
                    case .workspace(let name, let path, let source):
                        Button {
                            pick(path)
                            dismiss()
                        } label: {
                            Label {
                                VStack(alignment: .leading) {
                                    Text(name)
                                    if let source {
                                        Text("worker of \(URL(fileURLWithPath: source).lastPathComponent)")
                                            .font(.caption).foregroundStyle(.secondary)
                                    }
                                }
                            } icon: {
                                Image(systemName: "shippingbox")
                            }
                        }
                    }
                }
            }
        }
        .navigationTitle(path.map { URL(fileURLWithPath: $0).lastPathComponent } ?? "Workspaces")
        .task {
            do {
                load = .loaded(try await store.browse(path))
            } catch {
                load = .failed(String(describing: error))
            }
        }
    }
}
