import "./host-runtime.ts"
import { writePermissions } from "./config-fixture.ts"

import assert from "node:assert/strict"
import * as fs from "node:fs"
import * as path from "node:path"
import { describe, it } from "node:test"
import { getCurrentTools } from "@earendil-works/pi-ai"
import { createEventBus, type ExtensionAPI } from "@earendil-works/pi-coding-agent"
import { Type, type TSchema } from "typebox"

import { compile } from "../extensions/__lib/schema.ts"

import permissions from "../extensions/permissions/index.ts"
import initialize from "../workflows/extensions/init/index.ts"
import { activateWorkflowDiscovery } from "../workflows/extensions/__lib/tools.ts"
import { answer, call, nativeSessionFixture } from "./native-session-fixture.ts"

function accepts(schema: TSchema, input: unknown): boolean {
    return compile(schema).check(input)
}

function activation(options: { defaults?: string[]; registered?: boolean; active?: string[] } = {}) {
    let active = options.active ?? ["read"]
    const writes: string[][] = []
    const pi = {
        getSettings: () => ({ defaultTools: options.defaults }),
        getAllTools: () => (options.registered === false ? [] : [{ name: "codemode", exposure: "model-only" }]),
        getActiveTools: () => active,
        setActiveTools: (names: string[]) => {
            writes.push(names)
            active = names
        },
    } as unknown as ExtensionAPI
    return { pi, writes, active: () => active }
}

describe("workflow tool discovery", () => {
    it("activates only existing Codemode and preserves active tools", () => {
        const h = activation()
        assert.equal(activateWorkflowDiscovery(h.pi, []), undefined)
        assert.deepEqual(h.active(), ["read", "codemode"])
        assert.equal(activateWorkflowDiscovery(h.pi, []), undefined)
        assert.equal(h.writes.length, 1)
    })

    for (const [label, defaults, argv] of [
        ["setting exclusion", ["-codemode"], []],
        ["setting exclusion plus unrelated CLI modifier", ["-codemode"], ["--tools", "+read"]],
        ["explicit setting selection", ["read", "bash"], []],
        ["CLI exclusion", undefined, ["--tools", "-codemode"]],
        ["all tools disabled", undefined, ["--no-tools"]],
    ] as const) {
        it(`preserves ${label}`, () => {
            const h = activation({ defaults: defaults ? [...defaults] : undefined })
            assert.match(activateWorkflowDiscovery(h.pi, [...argv])!, /require Codemode discovery/)
            assert.deepEqual(h.writes, [])
        })
    }

    it("honors an explicit CLI addition over the setting exclusion", () => {
        const h = activation({ defaults: ["-codemode"] })
        assert.equal(activateWorkflowDiscovery(h.pi, ["--tools", "+codemode"]), undefined)
        assert.deepEqual(h.active(), ["read", "codemode"])
    })

    it("reports a missing or allowlist-filtered tool without registering a replacement", () => {
        const h = activation({ registered: false })
        assert.match(activateWorkflowDiscovery(h.pi, [])!, /Enable the existing codemode/)
        assert.deepEqual(h.writes, [])
    })

    it("registers concrete initialization operations with provider-specific required inputs", () => {
        const tools: Parameters<ExtensionAPI["registerTool"]>[0][] = []
        initialize({
            on() {},
            events: createEventBus(),
            registerTool: (tool: (typeof tools)[number]) => tools.push(tool),
        } as never)
        assert.equal(tools.length, 10)
        assert.ok(!tools.some((tool) => tool.name === "init" || tool.name.endsWith("_gig")))
        for (const tool of tools) {
            assert.ok(Type.IsObject(tool.parameters))
            assert.ok(!("anyOf" in tool.parameters))
            assert.equal(tool.exposure, "deferred")
            assert.equal(tool.namespace?.name, "workflows_initialization")
        }
        const input = { title: "Example", slug: "example", request: "Do the work.", kind: "refactor" }
        const preflight = tools.find((tool) => tool.name === "preflight_task")!.parameters
        assert.ok(accepts(preflight, input))
        assert.ok(!accepts(preflight, { ...input, entity: "task" }))
        assert.ok(!accepts(preflight, { ...input, priority: "High" }))
        assert.ok(!accepts(preflight, { ...input, kind: "unsupported" }))
        // New work is always standalone: preflight takes no parent.
        assert.ok(!accepts(preflight, { ...input, parent: { kind: "none" } }))
        const issue = { issueId: 123, issueNumber: 4, issueUrl: "https://github.com/o/r/issues/4" }
        const none = { kind: "none" }
        const epic = { kind: "epic", epicDir: ".piton/.local/epics/example" }
        const github = tools.find((tool) => tool.name === "init_github_task")!.parameters
        assert.ok(!accepts(github, input))
        assert.ok(!accepts(github, { ...input, object: issue }), "a Task names its parent")
        for (const parent of [none, epic]) {
            assert.ok(accepts(github, { ...input, parent, object: issue }))
            assert.ok(accepts(github, { ...input, parent, object: { ...issue, projectItemId: 456 } }))
        }
        assert.ok(!accepts(github, { ...input, parent: { kind: "epic" }, object: issue }))
        assert.ok(!accepts(github, { ...input, parent: { ...none, epicDir: "x" }, object: issue }))
        const { kind: _kind, ...withoutKind } = input
        assert.ok(!accepts(github, { ...withoutKind, parent: epic, object: issue }), "a Task always states its Kind")
        const linear = tools.find((tool) => tool.name === "init_linear_task")!.parameters
        assert.ok(!accepts(linear, { ...input, parent: none, object: issue }))
        const linearIssue = { issueId: "uuid", identifier: "ENG-4", issueUrl: "https://linear.app/x/issue/ENG-4" }
        assert.ok(accepts(linear, { ...input, parent: none, object: linearIssue }))
        assert.ok(accepts(linear, { ...input, parent: epic, object: { ...linearIssue, projectId: "p" } }))
        const local = tools.find((tool) => tool.name === "init_local_task")!.parameters
        assert.ok(accepts(local, input))
        assert.ok(!accepts(local, { ...input, object: issue }))
        assert.ok(!accepts(local, { ...input, parent: none }))
    })

    it("discovers initialization through native Codemode without declaring or inlining the catalog", async () => {
        const h = await nativeSessionFixture({
            extensions: [permissions, initialize],
            tools: ["+read"],
            setup() {
                writePermissions("global", { defaultMode: "allow" })
            },
        })
        try {
            assert.ok(h.session.getActiveToolNames().includes("codemode"))
            assert.ok(!h.session.getActiveToolNames().includes("init_github_task"))
            h.respond(
                call({ name: "codemode", arguments: { code: 'text(await describeTool("init_github_task"));' } }),
                answer("Discovered."),
            )
            await h.prompt("Find the standalone initialization operation.")
            for (const request of h.requests) {
                const tools = getCurrentTools(request.messages)
                assert.ok(!tools.some((tool) => tool.name === "init_github_task"))
                assert.ok(
                    !tools.find((tool) => tool.name === "codemode")!.description.includes("tools.init_github_task"),
                )
            }
            const result = h.manager.buildSessionProjection().messages.find((message) => message.role === "toolResult")
            assert.match(JSON.stringify(result), /init_github_task/)
            assert.match(JSON.stringify(result), /issueNumber/)
            assert.match(JSON.stringify(result), /refactor/)
            await h.reopen()
            assert.ok(h.session.getActiveToolNames().includes("codemode"))
            assert.ok(!h.session.getActiveToolNames().includes("init_github_task"))
        } finally {
            await h.cleanup()
        }
    })
})
