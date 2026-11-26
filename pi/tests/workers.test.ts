import "./host-runtime.ts"

import assert from "node:assert/strict"
import * as fs from "node:fs"
import * as net from "node:net"
import * as os from "node:os"
import * as path from "node:path"
import { describe, it } from "node:test"

import type { ExtensionAPI, ToolDefinition } from "@earendil-works/pi-coding-agent"

import { daemonDecides } from "../extensions/permissions/index.ts"
import workers, { registerWorkerTools, WORKER_TOOLS } from "../extensions/workers/index.ts"

/** A fake daemon: records each request and answers with one line from `answer`. */
async function fakeDaemon(answer: (request: Record<string, unknown>) => object) {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "piton-workers-"))
    const socket = path.join(dir, "agent.sock")
    const requests: Record<string, unknown>[] = []
    const server = net.createServer((connection) => {
        let buf = ""
        connection.setEncoding("utf-8")
        connection.on("data", (chunk: string) => {
            buf += chunk
            const newline = buf.indexOf("\n")
            if (newline < 0) return
            const envelope = JSON.parse(buf.slice(0, newline)) as { token: string; request: Record<string, unknown> }
            requests.push({ ...envelope.request, token: envelope.token })
            connection.end(`${JSON.stringify(answer(envelope.request))}\n`)
        })
    })
    await new Promise<void>((resolve) => server.listen(socket, resolve))
    return {
        socket,
        requests,
        async close() {
            await new Promise<void>((resolve) => server.close(() => resolve()))
            fs.rmSync(dir, { recursive: true, force: true })
        },
    }
}

function register(socket: string) {
    const tools = new Map<string, ToolDefinition>()
    const pi = { registerTool: (tool: ToolDefinition) => tools.set(tool.name, tool) } as unknown as ExtensionAPI
    registerWorkerTools(pi, { kind: "session", socket, token: "pi-token" })
    return tools
}

const run = async (tool: ToolDefinition | undefined, params: object) => {
    assert.ok(tool)
    const result = await tool.execute("call", params as never, undefined, undefined, undefined as never)
    return { text: result.content.map((part) => ("text" in part ? part.text : "")).join("\n"), details: result.details }
}

const worker = { path: "/p/repo--fix", source: "/p/repo", label: "fix", item_id: "github:7" }

describe("workers extension", () => {
    it("sends each operation as its own request with pi's token, and reports the typed outcome", async () => {
        const daemon = await fakeDaemon((request) => {
            switch (request.type) {
                case "worker_create":
                    return { type: "worker_created", worker }
                case "worker_list":
                    return { type: "workers", workers: [{ ...worker, sessions: [] }] }
                case "worker_delete":
                    return { type: "worker_deleted", stopped: 1, warning: null }
                default:
                    return { type: "refused", why: "invalid", message: "?" }
            }
        })
        try {
            const tools = register(daemon.socket)
            const created = await run(tools.get("create_worker"), { name: "fix", item_id: "github:7" })
            assert.match(created.text, /repo--fix/)
            assert.deepEqual(created.details, { kind: "done", value: worker })
            assert.match((await run(tools.get("list_workers"), {})).text, /item=github:7/)
            assert.match((await run(tools.get("delete_worker"), { path: "/p/repo--fix" })).text, /1 session/)
            assert.deepEqual(
                daemon.requests.map((r) => [r.type, r.token]),
                [
                    ["worker_create", "pi-token"],
                    ["worker_list", "pi-token"],
                    ["worker_delete", "pi-token"],
                ],
            )
            assert.deepEqual(daemon.requests[0], {
                type: "worker_create",
                name: "fix",
                item_id: "github:7",
                token: "pi-token",
            })
        } finally {
            await daemon.close()
        }
    })

    it("tells the agent a denial or a refusal, typed", async () => {
        const daemon = await fakeDaemon((request) =>
            request.type === "worker_create"
                ? { type: "not_done", reason: "the user denied it" }
                : { type: "worker_refused", refusal: { kind: "outside_project", path: "/q", project: "/p/repo" } },
        )
        try {
            const tools = register(daemon.socket)
            const denied = await run(tools.get("create_worker"), { name: "x" })
            assert.deepEqual(denied.details, { kind: "notDone", reason: "the user denied it" })
            const refused = await run(tools.get("delete_worker"), { path: "/q" })
            assert.equal((refused.details as { kind: string }).kind, "refused")
            assert.match(refused.text, /outside_project/)
        } finally {
            await daemon.close()
        }
    })

    it("registers nothing outside a Piton session", () => {
        const saved = { socket: process.env.PITON_AGENT_SOCKET, token: process.env.PITON_AGENT_TOKEN }
        delete process.env.PITON_AGENT_SOCKET
        delete process.env.PITON_AGENT_TOKEN
        try {
            const names: string[] = []
            workers({ registerTool: (tool: ToolDefinition) => names.push(tool.name) } as unknown as ExtensionAPI)
            assert.deepEqual(names, [])
        } finally {
            if (saved.socket !== undefined) process.env.PITON_AGENT_SOCKET = saved.socket
            if (saved.token !== undefined) process.env.PITON_AGENT_TOKEN = saved.token
        }
    })

    it("is decided by the daemon, not the permissions extension", () => {
        for (const name of WORKER_TOOLS) assert.ok(daemonDecides(name), name)
        assert.ok(!daemonDecides("bash"))
    })
})
