import Foundation
import Observation
import PitonKit

/// The text and code fonts in effect on this device: each one the host's shared value (cached for offline start) or
/// this device's own, as `ClientPreferences` says. Follows `UserDefaults`, so a pushed host value, a local change, or a
/// change in another window shows at once.
@MainActor
@Observable
final class FontPreferences {
    private(set) var sources: [ClientPreference: PreferenceSource] = [:]
    @ObservationIgnored private var observer: NSObjectProtocol?

    init() {
        reload()
        observer = NotificationCenter.default.addObserver(
            forName: UserDefaults.didChangeNotification, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.reload() }
        }
    }

    isolated deinit {
        if let observer { NotificationCenter.default.removeObserver(observer) }
    }

    private func reload() {
        let preferences = ClientPreferences()
        let next = Dictionary(uniqueKeysWithValues: ClientPreference.allCases.map { ($0, preferences.source($0)) })
        if next != sources { sources = next }
    }

    func source(_ preference: ClientPreference) -> PreferenceSource {
        sources[preference] ?? .shared(nil)
    }

    func choice(_ preference: ClientPreference) -> FontChoice {
        FontChoice(preference: source(preference).value)
    }
}
