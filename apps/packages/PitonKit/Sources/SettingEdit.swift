import Foundation

extension SettingValueType {
    /// A setting's value as editable text: lists one item per line; a map one `key = value` per line.
    public func text(_ value: JSON) -> String {
        switch value {
        case .array(let items): items.map(Self.scalar).joined(separator: "\n")
        case .object(let map):
            map.keys.sorted().map { "\($0) = \(Self.scalar(map[$0] ?? .null))" }.joined(separator: "\n")
        case .null: ""
        case .bool, .integer, .number, .string: Self.scalar(value)
        }
    }

    /// The edited text as a value for `settings.set`. Empty text resets to the default (`.null`) for every type; nil
    /// when the text isn't a value of this type.
    public func parse(_ input: String) -> JSON? {
        let trimmed = input.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty { return .null }
        let lines = trimmed.split(whereSeparator: \.isNewline)
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty }
        switch self {
        case .text: return .string(trimmed)
        case .integer: return Self.nonNegative(trimmed).map(JSON.integer)
        case .textList: return .array(lines.map(JSON.string))
        case .bool: return ["true": JSON.bool(true), "false": .bool(false)][trimmed]
        case .choice(let options): return options.contains(trimmed) ? .string(trimmed) : nil
        case .choiceList(let options):
            return lines.allSatisfy(options.contains) ? .array(lines.map(JSON.string)) : nil
        case .map(let values):
            var rows: [MapRow] = []
            for line in lines {
                // The last ` = ` splits: a key (a path, a host) never holds one, a value never does.
                guard let split = line.range(of: " = ", options: .backwards) else { return nil }
                rows.append(
                    MapRow(
                        key: String(line[..<split.lowerBound]).trimmingCharacters(in: .whitespaces),
                        value: String(line[split.upperBound...]).trimmingCharacters(in: .whitespaces)))
            }
            return Self.mapValue(rows, values: values)
        }
    }

    /// A map's rows as a value for `settings.set`: rows with an empty key are skipped; nil when a value isn't one of
    /// `values` or a key repeats. No rows resets it (`.null`).
    public static func mapValue(_ rows: [MapRow], values: [String]) -> JSON? {
        var map: [String: JSON] = [:]
        for row in rows {
            let key = row.key.trimmingCharacters(in: .whitespaces)
            if key.isEmpty { continue }
            guard values.contains(row.value), map[key] == nil else { return nil }
            map[key] = .string(row.value)
        }
        return map.isEmpty ? .null : .object(map)
    }

    /// A map value as rows, sorted by key.
    public static func mapRows(_ value: JSON) -> [MapRow] {
        guard case .object(let map) = value else { return [] }
        return map.keys.sorted().map { MapRow(key: $0, value: map[$0]?.string ?? "") }
    }

    private static func nonNegative(_ text: String) -> Int64? {
        Int64(text).flatMap { $0 >= 0 ? $0 : nil }
    }

    private static func scalar(_ value: JSON) -> String {
        switch value {
        case .string(let s): s
        case .integer(let n): String(n)
        case .number(let n): String(n)
        case .bool(let b): String(b)
        case .null: ""
        case .array, .object: String(describing: value)
        }
    }
}

/// One row of a map setting: a key (a path, a host, a service name) and its value.
public struct MapRow: Sendable, Hashable, Identifiable {
    public let id = UUID()
    public var key: String
    public var value: String

    public init(key: String, value: String) {
        self.key = key
        self.value = value
    }

    public static func == (a: MapRow, b: MapRow) -> Bool { a.key == b.key && a.value == b.value }
    public func hash(into hasher: inout Hasher) {
        hasher.combine(key)
        hasher.combine(value)
    }
}
