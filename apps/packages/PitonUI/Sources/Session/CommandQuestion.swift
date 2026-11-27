import PitonKit
import SwiftUI

/// The block question for a command: the agent asks to run it again after the sandbox blocked it. A switch picks where
/// it runs (a widened sandbox or the host), and the answers act there: once, this command for the session, deny, or
/// always for this command or a pattern, in this project or everywhere. Left and right arrows move the switch with a hardware keyboard.
struct CommandQuestionCard: View {
    let question: Question
    let decide: (Decision) -> Void
    @State private var runtime: CommandRuntime
    @State private var scope: PersistScope = .project
    @State private var pattern: String
    @State private var showVariables = false
    @Environment(\.typography) private var typography

    init(question: Question, decide: @escaping (Decision) -> Void) {
        self.question = question
        self.decide = decide
        _runtime = State(initialValue: question.runtime)
        _pattern = State(initialValue: question.command)
    }

    private var color: Color {
        switch runtime {
        case .sandbox: Theme.accent
        case .host: Theme.alert
        }
    }

    /// A bash call runs again; a program from pi's process runs for the first time.
    private var title: String {
        switch question.asker {
        case .agent, .piton: "Run again?"
        case .pi: "Run?"
        }
    }

    private var variableCount: Int { question.writableVars.reduce(0) { $0 + $1.names.count } }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            SectionLabel(title, color: color)
            Text(question.command).font(typography.data(.callout)).textSelection(.enabled)
            if let reason = question.reason {
                Text("Why: \(reason)").font(typography.content(.callout)).foregroundStyle(Theme.secondary)
            }
            if let note = question.note {
                Text("It \(note).").font(typography.content(.callout)).foregroundStyle(Theme.secondary)
            }
            if !question.refusals.isEmpty {
                VStack(alignment: .leading, spacing: 2) {
                    Text("The sandbox refused:").font(typography.content(.footnote)).foregroundStyle(Theme.dim)
                    ForEach(question.refusals, id: \.self) { refusal in
                        Text(refusal).font(typography.data(.footnote)).foregroundStyle(Theme.secondary)
                    }
                }
            }
            Picker("Where it runs", selection: $runtime) {
                Text("In the sandbox").tag(CommandRuntime.sandbox)
                Text("On the host").tag(CommandRuntime.host)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            Text(question.wording.text(runtime))
                .font(typography.content(.callout))
                .foregroundStyle(runtime == .host ? Theme.alert : Theme.secondary)
            if runtime == .host && variableCount > 0 {
                DisclosureGroup(isExpanded: $showVariables) {
                    VStack(alignment: .leading, spacing: 6) {
                        ForEach(question.writableVars, id: \.self) { group in
                            Text(group.source ?? "A tool that named no file")
                                .font(typography.data(.footnote)).foregroundStyle(Theme.dim)
                            Text(group.names.joined(separator: "  "))
                                .font(typography.data(.footnote)).foregroundStyle(Theme.secondary)
                                .textSelection(.enabled)
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                } label: {
                    Text("Variables set by files the agent can write (\(variableCount))")
                        .font(typography.content(.footnote)).foregroundStyle(Theme.secondary)
                }
            }
            HStack {
                Button(runtime == .host ? "Run once on the host" : "Allow once") { decide(.once(runtime)) }
                    .buttonStyle(OutlineButtonStyle(prominent: true, color: color))
                Button("For this session") { decide(.session(runtime)) }
                    .buttonStyle(OutlineButtonStyle(color: color))
                Button("Deny") { decide(.deny) }
                    .buttonStyle(OutlineButtonStyle(color: Theme.secondary))
            }
            DashedRule()
            SectionLabel(runtime == .host ? "Always run on the host" : "Always allow in the sandbox")
            HStack {
                TextField("Pattern", text: $pattern)
                    .font(typography.data(.callout))
                    .textFieldStyle(.plain)
                    .padding(8)
                    .background(Theme.canvas)
                    .overlay(Rectangle().stroke(Theme.hairline, lineWidth: 1))
                if pattern != question.suggestedPattern {
                    Button(question.suggestedPattern) { pattern = question.suggestedPattern }
                        .buttonStyle(OutlineButtonStyle(color: Theme.secondary))
                }
            }
            Text("`*` matches any text.").font(typography.content(.footnote)).foregroundStyle(Theme.dim)
            HStack {
                Picker("Where it's saved", selection: $scope) {
                    Text("This project").tag(PersistScope.project)
                    Text("Everywhere").tag(PersistScope.global)
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                Button("Save and run") { decide(.always(runtime, scope: scope, pattern: pattern)) }
                    .buttonStyle(OutlineButtonStyle(color: color))
                    .disabled(pattern.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .panel(border: color.opacity(0.7), fill: Theme.raised, marker: color)
        .focusable()
        .focusEffectDisabled()
        .onKeyPress(.leftArrow) {
            runtime = .sandbox
            return .handled
        }
        .onKeyPress(.rightArrow) {
            runtime = .host
            return .handled
        }
    }
}
