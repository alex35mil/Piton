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

import { registerSandboxBash, unprefixed, type RunLine } from "../extensions/permissions/sandbox.ts"

/** A fake daemon on a Unix socket: records each request and answers with the lines `answer` gives. */
async function fakeDaemon(answer: (request: Record<string, unknown>) => RunLine[]) {
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
            const request = JSON.parse(buf.slice(0, newline)) as Record<string, unknown>
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
        PITON_SESSION_TOKEN: "token",
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
        assert.equal(registerSandboxBash(pi, { PITON_AGENT_SOCKET: "/s", PITON_SESSION_TOKEN: "t" }), false)
        assert.equal(registerSandboxBash(pi, { PITON_RUNTIME: "sandbox" }), false)
        assert.deepEqual(tools, [])
    })

    it("sends every call to the daemon with the agent's command and pi's shell environment", async () => {
        const daemon = await fakeDaemon(() => [out("installed\n"), out("hooks\n"), { type: "exit", code: 2 }])
        try {
            await withPiSettings(daemon.dir, { shellCommandPrefix: "PREFIXED=yes" }, async () => {
                const { registered, bash } = register(daemon.socket)
                assert.ok(registered && bash)
                const updates: string[] = []
                const result = await bash.execute(
                    "call-7",
                    { command: "husky install", outside_sandbox: true, reason: "hooks" },
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
                assert.equal(request!.rerun, true)
                assert.equal(request!.reason, "hooks")
                const env = request!.env as Record<string, string>
                assert.equal(env.PI_SESSION_ID, "s", "pi's session variables")
                assert.ok(env.PATH!.startsWith(path.join(daemon.dir, "agent", "bin")), "pi's bin folder first")
            })
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
                    { command: "rm x", outside_sandbox: true },
                    undefined,
                    undefined,
                    context(daemon.dir),
                ),
                /Not run outside the sandbox: the user denied it/,
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
                assert.equal(request!.rerun, false)
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
