import Foundation

/// A dialog that asks to review a file change: pi's file review and the daemon's protected writes both send a `select`
/// whose title is `{"prompt", "toolName", "toolInput"}`. pi.nvim shows it as a diff; the apps show the change with
/// Accept and Reject, answered with the plain option values every reader of this format understands.
public struct FileReview: Sendable, Hashable {
    public enum Change: Sendable, Hashable {
        /// The whole new content of the file; `current` is what it holds now (nil for a new file or when the dialog
        /// doesn't say, as in pi's own review, where the file is on the agent's machine).
        case write(current: String?, content: String)
        /// Replacements in the file, in order.
        case edit([Replacement])
    }

    /// One line of a line diff.
    public enum Line: Sendable, Hashable {
        case same(String)
        case removed(String)
        case added(String)
    }

    public struct Replacement: Sendable, Hashable {
        public let old: String
        public let new: String
    }

    public let prompt: String
    public let path: String
    public let change: Change

    /// The review in `dialog`, or nil when it's an ordinary dialog.
    public init?(dialog: Dialog) {
        guard dialog.method == .select,
            let title = dialog.title,
            let data = title.data(using: .utf8),
            let payload = try? JSONDecoder().decode(Payload.self, from: data),
            let path = payload.toolInput.path
        else { return nil }
        switch payload.toolName {
        case "write":
            guard let content = payload.toolInput.content else { return nil }
            change = .write(current: payload.current, content: content)
        case "edit":
            guard let edits = payload.toolInput.edits, !edits.isEmpty else { return nil }
            change = .edit(edits.map { Replacement(old: $0.oldText, new: $0.newText) })
        default:
            return nil
        }
        self.prompt = payload.prompt
        self.path = path
    }

    /// The answers, as option values.
    public static let accept = "Accept"
    public static let reject = "Reject"

    /// The change as lines: a write against the current content (all added when it's unknown), an edit's replacements
    /// in order. Unchanged lines are kept only within `context` lines of a change.
    public func lines(context: Int = 3) -> [Line] {
        switch change {
        case .write(let current, let content):
            return Self.trim(Self.diff(Self.split(current ?? ""), Self.split(content)), context: context)
        case .edit(let replacements):
            return replacements.flatMap { r in
                Self.split(r.old).map(Line.removed) + Self.split(r.new).map(Line.added)
            }
        }
    }

    private static func split(_ text: String) -> [String] {
        text.isEmpty ? [] : text.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
    }

    /// A line diff by longest common subsequence: fine for configuration files' sizes.
    static func diff(_ old: [String], _ new: [String]) -> [Line] {
        let n = old.count
        let m = new.count
        var lcs = Array(repeating: Array(repeating: 0, count: m + 1), count: n + 1)
        for i in stride(from: n - 1, through: 0, by: -1) {
            for j in stride(from: m - 1, through: 0, by: -1) {
                lcs[i][j] = old[i] == new[j] ? lcs[i + 1][j + 1] + 1 : max(lcs[i + 1][j], lcs[i][j + 1])
            }
        }
        var out: [Line] = []
        var i = 0
        var j = 0
        while i < n || j < m {
            if i < n, j < m, old[i] == new[j] {
                out.append(.same(old[i]))
                i += 1
                j += 1
            } else if i < n, j == m || lcs[i + 1][j] >= lcs[i][j + 1] {
                // Removals before additions, as diffs read.
                out.append(.removed(old[i]))
                i += 1
            } else {
                out.append(.added(new[j]))
                j += 1
            }
        }
        return out
    }

    /// Unchanged lines far from any change are left out, so a one-line change in a long file reads as one line.
    static func trim(_ lines: [Line], context: Int) -> [Line] {
        let changed = lines.indices.filter {
            switch lines[$0] {
            case .same: false
            case .removed, .added: true
            }
        }
        return lines.indices.filter { index in
            changed.contains { abs($0 - index) <= context }
        }.map { lines[$0] }
    }

    private struct Payload: Decodable {
        let prompt: String
        let toolName: String
        let toolInput: Input
        let current: String?
    }

    private struct Input: Decodable {
        let path: String?
        let content: String?
        let edits: [Edit]?
    }

    private struct Edit: Decodable {
        let oldText: String
        let newText: String
    }
}
