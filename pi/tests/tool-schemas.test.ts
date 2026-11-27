import "./host-runtime.ts"

import assert from "node:assert/strict"
import { describe, it } from "node:test"
import { createEventBus, type ExtensionAPI, type ToolDefinition } from "@earendil-works/pi-coding-agent"
import { Type } from "typebox"

import { PROTECTED_TOOLS, registerProtectedTools } from "../extensions/permissions/protected.ts"
import { registerWorkerTools, WORKER_TOOLS } from "../extensions/workers/index.ts"
import { claudeCodeSchema } from "./claude-code-schema-fixture.ts"
import { codingFactories, workflowFactories } from "./workflow-tools-fixture.ts"

const json = (value: unknown) => JSON.parse(JSON.stringify(value))

/** Every tool the pi package registers, the workers extension's and the protected-file tools as in a Piton session. */
async function registeredTools(): Promise<ToolDefinition[]> {
    const tools: ToolDefinition[] = []
    const pi = {
        events: createEventBus(),
        on() {},
        registerCommand() {},
        registerProvider() {},
        registerTool: (definition: ToolDefinition) => tools.push(definition),
        getAllTools: () => [],
        getActiveTools: () => [],
        setActiveTools() {},
        getSettings: () => ({}),
    } as unknown as ExtensionAPI
    for (const factory of [...codingFactories, ...workflowFactories]) await factory(pi)
    registerWorkerTools(pi, { kind: "session", socket: "/nowhere", token: "t" })
    registerProtectedTools(pi, { kind: "session", socket: "/nowhere", token: "t" })
    return tools
}

// .agents/rules/tool-schemas.md: every tool works with every model (#81). Claude Code's bridge drops a root union, so
// one would reach Claude as an empty object; GLM rejects it.
describe("every registered tool's schema", () => {
    it("has a unique, short ASCII name and a plain object root that Claude Code's bridge keeps as is", async () => {
        const tools = await registeredTools()
        assert.ok(tools.length > 0)
        const names = tools.map((t) => t.name)
        assert.equal(new Set(names).size, names.length, "unique names")
        for (const tool of tools) {
            assert.match(tool.name, /^[A-Za-z0-9_-]{1,50}$/, tool.name)
            assert.ok(Type.IsObject(tool.parameters), `${tool.name}: an object root`)
            for (const key of ["anyOf", "oneOf", "allOf"]) assert.ok(!(key in tool.parameters), `${tool.name}: ${key}`)
            const checkChoices = (value: unknown, at: string): void => {
                if (!value || typeof value !== "object") return
                assert.notEqual(typeof (value as Record<string, unknown>).const, "string", `${at}: use a string enum`)
                for (const [key, child] of Object.entries(value)) checkChoices(child, `${at}.${key}`)
            }
            checkChoices(json(tool.parameters), tool.name)
            const schema = json(tool.parameters)
            assert.deepEqual(claudeCodeSchema(structuredClone(schema)), schema, `${tool.name}: kept by the bridge`)
        }
    })

    it("includes the worker tools, one per operation", async () => {
        const names = (await registeredTools()).map((t) => t.name)
        for (const name of [...WORKER_TOOLS, ...PROTECTED_TOOLS]) assert.ok(names.includes(name), name)
    })
})
