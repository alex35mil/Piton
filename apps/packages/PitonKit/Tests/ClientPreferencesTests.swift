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
                "key": .string("appearance.text_font"),
                "scope": .string("client"),
                "value": textFont,
                "location": .object([
                    "kind": .string("devices"), "sections": .object(["ipad": textFont]),
                    "section": .string("ipad"),
                ]),
                "value_type": .object(["kind": .string("text")]),
                "applies": .object(["kind": .string("live")]),
                "editable": .bool(true),
            ])
        ]),
    ])
    return try json.decode(DaemonState.self)
}

@Test func hostValuesAreCachedAndThisDevicesWin() throws {
    let suite = "piton.tests.\(UUID().uuidString)"
    let defaults = try #require(UserDefaults(suiteName: suite))
    defer { defaults.removePersistentDomain(forName: suite) }
    let preferences = ClientPreferences(defaults: defaults)

    #expect(preferences.source(.textFont) == .host(nil))
    preferences.cache(try state(textFont: .string("Iosevka")))
    #expect(preferences.source(.textFont) == .host("Iosevka"))

    preferences.setLocal(.textFont, "Berkeley Mono")
    preferences.cache(try state(textFont: .string("Fira Code")))
    #expect(preferences.source(.textFont) == .thisDevice("Berkeley Mono"))

    preferences.useHost(.textFont)
    #expect(preferences.source(.textFont) == .host("Fira Code"))
    preferences.cache(try state(textFont: .null))
    #expect(preferences.source(.textFont) == .host(nil))
    // The UserDefaults keys of this device's own value didn't change with the file key.
    #expect(ClientPreference.textFont.isLocalKey == "preference.isLocal.client.text_font")
}
