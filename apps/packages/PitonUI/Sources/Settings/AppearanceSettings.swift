import PitonKit
import SwiftUI
import UniformTypeIdentifiers

/// The Appearance settings section: the text and code fonts. Each is shared through the host (the same on every
/// device, kept in the host's config.client.toml) unless its Local box is checked, which keeps a value on this
/// device only. A shared change needs the connection; a local one doesn't.
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
                    "Shared fonts are the same on every device connected to this host. Check Local to choose one for this device only. A font this device doesn't have shows as the system font."
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
        Toggle("Local to this device", isOn: local(preference))
    }

    /// A shared value changes on the host: it needs the connection.
    private func canChange(_ source: PreferenceSource) -> Bool {
        switch source {
        case .local: true
        case .shared: store?.daemon != nil
        }
    }

    private func selection(_ preference: ClientPreference) -> Binding<FontChoice> {
        Binding(
            get: { FontChoice(preference: fonts.source(preference).value) },
            set: { choice in
                switch fonts.source(preference) {
                case .local: ClientPreferences().setLocal(preference, choice.preference)
                case .shared:
                    let value: JSON = choice.preference.map(JSON.string) ?? .null
                    Task { _ = await store?.setSetting(.client, preference.key, to: value) }
                }
            })
    }

    /// Checking Local keeps the font in use on this device; unchecking returns to the host's shared one.
    private func local(_ preference: ClientPreference) -> Binding<Bool> {
        Binding(
            get: {
                switch fonts.source(preference) {
                case .local: true
                case .shared: false
                }
            },
            set: { isLocal in
                let preferences = ClientPreferences()
                switch isLocal {
                case true: preferences.setLocal(preference, fonts.source(preference).value)
                case false: preferences.useShared(preference)
                }
            })
    }
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
