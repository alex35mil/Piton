import Foundation
import Testing

@testable import PitonKit

@Test func emptyTextResetsEveryType() {
    let types: [SettingValueType] = [
        .bool, .text, .textList, .integer, .choice(options: ["a"]), .choiceList(options: ["a"]),
        .map(values: ["allow", "deny"]),
    ]
    for type in types {
        #expect(type.parse("  \n ") == .null, "\(type)")
    }
}

@Test func editsParseByType() {
    #expect(SettingValueType.text.parse(" ~/p ") == .string("~/p"))
    #expect(SettingValueType.integer.parse("9000") == .integer(9000))
    #expect(SettingValueType.integer.parse("-1") == nil)
    #expect(SettingValueType.integer.parse("12a") == nil)
    #expect(SettingValueType.textList.parse("~/a\n\n  ~/b ") == .array([.string("~/a"), .string("~/b")]))
    let presets = SettingValueType.choiceList(options: ["rust", "swift"])
    #expect(presets.parse("rust\n swift") == .array([.string("rust"), .string("swift")]))
    #expect(presets.parse("rust\npython") == nil)
    #expect(SettingValueType.bool.parse("true") == .bool(true))
    #expect(SettingValueType.choice(options: ["info", "debug"]).parse("debug") == .string("debug"))
    #expect(SettingValueType.choice(options: ["info", "debug"]).parse("loud") == nil)
}

@Test func valuesShowAsEditableText() {
    #expect(SettingValueType.textList.text(.array([.string("~/a"), .string("~/b")])) == "~/a\n~/b")
    #expect(SettingValueType.integer.text(.integer(7357)) == "7357")
    #expect(SettingValueType.text.text(.null) == "")
    // Text round-trips through parse.
    let list: JSON = .array([.string("rust"), .string("swift")])
    let presets = SettingValueType.choiceList(options: ["rust", "swift"])
    #expect(presets.parse(presets.text(list)) == list)
}

@Test func mapsEditAsRows() {
    let type = SettingValueType.map(values: ["read", "write", "deny"])
    let value: JSON = .object(["~/.cache/x": .string("write"), "~/secrets": .string("deny")])
    #expect(type.text(value) == "~/.cache/x = write\n~/secrets = deny")
    #expect(type.parse(type.text(value)) == value)
    #expect(type.parse("~/x = allow") == nil, "not one of the values")
    #expect(type.parse("~/x") == nil, "no value")
    let rows = SettingValueType.mapRows(value)
    #expect(rows.map(\.key) == ["~/.cache/x", "~/secrets"])
    #expect(SettingValueType.mapValue(rows, values: ["read", "write", "deny"]) == value)
    // An empty key is a row being typed: skipped. A repeated key is refused.
    #expect(
        SettingValueType.mapValue(rows + [MapRow(key: " ", value: "read")], values: ["read", "write", "deny"])
            == value)
    #expect(
        SettingValueType.mapValue(
            [MapRow(key: "/a", value: "read"), MapRow(key: "/a", value: "deny")], values: ["read", "deny"]) == nil)
    #expect(SettingValueType.mapValue([], values: ["read"]) == .null)
}

@Test func mapTypesDecodeFromTheDaemon() throws {
    let data = Data(#"{"kind":"map","values":["allow","deny"]}"#.utf8)
    let type = try JSONDecoder().decode(SettingValueType.self, from: data)
    #expect(type == .map(values: ["allow", "deny"]))
    let back = try JSONDecoder().decode(SettingValueType.self, from: try JSONEncoder().encode(type))
    #expect(back == type)
}
