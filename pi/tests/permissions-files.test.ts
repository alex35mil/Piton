import "./host-runtime.ts"
import { writePermissions } from "./config-fixture.ts"

import assert from "node:assert/strict"
import * as fs from "node:fs"
import { syncBuiltinESMExports } from "node:module"
import * as net from "node:net"
import * as os from "node:os"
import * as path from "node:path"
import { describe, it } from "node:test"

import { Type } from "typebox"
import type { JsonObject } from "@earendil-works/pi-ai"
import {
    createEditToolDefinition,
    createWriteToolDefinition,
    type ToolDefinition,
} from "@earendil-works/pi-coding-agent"
import registerPermission, { deniedProtectedWrite } from "../extensions/permissions/index.ts"
import { decorateReviewCodemode, isFileReviewDetails } from "../extensions/permissions/review.ts"
import registerRules from "../extensions/rules/index.ts"
import { nativeMcpFixture } from "./native-mcp-fixture.ts"

const note = {
    path: "target.txt",
    side: "proposed" as const,
    lineStart: 1,
    lineEnd: 1,
    lines: ["after"],
    note: "Explain the complete naming choice before more work.\nDo not omit this second line.",
}
const args = (kind: "edit" | "write"): JsonObject =>
    kind === "edit"
        ? { path: "target.txt", edits: [{ oldText: "before", newText: "after" }] }
        : { path: "target.txt", content: "after" }
const textOf = (outcome: Awaited<ReturnType<ReturnType<typeof nativeMcpFixture>["run"]>>) =>
    outcome.result.content.map((part) => (part.type === "text" ? part.text : "")).join("\n")

function fixture(
    mode: "allow" | "ask" | "deny" = "ask",
    rules: { allow?: readonly string[]; ask?: readonly string[]; deny?: readonly string[] } = {},
) {
    const h = nativeMcpFixture({ servers: [] })
    const file = path.join(h.root, "target.txt")
    writePermissions(h.root, {
        defaultMode: mode,
        ask: [...(rules.ask ?? [])],
        deny: [...(rules.deny ?? [])],
        allow: ["codemode", "later", ...(rules.allow ?? [])],
    })
    fs.writeFileSync(file, "before")
    registerPermission(h.pi)
    let choice: string | undefined = "Accept"
    let editorContent: string | undefined
    let prompts = 0
    let status: string | undefined
    let later = 0
    let writes = 0
    const writeFile = fs.promises.writeFile
    fs.promises.writeFile = async (...parameters) => {
        if (parameters[0] === file) writes++
        return writeFile(...parameters)
    }
    syncBuiltinESMExports()
    h.context.ui.setStatus = (key: string, text: string | undefined) => {
        if (key === "permissions") status = text
    }
    h.context.ui.select = async () => {
        prompts++
        if (editorContent !== undefined) fs.writeFileSync(file, editorContent)
        return choice
    }
    h.pi.registerTool({
        name: "later",
        label: "later",
        description: "Record later work",
        parameters: Type.Object({}),
        async execute() {
            later++
            return { content: [{ type: "text", text: "later work" }], details: undefined }
        },
    })
    return {
        ...h,
        file,
        choose(value: string | undefined, applied?: string) {
            choice = value
            editorContent = applied
        },
        prompts: () => prompts,
        status: () => status,
        writes: () => writes,
        later: () => later,
        content: () => fs.readFileSync(file, "utf-8"),
        async cleanup() {
            fs.promises.writeFile = writeFile
            syncBuiltinESMExports()
            await h.cleanup()
        },
    }
}

describe("native file permission execution", () => {
    for (const kind of ["edit", "write"] as const) {
        for (const mode of ["allow", "ask"] as const) {
            it(`${mode} ${kind} delegates to native execution once`, async () => {
                const h = fixture(mode)
                try {
                    const result = await h.run(kind, args(kind))
                    assert.equal(result.isError, false)
                    assert.equal(h.content(), "after")
                    assert.equal(h.writes(), 1)
                    assert.equal(h.prompts(), mode === "ask" ? 1 : 0)
                    if (kind === "edit") assert.match(JSON.stringify(result.result.details), /patch/)
                } finally {
                    await h.cleanup()
                }
            })
        }
        for (const result of ["Accepted", "AcceptModified"] as const) {
            it(`${kind} ${result} returns editor success without another write`, async () => {
                const h = fixture()
                try {
                    const content = result === "Accepted" ? "after" : "the user's modified version"
                    h.choose(JSON.stringify({ result, content }), content)
                    const outcome = await h.run(kind, args(kind))
                    assert.equal(outcome.isError, false)
                    assert.equal(h.content(), content)
                    assert.equal(h.writes(), 0)
                    assert.equal(h.aborted(), false)
                    assert.ok(isFileReviewDetails(outcome.result.details))
                    assert.match(textOf(outcome), /^\[accepted\]/)
                    if (result === "AcceptModified") assert.match(textOf(outcome), /the user's modified version/)
                    assert.equal(h.hooks.has("message_end"), false)
                } finally {
                    await h.cleanup()
                }
            })
        }
        for (const notes of [[], [note]]) {
            it(`${kind} rejection ${notes.length ? "with" : "without"} notes preserves its abort policy`, async () => {
                const h = fixture()
                try {
                    h.choose(JSON.stringify({ result: "Rejected", notes }))
                    const outcome = await h.run(kind, args(kind))
                    assert.equal(outcome.isError, true)
                    assert.equal(h.content(), "before")
                    assert.equal(h.writes(), 0)
                    assert.equal(h.aborted(), notes.length === 0)
                    assert.match(textOf(outcome), /^\[rejected\]/)
                    if (notes.length) assert.ok(textOf(outcome).includes(note.note))
                } finally {
                    await h.cleanup()
                }
            })
        }
        it(`${kind} denial and no-UI ask block before review or mutation`, async () => {
            for (const mode of ["deny", "ask"] as const) {
                const h = fixture(mode)
                try {
                    if (mode === "ask") h.context.hasUI = false
                    const outcome = await h.run(kind, args(kind))
                    assert.equal(outcome.isError, true)
                    assert.equal(h.content(), "before")
                    assert.equal(h.prompts(), 0)
                    assert.equal(h.writes(), 0)
                    // Neither a deny nor a no-UI ask ends the turn.
                    assert.equal(h.aborted(), false)
                } finally {
                    await h.cleanup()
                }
            }
        })
        it(`${kind} rule denial names the rule and keeps the turn`, async () => {
            const h = fixture("allow", { deny: [`${kind}(target.txt)`] })
            try {
                const outcome = await h.run(kind, args(kind))
                assert.equal(outcome.isError, true)
                assert.ok(textOf(outcome).includes(`Denied by permission rule \`${kind}(target.txt)\``))
                assert.equal(h.content(), "before")
                assert.equal(h.writes(), 0)
                assert.equal(h.aborted(), false)
            } finally {
                await h.cleanup()
            }
        })
    }

    describe("auto-accept toggle", () => {
        const AUTO = "▶︎ Auto-accept edits"
        const REVIEW = "◉ Review edits"
        type Outcome = "applied" | "reviewed" | "denied"
        type Rules = Parameters<typeof fixture>[1]

        // Session overrides are process-wide: track them so every test leaves none behind.
        function toggler(h: ReturnType<typeof fixture>) {
            let overridden = false
            return {
                async toggle() {
                    await h.command("permissions-toggle-auto-accept")
                    overridden = !overridden
                },
                async mode() {
                    await h.command("permissions-mode")
                    overridden = true
                },
                async reset() {
                    if (overridden) await h.command("permissions-toggle-auto-accept")
                },
            }
        }

        // Runs one call against a fresh target and reports what happened to it.
        async function attempt(h: ReturnType<typeof fixture>, kind: "edit" | "write"): Promise<Outcome> {
            fs.writeFileSync(h.file, "before")
            const prompts = h.prompts()
            const outcome = await h.run(kind, args(kind))
            if (outcome.isError) {
                assert.equal(h.content(), "before")
                return "denied"
            }
            assert.equal(h.content(), "after")
            return h.prompts() > prompts ? "reviewed" : "applied"
        }

        // Outcome and status before toggling, after the first toggle, and after the second.
        const cases: {
            name: string
            mode: "allow" | "ask"
            rules: (kind: "edit" | "write") => Rules
            steps: [Outcome, string | undefined][]
        }[] = [
            {
                name: "review default turns auto-accept on, then back to review",
                mode: "ask",
                rules: () => ({}),
                steps: [
                    ["reviewed", undefined],
                    ["applied", AUTO],
                    ["reviewed", undefined],
                ],
            },
            {
                name: "auto-accept default turns review on, then back to auto-accept",
                mode: "ask",
                rules: () => ({ allow: ["edit", "write"] }),
                steps: [
                    ["applied", undefined],
                    ["reviewed", REVIEW],
                    ["applied", undefined],
                ],
            },
            {
                name: "defaultMode allow is an auto-accept default",
                mode: "allow",
                rules: () => ({}),
                steps: [
                    ["applied", undefined],
                    ["reviewed", REVIEW],
                    ["applied", undefined],
                ],
            },
            {
                name: "tool(*) allows count as an auto-accept default",
                mode: "ask",
                rules: () => ({ allow: ["edit(*)", "write(*)"] }),
                steps: [
                    ["applied", undefined],
                    ["reviewed", REVIEW],
                    ["applied", undefined],
                ],
            },
            {
                name: "path-scoped allows leave a review default",
                mode: "ask",
                rules: () => ({ allow: ["edit(target.txt)", "write(target.txt)"] }),
                steps: [
                    ["applied", undefined],
                    ["applied", AUTO],
                    ["applied", undefined],
                ],
            },
            {
                name: "allowing only the other tool leaves a review default",
                mode: "ask",
                rules: (kind) => ({ allow: [kind === "edit" ? "write" : "edit"] }),
                steps: [
                    ["reviewed", undefined],
                    ["applied", AUTO],
                    ["reviewed", undefined],
                ],
            },
            {
                name: "an explicit path ask still prompts with auto-accept on",
                mode: "ask",
                rules: (kind) => ({ ask: [`${kind}(target.txt)`] }),
                steps: [
                    ["reviewed", undefined],
                    ["reviewed", AUTO],
                    ["reviewed", undefined],
                ],
            },
            {
                name: "a blanket ask keeps review and shows no auto-accept status",
                mode: "allow",
                rules: (kind) => ({ ask: [kind] }),
                steps: [
                    ["reviewed", undefined],
                    ["reviewed", undefined],
                    ["reviewed", undefined],
                ],
            },
            {
                name: "an explicit deny blocks with auto-accept on",
                mode: "ask",
                rules: (kind) => ({ deny: [`${kind}(target.txt)`] }),
                steps: [
                    ["denied", undefined],
                    ["denied", AUTO],
                    ["denied", undefined],
                ],
            },
            {
                name: "an explicit deny blocks with review on",
                mode: "ask",
                rules: (kind) => ({ allow: ["edit", "write"], deny: [`${kind}(target.txt)`] }),
                steps: [
                    ["denied", undefined],
                    ["denied", REVIEW],
                    ["denied", undefined],
                ],
            },
        ]

        for (const kind of ["edit", "write"] as const) {
            for (const testCase of cases) {
                it(`${kind}: ${testCase.name}`, async () => {
                    const h = fixture(testCase.mode, testCase.rules(kind))
                    const t = toggler(h)
                    try {
                        await h.start()
                        for (const [index, [outcome, status]] of testCase.steps.entries()) {
                            if (index > 0) await t.toggle()
                            assert.equal(h.status(), status, `status at step ${index}`)
                            assert.equal(await attempt(h, kind), outcome, `outcome at step ${index}`)
                        }
                    } finally {
                        await t.reset()
                        await h.cleanup()
                    }
                })
            }
        }

        it("refreshes the status after /permissions-mode and shows none for a session deny", async () => {
            const h = fixture("ask", { allow: ["edit", "write"] })
            const t = toggler(h)
            const selections = ["ask", "deny"]
            Object.assign(h.context.ui, {
                input: async () => "write",
                select: async () => selections.shift(),
            })
            try {
                await h.start()
                assert.equal(h.status(), undefined)
                await t.mode()
                assert.equal(h.status(), REVIEW)
                await t.mode()
                assert.equal(h.status(), undefined)
                // Toggling clears the session deny and returns to the default.
                await t.toggle()
                assert.equal(h.status(), undefined)
                await t.toggle()
                assert.equal(h.status(), REVIEW)
            } finally {
                await t.reset()
                await h.cleanup()
            }
        })
    })

    it("preserves native schemas, preparation, prompt contributions, and rendering", async () => {
        const h = fixture()
        try {
            for (const native of [createEditToolDefinition(h.root), createWriteToolDefinition(h.root)]) {
                const wrapped = h.tools.get(native.name)!
                assert.deepEqual(wrapped.parameters, native.parameters)
                assert.equal(wrapped.prepareArguments, native.prepareArguments)
                assert.equal(wrapped.renderCall, native.renderCall)
                assert.equal(wrapped.renderShell, native.renderShell)
                assert.equal(wrapped.promptSnippet, native.promptSnippet)
                assert.deepEqual(wrapped.promptGuidelines, native.promptGuidelines)
                assert.deepEqual(wrapped.constrainedSampling, native.constrainedSampling)
            }
        } finally {
            await h.cleanup()
        }
    })

    it("runs the rules gate before editor review even when permission registered first", async () => {
        const h = fixture()
        try {
            fs.mkdirSync(path.join(h.root, ".agents/rules"), { recursive: true })
            fs.writeFileSync(
                path.join(h.root, ".agents/rules/scoped.md"),
                '---\npaths: ["target.txt"]\n---\nRead this scope before editing.\n',
            )
            registerRules(h.pi)
            await h.start()
            const outcome = await h.run("edit", args("edit"))
            assert.equal(outcome.isError, true)
            assert.match(textOf(outcome), /rules extension/)
            assert.equal(h.prompts(), 0)
            assert.equal(h.writes(), 0)
        } finally {
            await h.cleanup()
        }
    })

    it("blocks invalid permission settings without asking or writing", async () => {
        const h = fixture()
        try {
            fs.writeFileSync(path.join(h.root, ".piton/config.toml"), "[permissions]\nallow = [7]\n")
            const outcome = await h.run("write", args("write"))
            assert.equal(outcome.isError, true)
            assert.equal(h.prompts(), 0)
            assert.equal(h.writes(), 0)
            assert.equal(h.aborted(), true)
        } finally {
            await h.cleanup()
        }
    })

    it("keeps checking tools when an unrelated configuration file is broken", async () => {
        const h = fixture("allow")
        try {
            const file = path.join(h.root, ".piton/config.toml")
            fs.writeFileSync(file, `tracker = 1\n${fs.readFileSync(file, "utf8")}[branches]\nunknown = 1\n`)
            const outcome = await h.run("write", args("write"))
            assert.equal(outcome.isError, false)
            assert.equal(h.writes(), 1)
            assert.equal(h.aborted(), false)
        } finally {
            await h.cleanup()
        }
    })
})

/** A fake daemon on `agent.sock`: records each request and answers with the next outcome. */
function fakeDaemon(outcomes: unknown[]) {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "piton-agent-"))
    const socket = path.join(dir, "agent.sock")
    const requests: { token: string; path: string; content: string }[] = []
    const server = net.createServer((connection) => {
        let buf = ""
        connection.setEncoding("utf-8")
        connection.on("data", (chunk: string) => {
            buf += chunk
            if (!buf.includes("\n")) return
            requests.push(JSON.parse(buf))
            connection.end(`${JSON.stringify({ type: "protected_write", ...(outcomes.shift() as object) })}\n`)
        })
    })
    const env = {
        PITON_AGENT_SOCKET: process.env.PITON_AGENT_SOCKET,
        PITON_SESSION_TOKEN: process.env.PITON_SESSION_TOKEN,
    }
    return {
        requests,
        async start() {
            await new Promise<void>((resolve) => server.listen(socket, resolve))
            process.env.PITON_AGENT_SOCKET = socket
            process.env.PITON_SESSION_TOKEN = "t0ken"
        },
        async stop() {
            for (const [key, value] of Object.entries(env)) {
                if (value === undefined) delete process.env[key]
                else process.env[key] = value
            }
            await new Promise<void>((resolve) => server.close(() => resolve()))
            fs.rmSync(dir, { recursive: true, force: true })
        },
    }
}

describe("protected files", () => {
    const protectedArgs = (kind: "edit" | "write"): JsonObject =>
        kind === "edit"
            ? { path: ".piton/config.toml", edits: [{ oldText: '"codemode"', newText: '"codemode", "read"' }] }
            : { path: ".piton/config.toml", content: "" }

    for (const kind of ["edit", "write"] as const) {
        it(`${kind} goes through the daemon under piton, even when edits are allowed`, async () => {
            const h = fixture("allow", { allow: ["edit", "write"] })
            const daemon = fakeDaemon([{ outcome: "approved", modified: false, content: "x", notes: [] }])
            await daemon.start()
            try {
                const before = fs.readFileSync(path.join(h.root, ".piton/config.toml"), "utf-8")
                const outcome = await h.run(kind, protectedArgs(kind))
                assert.equal(outcome.isError, false, textOf(outcome))
                assert.equal(daemon.requests.length, 1)
                const request = daemon.requests[0]!
                assert.equal(request.token, "t0ken")
                // The daemon canonicalizes the path itself; the pi sends it as the tool resolved it.
                assert.equal(fs.realpathSync(request.path), path.join(fs.realpathSync(h.root), ".piton/config.toml"))
                assert.equal(request.content, kind === "edit" ? before.replace('"codemode"', '"codemode", "read"') : "")
                // The daemon writes; the pi never does.
                assert.equal(fs.readFileSync(path.join(h.root, ".piton/config.toml"), "utf-8"), before)
                assert.equal(h.prompts(), 0)
            } finally {
                await daemon.stop()
                await h.cleanup()
            }
        })
    }

    it("returns the user's version, rejection, and refusals as the outcome", async () => {
        const h = fixture("allow")
        const daemon = fakeDaemon([
            { outcome: "approved", modified: true, content: "mine", notes: [note] },
            { outcome: "rejected", notes: [] },
            { outcome: "refused", reason: "another protected write is waiting" },
        ])
        await daemon.start()
        try {
            const modified = await h.run("write", protectedArgs("write"))
            assert.equal(modified.isError, false)
            assert.match(textOf(modified), /user's version[\s\S]*mine/)
            assert.ok(textOf(modified).includes(note.note))

            const rejected = await h.run("write", protectedArgs("write"))
            assert.equal(rejected.isError, true)
            assert.match(textOf(rejected), /^\[rejected\]/)
            assert.equal(h.aborted(), true, "a rejection without notes ends the turn, like the review")

            const refused = await h.run("write", protectedArgs("write"))
            assert.equal(refused.isError, true)
            assert.match(textOf(refused), /another protected write/)
        } finally {
            await daemon.stop()
            await h.cleanup()
        }
    })

    it("recognizes a shell write the sandbox denied on a protected file of this workspace", () => {
        // This workspace's protected files, as the resolver answers.
        const asked: string[] = []
        const isProtected = (file: string) => {
            asked.push(file)
            return !file.startsWith("/other/")
        }
        for (const output of [
            "sh: .piton/config.toml: Operation not permitted",
            "rm: /u/repo/.pi/settings.json: Operation not permitted",
            "mkdir: apps/.agents/skills/x: Operation not permitted",
            "error: unable to unlink old '.piton/config.toml': Operation not permitted",
            "sh: .PITON/config.toml: Operation not permitted",
        ]) {
            assert.equal(deniedProtectedWrite(output, isProtected), true, output)
        }
        asked.length = 0
        for (const output of [
            "sh: /etc/hosts: Operation not permitted",
            "cat .piton/config.toml",
            "touch .piton/.local/x: Operation not permitted",
            "sh: apps/.pix/f: Operation not permitted",
            // Another checkout's folder the sandbox can't read: not a protected write here.
            "ls: /other/repo/.piton: Operation not permitted",
        ]) {
            assert.equal(deniedProtectedWrite(output, isProtected), false, output)
        }
        // Only names that could be protected are asked about.
        assert.deepEqual(asked, ["/other/repo/.piton"])
    })

    it("asks on the host even when edits are allowed, and leaves other files alone", async () => {
        const h = fixture("allow", { allow: ["edit", "write"] })
        try {
            const plain = await h.run("write", args("write"))
            assert.equal(plain.isError, false)
            assert.equal(h.prompts(), 0, "an allowed ordinary file doesn't ask")
            const reviewed = await h.run("write", protectedArgs("write"))
            assert.equal(reviewed.isError, false)
            assert.equal(h.prompts(), 1, "a protected file always asks")
        } finally {
            await h.cleanup()
        }
    })
})

describe("native codemode review handoff", () => {
    for (const result of ["Accepted", "AcceptModified", "Rejected"] as const) {
        for (const caught of [false, true]) {
            it(`${result} notes survive ${caught ? "caught errors" : "discarded results"} and stop later calls`, async () => {
                const h = fixture()
                const outer = new AbortController()
                try {
                    const longNote = { ...note, note: note.note + " Full review text.".repeat(600) }
                    h.choose(
                        JSON.stringify({ result, content: "modified", notes: [longNote] }),
                        result === "Rejected" ? undefined : result === "Accepted" ? "after" : "modified",
                    )
                    const call = `await tools.edit(${JSON.stringify(args("edit"))});`
                    const code =
                        '// @options: {"max_output_tokens":1}\ntext("prior output"); ' +
                        (caught ? `try { ${call} } catch (_) {}` : call) +
                        " await tools.later({});"
                    const outcome = await h.run("codemode", { code }, outer.signal)
                    assert.equal(outcome.isError, false)
                    assert.equal(
                        h.content(),
                        result === "Rejected" ? "before" : result === "Accepted" ? "after" : "modified",
                    )
                    assert.equal(h.writes(), 0)
                    assert.equal(h.later(), 0)
                    assert.equal(h.aborted(), false)
                    assert.equal(outer.signal.aborted, false)
                    assert.ok(textOf(outcome).includes(longNote.note))
                    const details = outcome.result.details
                    assert.equal(details.reviewHandoff.status, "returned-to-model")
                    assert.equal(details.reviewHandoff.outcomes[0].isError, result === "Rejected")
                    assert.equal(details.calls[0].status, result === "Rejected" ? "cancelled" : "ok")
                    assert.ok(details.fullOutputPath, "Native truncated output stays available")
                } finally {
                    await h.cleanup()
                }
            })
        }
    }

    it("continues normally after acceptance without notes and preserves genuine script failures", async () => {
        for (const fail of [false, true]) {
            const h = fixture()
            try {
                h.choose(JSON.stringify({ result: "Accepted" }), "after")
                const code =
                    'text("prior output"); ' +
                    `await tools.edit(${JSON.stringify(args("edit"))}); ` +
                    (fail ? 'throw new Error("genuine failure");' : "await tools.later({});")
                const outcome = await h.run("codemode", { code })
                assert.equal(outcome.isError, fail)
                assert.equal(h.later(), fail ? 0 : 1)
                assert.equal(h.content(), "after")
                assert.equal(h.writes(), 0)
                assert.equal(h.aborted(), false)
                assert.match(textOf(outcome), /prior output/)
                if (fail) assert.match(textOf(outcome), /genuine failure/)
                assert.equal(outcome.result.details.reviewHandoff, undefined)
            } finally {
                await h.cleanup()
            }
        }
    })

    it("preserves external cancellation", async () => {
        const h = fixture()
        const outer = new AbortController()
        try {
            h.context.ui.select = async () => {
                outer.abort(new Error("External cancellation"))
                return "Accept"
            }
            const outcome = await h.run(
                "codemode",
                { code: `await tools.edit(${JSON.stringify(args("edit"))});` },
                outer.signal,
            )
            assert.equal(outcome.isError, true)
            assert.equal(h.content(), "before")
            assert.equal(h.writes(), 0)
            assert.equal(outcome.result.details.reviewHandoff, undefined)
        } finally {
            await h.cleanup()
        }
    })

    it("retains native parent usage and partial output during owned handoff", async () => {
        const h = fixture()
        try {
            h.choose(JSON.stringify({ result: "Accepted", notes: [note] }), "after")
            const usage = {
                input: 2,
                output: 3,
                cacheRead: 0,
                cacheWrite: 0,
                totalTokens: 5,
                cost: { input: 0.1, output: 0.2, cacheRead: 0, cacheWrite: 0, total: 0.3 },
            }
            const native: ToolDefinition = {
                name: "parent",
                label: "parent",
                description: "Fixture parent result",
                parameters: Type.Unknown(),
                async execute(_id, _input, _signal, _update, ctx) {
                    await ctx.executeTool("edit", args("edit"), { signal: new AbortController().signal })
                    return {
                        content: [{ type: "text", text: "partial output" }],
                        details: { calls: [], retained: "native" },
                        usage,
                        isError: true,
                    }
                },
            }
            const result = await decorateReviewCodemode(native).execute("parent", {}, undefined, undefined, h.context)
            assert.equal(result.isError, false)
            assert.equal(result.usage, usage)
            assert.match(JSON.stringify(result.content), /partial output/)
            assert.equal((result.details as { retained: string }).retained, "native")
        } finally {
            await h.cleanup()
        }
    })
})
