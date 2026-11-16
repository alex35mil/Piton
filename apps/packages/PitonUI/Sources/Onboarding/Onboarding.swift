#if os(macOS)
    import PitonKit
    import ServiceManagement
    import SwiftUI

    /// The Mac's first launch: what this Mac does for piton (host or client), then, for a host, every setup step with
    /// its fix. A client asks for its host's address, like the phone. The choice is kept in `UserDefaults`
    /// (`MacRole.defaultsKey`) and can be changed later in Settings.
    public struct Onboarding: View {
        @Binding var role: MacRole?
        @Binding var hostAddress: String
        @State private var picked: MacRole = .host

        public init(role: Binding<MacRole?>, hostAddress: Binding<String>) {
            _role = role
            _hostAddress = hostAddress
        }

        public var body: some View {
            VStack(alignment: .leading, spacing: 20) {
                Text("Welcome to Piton").font(.largeTitle.bold())
                Picker("This Mac", selection: $picked) {
                    Text("Runs my agents (host)").tag(MacRole.host)
                    Text("Connects to another Mac (client)").tag(MacRole.client)
                }
                .pickerStyle(.radioGroup)
                Text(explanation)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                switch picked {
                case .host: HostSetup { role = .host }
                case .client: ClientSetup(address: $hostAddress) { role = .client }
                }
            }
            .padding(32)
            .frame(width: 620, alignment: .leading)
        }

        private var explanation: String {
            switch picked {
            case .host:
                "This Mac runs pi and keeps your projects and sessions; this app connects to it directly, and your phone and other Macs reach it over Tailscale."
            case .client:
                "Another Mac runs your agents; this app shows its sessions and settings over Tailscale, like the phone."
            }
        }
    }

    /// The host's setup steps, from `piton setup check`, each with what it needs.
    struct HostSetup: View {
        let done: () -> Void
        @State private var report: SetupReport?
        @State private var error: String?
        @State private var working: SetupStep?
        private let runner = SetupRunner()

        var body: some View {
            VStack(alignment: .leading, spacing: 12) {
                if let report {
                    ForEach(report.steps) { step in
                        StepRow(step: step, working: working == step.step) { await fix(step) }
                    }
                } else if error == nil {
                    ProgressView("Checking this Mac…")
                }
                if let error {
                    Text(error).foregroundStyle(.red).textSelection(.enabled)
                }
                HStack {
                    Button("Check Again") { Task { await check() } }
                    Spacer()
                    Button(report?.isComplete == true ? "Done" : "Continue Anyway", action: done)
                        .keyboardShortcut(.defaultAction)
                }
            }
            .task { await check() }
        }

        private func check() async {
            do {
                report = try await runner.check()
                error = nil
            } catch {
                self.error = String(describing: error)
            }
        }

        private func fix(_ step: SetupStepReport) async {
            guard case .missing(_, let fix) = step.status else { return }
            working = step.step
            defer { working = nil }
            do {
                switch fix {
                case .apply: _ = try await runner.apply(step.step)
                case .app: try DaemonRegistration.register()
                case .manual: return
                }
                await check()
            } catch {
                self.error = String(describing: error)
            }
        }
    }

    struct StepRow: View {
        let step: SetupStepReport
        let working: Bool
        let fix: () async -> Void

        var body: some View {
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Image(systemName: symbol).foregroundStyle(tint)
                VStack(alignment: .leading, spacing: 4) {
                    Text(step.step.title).font(.headline)
                    ForEach(lines, id: \.self) { line in
                        Text(line).font(.callout).foregroundStyle(.secondary).textSelection(.enabled)
                    }
                }
                Spacer()
                if working {
                    ProgressView().controlSize(.small)
                } else if let action {
                    Button(action) { Task { await fix() } }
                }
            }
        }

        private var symbol: String {
            switch step.status {
            case .ok: "checkmark.circle.fill"
            case .missing: "circle.dashed"
            case .failed: "exclamationmark.triangle.fill"
            }
        }

        private var tint: Color {
            switch step.status {
            case .ok: .green
            case .missing: .orange
            case .failed: .red
            }
        }

        private var lines: [String] {
            switch step.status {
            case .ok(let detail): detail.split(separator: "\n").map(String.init)
            case .missing(let problems, .manual(let instructions)): problems + instructions
            case .missing(let problems, .apply): problems
            // Without a bundled daemon this app can't register it: say how a development build gets one.
            case .missing(let problems, .app):
                DaemonRegistration.isAvailable
                    ? problems : problems + ["Run `piton service install` in a terminal (development build)."]
            case .failed(let message): [message]
            }
        }

        /// The button for a fix this app can do.
        private var action: String? {
            switch step.status {
            case .missing(_, .apply): "Fix"
            case .missing(_, .app): DaemonRegistration.isAvailable ? "Turn On" : nil
            case .missing(_, .manual), .ok, .failed: nil
            }
        }
    }

    /// The daemon as a login item: the bundle's LaunchAgent, registered with `SMAppService`. A development build
    /// carries no bundled daemon; `piton service install` installs one instead.
    enum DaemonRegistration {
        static let plistName = "com.alex35mil.piton.plist"

        static var isAvailable: Bool { SetupRunner.isBundled }

        static func register() throws {
            guard isAvailable else {
                throw SetupRunner.Failure.app(
                    "This build doesn't include the daemon; run `piton service install` in a terminal.")
            }
            try SMAppService.agent(plistName: plistName).register()
        }
    }

    /// A client Mac: its host's address.
    struct ClientSetup: View {
        @Binding var address: String
        let done: () -> Void
        @State private var draft = ""

        var body: some View {
            VStack(alignment: .leading, spacing: 12) {
                TextField("my-mac.example.ts.net", text: $draft)
                    .textFieldStyle(.roundedBorder)
                Text(
                    "The host Mac's MagicDNS name or 100.x address. It accepts only devices on your Tailscale account."
                )
                .font(.callout)
                .foregroundStyle(.secondary)
                HStack {
                    Spacer()
                    Button("Connect") {
                        address = trimmed
                        done()
                    }
                    .keyboardShortcut(.defaultAction)
                    .disabled(trimmed.isEmpty)
                }
            }
            .onAppear { draft = address }
        }

        private var trimmed: String { draft.trimmingCharacters(in: .whitespaces) }
    }
#endif
