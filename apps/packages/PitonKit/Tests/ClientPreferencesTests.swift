import Foundation
import Testing

@testable import PitonKit

private func state(textFont: JSON) throws -> DaemonState {
    let json: JSON = .object([
        "version": .string("0.1.0 (abc)"),
        "host_id": .string("h"),
        "keep_awake": .bool(true),
        "awake": .bool(false),
        "sessions": .integer(0),
        "logging": .object(["policy": .object(["mode": .string("info")]), "default": .string("info")]),
        "settings_error": .null,
        "settings": .array([
            .object([
                "key": .string("client.text_font"),
                "value": textFont,
                "kind": .object(["kind": .string("config"), "file": .string("settings"), "placement": .null]),
                "value_type": .object(["kind": .string("text")]),
                "applies": .object(["kind": .string("live")]),
                "editable": .bool(true),
            ])
        ]),
    ])
    return try json.decode(DaemonState.self)
}

@Test func sharedPreferencesAreCachedAndLocalOnesWin() throws {
    let suite = "piton.tests.\(UUID().uuidString)"
    let defaults = try #require(UserDefaults(suiteName: suite))
    defer { defaults.removePersistentDomain(forName: suite) }
    let preferences = ClientPreferences(defaults: defaults)

    #expect(preferences.source(.textFont) == .shared(nil))
    preferences.cache(try state(textFont: .string("Iosevka")))
    #expect(preferences.source(.textFont) == .shared("Iosevka"))

    preferences.setLocal(.textFont, "Berkeley Mono")
    preferences.cache(try state(textFont: .string("Fira Code")))
    #expect(preferences.source(.textFont) == .local("Berkeley Mono"))

    preferences.useShared(.textFont)
    #expect(preferences.source(.textFont) == .shared("Fira Code"))
    preferences.cache(try state(textFont: .null))
    #expect(preferences.source(.textFont) == .shared(nil))
}
