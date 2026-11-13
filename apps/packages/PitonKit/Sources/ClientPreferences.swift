import Foundation

/// A client preference: shared (kept by the host in its `config.toml` `[client]`, the same for every client) or local
/// (this app's own value, kept on this device). The Local checkbox moves a preference between the two.
public enum ClientPreference: String, CaseIterable, Sendable {
    case textFont = "client.text_font"
    case codeFont = "client.code_font"

    /// Where this device keeps its own value (and whether it's local) in `UserDefaults`. Public so views can observe
    /// them (`@AppStorage`).
    public var localKey: String { "preference.local.\(rawValue)" }
    public var isLocalKey: String { "preference.isLocal.\(rawValue)" }
    /// The last shared value the host sent, for starting offline.
    public var cacheKey: String { "preference.shared.\(rawValue)" }
}

/// Where a client preference's value comes from on this device.
public enum PreferenceSource: Sendable, Hashable {
    /// The host's shared value (nil: the host sets none, the default applies).
    case shared(String?)
    /// This device's own value (nil: the default).
    case local(String?)

    public var value: String? {
        switch self {
        case .shared(let value), .local(let value): value
        }
    }
}

/// Client preferences on this device: the local values, and the cache of the host's shared ones.
public struct ClientPreferences {
    let defaults: UserDefaults

    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    public func source(_ preference: ClientPreference) -> PreferenceSource {
        switch defaults.bool(forKey: preference.isLocalKey) {
        case true: .local(defaults.string(forKey: preference.localKey))
        case false: .shared(defaults.string(forKey: preference.cacheKey))
        }
    }

    /// Keeps the host's shared values from a pushed daemon state, for the next offline start.
    public func cache(_ state: DaemonState) {
        for preference in ClientPreference.allCases {
            guard let setting = state.setting(preference.rawValue) else { continue }
            switch setting.value {
            case .string(let value): defaults.set(value, forKey: preference.cacheKey)
            case .null, .bool, .integer, .number, .array, .object: defaults.removeObject(forKey: preference.cacheKey)
            }
        }
    }

    /// Sets this device's own value and makes the preference local.
    public func setLocal(_ preference: ClientPreference, _ value: String?) {
        defaults.set(true, forKey: preference.isLocalKey)
        defaults.set(value, forKey: preference.localKey)
    }

    /// Back to the host's shared value; this device's own one is forgotten.
    public func useShared(_ preference: ClientPreference) {
        defaults.set(false, forKey: preference.isLocalKey)
        defaults.removeObject(forKey: preference.localKey)
    }
}
