import { resetGlobalSettings, TEST_CONFIG_HOME, writeConfig, writeGlobalSetting } from "./host-runtime.ts"
import { permissionsTable } from "./config-fixture.ts"

import assert from "node:assert/strict"
import { execFileSync } from "node:child_process"
import * as fs from "node:fs"
import * as path from "node:path"
import { describe, it } from "node:test"

import { readMcpConfiguration } from "../extensions/__lib/mcp.ts"
import { requireRuntime, resolveRuntime } from "../extensions/__lib/runtime.ts"
import { clearSettingsCache, resolveSettings, section } from "../extensions/__lib/settings.ts"
import { inspectPersistentPermissions } from "../extensions/permissions/index.ts"
import registerWebSearch from "../extensions/web-search/index.ts"
import { inspectProjectMcp, registeredCredentialEnvironment } from "../workflows/extensions/integrations/mcp.ts"
import { nativeMcpFixture } from "./native-mcp-fixture.ts"

function write(file: string, value: unknown) {
    fs.mkdirSync(path.dirname(file), { recursive: true })
    fs.writeFileSync(file, JSON.stringify(value))
    clearSettingsCache()
}

function fixture() {
    const h = nativeMcpFixture()
    execFileSync("git", ["init", "--quiet"], { cwd: h.root })
    const nested = path.join(h.root, "src/nested")
    fs.mkdirSync(nested, { recursive: true })
    const repositoryRoot = fs.realpathSync(h.root)
    return { ...h, nested, repositoryRoot, piton: path.join(repositoryRoot, ".piton") }
}

describe("runtime selection", () => {
    it("reads PITON_RUNTIME as host, sandbox, or invalid", () => {
        assert.deepEqual(resolveRuntime({}), { runtime: "host" })
        assert.deepEqual(resolveRuntime({ PITON_RUNTIME: "" }), { runtime: "host" })
        assert.deepEqual(resolveRuntime({ PITON_RUNTIME: "sandbox" }), { runtime: "sandbox" })
        const invalid = resolveRuntime({ PITON_RUNTIME: "sandboxed" })
        assert.ok("error" in invalid)
        assert.match(invalid.error.message, /PITON_RUNTIME "sandboxed"/)
        assert.equal(requireRuntime({ PITON_RUNTIME: "sandbox" }), "sandbox")
        assert.throws(() => requireRuntime({ PITON_RUNTIME: "Sandbox" }), /invalid PITON_RUNTIME/)
    })

    it("runs every test file in the host runtime regardless of the launching runtime", () => {
        const testsDir = import.meta.dirname
        const files = fs.readdirSync(testsDir).filter((file) => file.endsWith(".test.ts"))
        assert.ok(files.length > 0)
        const missing = files.filter((file) => {
            const source = fs.readFileSync(path.join(testsDir, file), "utf8")
            const firstImport = source.match(/^import\b.*$/m)?.[0]
            return !firstImport?.endsWith('from "./host-runtime.ts"') && firstImport !== 'import "./host-runtime.ts"'
        })
        assert.deepEqual(missing, [])
    })
})

describe("shared settings contract", () => {
    it("resolves the repository's .piton/ from a nested folder, after the global tier", async () => {
        const h = fixture()
        try {
            const resolved = resolveSettings(h.nested, "host")
            assert.equal(resolved.source, h.repositoryRoot)
            assert.equal(resolved.global, TEST_CONFIG_HOME)
            // A missing level is watched as a folder; an existing one also by its candidate files.
            assert.ok(resolved.watch.includes(h.piton))
            assert.ok(resolved.watch.includes(path.join(TEST_CONFIG_HOME, "config.toml")))
            fs.mkdirSync(h.piton)
            assert.ok(resolveSettings(h.nested, "host").watch.includes(path.join(h.piton, "mcp.local.json")))
        } finally {
            await h.cleanup()
        }
    })

    it("shares exact-name MCP precedence and credential-reference provenance with inspection", async () => {
        const h = fixture()
        try {
            const url = "https://api.githubcopilot.com/mcp/"
            writeGlobalSetting("mcp.json", {
                mcpServers: {
                    shared: { url, headers: { Authorization: "Bearer ${GLOBAL_TOKEN}" }, timeout: 12 },
                    global: { command: "fixture" },
                },
                autoEnableCodemode: false,
            })
            write(path.join(h.piton, "mcp.json"), {
                mcpServers: {
                    shared: { url, headers: { Authorization: "Bearer ${PROJECT_TOKEN}" }, timeout: 20 },
                    project: { command: "fixture" },
                },
                autoEnableCodemode: true,
            })
            const local = { url, headers: { Authorization: "Bearer ${LOCAL_TOKEN}", "X-MCP-Toolsets": "issues" } }
            write(path.join(h.piton, "mcp.local.json"), { mcpServers: { shared: local }, autoEnableCodemode: false })
            const loaded = readMcpConfiguration(h.nested, true)
            const shared = loaded.servers.find(({ name }) => name === "shared")!
            assert.deepEqual(shared.config, local)
            assert.equal(shared.path, path.join(h.piton, "mcp.local.json"))
            assert.equal(shared.level, "repository")
            assert.equal(loaded.autoEnableCodemode, false)
            assert.deepEqual(loaded.servers.map(({ name }) => name).sort(), ["global", "project", "shared"])
            const inspected = inspectProjectMcp(h.nested, true).effectiveServers.find(({ name }) => name === "shared")
            assert.equal(inspected?.provider, "github")
            assert.equal(inspected?.credentialEnv, "LOCAL_TOKEN")
            assert.equal(registeredCredentialEnvironment(h.nested, "shared", "github", true), "LOCAL_TOKEN")
            assert.equal(registeredCredentialEnvironment(h.nested, "shared", "github", false), "GLOBAL_TOKEN")
        } finally {
            resetGlobalSettings()
            await h.cleanup()
        }
    })

    it("loads only the global tier's servers without project trust, and reports malformed settings", async () => {
        const h = fixture()
        try {
            writeGlobalSetting("mcp.json", { mcpServers: { global: { command: "fixture" } } })
            write(path.join(h.piton, "mcp.json"), { mcpServers: { project: { command: "fixture" } } })
            assert.deepEqual(
                readMcpConfiguration(h.nested, false).servers.map(({ name }) => name),
                ["global"],
            )
            const untrusted = inspectProjectMcp(h.nested, false)
            assert.deepEqual(
                untrusted.untrustedServers.map(({ name }) => name),
                ["project"],
            )
            fs.writeFileSync(path.join(h.piton, "mcp.local.json"), "not JSON")
            clearSettingsCache()
            assert.throws(() => readMcpConfiguration(h.nested, true), /mcp\.local\.json/)
            assert.match(inspectProjectMcp(h.nested, true).error!, /mcp\.local\.json/)
        } finally {
            resetGlobalSettings()
            await h.cleanup()
        }
    })

    it("merges permission tables level by level, each file's shared table before its runtime table", async () => {
        const h = fixture()
        try {
            writeConfig("global", {
                permissions: permissionsTable({ defaultMode: "ask", allow: ["read"] }),
                "permissions.sandbox": permissionsTable({ defaultMode: "allow", allow: ["bash(npm *)"] }),
                "permissions.host": permissionsTable({ deny: ["bash(host-only *)"] }),
            })
            writeConfig(h.repositoryRoot, { permissions: permissionsTable({ defaultMode: "deny" }) })
            writeConfig(h.repositoryRoot, { "permissions.sandbox.ask": ["bash(git push *)"] }, true)

            const sandbox = inspectPersistentPermissions(h.nested, "sandbox")
            assert.equal(sandbox.runtime, "sandbox")
            assert.deepEqual(
                sandbox.sources.map(
                    ({ level, scope, sharing }) => `${level}:${scope}${sharing === "local" ? ":local" : ""}`,
                ),
                ["global:shared", "global:sandbox", "repository:shared", "repository:sandbox:local"],
            )
            // The repository's shared file is nearer than the global runtime file, so its defaultMode wins.
            assert.equal(sandbox.settings.defaultMode, "deny")
            assert.deepEqual(sandbox.settings.allow, ["read", "bash(npm *)"])
            assert.deepEqual(sandbox.settings.ask, ["bash(git push *)"])
            assert.deepEqual(sandbox.settings.deny, [])

            const host = inspectPersistentPermissions(h.nested, "host")
            assert.deepEqual(host.settings.allow, ["read"])
            assert.deepEqual(host.settings.deny, ["bash(host-only *)"])
            assert.deepEqual(host.settings.ask, [])
        } finally {
            resetGlobalSettings()
            await h.cleanup()
        }
    })

    it("fails only the part whose table is broken", async () => {
        const h = fixture()
        try {
            writeConfig(h.repositoryRoot, { "permissions.allow": ["read"] })
            fs.appendFileSync(path.join(h.piton, "config.toml"), "[branches]\nformat = 1\n")
            clearSettingsCache()
            const resolved = resolveSettings(h.nested, "host")
            assert.throws(() => section(resolved.branches), /config\.toml/)
            assert.deepEqual(section(resolved.permissions).merged.allow, ["read"])
            assert.doesNotThrow(() => section(resolved.mcp))
        } finally {
            await h.cleanup()
        }
    })

    it("uses the nearest web-search key through its real registered tool", async () => {
        const h = fixture()
        const cwd = process.cwd()
        const fetch = globalThis.fetch
        const apiKey = process.env.BRAVE_SEARCH_API_KEY
        try {
            // A secret: only ever in a config.local.toml.
            writeConfig("global", { "web_search.api_key": "global-fixture" }, true)
            assert.throws(
                () => writeConfig(h.repositoryRoot, { "web_search.api_key": "shared" }),
                /config\.local\.toml/,
            )
            writeConfig(h.repositoryRoot, { "web_search.api_key": "local-fixture" }, true)
            delete process.env.BRAVE_SEARCH_API_KEY
            process.chdir(h.nested)
            let calls = 0
            globalThis.fetch = async (_url, options) => {
                calls++
                assert.equal(new Headers(options?.headers).get("X-Subscription-Token"), "local-fixture")
                return new Response(JSON.stringify({ web: { results: [] } }), { status: 200 })
            }
            await registerWebSearch(h.pi)
            const outcome = await h.run("web_search", { query: "fixture query" })
            assert.equal(outcome.isError, false)
            assert.equal(calls, 1)
        } finally {
            process.chdir(cwd)
            globalThis.fetch = fetch
            if (apiKey === undefined) delete process.env.BRAVE_SEARCH_API_KEY
            else process.env.BRAVE_SEARCH_API_KEY = apiKey
            resetGlobalSettings()
            await h.cleanup()
        }
    })
})
