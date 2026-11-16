import Testing

@testable import PitonKit

@Test func emptyTextResetsEveryType() {
    let types: [SettingValueType] = [
        .bool, .text, .textList, .integer, .integerList, .choice(options: ["a"]), .networkProfile,
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
    #expect(SettingValueType.integerList.parse("0\n7357") == .array([.integer(0), .integer(7357)]))
    #expect(SettingValueType.integerList.parse("0\nx") == nil)
    #expect(SettingValueType.networkProfile.parse("false") == .bool(false))
    #expect(SettingValueType.networkProfile.parse("developer") == .string("developer"))
    #expect(SettingValueType.bool.parse("true") == .bool(true))
    #expect(SettingValueType.choice(options: ["info", "debug"]).parse("debug") == .string("debug"))
    #expect(SettingValueType.choice(options: ["info", "debug"]).parse("loud") == nil)
}

@Test func valuesShowAsEditableText() {
    #expect(SettingValueType.textList.text(.array([.string("~/a"), .string("~/b")])) == "~/a\n~/b")
    #expect(SettingValueType.integer.text(.integer(7357)) == "7357")
    #expect(SettingValueType.text.text(.null) == "")
    // Text round-trips through parse.
    let list: JSON = .array([.integer(0), .integer(22)])
    #expect(SettingValueType.integerList.parse(SettingValueType.integerList.text(list)) == list)
}
