import Foundation

extension SettingValueType {
    /// A setting's value as editable text: lists one item per line.
    public func text(_ value: JSON) -> String {
        switch value {
        case .array(let items): items.map(Self.scalar).joined(separator: "\n")
        case .null: ""
        case .bool, .integer, .number, .string, .object: Self.scalar(value)
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
        }
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
