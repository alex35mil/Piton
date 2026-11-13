import Foundation
import Testing

@testable import PitonKit

private func dialog(title: String?, method: String = "select") throws -> Dialog {
    var fields: [String: JSON] = [
        "id": .string("piton:write:1"),
        "method": .string(method),
        "options": .array([.string("Accept"), .string("Reject")]),
    ]
    if let title { fields["title"] = .string(title) }
    return try JSON.object(fields).decode(Dialog.self)
}

private func payload(_ object: [String: Any]) throws -> String {
    String(decoding: try JSONSerialization.data(withJSONObject: object), as: UTF8.self)
}

@Test func readsTheDaemonsProtectedWrite() throws {
    // The daemon's dialog (daemon/src/protected_write.rs): a write of the whole file.
    let title = try payload([
        "prompt": "write (protected): /r/.piton/config.toml",
        "toolName": "write",
        "toolInput": ["path": "/r/.piton/config.toml", "content": "a\nB\nc"],
        "current": "a\nb\nc",
    ])
    let review = try #require(FileReview(dialog: dialog(title: title)))
    #expect(review.path == "/r/.piton/config.toml")
    #expect(review.change == .write(current: "a\nb\nc", content: "a\nB\nc"))
    // A diff against the current content, not the whole file.
    #expect(review.lines() == [.same("a"), .removed("b"), .added("B"), .same("c")])
}

@Test func showsOnlyTheChangeInALongFile() {
    let old = (1...100).map { "line \($0)" }
    var new = old
    new[49] = "changed"
    let lines = FileReview.trim(FileReview.diff(old, new), context: 1)
    #expect(lines == [.same("line 49"), .removed("line 50"), .added("changed"), .same("line 51")])
}

@Test func aNewFileIsAllAdded() throws {
    let title = try payload(["prompt": "p", "toolName": "write", "toolInput": ["path": "/x", "content": "a\nb"]])
    let review = try #require(FileReview(dialog: dialog(title: title)))
    #expect(review.lines() == [.added("a"), .added("b")])
}

@Test func readsPisEditReview() throws {
    // pi's file review (pi/extensions/permissions/files.ts): an edit's replacements.
    let title = try payload([
        "prompt": "edit: src/a.rs",
        "toolName": "edit",
        "toolInput": ["path": "src/a.rs", "edits": [["oldText": "a", "newText": "b"]]],
    ])
    let review = try #require(FileReview(dialog: dialog(title: title)))
    #expect(review.change == .edit([.init(old: "a", new: "b")]))
}

@Test func ordinaryDialogsAreNotReviews() throws {
    #expect(FileReview(dialog: try dialog(title: "Pick one")) == nil)
    #expect(FileReview(dialog: try dialog(title: nil)) == nil)
    let bash = try payload(["prompt": "p", "toolName": "bash", "toolInput": ["command": "ls"]])
    #expect(FileReview(dialog: try dialog(title: bash)) == nil)
    let write = try payload(["prompt": "p", "toolName": "write", "toolInput": ["path": "/x", "content": ""]])
    #expect(FileReview(dialog: try dialog(title: write, method: "confirm")) == nil)
}
