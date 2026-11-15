import Foundation

/// What a Mac does for piton, chosen on first launch and changeable in Settings.
public enum MacRole: String, Codable, Sendable, CaseIterable {
    /// Runs the daemon: pi, the projects, and the state; this app is its client through the Unix socket.
    case host
    /// Only a client of another host, over the tailnet like the phone.
    case client

    /// Where the app stores the choice (`UserDefaults`).
    public static let defaultsKey = "macRole"
}

/// `piton setup check --json`: each step a host needs, written by the daemon's tests as the `setup.json` fixture.
public struct SetupReport: Decodable, Sendable, Hashable {
    public let format: Int
    public let steps: [SetupStepReport]

    /// The output version this app understands.
    public static let format = 1

    public var isComplete: Bool {
        steps.allSatisfy { step in
            switch step.status {
            case .ok: true
            case .missing, .failed: false
            }
        }
    }
}

public enum SetupStep: String, Codable, Sendable, Hashable {
    case prerequisites
    case configuration
    case cli
    case daemon
    case state
    case remoteAccess = "remote_access"

    public var title: String {
        switch self {
        case .prerequisites: "Tools"
        case .configuration: "Configuration"
        case .cli: "Command line"
        case .daemon: "Start at login"
        case .state: "Host state"
        case .remoteAccess: "Remote access"
        }
    }
}

public struct SetupStepReport: Decodable, Sendable, Hashable, Identifiable {
    public let step: SetupStep
    public let status: SetupStatus

    public var id: SetupStep { step }

    public init(from decoder: Decoder) throws {
        step = try decoder.container(keyedBy: Keys.self).decode(SetupStep.self, forKey: .step)
        status = try SetupStatus(from: decoder)
    }

    enum Keys: String, CodingKey { case step }
}

public enum SetupStatus: Decodable, Sendable, Hashable {
    case ok(detail: String)
    case missing(problems: [String], fix: SetupFix)
    case failed(message: String)

    enum Keys: String, CodingKey { case status, detail, problems, fix, message }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        switch try c.decode(String.self, forKey: .status) {
        case "ok": self = .ok(detail: try c.decode(String.self, forKey: .detail))
        case "missing":
            self = .missing(
                problems: try c.decode([String].self, forKey: .problems),
                fix: try c.decode(SetupFix.self, forKey: .fix))
        case "failed": self = .failed(message: try c.decode(String.self, forKey: .message))
        case let other:
            throw DecodingError.dataCorruptedError(
                forKey: .status, in: c, debugDescription: "unknown setup status \(other)")
        }
    }
}

/// How a missing step gets done.
public enum SetupFix: Decodable, Sendable, Hashable {
    /// `piton setup apply <step>` does it.
    case apply(summary: String)
    /// This app does it (registering the daemon to start at login).
    case app(summary: String)
    /// The user does it.
    case manual(instructions: [String])

    enum Keys: String, CodingKey { case kind, summary, instructions }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        switch try c.decode(String.self, forKey: .kind) {
        case "apply": self = .apply(summary: try c.decode(String.self, forKey: .summary))
        case "app": self = .app(summary: try c.decode(String.self, forKey: .summary))
        case "manual": self = .manual(instructions: try c.decode([String].self, forKey: .instructions))
        case let other:
            throw DecodingError.dataCorruptedError(forKey: .kind, in: c, debugDescription: "unknown setup fix \(other)")
        }
    }
}

#if os(macOS)
    /// Runs the `piton` inside this app's bundle for onboarding: `setup check` and `setup apply`.
    public struct SetupRunner: Sendable {
        public enum Failure: Error, CustomStringConvertible {
            /// No `piton` to run: not in the bundle, and none installed for a development build.
            case missingBinary(looked: [String])
            case unknownFormat(Int)
            /// `piton` exited with a status setup doesn't use, or printed something that isn't its JSON.
            case exited(status: Int32, stderr: String)
            /// `piton setup apply` refused or failed, with its message.
            case refused(String)
            /// Something this app does itself failed.
            case app(String)

            public var description: String {
                switch self {
                case .missingBinary(let looked):
                    "Can't find piton (looked in \(looked.joined(separator: ", "))). A development build needs `mise run cli:install`."
                case .unknownFormat(let format):
                    "this piton reports setup format \(format); the app understands \(SetupReport.format)"
                case .exited(let status, let stderr):
                    stderr.isEmpty ? "piton setup exited with status \(status)" : stderr
                case .refused(let message), .app(let message): message
                }
            }
        }

        /// The binary onboarding runs and links: the one inside this app's bundle (`Contents/MacOS/piton`), or, in a
        /// development build without it, an installed one; nil when there's none.
        public let binary: URL?

        public init(binary: URL? = nil) {
            self.binary = binary ?? Self.defaultBinary
        }

        /// Where a development build looks for `piton`: the CLI link, then `mise run cli:install`'s.
        static let installed = [".local/bin/piton", ".cargo/bin/piton"]

        public static var defaultBinary: URL? {
            let bundled = Bundle.main.bundleURL.appendingPathComponent("Contents/MacOS/piton")
            if FileManager.default.isExecutableFile(atPath: bundled.path) { return bundled }
            let home = URL(fileURLWithPath: NSHomeDirectory())
            return installed.lazy
                .map { home.appendingPathComponent($0).resolvingSymlinksInPath() }
                .first { FileManager.default.isExecutableFile(atPath: $0.path) }
        }

        /// Whether this app carries its own `piton` (a packaged build) and so can register the daemon itself.
        public static var isBundled: Bool {
            FileManager.default.isExecutableFile(
                atPath: Bundle.main.bundleURL.appendingPathComponent("Contents/MacOS/piton").path)
        }

        /// Every step. `piton setup check` exits 0 when all are ok and 3 when some aren't; both are a report.
        public func check() async throws -> SetupReport {
            let output = try await run(["setup", "check", "--json"])
            switch output.status {
            case 0, 3:
                let report = try decode(SetupReport.self, output)
                guard report.format == SetupReport.format else { throw Failure.unknownFormat(report.format) }
                return report
            default: throw Failure.exited(status: output.status, stderr: output.stderr)
            }
        }

        /// One step piton does itself. Exits 0 with the step's new report, or 1 with `{"error"}`.
        public func apply(_ step: SetupStep) async throws -> SetupStepReport {
            let output = try await run(["setup", "apply", step.rawValue, "--json"])
            switch output.status {
            case 0: return try decode(SetupStepReport.self, output)
            case 1: throw Failure.refused(try decode(ApplyError.self, output).error)
            default: throw Failure.exited(status: output.status, stderr: output.stderr)
            }
        }

        private struct ApplyError: Decodable { let error: String }

        struct Output: Sendable {
            let status: Int32
            let stdout: Data
            let stderr: String
        }

        private func decode<T: Decodable>(_ type: T.Type, _ output: Output) throws -> T {
            do {
                return try JSONDecoder().decode(type, from: output.stdout)
            } catch {
                throw Failure.exited(status: output.status, stderr: output.stderr)
            }
        }

        private func run(_ arguments: [String]) async throws -> Output {
            guard let binary else {
                throw Failure.missingBinary(looked: ["the app"] + Self.installed.map { "~/" + $0 })
            }
            // The link points at the binary that ran the check: the bundled one in a packaged build.
            let arguments = arguments + ["--binary", binary.path]
            return try await Task.detached {
                let process = Process()
                process.executableURL = binary
                process.arguments = arguments
                let stdout = Pipe()
                let stderr = Pipe()
                process.standardOutput = stdout
                process.standardError = stderr
                try process.run()
                // Both pipes at once: a full stderr pipe would otherwise block the process before stdout ends.
                async let err = Task.detached { stderr.fileHandleForReading.readDataToEndOfFile() }.value
                let out = stdout.fileHandleForReading.readDataToEndOfFile()
                let errData = await err
                process.waitUntilExit()
                return Output(
                    status: process.terminationStatus,
                    stdout: out,
                    stderr: String(decoding: errData, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines))
            }.value
        }
    }
#endif
