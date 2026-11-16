import PitonKit
import SwiftUI

/// A pending extension dialog: a file change to review, or a select, confirm, input, or editor.
struct DialogCard: View {
    let dialog: Dialog
    let answer: (DialogAnswer) -> Void

    var body: some View {
        if let review = FileReview(dialog: dialog) {
            FileReviewCard(review: review, answer: answer)
        } else {
            QuestionCard(dialog: dialog, answer: answer)
        }
    }
}

/// A file change to approve: pi's file review, or a protected file the daemon writes on approval. Shows the file and
/// the change; the answer is the plain Accept or Reject every reader of this dialog understands.
struct FileReviewCard: View {
    let review: FileReview
    let answer: (DialogAnswer) -> Void
    @Environment(\.typography) private var typography

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            SectionLabel("Review change", color: Theme.accent)
            Text(review.path).font(typography.data(.callout)).textSelection(.enabled)
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    ForEach(Array(review.lines().enumerated()), id: \.offset) { _, line in
                        row(line)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .frame(maxHeight: 260)
            .padding(8)
            .background(Theme.canvas)
            .overlay(Rectangle().stroke(Theme.hairline, lineWidth: 1))
            HStack {
                Button("Accept") { answer(.value(FileReview.accept)) }
                    .buttonStyle(OutlineButtonStyle(prominent: true))
                Button("Reject") { answer(.value(FileReview.reject)) }
                    .buttonStyle(OutlineButtonStyle(color: Theme.secondary))
            }
        }
        .panel(border: Theme.accent.opacity(0.7), fill: Theme.raised, marker: Theme.accent)
    }

    private func row(_ line: FileReview.Line) -> some View {
        let (marker, text, color): (String, String, Color) =
            switch line {
            case .same(let text): (" ", text, Theme.secondary)
            case .removed(let text): ("-", text, Theme.alert)
            case .added(let text): ("+", text, Theme.ok)
            }
        return Text("\(marker) \(text)")
            .font(typography.data(.footnote))
            .foregroundStyle(color)
            .textSelection(.enabled)
    }
}

/// A question: select, confirm, input, or editor.
struct QuestionCard: View {
    let dialog: Dialog
    let answer: (DialogAnswer) -> Void
    @State private var text = ""
    @Environment(\.typography) private var typography

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            SectionLabel("Needs input", color: Theme.accent)
            Text(dialog.title ?? "Pi is asking").font(typography.content(.headline))
            if let message = dialog.message {
                Text(message).font(typography.content(.callout)).foregroundStyle(Theme.secondary)
            }
            switch dialog.method {
            case .select:
                ForEach(dialog.options ?? [], id: \.self) { option in
                    Button(option) { answer(.value(option)) }
                        .buttonStyle(OutlineButtonStyle())
                }
            case .confirm:
                HStack {
                    Button("Yes") { answer(.confirmed(true)) }.buttonStyle(OutlineButtonStyle(prominent: true))
                    Button("No") { answer(.confirmed(false)) }.buttonStyle(OutlineButtonStyle())
                }
            case .input, .editor:
                TextField(dialog.placeholder ?? "Answer", text: $text, axis: .vertical)
                    .lineLimit(dialog.method == .editor ? 4...12 : 1...3)
                    .font(typography.content())
                    .textFieldStyle(.plain)
                    .padding(10)
                    .background(Theme.canvas)
                    .overlay(Rectangle().stroke(Theme.hairline, lineWidth: 1))
                    .onAppear { text = dialog.prefill ?? "" }
                Button("Send") { answer(.value(text)) }.buttonStyle(OutlineButtonStyle(prominent: true))
            }
            Button("Dismiss") { answer(.cancelled) }
                .buttonStyle(OutlineButtonStyle(color: Theme.secondary))
        }
        .panel(border: Theme.accent.opacity(0.7), fill: Theme.raised, marker: Theme.accent)
    }
}
