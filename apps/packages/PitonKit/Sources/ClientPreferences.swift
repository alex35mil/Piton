import Foundation

/// A client preference: kept by the host in its `config.client.toml`, for every device or per device section, or this
/// app's own value, kept on this device. The raw value names this device's `UserDefaults` keys.
public enum ClientPreference: String, CaseIterable, Sendable {
    case textFont = "client.text_font"
    case codeFont = "client.code_font"

    /// The key in `config.client.toml`.
    public var key: String {
        switch self {
        case .textFont: "appearance.text_font"
        case .codeFont: "appearance.code_font"
        }
    }

    /// Where this device keeps its own value (and whether it uses it) in `UserDefaults`. Public so views can observe
    /// them (`@AppStorage`).
    public var localKey: String { "preference.local.\(rawValue)" }
    public var isLocalKey: String { "preference.isLocal.\(rawValue)" }
    /// The last value the host sent for this device, for starting offline.
    public var cacheKey: String { "preference.shared.\(rawValue)" }
}

/// Where a client preference's value comes from on this device.
public enum PreferenceSource: Sendable, Hashable {
    /// The host's value for this device (nil: no section applies, the default does).
    case host(String?)
    /// This device's own value (nil: the default).
    case thisDevice(String?)

    public var value: String? {
        switch self {
        case .host(let value), .thisDevice(let value): value
        }
    }
}

/// Client preferences on this device: its own values, and the cache of the host's values for this device.
public struct ClientPreferences {
    let defaults: UserDefaults

    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    public func source(_ preference: ClientPreference) -> PreferenceSource {
        switch defaults.bool(forKey: preference.isLocalKey) {
        case true: .thisDevice(defaults.string(forKey: preference.localKey))
        case false: .host(defaults.string(forKey: preference.cacheKey))
        }
    }

    /// Keeps the host's values for this device from a pushed daemon state, for the next offline start.
    public func cache(_ state: DaemonState) {
        for preference in ClientPreference.allCases {
            guard let setting = state.setting(.client, preference.key) else { continue }
            switch setting.value {
            case .string(let value): defaults.set(value, forKey: preference.cacheKey)
            case .null, .bool, .integer, .number, .array, .object: defaults.removeObject(forKey: preference.cacheKey)
            }
        }
    }

    /// Sets this device's own value and uses it.
    public func setLocal(_ preference: ClientPreference, _ value: String?) {
        defaults.set(true, forKey: preference.isLocalKey)
        defaults.set(value, forKey: preference.localKey)
    }

    /// Back to the host's value; this device's own one is forgotten.
    public func useHost(_ preference: ClientPreference) {
        defaults.set(false, forKey: preference.isLocalKey)
        defaults.removeObject(forKey: preference.localKey)
    }
}
