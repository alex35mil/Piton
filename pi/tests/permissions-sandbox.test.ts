import "./host-runtime.ts"

import assert from "node:assert/strict"
import * as fs from "node:fs"
import * as net from "node:net"
import * as os from "node:os"
import * as path from "node:path"
import { describe, it } from "node:test"

import type {
    BashOperations,
    ExtensionAPI,
    ExtensionToolContext,
    ToolDefinition,
} from "@earendil-works/pi-coding-agent"

import type { RefusedLine, RunLine } from "../extensions/__lib/agent.ts"
import {
    addonPath,
    holdLifts,
    lift,
    loadAddon,
    resetAddon,
    type Addon,
    type LiftGranted,
} from "../extensions/__lib/lift.ts"
import { compile } from "../extensions/__lib/schema.ts"
import {
    BLOCKED_SEPARATOR,
    ranLine,
    registerSandboxBash,
    RerunSchema,
    SANDBOX_GUIDELINE,
    unprefixed,
} from "../extensions/permissions/sandbox.ts"

/**
 * A fake daemon on a Unix socket: records each request (and the token it came with) and answers with the lines `answer`
 * gives.
 */
async function fakeDaemon(answer: (request: Record<string, unknown>) => (RunLine | RefusedLine)[]) {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "piton-sandbox-"))
    const socket = path.join(dir, "agent.sock")
    const requests: Record<string, unknown>[] = []
    let closedEarly = false
    const server = net.createServer((connection) => {
        let buf = ""
        let answered = false
        connection.setEncoding("utf-8")
        connection.on("data", (chunk: string) => {
            buf += chunk
            const newline = buf.indexOf("\n")
            if (newline < 0 || answered) return
            answered = true
            const envelope = JSON.parse(buf.slice(0, newline)) as { token: string; request: Record<string, unknown> }
            assert.deepEqual(Object.keys(envelope).sort(), ["request", "token"])
            const request = { ...envelope.request, token: envelope.token }
            requests.push(request)
            const lines = answer(request)
            if (lines.length === 0) return
            connection.end(lines.map((line) => `${JSON.stringify(line)}\n`).join(""))
        })
        connection.on("close", () => {
            if (answered) closedEarly = true
        })
    })
    await new Promise<void>((resolve) => server.listen(socket, resolve))
    return {
        dir,
        socket,
        requests,
        closedEarly: () => closedEarly,
        async close() {
            await new Promise<void>((resolve) => server.close(() => resolve()))
            fs.rmSync(dir, { recursive: true, force: true })
        },
    }
}

function register(socket: string) {
    const tools = new Map<string, ToolDefinition>()
    const handlers = new Map<string, (event: unknown) => unknown>()
    const pi = {
        registerTool(tool: ToolDefinition) {
            tools.set(tool.name, tool)
        },
        on(name: string, handler: (event: unknown) => unknown) {
            handlers.set(name, handler)
        },
    } as unknown as ExtensionAPI
    const registered = registerSandboxBash(pi, {
        PITON_RUNTIME: "sandbox",
        PITON_AGENT_SOCKET: socket,
        PITON_AGENT_TOKEN: "token",
    })
    return { registered, bash: tools.get("bash"), userBash: handlers.get("user_bash") }
}

function context(cwd: string) {
    return {
        cwd,
        sessionManager: { getSessionId: () => "s", getSessionFile: () => undefined },
    } as unknown as ExtensionToolContext
}

const out = (s: string): RunLine => ({ type: "output", data: Buffer.from(s).toString("base64") })
const text = (result: { content: { type: string; text?: string }[] }) =>
    result.content.map((part) => part.text ?? "").join("\n")

/** Runs with pi's agent directory at `dir/agent`, holding `settings`. */
async function withPiSettings<T>(dir: string, settings: object, run: () => Promise<T>): Promise<T> {
    const agentDir = path.join(dir, "agent")
    fs.mkdirSync(agentDir, { recursive: true })
    fs.writeFileSync(path.join(agentDir, "settings.json"), JSON.stringify(settings))
    const previous = process.env.PI_CODING_AGENT_DIR
    process.env.PI_CODING_AGENT_DIR = agentDir
    try {
        return await run()
    } finally {
        if (previous === undefined) delete process.env.PI_CODING_AGENT_DIR
        else process.env.PI_CODING_AGENT_DIR = previous
    }
}

describe("bash in a sandboxed session under piton", () => {
    it("is pi's own bash outside such a session", () => {
        const tools: string[] = []
        const pi = {
            registerTool: (tool: ToolDefinition) => tools.push(tool.name),
            on: () => tools.push("on"),
        } as unknown as ExtensionAPI
        assert.equal(registerSandboxBash(pi, {}), false)
        assert.equal(registerSandboxBash(pi, { PITON_AGENT_SOCKET: "/s", PITON_AGENT_TOKEN: "t" }), false)
        assert.equal(registerSandboxBash(pi, { PITON_RUNTIME: "sandbox" }), false)
        assert.deepEqual(tools, [])
    })

    it("sends every call to the daemon with the agent's command and pi's shell environment", async () => {
        const daemon = await fakeDaemon(() => [out("installed\n"), out("hooks\n"), { type: "exit", code: 2 }])
        const previous = process.env.PITON_AGENT_TOKEN
        process.env.PITON_AGENT_TOKEN = "pi's own"
        try {
            await withPiSettings(daemon.dir, { shellCommandPrefix: "PREFIXED=yes" }, async () => {
                const { registered, bash } = register(daemon.socket)
                assert.ok(registered && bash)
                const updates: string[] = []
                const result = await bash.execute(
                    "call-7",
                    { command: "husky install", rerun: { runtime: "host" }, reason: "hooks" },
                    undefined,
                    (update) => updates.push(update.content.map((p) => (p.type === "text" ? p.text : "")).join("")),
                    context(daemon.dir),
                )
                assert.match(text(result), /installed\nhooks/)
                assert.match(text(result), /exited with code 2/)
                assert.ok(
                    updates.some((u) => u.includes("installed")),
                    "streamed",
                )
                const [request] = daemon.requests
                assert.equal(request!.type, "run_command")
                assert.equal(request!.token, "token")
                assert.equal(request!.call_id, "call-7")
                assert.equal(request!.command, "husky install", "the agent's command, unprefixed")
                assert.deepEqual(request!.rerun, { runtime: "host" }, "the hinted runtime")
                assert.equal(request!.reason, "hooks")
                const env = request!.env as Record<string, string>
                assert.equal(env.PI_SESSION_ID, "s", "pi's session variables")
                assert.ok(env.PATH!.startsWith(path.join(daemon.dir, "agent", "bin")), "pi's bin folder first")
                assert.ok(!("PITON_AGENT_TOKEN" in env), "pi's token never reaches a call")
            })
        } finally {
            if (previous === undefined) delete process.env.PITON_AGENT_TOKEN
            else process.env.PITON_AGENT_TOKEN = previous
            await daemon.close()
        }
    })

    it("fails with the daemon's reason when it refuses the request", async () => {
        const daemon = await fakeDaemon(() => [
            { type: "refused", why: "token", message: "the token belongs to another process" },
        ])
        try {
            const { bash } = register(daemon.socket)
            await assert.rejects(
                bash!.execute("c", { command: "ls" }, undefined, undefined, context(daemon.dir)),
                /refused: the token belongs to another process/,
            )
        } finally {
            await daemon.close()
        }
    })

    it("reports a command the daemon didn't run, and stops a running one", async () => {
        const daemon = await fakeDaemon((request) =>
            request.command === "wait" ? [] : [{ type: "not_run", reason: "the user denied it" }],
        )
        try {
            const { bash } = register(daemon.socket)
            await assert.rejects(
                bash!.execute(
                    "c",
                    { command: "rm x", rerun: { runtime: "sandbox" } },
                    undefined,
                    undefined,
                    context(daemon.dir),
                ),
                /Not run: the user denied it/,
            )
            const stop = new AbortController()
            const running = bash!.execute("d", { command: "wait" }, stop.signal, undefined, context(daemon.dir))
            setTimeout(() => stop.abort(), 50)
            await assert.rejects(running, /aborted/)
            await new Promise((resolve) => setTimeout(resolve, 50))
            assert.ok(daemon.closedEarly(), "the connection closed, so the daemon kills the command")
        } finally {
            await daemon.close()
        }
    })

    it("says where a widened or host run ran, and nothing for the session's own sandbox", async () => {
        const daemon = await fakeDaemon((request) =>
            request.command === "plain"
                ? [out("ok\n"), { type: "exit", code: 0 }]
                : [{ type: "ran", runtime: "host", by: "approval" }, out("probed\n"), { type: "exit", code: 1 }],
        )
        try {
            const { bash } = register(daemon.socket)
            const host = await bash!.execute(
                "c",
                { command: "sh probe.sh", rerun: { runtime: "host" } },
                undefined,
                undefined,
                context(daemon.dir),
            )
            assert.match(text(host), /probed/)
            assert.match(text(host), /on the host/)
            assert.deepEqual((host.structuredContent as { ran?: unknown }).ran, { runtime: "host", by: "approval" })
            const plain = await bash!.execute("d", { command: "plain" }, undefined, undefined, context(daemon.dir))
            assert.doesNotMatch(text(plain), /on the host|widened/)
        } finally {
            await daemon.close()
        }
    })

    it("lists a failed run's blocks; Piton's rerun replaces the result, a denial keeps the first exit", async () => {
        const daemon = await fakeDaemon((request) => {
            switch (request.command) {
                case "reported":
                    return [
                        out("denied\n"),
                        { type: "blocks", blocks: ["file-write-create /h/.pi/x (protected: no approval lifts it)"] },
                        { type: "exit", code: 1 },
                    ]
                case "approved":
                    return [
                        out("Operation not permitted\n"),
                        { type: "blocks", blocks: ["file-write-create /h/x"] },
                        { type: "blocked" },
                        { type: "ran", runtime: "sandbox", by: "approval" },
                        out("done\n"),
                        { type: "exit", code: 0 },
                    ]
                default:
                    return [
                        out("Operation not permitted\n"),
                        { type: "blocks", blocks: ["file-write-create /h/x"] },
                        { type: "blocked" },
                        { type: "not_rerun", reason: "the user declined" },
                        { type: "exit", code: 1 },
                    ]
            }
        })
        try {
            const { bash } = register(daemon.socket)
            const reported = await bash!.execute(
                "a",
                { command: "reported" },
                undefined,
                undefined,
                context(daemon.dir),
            )
            assert.match(text(reported), /\/h\/\.pi\/x \(protected/)
            assert.deepEqual((reported.structuredContent as { blocks?: string[] }).blocks?.length, 1)

            const approved = await bash!.execute(
                "b",
                { command: "approved" },
                undefined,
                undefined,
                context(daemon.dir),
            )
            assert.ok(text(approved).includes(BLOCKED_SEPARATOR.trim()), text(approved))
            assert.match(text(approved), /done/)
            assert.doesNotMatch(text(approved), /exited with code/, "the rerun's exit is the result's")
            assert.match(text(approved), /file-write-create \/h\/x/)
            assert.match(text(approved), /everything open/)

            const denied = await bash!.execute("c", { command: "denied" }, undefined, undefined, context(daemon.dir))
            assert.match(text(denied), /exited with code 1/, "the first run's exit")
            assert.match(text(denied), /user declined/)
        } finally {
            await daemon.close()
        }
    })

    it("adds the sandbox guideline to pi's own bash guidelines", () => {
        const { bash } = register("/nowhere")
        assert.ok(bash!.promptGuidelines?.includes(SANDBOX_GUIDELINE))
    })

    it("names each runtime and cause", () => {
        assert.match(ranLine({ runtime: "host", by: "grant" }), /host[\s\S]*saved/)
        assert.match(ranLine({ runtime: "sandbox", by: "approval" }), /everything open[\s\S]*user/)
        assert.match(ranLine({ runtime: "sandbox", by: "grant" }), /widened[\s\S]*saved/)
        assert.match(ranLine({ runtime: "host", by: "session" }), /host[\s\S]*this session/)
        assert.doesNotMatch(ranLine({ runtime: "sandbox", by: "grant" }), /everything/, "a saved grant may be narrowed")
    })

    it("takes a rerun that hints only the runtime", () => {
        const { bash } = register("/nowhere")
        const properties = (bash!.parameters as { properties: Record<string, unknown> }).properties
        assert.ok(properties.rerun && !("outside_sandbox" in properties))
        const validator = compile(RerunSchema)
        for (const ok of [{ runtime: "sandbox" }, { runtime: "host" }]) assert.ok(validator.validate(ok).ok)
        for (const bad of [{ runtime: "sandbox", grant: "all" }, { runtime: "vm" }, {}, true])
            assert.ok(!validator.validate(bad).ok, JSON.stringify(bad))
    })

    it("maps the daemon's timeout to pi's timeout result", async () => {
        const daemon = await fakeDaemon(() => [out("partial\n"), { type: "timed_out" }])
        try {
            const { bash } = register(daemon.socket)
            await assert.rejects(
                bash!.execute("c", { command: "sleep 9", timeout: 1 }, undefined, undefined, context(daemon.dir)),
                /partial[\s\S]*timed out after 1 seconds/,
            )
            assert.equal(daemon.requests[0]!.timeout, 1)
        } finally {
            await daemon.close()
        }
    })

    it("runs `!` commands through the daemon too, as typed by the user", async () => {
        const daemon = await fakeDaemon(() => [out("listed\n"), { type: "exit", code: 0 }])
        try {
            await withPiSettings(daemon.dir, { shellCommandPrefix: "set -e" }, async () => {
                const { userBash } = register(daemon.socket)
                const handled = userBash!({
                    type: "user_bash",
                    command: "ls",
                    cwd: daemon.dir,
                    excludeFromContext: false,
                })
                const { operations } = handled as { operations: BashOperations }
                const chunks: string[] = []
                const result = await operations.exec("set -e\nls", daemon.dir, {
                    onData: (data) => chunks.push(data.toString()),
                })
                assert.equal(result.exitCode, 0)
                assert.deepEqual(chunks, ["listed\n"])
                const [request] = daemon.requests
                assert.equal(request!.command, "ls", "pi's prefix removed")
                assert.match(String(request!.call_id), /^user:\d+$/)
                assert.ok(!("rerun" in request!), "a typed command never asks for a rerun")
            })
        } finally {
            await daemon.close()
        }
    })

    it("strips only pi's own prefix", () => {
        assert.equal(unprefixed("set -e\nls", "set -e"), "ls")
        assert.equal(unprefixed("ls", "set -e"), "ls")
        assert.equal(unprefixed("set -ex\nls", "set -e"), "set -ex\nls")
        assert.equal(unprefixed("ls", undefined), "ls")
    })
})

/** A fake addon: records what's consumed and released; `fail` is a token it refuses. */
function fakeAddon(fail?: string) {
    const consumed: string[] = []
    const released: number[] = []
    let next = 10
    const addon: Addon = {
        consume(token) {
            if (fail === token) throw new Error("sandbox_extension_consume: Operation not permitted")
            consumed.push(token)
            return next++
        },
        release(handle) {
            released.push(handle)
        },
    }
    return { addon, consumed, released }
}

function withAddon(addon: Addon, socket: string) {
    resetAddon()
    const env = { PITON_AGENT_SOCKET: socket, PITON_AGENT_TOKEN: "token" }
    loadAddon(env, () => addon)
    return env
}

async function waitFor(ready: () => boolean) {
    for (let i = 0; i < 200 && !ready(); i++) await new Promise((r) => setTimeout(r, 5))
    assert.ok(ready(), "timed out")
}

describe("lifts for pi's live sandbox", { skip: process.platform !== "darwin" }, () => {
    it("loads the addon next to the daemon's socket, or says why not", () => {
        resetAddon()
        assert.equal(addonPath({ PITON_AGENT_SOCKET: "/s/agent.sock" }), "/s/native/lift.node")
        assert.equal(addonPath({}), undefined)
        assert.equal(loadAddon({ PITON_AGENT_SOCKET: "/no/such/agent.sock" }).kind, "unavailable")
        resetAddon()
        assert.equal(loadAddon({}).kind, "unavailable")
        resetAddon()
    })

    it("runs the action directly outside a session", async () => {
        assert.deepEqual(await lift([{ kind: "listen" }], "r", async () => 7, { env: {} }), { kind: "done", value: 7 })
    })

    it("consumes the granted tokens, runs the action, and releases them, also when it throws", async () => {
        const daemon = await fakeDaemon(() => [{ type: "granted", tokens: ["t1", "t2"] } as unknown as RunLine])
        const { addon, consumed, released } = fakeAddon()
        try {
            const env = withAddon(addon, daemon.socket)
            const targets = [
                { kind: "write", path: "/w/.git/config" },
                { kind: "service", name: "com.example" },
            ] as const
            const outcome = await lift(
                [...targets],
                "delete a merged branch",
                async () => {
                    assert.deepEqual(consumed, ["t1", "t2"], "open while the action runs")
                    assert.deepEqual(released, [])
                    return "ok"
                },
                { env },
            )
            assert.deepEqual(outcome, { kind: "done", value: "ok" })
            assert.deepEqual(released, [10, 11])
            assert.deepEqual(daemon.requests[0], {
                type: "lift",
                targets,
                reason: "delete a merged branch",
                token: "token",
            })
            const boom = lift(
                [{ kind: "listen" }],
                "r",
                async () => {
                    throw new Error("boom")
                },
                { env },
            )
            await assert.rejects(boom, /boom/)
            assert.deepEqual(released, [10, 11, 12, 13], "released after a throw")
        } finally {
            resetAddon()
            await daemon.close()
        }
    })

    it("never runs the action when the user denies or the daemon refuses", async () => {
        const cases = [
            [{ type: "denied" }, { kind: "denied" }],
            [
                { type: "not_granted", reason: "floor" },
                { kind: "refused", reason: "floor" },
            ],
            [{ type: "unsupported" }, { kind: "unsupported" }],
        ] as const
        for (const [line, expected] of cases) {
            const daemon = await fakeDaemon(() => [line as unknown as RunLine])
            const { addon, consumed } = fakeAddon()
            try {
                const env = withAddon(addon, daemon.socket)
                let ran = false
                const outcome = await lift(
                    [{ kind: "listen" }],
                    "r",
                    async () => {
                        ran = true
                    },
                    { env },
                )
                assert.deepEqual(outcome, expected)
                assert.equal(ran, false)
                assert.deepEqual(consumed, [])
            } finally {
                resetAddon()
                await daemon.close()
            }
        }
    })

    it("holds the stream: consumes grants, replies, releases on revoke and when it ends", async () => {
        const dir = fs.mkdtempSync(path.join(os.tmpdir(), "piton-lifts-"))
        const socket = path.join(dir, "agent.sock")
        const replies: unknown[] = []
        let peer: net.Socket | undefined
        let opened: () => void = () => {}
        const open = new Promise<void>((resolve) => (opened = resolve))
        const server = net.createServer((connection) => {
            peer = connection
            connection.setEncoding("utf-8")
            let buf = ""
            connection.on("data", (chunk: string) => {
                buf += chunk
                let newline: number
                while ((newline = buf.indexOf("\n")) >= 0) {
                    const line = JSON.parse(buf.slice(0, newline)) as Record<string, unknown>
                    buf = buf.slice(newline + 1)
                    if ("token" in line) {
                        assert.deepEqual(line, { token: "token", request: { type: "lifts" } })
                        opened()
                    } else replies.push(line)
                }
            })
        })
        await new Promise<void>((resolve) => server.listen(socket, resolve))
        const { addon, consumed, released } = fakeAddon("bad")
        const granted: LiftGranted[] = []
        try {
            const env = withAddon(addon, socket)
            const stream = holdLifts((g) => granted.push(g), env)
            assert.ok(stream)
            await open
            const send = (line: object) => peer!.write(`${JSON.stringify(line)}\n`)
            send({
                type: "grant",
                id: 1,
                token: "good",
                lift: { kind: "write", path: "/w/.git/config" },
                for_child: false,
            })
            send({ type: "grant", id: 2, token: "bad", lift: { kind: "listen" }, for_child: false })
            send({ type: "grant", id: 3, token: "other", lift: { kind: "listen" }, for_child: true })
            await waitFor(() => replies.length === 3)
            assert.deepEqual(
                replies.map((r) => (r as { type: string }).type),
                ["consumed", "failed", "consumed"],
            )
            assert.deepEqual(consumed, ["good", "other"])
            assert.deepEqual(
                granted.map((g) => [g.lift.kind, g.forChild]),
                [
                    ["write", false],
                    ["listen", true],
                ],
            )
            send({ type: "revoke", id: 1 })
            await waitFor(() => released.length === 1)
            assert.deepEqual(released, [10])
            peer!.end()
            await stream.closed
            assert.deepEqual(released, [10, 11], "every open grant released when the stream ends")
        } finally {
            resetAddon()
            await new Promise<void>((resolve) => server.close(() => resolve()))
            fs.rmSync(dir, { recursive: true, force: true })
        }
    })
})
