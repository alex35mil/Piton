import PitonKit
import SwiftUI
import UniformTypeIdentifiers

/// The Appearance settings section: the text and code fonts. Each applies where its scope says: a section of the host's
/// config.client.toml (every device, every Mac, iPhone and iPad, every iPhone, every iPad), or this device only, kept
/// in the app. A change on the host needs the connection; one for this device only doesn't.
public struct AppearanceSettings: View {
    /// The host connection, for shared values; nil while there's none (the phone before setup).
    let store: Store?
    @State private var fonts = FontPreferences()
    @State private var importing = false
    @State private var imported: [String] = []
    @State private var problem: String?
    @Environment(\.typography) private var typography

    public init(store: Store?) {
        self.store = store
    }

    public var body: some View {
        Form {
            Section {
                fontRow(.textFont, title: "Text", detail: "Messages and titles.", design: .default)
                fontRow(.codeFont, title: "Code", detail: "Labels, tool calls, and output.", design: .monospaced)
            } header: {
                Text("Fonts")
            } footer: {
                Text(
                    "A font applies to the devices its scope names, through the host; Only this device keeps it in this app. A font this device doesn't have shows as the system font."
                )
            }
            Section("Preview") {
                VStack(alignment: .leading, spacing: 8) {
                    SectionLabel("Tool call", color: Theme.accent)
                    Text("Let me check the issue.").font(typography.content())
                    Text("gh issue view 1052 --repo nolabs-ai/nono")
                        .font(typography.data())
                        .foregroundStyle(Theme.secondary)
                }
                .panel()
                .listRowBackground(Color.clear)
                .listRowInsets(EdgeInsets())
            }
            Section {
                LabeledContent {
                    Button("Import…") { importing = true }
                } label: {
                    Text("Font Files")
                    Text(
                        "For fonts you own that aren't installed here (Berkeley Mono, say): pick their .otf or .ttf files. They're copied into Piton on this device only."
                    )
                }
                if !imported.isEmpty {
                    LabeledContent("Imported", value: imported.joined(separator: ", "))
                    Button("Remove Imported Fonts", role: .destructive) {
                        FontLibrary.removeImported()
                        imported = []
                    }
                }
                if let problem {
                    Text(problem).foregroundStyle(Theme.alert)
                }
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Appearance")
        .onAppear { imported = FontLibrary.importedFamilies() }
        .fileImporter(isPresented: $importing, allowedContentTypes: [.font], allowsMultipleSelection: true) { result in
            switch result {
            case .success(let urls):
                do throws(FontLibrary.ImportError) {
                    _ = try FontLibrary.importFiles(urls)
                    imported = FontLibrary.importedFamilies()
                    problem = nil
                } catch {
                    switch error {
                    case .notAFont(let file): problem = "\(file) isn't a font file."
                    case .storage(let file, let code): problem = "Couldn't import \(file) (error \(code))."
                    }
                }
            case .failure:
                problem = "Couldn't open the files."
            }
        }
    }

    private func name(_ choice: FontChoice) -> String {
        switch choice {
        case .system: "System"
        case .family(let family): family
        }
    }

    @ViewBuilder
    private func fontRow(
        _ preference: ClientPreference, title: String, detail: String, design: Font.Design
    ) -> some View {
        let source = fonts.source(preference)
        NavigationLink {
            FontList(title: "\(title) Font", selection: selection(preference), design: design)
        } label: {
            LabeledContent {
                Text(name(FontChoice(preference: source.value)))
            } label: {
                Text(title)
                Text(detail)
            }
        }
        .disabled(!canChange(source))
        Picker("Applies to", selection: scope(preference)) {
            ForEach(scopes, id: \.self) { scope in
                Text(scopeTitle(scope)).tag(scope)
                    // A section is the host's: it needs the connection. This device's own value doesn't.
                    .selectionDisabled(!canChoose(scope))
            }
        }
        if let others = otherSections(preference) {
            Text(others).font(.caption).foregroundStyle(.secondary)
        }
    }

    /// The device this app runs on.
    private var device: ClientDevice { store?.device ?? .current }

    /// The scopes a font can have on this device, broadest first.
    private var scopes: [FontScope] { device.sections.map(FontScope.section) + [.thisDevice] }

    private func scopeTitle(_ scope: FontScope) -> String {
        switch scope {
        case .section(.default): "Every device"
        case .section(.mac): "Every Mac"
        case .section(.mobile): "iPhone and iPad"
        case .section(.iphone): "Every iPhone"
        case .section(.ipad): "Every iPad"
        case .thisDevice:
            switch device {
            case .mac: "Only this Mac"
            case .iphone: "Only this iPhone"
            case .ipad: "Only this iPad"
            }
        }
    }

    /// The host's setting for `preference`, as this device sees it.
    private func setting(_ preference: ClientPreference) -> Setting? {
        store?.daemon?.setting(.client, preference.key)
    }

    /// The section the host's value comes from on this device; every device when none applies yet.
    private func section(_ preference: ClientPreference) -> DeviceSection {
        switch setting(preference)?.location {
        case .devices(_, let section): section ?? .default
        case .file, nil: .default
        }
    }

    /// What the other sections of the file set, so a value hidden on this device isn't a surprise.
    private func otherSections(_ preference: ClientPreference) -> String? {
        guard case .devices(let sections, let inEffect) = setting(preference)?.location else { return nil }
        let others = DeviceSection.allCases.filter { $0 != inEffect }.compactMap { section -> String? in
            guard let value = sections[section]?.string else { return nil }
            return "\(scopeTitle(.section(section))): \(value)"
        }
        return others.isEmpty ? nil : "Also set: " + others.joined(separator: ", ")
    }

    /// A section is written on the host: it needs the connection.
    private func canChoose(_ scope: FontScope) -> Bool {
        switch scope {
        case .thisDevice: true
        case .section: store?.daemon != nil
        }
    }

    /// A host value changes on the host: it needs the connection.
    private func canChange(_ source: PreferenceSource) -> Bool {
        switch source {
        case .thisDevice: true
        case .host: store?.daemon != nil
        }
    }

    private func selection(_ preference: ClientPreference) -> Binding<FontChoice> {
        Binding(
            get: { FontChoice(preference: fonts.source(preference).value) },
            set: { choice in
                switch fonts.source(preference) {
                case .thisDevice: ClientPreferences().setLocal(preference, choice.preference)
                case .host: write(preference, choice.preference, to: section(preference))
                }
            })
    }

    /// Sets `value` in `section` of the host's file (nil, the system font, removes that section).
    private func write(_ preference: ClientPreference, _ value: String?, to section: DeviceSection) {
        let json: JSON = value.map(JSON.string) ?? .null
        Task { _ = await store?.setSetting(.client, preference.key, to: json, target: .section(section)) }
    }

    /// The font's scope. Choosing a section writes the font in use there (the host makes it the one in effect on this
    /// device); choosing this device keeps the font in use in the app.
    private func scope(_ preference: ClientPreference) -> Binding<FontScope> {
        Binding(
            get: {
                switch fonts.source(preference) {
                case .thisDevice: .thisDevice
                case .host: .section(section(preference))
                }
            },
            set: { scope in
                guard canChoose(scope) else { return }
                let preferences = ClientPreferences()
                let current = fonts.source(preference).value
                switch scope {
                case .thisDevice: preferences.setLocal(preference, current)
                case .section(let section):
                    preferences.useHost(preference)
                    write(preference, current, to: section)
                }
            })
    }
}

/// Where a font applies: a section of the host's client file, or this device only.
enum FontScope: Hashable {
    case section(DeviceSection)
    case thisDevice
}

/// Every available family, each shown in its own face; a search field narrows the list.
struct FontList: View {
    let title: String
    @Binding var selection: FontChoice
    let design: Font.Design
    @State private var families: [String] = []
    @State private var query = ""
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        List {
            row(.system, label: "System")
            ForEach(filtered, id: \.self) { family in
                row(.family(family), label: family)
            }
        }
        .searchable(text: $query)
        .navigationTitle(title)
        .onAppear { families = FontLibrary.availableFamilies() }
    }

    private var filtered: [String] {
        query.isEmpty ? families : families.filter { $0.localizedCaseInsensitiveContains(query) }
    }

    private func row(_ choice: FontChoice, label: String) -> some View {
        Button {
            selection = choice
            dismiss()
        } label: {
            HStack {
                Text(label).font(font(choice))
                Spacer()
                if choice == selection {
                    Image(systemName: "checkmark").foregroundStyle(Theme.accent)
                }
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private func font(_ choice: FontChoice) -> Font {
        switch choice {
        case .system: .system(.body, design: design)
        case .family(let family): .custom(family, size: 16, relativeTo: .body)
        }
    }
}
