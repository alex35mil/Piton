import { resetGlobalSettings, writeConfig as writeSettings } from "./host-runtime.ts"

import assert from "node:assert/strict"
import * as cp from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { describe, it } from "node:test"

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent"

import { compile } from "../extensions/__lib/schema.ts"
import { clearSettingsCache } from "../extensions/__lib/settings.ts"
import { inspectPersistentPermissions } from "../extensions/permissions/index.ts"

import { inspectProjectMcp, registeredCredentialEnvironment } from "../workflows/extensions/integrations/mcp.ts"
import { configureProjectMcp } from "../workflows/extensions/project-setup/mcp.ts"
import { configureProjectPermissions } from "../workflows/extensions/project-setup/permissions.ts"
import {
    assertArtifactPersistencePrepared,
    assertWorkerArtifactsLinked,
    detectInstallation,
    developmentCheckout,
    loadProjectConfig,
    loadSessionAnalysis,
    LOCAL_SETTINGS_EXCLUDE,
    prepareArtifactPersistence,
    PROJECT_LOCAL_IGNORE,
    setInstallationForTests,
} from "../workflows/extensions/__lib/project-config.ts"
import registerProjectSetup, { inspectProjectSetup } from "../workflows/extensions/project-setup/index.ts"

function repository(): string {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), "workflow-project-config-"))
    git(root, ["init", "-b", "main"])
    git(root, ["config", "user.email", "test@example.com"])
    git(root, ["config", "user.name", "Test"])
    fs.writeFileSync(path.join(root, "README.md"), "test\n")
    git(root, ["add", "README.md"])
    git(root, ["commit", "-m", "initial"])
    return root
}

/** `[branches]` of the repository's config.toml. */
function writeConfig(root: string, input: { branches: { format?: string } }): void {
    writeSettings(root, { branches: input.branches })
}

function config(branches: { format?: string } = { format: "identifier-title" }) {
    return { branches }
}

function git(root: string, args: string[]): string {
    return cp.execFileSync("git", args, { cwd: root, encoding: "utf-8" }).trim()
}

describe("workflow project configuration", () => {
    it("reads a personal username only from a config.local.toml, the repository's over the global one", () => {
        const root = repository()
        resetGlobalSettings()
        try {
            writeConfig(root, config({ format: "username/identifier-title" }))
            assert.throws(() => loadProjectConfig(root), /username is required/)
            writeSettings("global", { "branches.username": "global-user" }, true)
            assert.equal(loadProjectConfig(root).branches.username, "global-user")
            writeSettings(root, { "branches.username": "local-user" }, true)
            assert.equal(loadProjectConfig(root).branches.username, "local-user")
            // A shared file can't hold it.
            assert.throws(() => writeSettings(root, { "branches.username": "x" }), /config\.local\.toml/)
        } finally {
            resetGlobalSettings()
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    it("detects a development install from the package's own checkout", () => {
        const root = fs.realpathSync(repository())
        try {
            // This test runs from piton's checkout: the package is its `pi/`.
            const checkout = fs.realpathSync(path.resolve(import.meta.dirname, "../.."))
            assert.deepEqual(detectInstallation(path.join(checkout, "pi")), { kind: "development", checkout })
            // Another repository's folder, or no repository at all, is an app install.
            fs.mkdirSync(path.join(root, "pi"))
            assert.deepEqual(detectInstallation(path.join(root, "pi")), { kind: "app" })
            const outside = fs.mkdtempSync(path.join(os.tmpdir(), "piton-app-"))
            assert.deepEqual(detectInstallation(outside), { kind: "app" })
            fs.rmSync(outside, { recursive: true, force: true })

            // A package path whose case differs from the checkout's real name is still a development install.
            const cased = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "PitonCase-")))
            try {
                cp.execFileSync("git", ["init", "-q", cased])
                fs.mkdirSync(path.join(cased, "pi"))
                fs.mkdirSync(path.join(cased, "cli"))
                fs.writeFileSync(path.join(cased, "cli", "Cargo.toml"), "")
                const lowered = path.join(path.dirname(cased), path.basename(cased).toLowerCase(), "pi")
                if (fs.existsSync(lowered)) {
                    assert.deepEqual(detectInstallation(lowered), { kind: "development", checkout: cased })
                }
            } finally {
                fs.rmSync(cased, { recursive: true, force: true })
            }

            setInstallationForTests({ kind: "development", checkout: root })
            assert.equal(developmentCheckout(), root)
            setInstallationForTests({ kind: "app" })
            assert.throws(() => developmentCheckout(), /github\.com\/alex35mil\/Piton\/issues\/new/)
        } finally {
            setInstallationForTests(undefined)
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    it("runs session analysis in a development install unless the global setting turns it off", () => {
        const root = repository()
        resetGlobalSettings()
        try {
            setInstallationForTests({ kind: "development", checkout: root })
            assert.equal(loadSessionAnalysis(), "enabled")
            writeSettings("host", { "development.session_analysis": false })
            assert.equal(loadSessionAnalysis(), "disabled")
            // It's a host setting: a project's config.toml has no such table.
            assert.throws(() => writeSettings(root, { "development.session_analysis": true }), /development/)
            setInstallationForTests({ kind: "app" })
            writeSettings("host", { "development.session_analysis": true })
            assert.equal(loadSessionAnalysis(), "disabled")
        } finally {
            setInstallationForTests(undefined)
            resetGlobalSettings()
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    it("loads operational Markdown dependencies from workflow references", () => {
        const skill = fs.readFileSync("workflows/skills/project-setup/SKILL.md", "utf-8")
        const markdownDependencies = [
            ...[...skill.matchAll(/`(\.\.\/\.\.\/[^`\n]+\.md)`/g)].map((match) => match[1]),
            ...[...skill.matchAll(/\]\((\.\.\/\.\.\/[^)#\s]+\.md)(?:#[^)]*)?\)/g)].map((match) => match[1]),
        ]

        assert.ok(markdownDependencies.length > 0)
        assert.ok(markdownDependencies.every((dependency) => dependency.startsWith("../../references/")))
        assert.ok(markdownDependencies.includes("../../references/setup.md"))
    })

    it("starts guided setup without creating artifacts", async () => {
        const root = repository()
        let sent = ""
        let setup:
            | { handler: (args: string, ctx: { cwd: string; isProjectTrusted(): boolean }) => Promise<void> }
            | undefined
        const tools: string[] = []
        const pi = {
            events: { on: () => () => {} },
            on() {},
            registerCommand: (name: string, command: typeof setup) => {
                if (name === "project-setup") setup = command
            },
            registerTool: (tool: { name: string }) => tools.push(tool.name),
            sendUserMessage: (message: string) => {
                sent = message
            },
        } as unknown as ExtensionAPI
        try {
            fs.mkdirSync(path.join(root, ".piton"))
            fs.writeFileSync(
                path.join(root, ".piton", "mcp.json"),
                JSON.stringify({
                    mcpServers: {
                        existing: {
                            url: "https://example.com/mcp",
                            headers: { Authorization: "Bearer top-secret" },
                        },
                    },
                }),
            )
            registerProjectSetup(pi)
            assert.ok(setup)
            assert.ok(tools.includes("inspect_project_setup"))
            assert.ok(tools.includes("configure_mcp"))
            assert.ok(tools.includes("configure_permissions"))
            assert.ok(tools.includes("inspect_permissions"))
            assert.ok(tools.includes("inspect_linear_workspace"))
            assert.ok(tools.includes("inspect_github_project_setup"))
            assert.ok(tools.includes("preview_github_project_setup"))
            assert.ok(tools.includes("apply_github_project_setup"))
            await setup.handler("Configure workflows", { cwd: root, isProjectTrusted: () => true })
            assert.match(sent, /Configure workflows/)
            assert.match(sent, /Use the project-setup skill/)
            assert.match(sent, /"repositoryRoot":/)
            assert.match(sent, /"path": "\.piton\/config\.toml"/)
            assert.match(sent, /"path": "\.piton\/config\.local\.toml"/)
            assert.match(sent, /"globalConfig":/)
            assert.match(sent, /"name": "existing"/)
            assert.match(sent, /"boundEntityTrackers": \[\]/)
            assert.match(sent, /"projectPathsInGit": \[\]/)
            assert.doesNotMatch(sent, /top-secret|defaultBranch/)
            assert.equal(fs.existsSync(path.join(root, ".piton/.local")), false)
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    it("extracts only safe GitHub identities from fetch remotes without changing Git state", () => {
        const root = repository()
        try {
            assert.deepEqual(inspectProjectSetup(root, true).repositoryIdentity.githubCandidates, [])
            const remotes = [
                [
                    "origin",
                    "https://user:remote-secret@github.com/Owner/Example.git?token=query-secret#fragment-secret",
                ],
                ["ssh", "git@github.com:owner/example.git"],
                ["url", "ssh://git@github.com/Owner/Example.git"],
                ["upstream", "https://github.com/Upstream/Example"],
                ["other", "https://private-secret.example/hidden-secret/repo"],
                ["alias", "git@github-personal:hidden-secret/repo"],
                ["local", "/tmp/private-secret/repo"],
            ]
            for (const [name, url] of remotes) git(root, ["remote", "add", name, url])
            git(root, ["remote", "set-url", "--push", "origin", "https://github.com/PushOnly/Example.git"])
            const before = fs.readFileSync(path.join(root, ".git/config"))
            const index = git(root, ["ls-files", "--stage"])
            const snapshot = inspectProjectSetup(root, true)
            assert.deepEqual(snapshot.repositoryIdentity.githubCandidates, [
                { owner: "Owner", repo: "Example", remotes: ["origin", "ssh", "url"] },
                { owner: "Upstream", repo: "Example", remotes: ["upstream"] },
            ])
            assert.doesNotMatch(
                JSON.stringify(snapshot),
                /remote-secret|query-secret|fragment-secret|private-secret|hidden-secret|PushOnly|defaultBranch/,
            )
            assert.deepEqual(fs.readFileSync(path.join(root, ".git/config")), before)
            assert.equal(git(root, ["ls-files", "--stage"]), index)
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    it("tells setup whether to append or keep the project-local ignore rule", () => {
        const root = repository()
        try {
            const ignore = () => inspectProjectSetup(root, true).projectLocalIgnore
            assert.deepEqual(ignore(), { state: "append", rule: PROJECT_LOCAL_IGNORE })
            fs.writeFileSync(path.join(root, ".gitignore"), `${PROJECT_LOCAL_IGNORE}\n`)
            assert.deepEqual(ignore(), { state: "present", rule: PROJECT_LOCAL_IGNORE })
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    // `project`: the repository's config.toml, as text.
    for (const scenario of [
        { name: "missing", project: undefined },
        { name: "existing", project: '[branches]\nformat = "identifier-title"\n' },
        { name: "incompatible", project: '[branches]\nunsupported = true\n\n[tracker]\nprovider = "github"\n' },
        { name: "malformed", project: "[branches\n" },
    ]) {
        it(`shares a read-only command/tool snapshot with ${scenario.name} setup`, async () => {
            const root = repository()
            git(root, ["remote", "add", "origin", "https://snapshot-secret@github.com/owner/example.git"])
            type Tool = Parameters<ExtensionAPI["registerTool"]>[0]
            const tools = new Map<string, Tool>()
            let setup:
                | { handler: (args: string, ctx: { cwd: string; isProjectTrusted(): boolean }) => Promise<void> }
                | undefined
            let sent = ""
            const pi = {
                events: { on: () => () => {} },
                on() {},
                registerCommand: (name: string, command: typeof setup) => {
                    if (name === "project-setup") setup = command
                },
                registerTool: (tool: Tool) => tools.set(tool.name, tool),
                sendUserMessage: (message: string) => {
                    sent = message
                },
            } as unknown as ExtensionAPI
            try {
                fs.mkdirSync(path.join(root, ".piton"))
                fs.writeFileSync(
                    path.join(root, ".piton/mcp.json"),
                    JSON.stringify({
                        mcpServers: {
                            "setup-github": {
                                url: "https://api.githubcopilot.com/mcp/",
                                headers: { Authorization: "Bearer ${PROJECT_SETUP_GITHUB_TOKEN}" },
                            },
                            "setup-linear": {
                                url: "https://mcp.linear.app/mcp",
                                headers: { Authorization: "Bearer ${SETUP_LINEAR_TOKEN}" },
                            },
                            "setup-custom": {
                                url: "https://example.com/mcp",
                                headers: { Authorization: "Bearer literal-setup-secret" },
                            },
                        },
                    }),
                )
                fs.writeFileSync(
                    path.join(root, ".piton/mcp.local.json"),
                    JSON.stringify({
                        mcpServers: {
                            "setup-github": {
                                url: "https://api.githubcopilot.com/mcp/",
                                headers: { Authorization: "Bearer ${LOCAL_SETUP_GITHUB_TOKEN}" },
                            },
                        },
                    }),
                )
                if (scenario.project !== undefined) {
                    fs.writeFileSync(path.join(root, ".piton/config.toml"), scenario.project)
                }
                if (scenario.name === "existing") {
                    fs.mkdirSync(path.join(root, ".piton/.local/gigs/example"), { recursive: true })
                    fs.writeFileSync(
                        path.join(root, ".piton/.local/gigs/example/metadata.json"),
                        JSON.stringify({
                            tracker: { provider: "github", external: { issueNumber: 2 } },
                        }),
                    )
                    git(root, ["add", ".piton/config.toml"])
                }
                const observedPaths = [
                    ".git/index",
                    ".git/info/exclude",
                    ".piton/mcp.json",
                    ".piton/mcp.local.json",
                    ".piton/config.toml",
                ]
                const fileContents = () =>
                    observedPaths.map((file) =>
                        fs.existsSync(path.join(root, file)) ? fs.readFileSync(path.join(root, file)) : null,
                    )
                const statusBefore = git(root, ["status", "--porcelain=v1"])
                const indexBefore = git(root, ["ls-files", "--stage"])
                const filesBefore = fileContents()
                registerProjectSetup(pi)
                const tool = tools.get("inspect_project_setup")
                assert.ok(tool)
                assert.ok(setup)
                assert.equal(compile(tool.parameters).check({}), true)
                assert.equal(compile(tool.parameters).check({ unexpected: true }), false)
                const inspected = await tool.execute("inspect", {}, undefined, undefined, {
                    cwd: root,
                    isProjectTrusted: () => true,
                } as never)
                assert.equal(sent, "", "inspection must not inject a user message")
                const snapshot = inspected.details as ReturnType<typeof inspectProjectSetup>
                const text = inspected.content.map((block) => (block.type === "text" ? block.text : "")).join("\n")
                assert.deepEqual(JSON.parse(text), snapshot)
                assert.equal(snapshot.repositoryRoot, fs.realpathSync(root))
                const projectFile = snapshot.files.find((file) => file.path === ".piton/config.toml")
                assert.equal(projectFile?.exists ? projectFile.content : undefined, scenario.project)
                // A broken table fails only its part, and a broken file every part it holds: MCP still resolves, and
                // setup sees which parts are broken.
                assert.equal(
                    snapshot.mcp.effectiveServers.find((server) => server.name === "setup-github")?.credentialEnv,
                    "LOCAL_SETUP_GITHUB_TOKEN",
                )
                assert.equal(
                    snapshot.mcp.effectiveServers.find((server) => server.name === "setup-linear")?.credentialEnv,
                    "SETUP_LINEAR_TOKEN",
                )
                const broken = ("errors" in snapshot.configuration && snapshot.configuration.errors) || []
                const expected = {
                    missing: [],
                    existing: [],
                    incompatible: ["branches"],
                    malformed: ["permissions", "web_search", "branches", "tracker", "forge"],
                }
                assert.deepEqual(
                    broken.map((error) => error.part),
                    expected[scenario.name as keyof typeof expected],
                )
                assert.doesNotMatch(text, /literal-setup-secret/)
                assert.doesNotMatch(JSON.stringify(snapshot), /literal-setup-secret/)
                if (scenario.name === "existing") {
                    assert.deepEqual(snapshot.boundEntityTrackers, [
                        {
                            path: ".piton/.local/gigs/example/metadata.json",
                            entity: "example",
                            provider: "github",
                            reference: "#2",
                        },
                    ])
                    assert.ok(snapshot.projectPathsInGit.includes(".piton/config.toml"))
                    assert.ok(snapshot.stagedProjectPaths.includes(".piton/config.toml"))
                }
                await setup.handler("Inspect existing setup", { cwd: root, isProjectTrusted: () => true })
                const prefix = "Read-only setup context:\n"
                const start = sent.indexOf(prefix) + prefix.length
                const end = sent.indexOf("\n\nUse the project-setup skill.", start)
                assert.ok(start >= prefix.length && end > start)
                assert.deepEqual(JSON.parse(sent.slice(start, end)), snapshot)
                assert.doesNotMatch(sent, /literal-setup-secret/)
                assert.deepEqual(fileContents(), filesBefore)
                assert.equal(git(root, ["ls-files", "--stage"]), indexBefore)
                assert.equal(git(root, ["status", "--porcelain=v1"]), statusBefore)
                assert.equal(fs.existsSync(path.join(root, ".piton/.local")), scenario.name === "existing")
            } finally {
                fs.rmSync(root, { recursive: true, force: true })
            }
        })
    }

    it("reports reusable official MCP credential environment references without exposing values", () => {
        const root = repository()
        try {
            fs.mkdirSync(path.join(root, ".piton"))
            fs.writeFileSync(
                path.join(root, ".piton", "mcp.json"),
                JSON.stringify({
                    mcpServers: {
                        linear: {
                            url: "https://mcp.linear.app/mcp",
                            headers: { Authorization: "Bearer ${LINEAR_SETUP_KEY}" },
                        },
                        custom: {
                            url: "https://example.com/mcp",
                            headers: { Authorization: "Bearer literal-secret" },
                        },
                    },
                }),
            )

            const inspected = inspectProjectMcp(root, true)
            assert.deepEqual(
                inspected.effectiveServers.find((server) => server.name === "linear"),
                {
                    name: "linear",
                    level: "repository",
                    path: path.join(fs.realpathSync(root), ".piton/mcp.json"),
                    provider: "linear",
                    credentialEnv: "LINEAR_SETUP_KEY",
                },
            )
            assert.deepEqual(
                inspected.effectiveServers.find((server) => server.name === "custom"),
                {
                    name: "custom",
                    level: "repository",
                    path: path.join(fs.realpathSync(root), ".piton/mcp.json"),
                },
            )
            assert.doesNotMatch(JSON.stringify(inspected), /literal-secret/)
            assert.equal(registeredCredentialEnvironment(root, "linear", "linear", true), "LINEAR_SETUP_KEY")
            assert.throws(
                () => registeredCredentialEnvironment(root, "custom", "linear", true),
                /not a recognized linear registration/,
            )
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    it("adds approved MCP registrations without credentials or index changes", async () => {
        const root = repository()
        const globalSettingsPath = path.join(root, "global-mcp.json")
        try {
            fs.mkdirSync(path.join(root, ".piton"))
            fs.writeFileSync(
                path.join(root, ".piton", "mcp.local.json"),
                `${JSON.stringify({ mcpServers: { existing: { command: "noop" } }, autoEnableCodemode: true }, null, 4)}\n`,
            )
            const indexBefore = git(root, ["ls-files", "--stage"])

            const configured = await configureProjectMcp(root, {
                target: "local",
                registrations: [
                    {
                        provider: "linear",
                        name: "linear-workflow",
                        credentialEnv: "LINEAR_WORKFLOW_TOKEN",
                    },
                    {
                        provider: "github",
                        name: "github-workflow",
                        credentialEnv: "GITHUB_WORKFLOW_TOKEN",
                    },
                ],
            })

            assert.deepEqual(configured.changed, ["local"])
            assert.equal(configured.restartRequired, true)
            assert.equal(git(root, ["ls-files", "--stage"]), indexBefore)
            assert.doesNotThrow(() => git(root, ["check-ignore", "-q", ".piton/mcp.local.json"]))
            assert.ok(
                fs
                    .readFileSync(path.join(root, ".git", "info", "exclude"), "utf-8")
                    .split(/\r?\n/)
                    .includes(LOCAL_SETTINGS_EXCLUDE),
            )

            const local = JSON.parse(fs.readFileSync(path.join(root, ".piton", "mcp.local.json"), "utf-8"))
            assert.equal(local.autoEnableCodemode, true)
            assert.deepEqual(local.mcpServers.existing, { command: "noop" })
            assert.deepEqual(local.mcpServers["linear-workflow"], {
                url: "https://mcp.linear.app/mcp",
                headers: { Authorization: "Bearer ${LINEAR_WORKFLOW_TOKEN}" },
            })

            assert.deepEqual(local.mcpServers["github-workflow"], {
                url: "https://api.githubcopilot.com/mcp/",
                headers: {
                    Authorization: "Bearer ${GITHUB_WORKFLOW_TOKEN}",
                    "X-MCP-Toolsets": "context,issues,projects,pull_requests",
                },
            })

            const committed = await configureProjectMcp(root, {
                target: "project",
                registrations: [
                    {
                        provider: "github",
                        name: "github-committed",
                        credentialEnv: "GITHUB_WORKFLOW_TOKEN",
                    },
                ],
            })
            assert.deepEqual(committed.changed, ["project"])
            const project = JSON.parse(fs.readFileSync(path.join(root, ".piton", "mcp.json"), "utf-8"))
            // Every GitHub registration gets every workflow toolset, so a forge-only server can still file workflow reports.
            assert.equal(
                project.mcpServers["github-committed"].headers["X-MCP-Toolsets"],
                "context,issues,projects,pull_requests",
            )
            await assert.rejects(
                async () =>
                    configureProjectMcp(root, {
                        target: "project",
                        registrations: [{ provider: "github", name: "gh-roles", credentialEnv: "T", roles: ["forge"] }],
                    }),
                /\/registrations\/0\/roles: unknown field/,
            )
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    it("adds approved permission gates without replacing settings or changing the Git index", async () => {
        const root = repository()
        try {
            const settingsPath = writeSettings(
                root,
                { permissions: { default_mode: "deny", allow: ["read"], ask: ["write(existing.md)"] } },
                true,
            )
            const indexBefore = git(root, ["ls-files", "--stage"])
            const rules = ["write(.piton/*/plan.md)", "edit(.piton/*/plan.md)"]

            const configured = await configureProjectPermissions(
                root,
                { target: "local", allow: [], ask: rules },
                new Map(),
            )

            assert.deepEqual(configured.added, { allow: [], ask: rules })
            assert.equal(configured.changed, true)
            assert.equal(configured.localExclusionChanged, true)
            assert.equal(git(root, ["ls-files", "--stage"]), indexBefore)
            assert.doesNotThrow(() => git(root, ["check-ignore", "-q", ".piton/config.local.toml"]))
            assert.ok(
                fs
                    .readFileSync(path.join(root, ".git", "info", "exclude"), "utf-8")
                    .split(/\r?\n/)
                    .includes(LOCAL_SETTINGS_EXCLUDE),
            )

            // Only the changed list is rewritten; the rest of the table stays as it was.
            const text = fs.readFileSync(settingsPath, "utf-8")
            assert.match(text, /default_mode = "deny"/)
            assert.match(text, /allow = \["read"\]/)
            clearSettingsCache()
            const local = inspectPersistentPermissions(root, "host").sources.find(
                (source) => source.sharing === "local",
            )!.settings
            assert.deepEqual(local.ask, ["write(existing.md)", ...rules])

            assert.deepEqual(
                await configureProjectPermissions(root, { target: "local", allow: [], ask: rules }, new Map()),
                {
                    target: "local",
                    file: ".piton/config.local.toml",
                    added: { allow: [], ask: [] },
                    skipped: { allow: [], ask: rules },
                    inspection: [],
                    changed: false,
                    localExclusionChanged: false,
                },
            )
            await assert.rejects(
                async () =>
                    configureProjectPermissions(
                        root,
                        { target: "project", allow: [], ask: ["mcp__github__issue_read"] },
                        new Map(),
                    ),
                /legacy mcp__ rules are not supported/,
            )
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    it("requires strict explicit configuration", () => {
        const root = repository()
        try {
            assert.throws(() => loadProjectConfig(root), /missing branches\.format/)
            const file = path.join(root, ".piton", "config.toml")
            const raw = (text: string) => {
                fs.mkdirSync(path.dirname(file), { recursive: true })
                fs.writeFileSync(file, text)
                clearSettingsCache()
            }
            raw('[branches]\nformat = "identifier-title"\nextra = true\n')
            assert.throws(() => loadProjectConfig(root), /config\.toml: \[branches\] unknown field `extra`/)
            raw('[branches]\nformat = "feature/identifier"\n')
            assert.throws(
                () => loadProjectConfig(root),
                /config\.toml: \[branches\] unknown variant `feature\/identifier`/,
            )
            raw('[branches.format]\nepic = "identifier"\n')
            assert.throws(() => loadProjectConfig(root), /config\.toml/)

            // The removed blocked-review setting is an unknown key like any other.
            raw("[reviews]\nuser_confirmation_after = 3\n")
            assert.throws(() => loadProjectConfig(root), /unknown key `reviews`/)
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    it("resolves branch username from strict local and global settings", () => {
        const root = repository()
        resetGlobalSettings()
        try {
            writeConfig(root, config({ format: "tracker" }))
            assert.equal(loadProjectConfig(root).branches.username, undefined)

            writeConfig(root, config({ format: "username/identifier-title" }))
            assert.throws(() => loadProjectConfig(root), /username is required/)

            writeSettings("global", { "branches.username": "global-user" }, true)
            assert.equal(loadProjectConfig(root).branches.username, "global-user")

            const localSettingsPath = writeSettings(root, { "branches.username": "alex" }, true)
            const configured = loadProjectConfig(root)
            assert.equal(configured.branches.format, "username/identifier-title")
            assert.equal(configured.branches.username, "alex")

            fs.writeFileSync(path.join(root, ".gitignore"), `${PROJECT_LOCAL_IGNORE}\n`)
            prepareArtifactPersistence(root)
            assert.ok(
                fs
                    .readFileSync(path.join(root, ".git", "info", "exclude"), "utf-8")
                    .split(/\r?\n/)
                    .includes(LOCAL_SETTINGS_EXCLUDE),
            )
            assert.doesNotMatch(fs.readFileSync(path.join(root, ".piton", "config.toml"), "utf-8"), /alex/)

            fs.writeFileSync(localSettingsPath, '[branches]\nusername = "alex"\nextra = true\n')
            clearSettingsCache()
            assert.throws(() => loadProjectConfig(root), /unknown field `extra`/)
        } finally {
            resetGlobalSettings()
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    it("requires the project-local gitignore rule without changing the index", () => {
        const root = repository()
        try {
            writeConfig(root, config())
            const indexBefore = git(root, ["ls-files", "--stage"])
            assert.throws(
                () => prepareArtifactPersistence(root),
                /\/\.piton\/\.local \(no trailing slash\) in \.gitignore/,
            )
            assert.throws(
                () => assertArtifactPersistencePrepared(root),
                /\/\.piton\/\.local \(no trailing slash\) in \.gitignore/,
            )
            assert.equal(git(root, ["ls-files", "--stage"]), indexBefore)

            fs.writeFileSync(path.join(root, ".gitignore"), `${PROJECT_LOCAL_IGNORE}\n`)
            assert.equal(prepareArtifactPersistence(root).state, "ready")
            assertArtifactPersistencePrepared(root)
            assert.equal(git(root, ["ls-files", "--stage"]), indexBefore)
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    it("accepts the committed ignore rule for a missing, real, or linked local root", () => {
        type Shape = "missing" | "directory" | "symlink"
        // A trailing-slash rule matches only a real directory, so a worker's symlink would be left unignored.
        const cases: { rule: string; shape: Shape; ok: boolean }[] = [
            { rule: `${PROJECT_LOCAL_IGNORE}/`, shape: "symlink", ok: false },
            { rule: PROJECT_LOCAL_IGNORE, shape: "missing", ok: true },
            { rule: PROJECT_LOCAL_IGNORE, shape: "directory", ok: true },
            { rule: PROJECT_LOCAL_IGNORE, shape: "symlink", ok: true },
        ]
        for (const { rule, shape, ok } of cases) {
            const root = repository()
            const source = fs.mkdtempSync(path.join(os.tmpdir(), "workflow-local-source-"))
            try {
                writeConfig(root, config())
                fs.writeFileSync(path.join(root, ".gitignore"), `${rule}\n`)
                const local = path.join(root, ".piton", ".local")
                if (shape === "directory") fs.mkdirSync(local)
                if (shape === "symlink") fs.symlinkSync(source, local)
                if (ok) assert.equal(prepareArtifactPersistence(root).state, "ready", `${rule} ${shape}`)
                else assert.throws(() => prepareArtifactPersistence(root), /no trailing slash/, `${rule} ${shape}`)
            } finally {
                fs.rmSync(root, { recursive: true, force: true })
                fs.rmSync(source, { recursive: true, force: true })
            }
        }
    })

    it("accepts a private setup's exclude but not the global excludes file or a negation", () => {
        const root = repository()
        try {
            writeConfig(root, config())
            fs.writeFileSync(path.join(root, "global-ignore"), `${PROJECT_LOCAL_IGNORE}\n`)
            git(root, ["config", "core.excludesFile", path.join(root, "global-ignore")])
            assert.throws(() => prepareArtifactPersistence(root), /in \.gitignore/)
            const exclude = path.join(root, ".git", "info", "exclude")
            const before = fs.readFileSync(exclude, "utf-8")
            fs.appendFileSync(exclude, `${PROJECT_LOCAL_IGNORE}\n`)
            fs.writeFileSync(path.join(root, ".gitignore"), `!${PROJECT_LOCAL_IGNORE}\n`)
            assert.throws(() => prepareArtifactPersistence(root), /in \.gitignore/)
            fs.rmSync(path.join(root, ".gitignore"))
            fs.writeFileSync(exclude, `${before}/.piton\n`)
            assert.equal(prepareArtifactPersistence(root).state, "ready")
            assert.equal(git(root, ["status", "--porcelain", "--untracked-files=all", "--", ".piton"]), "")
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    it("revalidates when the local root changes from a directory to a symlink", () => {
        const root = repository()
        const source = fs.mkdtempSync(path.join(os.tmpdir(), "workflow-local-source-"))
        try {
            writeConfig(root, config())
            fs.writeFileSync(path.join(root, ".gitignore"), `${PROJECT_LOCAL_IGNORE}/\n`)
            const local = path.join(root, ".piton", ".local")
            fs.mkdirSync(local)
            assertArtifactPersistencePrepared(root)
            fs.rmSync(local, { recursive: true })
            fs.symlinkSync(source, local)
            assert.throws(() => assertArtifactPersistencePrepared(root), /no trailing slash/)
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
            fs.rmSync(source, { recursive: true, force: true })
        }
    })

    it("creates a missing project-local folder once the ignore rule is in place", () => {
        const root = repository()
        try {
            writeConfig(root, config())
            const local = path.join(root, ".piton", ".local")
            assert.throws(() => prepareArtifactPersistence(root), /in \.gitignore/)
            assert.equal(fs.existsSync(local), false)
            fs.writeFileSync(path.join(root, ".gitignore"), `${PROJECT_LOCAL_IGNORE}\n`)
            prepareArtifactPersistence(root)
            assert.ok(fs.statSync(local).isDirectory())
            assert.equal(git(root, ["status", "--porcelain", "--untracked-files=all", "--", ".piton/.local"]), "")
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    for (const runtime of ["host", "sandbox"] as const)
        for (const setup of ["gitignore", "private"] as const)
            it(`keeps a real project-local folder in a source checkout (${runtime} runtime, ${setup} setup)`, () => {
                const root = repository()
                const previous = process.env.PITON_RUNTIME
                try {
                    if (runtime === "sandbox") process.env.PITON_RUNTIME = "sandbox"
                    else delete process.env.PITON_RUNTIME
                    writeConfig(root, config())
                    if (setup === "gitignore")
                        fs.writeFileSync(path.join(root, ".gitignore"), `${PROJECT_LOCAL_IGNORE}\n`)
                    else fs.appendFileSync(path.join(root, ".git", "info", "exclude"), "/.piton\n")
                    const local = path.join(root, ".piton", ".local")
                    assertWorkerArtifactsLinked(root)
                    assert.equal(prepareArtifactPersistence(root).state, "ready")
                    assert.ok(fs.lstatSync(local).isDirectory())
                    assertWorkerArtifactsLinked(root)
                    assertArtifactPersistencePrepared(root)
                } finally {
                    if (previous === undefined) delete process.env.PITON_RUNTIME
                    else process.env.PITON_RUNTIME = previous
                    fs.rmSync(root, { recursive: true, force: true })
                }
            })

    for (const runtime of ["host", "sandbox"] as const)
        it(`requires a worker's project-local folder to link to its source's (${runtime} runtime)`, () => {
            const root = fs.realpathSync(repository())
            const worker = `${root}--w`
            const elsewhere = fs.mkdtempSync(path.join(os.tmpdir(), "workflow-local-elsewhere-"))
            const previous = process.env.PITON_RUNTIME
            try {
                if (runtime === "sandbox") process.env.PITON_RUNTIME = "sandbox"
                else delete process.env.PITON_RUNTIME
                writeConfig(root, config())
                fs.writeFileSync(path.join(root, ".gitignore"), `${PROJECT_LOCAL_IGNORE}\n`)
                git(root, ["add", ".gitignore", ".piton/config.toml"])
                git(root, ["commit", "-m", "config"])
                prepareArtifactPersistence(root)
                git(path.dirname(root), ["clone", "-q", root, worker])
                git(worker, ["config", "--local", "agent.source", root])
                clearSettingsCache()
                const local = path.join(worker, ".piton", ".local")
                const unlinked = /isn't linked to its source checkout's/

                // Missing: setup refuses rather than creating a folder the worker would lose.
                assert.throws(() => assertWorkerArtifactsLinked(worker), unlinked)
                assert.throws(() => prepareArtifactPersistence(worker), unlinked)
                assert.equal(fs.existsSync(local), false)

                fs.mkdirSync(local)
                assert.throws(() => assertWorkerArtifactsLinked(worker), unlinked)
                assert.throws(() => prepareArtifactPersistence(worker), unlinked)
                fs.rmSync(local, { recursive: true })
                for (const target of [path.join(root, "missing"), elsewhere]) {
                    fs.symlinkSync(target, local)
                    assert.throws(() => assertWorkerArtifactsLinked(worker), unlinked)
                    fs.rmSync(local)
                }

                fs.symlinkSync(path.join(root, ".piton", ".local"), local)
                assertWorkerArtifactsLinked(worker)
                assert.equal(prepareArtifactPersistence(worker).state, "ready")
            } finally {
                if (previous === undefined) delete process.env.PITON_RUNTIME
                else process.env.PITON_RUNTIME = previous
                fs.rmSync(root, { recursive: true, force: true })
                fs.rmSync(worker, { recursive: true, force: true })
                fs.rmSync(elsewhere, { recursive: true, force: true })
            }
        })

    it("refuses project setup in a worker clone, whose files piton never reads", async () => {
        const root = repository()
        const worker = `${root}--w`
        try {
            git(path.dirname(root), ["clone", "-q", root, worker])
            git(worker, ["config", "--local", "agent.source", root])
            const refusal = /run it in .*workflow-project-config-/
            assert.throws(() => inspectProjectSetup(worker, true), refusal)
            await assert.rejects(
                async () =>
                    configureProjectPermissions(worker, { target: "local", allow: ["read"], ask: [] }, new Map()),
                refusal,
            )
            await assert.rejects(
                async () =>
                    configureProjectMcp(worker, {
                        target: "local",
                        registrations: [{ name: "linear", provider: "linear", credentialEnv: "LINEAR_API_KEY" }],
                    }),
                refusal,
            )
            assert.equal(fs.existsSync(path.join(root, ".piton")), false)
            assert.equal(fs.existsSync(path.join(worker, ".piton")), false)
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
            fs.rmSync(worker, { recursive: true, force: true })
        }
    })

    it("commits project configuration while ignoring every entity artifact", () => {
        const root = repository()
        try {
            writeConfig(root, config())
            fs.writeFileSync(path.join(root, ".gitignore"), `${PROJECT_LOCAL_IGNORE}\n`)
            const gig = path.join(root, ".piton", ".local", "gigs", "example")
            fs.mkdirSync(gig, { recursive: true })
            fs.writeFileSync(path.join(gig, "metadata.json"), "{}\n")
            prepareArtifactPersistence(root)
            git(root, ["add", ".gitignore", ".piton"])
            assert.deepEqual(git(root, ["diff", "--cached", "--name-only"]).split("\n"), [
                ".gitignore",
                ".piton/config.toml",
            ])
            assertArtifactPersistencePrepared(root)
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    it("asserts a missing local-settings exclusion without writing and prepares it once", () => {
        const root = repository()
        try {
            writeConfig(root, config({ format: "username/identifier-title" }))
            fs.writeFileSync(path.join(root, ".gitignore"), `${PROJECT_LOCAL_IGNORE}\n`)
            writeSettings(root, { "branches.username": "alex" }, true)
            const exclude = path.join(root, ".git", "info", "exclude")
            const before = fs.readFileSync(exclude, "utf-8")
            assert.throws(() => assertArtifactPersistencePrepared(root), /not prepared/)
            assert.equal(fs.readFileSync(exclude, "utf-8"), before)
            prepareArtifactPersistence(root)
            prepareArtifactPersistence(root)
            assert.equal(
                fs
                    .readFileSync(exclude, "utf-8")
                    .split(/\r?\n/)
                    .filter((line) => line === LOCAL_SETTINGS_EXCLUDE).length,
                1,
            )
            assert.equal(assertArtifactPersistencePrepared(root).branches.username, "alex")
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    it("rejects project-local artifacts in Git, including staged deletions, without changing the index", () => {
        const root = repository()
        try {
            writeConfig(root, config())
            fs.writeFileSync(path.join(root, ".gitignore"), `${PROJECT_LOCAL_IGNORE}\n`)
            const gig = path.join(root, ".piton", ".local", "gigs", "example")
            fs.mkdirSync(gig, { recursive: true })
            fs.writeFileSync(path.join(gig, "notes.md"), "# Notes\n")
            git(root, ["add", "-f", ".piton/.local/gigs/example/notes.md"])
            const indexBefore = git(root, ["ls-files", "--stage"])
            assert.throws(() => prepareArtifactPersistence(root), /\.piton\/\.local\/ must remain outside Git/)
            assert.equal(git(root, ["ls-files", "--stage"]), indexBefore)

            git(root, ["commit", "-m", "tracked local artifact"])
            git(root, ["rm", "--cached", ".piton/.local/gigs/example/notes.md"])
            const stagedBefore = git(root, ["diff", "--cached", "--raw"])
            assert.throws(() => prepareArtifactPersistence(root), /must remain outside Git/)
            assert.equal(git(root, ["diff", "--cached", "--raw"]), stagedBefore)
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })
})
