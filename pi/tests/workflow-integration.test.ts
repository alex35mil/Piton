import { resetGlobalSettings, TEST_CONFIG_HOME, writeConfig } from "./host-runtime.ts"
import { writeInvalidIntegrations, writeProjectConfig } from "./config-fixture.ts"
import { clearSettingsCache } from "../extensions/__lib/settings.ts"

import assert from "node:assert/strict"
import * as cp from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { describe, it, type TestContext } from "node:test"

import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent"
import {
    githubPhaseReads,
    kindLabelNames,
    kindLabels,
    linearKindGroup,
    linearPhases,
    phaseValues,
} from "./workflow-phase-fixtures.ts"
import { withToolExecution } from "./extension-context-fixture.ts"

import {
    assertConfiguredPriority,
    integrationPresence,
    loadIntegrationRoles,
    requireTracker,
    RoleUnavailableError,
} from "../workflows/extensions/integrations/config.ts"
import { prepareArtifactPersistence, setInstallationForTests } from "../workflows/extensions/__lib/project-config.ts"
import registerIntegrations, {
    buildIntegrationContext,
    IntegrationContextValidator,
    renderEntityProviderBody,
} from "../workflows/extensions/integrations/index.ts"

import { verifyMarkdownProjection } from "../workflows/extensions/integrations/projection.ts"
import { initializeEntity, initializeLocalTask } from "../workflows/extensions/init/entity.ts"
import { linearPriorityNumber } from "../workflows/extensions/integrations/records.ts"

import { workflowMcpCatalog } from "./mcp-catalog-fixture.ts"

const mcpTools = workflowMcpCatalog("github", "linear", "workflow-github", "workflow-linear")

/** The configured tracker, for validation tests: it must be on. */
function loadTracker(cwd: string) {
    return requireTracker(loadIntegrationRoles(cwd), "test")
}

const statuses = {
    backlog: "Backlog",
    todo: "Todo",
    inProgress: "In Progress",
    inReview: "In Review",
    done: "Done",
    canceled: "Canceled",
}

function config(scope: "project" | "issue" = "project", withKindLabels = true) {
    return {
        provider: "github" as const,
        mcpServer: "github",
        repository: { owner: "alex", repo: "example" },
        project: {
            owner: "alex",
            ownerType: scope === "project" ? ("user" as const) : ("org" as const),
            number: 3,
        },
        ...(withKindLabels ? { labels: kindLabels() } : {}),
        fields: {
            status: { field: "Status", values: { ...statuses } },
            phase: { field: "Phase", values: { ...phaseValues } },
            priority: {
                scope,
                field: "Priority",
                values: ["Urgent", "High", "Medium", "Low"],
            },
        },
    }
}

function linearConfig() {
    return {
        provider: "linear" as const,
        mcpServer: "linear",
        team: "Engineering",
        phase: linearPhases(),
        statuses: {
            issues: { ...statuses },
            projects: { ...statuses },
        },
    }
}

function forgeConfig() {
    return {
        provider: "github" as const,
        mcpServer: "github",
        repository: { owner: "alex", repo: "example" },
    }
}

/** A tracker config as every context but `inspect` returns it: without the label catalog. */
function withoutLabels<T extends object>(tracker: T): Omit<T, "labels"> {
    const { labels: _labels, ...board } = tracker as T & { labels?: unknown }
    return board
}

const queueSelection = {
    operation: "queueIntake" as const,
    target: "current" as const,
    entity: "task" as const,
    priority: "High",
    kind: "refactor" as const,
}

function temporaryProject(): string {
    return fs.mkdtempSync(path.join(os.tmpdir(), "workflow-integration-"))
}

function prepareWorkflowProject(root: string, tracker: unknown = config()): void {
    cp.execFileSync("git", ["init", "-b", "main"], { cwd: root })
    cp.execFileSync("git", ["config", "user.email", "test@example.com"], {
        cwd: root,
    })
    cp.execFileSync("git", ["config", "user.name", "Test"], { cwd: root })
    fs.writeFileSync(path.join(root, "README.md"), "test\n")
    fs.writeFileSync(path.join(root, ".gitignore"), "/.piton/.local\n")
    cp.execFileSync("git", ["add", "README.md", ".gitignore"], { cwd: root })
    cp.execFileSync("git", ["commit", "-m", "initial"], { cwd: root })
    writeProjectConfig(root, { branchFormat: "identifier-title", tracker, forge: forgeConfig() })
}

function writeEntity(cwd: string, entity: "epic" | "task", name: string, tracker: unknown, slug: string): string {
    const directory = path.join(".piton", ".local", entity === "epic" ? "epics" : "tasks", name)
    fs.mkdirSync(path.join(cwd, directory), { recursive: true })
    const metadata = {
        slug,
        title: slug,
        entity,
        ...(entity === "epic" ? {} : { kind: "feature" }),
        branch: {
            state: "ready",
            name: name.toLowerCase(),
            start: "main",
            target: "main",
            source: "generated",
        },
        tracker,
    }
    fs.writeFileSync(path.join(cwd, directory, "metadata.json"), `${JSON.stringify(metadata, null, 2)}\n`)
    return directory
}

function writeGitHubTaskEntity(cwd: string): string {
    return writeEntity(
        cwd,
        "task",
        "42-github-provider",
        {
            provider: "github",
            repository: { owner: "alex", repo: "example" },
            external: { issueNumber: 42 },
            planComment: { state: "unpublished" },
        },
        "github-provider",
    )
}

function writeLinearTaskEntity(cwd: string): string {
    return writeEntity(
        cwd,
        "task",
        "ENG-123-mixed-provider",
        {
            provider: "linear",
            resource: "issue",
            external: { issueId: "linear-uuid", identifier: "ENG-123" },
            parent: { kind: "none" },
            planDocument: { state: "unpublished" },
        },
        "mixed-provider",
    )
}

function workflowFixture(t: TestContext) {
    const root = fs.realpathSync(temporaryProject())
    const source = path.join(root, "source")
    const consumer = path.join(root, "consumer")
    const agent = path.join(root, "agent")
    for (const directory of [source, consumer, agent]) fs.mkdirSync(directory)
    const previous = process.env.PI_CODING_AGENT_DIR
    process.env.PI_CODING_AGENT_DIR = agent
    t.after(() => {
        if (previous === undefined) delete process.env.PI_CODING_AGENT_DIR
        else process.env.PI_CODING_AGENT_DIR = previous
        setInstallationForTests(undefined)
        resetGlobalSettings()
        fs.rmSync(root, { recursive: true, force: true })
    })
    // A development install: the pi package runs from the source checkout, which takes workflow reports.
    setInstallationForTests({ kind: "development", checkout: source })
    return {
        root,
        source,
        consumer,
        ctx: { cwd: consumer } as ExtensionContext,
    }
}

function fileSnapshot(root: string): [string, string][] {
    return fs
        .readdirSync(root, { withFileTypes: true })
        .sort((a, b) => a.name.localeCompare(b.name))
        .flatMap((entry) => {
            const file = path.join(root, entry.name)
            return entry.isDirectory() ? fileSnapshot(file) : [[file, fs.readFileSync(file).toString("hex")]]
        })
}

describe("workflow-source queue context", () => {
    it("requires an explicit target only on project inspection and queue intake", async () => {
        for (const target of ["current", "workflow"]) {
            assert.equal(IntegrationContextValidator.check({ operation: "inspect", target }), true)
            assert.equal(IntegrationContextValidator.check({ ...queueSelection, target }), true)
            assert.equal(
                IntegrationContextValidator.check({
                    operation: "resume",
                    entityDir: "task",
                    target,
                }),
                false,
            )
            assert.equal(
                IntegrationContextValidator.check({
                    operation: "adopt",
                    entity: "task",
                    target,
                }),
                false,
            )
        }
        assert.equal(IntegrationContextValidator.check({ operation: "inspect" }), false)
        const { target, ...withoutTarget } = queueSelection
        assert.equal(IntegrationContextValidator.check(withoutTarget), false)
        assert.equal(
            IntegrationContextValidator.check({
                ...queueSelection,
                target: "other",
            }),
            false,
        )
        assert.equal(
            IntegrationContextValidator.check({
                ...queueSelection,
                kind: "review",
            }),
            false,
        )
        // A queued Task is standalone; accepted Epic plans create every Epic Task issue.
        assert.equal(IntegrationContextValidator.check({ ...queueSelection, entity: "gig" }), false)
        assert.equal(IntegrationContextValidator.check({ ...queueSelection, entity: "epic" }), true)
    })

    for (const provider of ["github", "linear"] as const) {
        for (const consumingSetup of ["missing", "different", "malformed"] as const) {
            it(`uses ${provider} source configuration with ${consumingSetup} consuming setup`, (t) => {
                const f = workflowFixture(t)
                const tracker =
                    provider === "github"
                        ? {
                              ...config(),
                              mcpServer: "workflow-github",
                              repository: { owner: "maintainer", repo: "workflows" },
                          }
                        : {
                              ...linearConfig(),
                              mcpServer: "workflow-linear",
                              team: "Workflow Team",
                          }
                prepareWorkflowProject(f.source, tracker)
                prepareArtifactPersistence(f.source)
                if (consumingSetup !== "missing") {
                    prepareWorkflowProject(f.consumer)
                    if (consumingSetup === "malformed")
                        fs.writeFileSync(path.join(f.consumer, ".piton/config.toml"), "[broken")
                }
                const before = fileSnapshot(f.root)
                const inspected = buildIntegrationContext(f.ctx, { operation: "inspect", target: "workflow" }, mcpTools)
                const intake = buildIntegrationContext(f.ctx, { ...queueSelection, target: "workflow" }, mcpTools)
                for (const [context, config] of [
                    [inspected, tracker],
                    [intake, withoutLabels(tracker)],
                ] as const) {
                    assert.equal(context.target, "workflow")
                    assert.equal(context.projectRoot, f.source)
                    assert.deepEqual(context.roles.tracker, { state: "enabled", provider, config })
                }
                assert.deepEqual(fileSnapshot(f.root), before, "context must not modify either project or settings")
                if (consumingSetup === "different") {
                    prepareArtifactPersistence(f.consumer)
                    const current = buildIntegrationContext(f.ctx, { ...queueSelection, target: "current" }, mcpTools)
                    assert.deepEqual(current.roles.tracker, {
                        state: "enabled",
                        provider: "github",
                        config: withoutLabels(config()),
                    })
                }
                const updated =
                    provider === "github"
                        ? {
                              ...tracker,
                              repository: { owner: "maintainer", repo: "new-inbox" },
                          }
                        : { ...tracker, team: "New Workflow Team" }
                writeProjectConfig(f.source, { tracker: updated })
                const reread = buildIntegrationContext(f.ctx, { ...queueSelection, target: "workflow" }, mcpTools)
                assert.deepEqual(reread.roles.tracker?.config, withoutLabels(updated))
            })
        }
    }

    it("refuses the workflow source in an app install without requiring it for current intake", (t) => {
        const f = workflowFixture(t)
        prepareWorkflowProject(f.consumer)
        prepareArtifactPersistence(f.consumer)
        setInstallationForTests({ kind: "app" })
        const before = fileSnapshot(f.root)
        assert.throws(
            () => buildIntegrationContext(f.ctx, { operation: "inspect", target: "workflow" }, mcpTools),
            /development install[\s\S]*issues\/new/,
        )
        assert.equal(buildIntegrationContext(f.ctx, queueSelection, mcpTools).state, "enabled")
        assert.deepEqual(fileSnapshot(f.root), before)
    })

    it("inspects and queues from a worker without guarding artifact-free operations", (t) => {
        const f = workflowFixture(t)
        // The consuming project's source checkout owns a real .piton/.local; its worker links to it.
        prepareWorkflowProject(f.source)
        prepareArtifactPersistence(f.source)
        prepareWorkflowProject(f.consumer)
        prepareArtifactPersistence(f.consumer)
        const worker = path.join(`${f.consumer}.workers`, "w")
        fs.mkdirSync(path.dirname(worker))
        cp.execFileSync("git", ["clone", "-q", f.consumer, worker])
        cp.execFileSync("git", ["config", "--local", "agent.source", f.consumer], { cwd: worker })
        // Piton reads .piton/ from the source; the worker's clone has only the link.
        fs.mkdirSync(path.join(worker, ".piton"), { recursive: true })
        fs.symlinkSync(path.join(f.consumer, ".piton/.local"), path.join(worker, ".piton/.local"))
        clearSettingsCache()
        const ctx = { cwd: worker } as ExtensionContext

        const before = fileSnapshot(f.source)
        for (const input of [
            { operation: "inspect" as const, target: "workflow" as const },
            { ...queueSelection, target: "workflow" as const },
        ]) {
            const context = buildIntegrationContext(ctx, input, mcpTools)
            assert.equal(context.projectRoot, f.source)
            assert.equal(context.roles.tracker?.provider, "github")
        }
        assert.deepEqual(fileSnapshot(f.source), before)
        assert.equal(buildIntegrationContext(ctx, { operation: "adopt", entity: "task" }, mcpTools).state, "enabled")

        // The source checkout keeps its real folder and addresses entities.
        assert.equal(buildIntegrationContext(f.ctx, { operation: "adopt", entity: "task" }, mcpTools).state, "enabled")

        // An unlinked worker still inspects and queues, but can't address an entity.
        fs.rmSync(path.join(worker, ".piton/.local"))
        fs.mkdirSync(path.join(worker, ".piton/.local"))
        for (const input of [{ operation: "inspect" as const, target: "current" as const }, queueSelection])
            assert.equal(buildIntegrationContext(ctx, input, mcpTools).state, "enabled")
        for (const input of [
            { operation: "adopt" as const, entity: "task" as const },
            { operation: "resume" as const, entityDir: ".piton/.local/tasks/1-work" },
        ])
            assert.throws(() => buildIntegrationContext(ctx, input, mcpTools), /isn't linked to its source checkout/)
    })

    it("names source config errors without falling back or preparing artifacts", (t) => {
        const f = workflowFixture(t)
        prepareWorkflowProject(f.source)
        const file = path.join(f.source, ".piton/config.toml")
        writeProjectConfig(f.source, { tracker: config(), forge: forgeConfig() })
        fs.appendFileSync(file, "[tracker\n")
        const broken = fileSnapshot(f.root)
        assert.throws(
            () => buildIntegrationContext(f.ctx, { operation: "inspect", target: "workflow" }, mcpTools),
            (error: Error) => error.message.includes(f.source) && error.message.includes("config.toml"),
        )
        assert.deepEqual(fileSnapshot(f.root), broken)
        // A missing role isn't an error: inspection reports it off, and a step that needs it refuses.
        fs.writeFileSync(file, "")
        writeProjectConfig(f.source, { branchFormat: "identifier-title", forge: forgeConfig() })
        prepareArtifactPersistence(f.source)
        const inspected = buildIntegrationContext(f.ctx, { operation: "inspect", target: "workflow" }, mcpTools)
        assert.equal(inspected.roles.tracker?.state, "disabled")
        assert.equal(inspected.roles.forge?.state, "enabled")
        assert.throws(
            () => buildIntegrationContext(f.ctx, { ...queueSelection, target: "workflow" }, mcpTools),
            (error: Error) => error.message.includes(f.source) && /needs the tracker/.test(error.message),
        )
        fs.writeFileSync(file, "")
        writeProjectConfig(f.source, {
            branchFormat: "identifier-title",
            tracker: config(),
            forge: forgeConfig(),
            username: "alex",
        })
        const unprepared = fileSnapshot(f.root)
        assert.throws(
            () => buildIntegrationContext(f.ctx, { ...queueSelection, target: "workflow" }, mcpTools),
            /not prepared/,
        )
        assert.deepEqual(fileSnapshot(f.root), unprepared)
        prepareArtifactPersistence(f.source)
        fs.appendFileSync(file, "[branches.oops\n")
        clearSettingsCache()
        assert.throws(
            () => buildIntegrationContext(f.ctx, { operation: "inspect", target: "workflow" }, mcpTools),
            (error: Error) => error.message.includes(f.source) && error.message.includes("config.toml"),
        )
        fs.writeFileSync(file, "")
        writeProjectConfig(f.source, { tracker: config(), forge: forgeConfig() })
        assert.throws(
            () => buildIntegrationContext(f.ctx, { operation: "inspect", target: "workflow" }, mcpTools),
            /missing branches\.format/,
        )
    })

    it("exposes source selection through the existing registered context tool without remote calls", async (t) => {
        const f = workflowFixture(t)
        prepareWorkflowProject(f.source)
        prepareArtifactPersistence(f.source)
        type Tool = Parameters<ExtensionAPI["registerTool"]>[0]
        const tools = new Map<string, Tool>()
        registerIntegrations({
            events: { on: () => () => {} },
            on: () => {},
            registerCommand: () => {},
            registerTool: (tool: Tool) => tools.set(tool.name, tool),
        } as unknown as ExtensionAPI)
        const before = fileSnapshot(f.root)
        const result = await tools
            .get("inspect_integrations")!
            .execute("inspect", { target: "workflow" }, undefined, undefined, withToolExecution(f.ctx))
        assert.equal((result.details as { projectRoot: string }).projectRoot, f.source)
        assert.deepEqual(fileSnapshot(f.root), before)
    })

    it("reports session analysis: on in a development install unless turned off, off in an app install", async (t) => {
        const f = workflowFixture(t)
        type Tool = Parameters<ExtensionAPI["registerTool"]>[0]
        const tools = new Map<string, Tool>()
        registerIntegrations({
            events: { on: () => () => {} },
            on: () => {},
            registerCommand: () => {},
            registerTool: (tool: Tool) => tools.set(tool.name, tool),
        } as unknown as ExtensionAPI)
        const inspect = async () =>
            (
                await tools
                    .get("inspect_session_analysis")!
                    .execute("inspect", {}, undefined, undefined, withToolExecution(f.ctx))
            ).details
        const before = fileSnapshot(f.root)
        assert.deepEqual(await inspect(), { sessionAnalysis: "enabled" })
        assert.deepEqual(fileSnapshot(f.root), before)
        writeConfig("host", { "debugging.session_analysis": false })
        assert.deepEqual(await inspect(), { sessionAnalysis: "disabled" })
        setInstallationForTests({ kind: "app" })
        writeConfig("host", { "debugging.session_analysis": true })
        assert.deepEqual(await inspect(), { sessionAnalysis: "disabled" })
        // A broken setting is reported, not read as off.
        setInstallationForTests({ kind: "development", checkout: f.source })
        fs.writeFileSync(path.join(TEST_CONFIG_HOME, "config.host.toml"), "[debugging]\nsession_analysis = 1\n")
        clearSettingsCache()
        await assert.rejects(inspect, /session_analysis/)
    })
})

describe("workflow integration configuration", () => {
    it("requires exact provider Markdown and returns a concise remediation hint", async () => {
        assert.deepEqual(verifyMarkdownProjection("same\n", "same\n"), {
            result: "exact",
            verified: true,
            requiresApproval: false,
        })

        const difference = verifyMarkdownProjection(
            "- first\r\nVisit https://example.com\r\n",
            "* first\nVisit <https://example.com>\n",
        )
        assert.equal(difference.result, "different")
        assert.equal(difference.verified, false)
        assert.equal(difference.requiresApproval, true)
        assert.equal(
            difference.guidance,
            "Review the exact diff. If this is harmless formatting and byte-for-byte comparison is annoying, consider using a Markdown parser and normalized comparison.",
        )
    })

    it("keeps Backlog and Todo command intake external-only", async () => {
        const cwd = temporaryProject()
        let sent = ""
        const notices: string[] = []
        type Command = {
            description: string
            handler: (args: string, ctx: unknown) => Promise<void>
        }
        const commands = new Map<string, Command>()
        const tools: string[] = []
        const pi = {
            events: { on: () => () => {} },
            on: () => {},
            registerCommand: (name: string, command: Command) => commands.set(name, command),
            registerTool: (tool: { name: string }) => tools.push(tool.name),
            sendUserMessage: (message: string) => {
                sent = message
            },
        } as unknown as ExtensionAPI
        try {
            registerIntegrations(pi)
            assert.ok(commands.has("backlog"))
            assert.ok(commands.has("todo"))
            for (const tool of [
                "verify_artifact_projection",
                "render_provider_body",
                "prepare_tracker_publication",
                "record_tracker_publication",
                "recover_tracker_publication",
            ])
                assert.ok(tools.includes(tool), tool)
            assert.equal(tools.includes("finalize_linear_branch"), false)
            assert.equal(
                commands.get("backlog")!.description,
                "Add potential work to the configured external Backlog without initializing workflow work",
            )
            assert.equal(
                commands.get("todo")!.description,
                "Add queued work to the configured external Todo queue without initializing workflow work",
            )
            const context = {
                cwd,
                ui: { notify: (message: string) => notices.push(message) },
            }
            await commands.get("backlog")!.handler("", context)
            await commands.get("todo")!.handler("", context)
            assert.deepEqual(notices, ["Usage: /backlog <potential work>", "Usage: /todo <queued work>"])
            await commands.get("backlog")!.handler("Capture a potential idea", context)
            assert.match(sent, /Use the backlog skill.*external Backlog intake, not Epic or Task initialization/)
            await commands.get("todo")!.handler("Queue committed work", context)
            assert.match(sent, /Use the todo skill.*external Todo intake, not Epic or Task initialization/)
            assert.equal(fs.existsSync(path.join(cwd, ".piton")), false)
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("loads each integration role as on or off", async () => {
        const cwd = temporaryProject()
        try {
            assert.deepEqual(loadIntegrationRoles(cwd), {
                tracker: { state: "off", reason: "absent" },
                forge: { state: "off", reason: "absent" },
            })
            writeProjectConfig(cwd, { forge: forgeConfig() })
            const forgeOnly = loadIntegrationRoles(cwd)
            assert.deepEqual(forgeOnly.forge, { state: "on", config: forgeConfig() })
            assert.throws(
                () => requireTracker(forgeOnly, "plan publication"),
                (error: unknown) =>
                    error instanceof RoleUnavailableError &&
                    error.role === "tracker" &&
                    error.off.reason === "absent" &&
                    /plan publication/.test(error.message),
            )
            writeProjectConfig(cwd, { tracker: config(), forge: { provider: "none" } })
            const trackerOnly = loadIntegrationRoles(cwd)
            assert.deepEqual(trackerOnly.tracker, { state: "on", config: config() })
            assert.equal(trackerOnly.forge.state, "off")
            assert.ok(trackerOnly.forge.state === "off" && trackerOnly.forge.reason === "disabled")
            // Both roles off: still configured, so the workflow rules stay in the system prompt.
            writeProjectConfig(cwd, { tracker: { provider: "none" }, forge: { provider: "none" } })
            assert.deepEqual(integrationPresence(cwd), { kind: "configured" })
            writeInvalidIntegrations(cwd, { tracker: { provider: "none", mcpServer: "github" }, forge: forgeConfig() })
            assert.throws(
                () => loadIntegrationRoles(cwd),
                /invalid .*\.piton\/config\.toml: line \d+, tracker\.mcp_server/,
            )
            writeInvalidIntegrations(cwd, { tracker: { provider: "jira" } })
            assert.throws(() => loadIntegrationRoles(cwd), /tracker\.provider: unknown provider "jira"/)
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("rejects an entity bound to another tracker provider", async () => {
        const cwd = temporaryProject()
        try {
            prepareWorkflowProject(cwd)
            prepareArtifactPersistence(cwd)
            const entityDir = writeLinearTaskEntity(cwd)
            assert.throws(
                () =>
                    buildIntegrationContext({ cwd } as ExtensionContext, { operation: "resume", entityDir }, mcpTools),
                /bound to linear, not the configured tracker/,
            )
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("plans an item's complete label set and keeps the selection on a Kind change", async () => {
        const cwd = temporaryProject()
        try {
            const area = {
                appliesTo: ["deliverable"],
                cardinality: "zeroOrMore",
                values: { fe: { name: "Area: FE", color: "#1D76DB", description: "" } },
            }
            const perf = {
                appliesTo: ["deliverable"],
                cardinality: "zeroOrOne",
                values: { perf: { name: "Perf", color: "#1D76DB", description: "" } },
            }
            prepareWorkflowProject(cwd, { ...config(), labels: { groups: { ...kindLabels().groups, area, perf } } })
            prepareArtifactPersistence(cwd)
            const work = await initializeEntity(
                {
                    entity: "task",
                    title: "Work",
                    slug: "work",
                    request: "Work",
                    kind: "bugfix",
                    parent: { kind: "none" },
                    object: { issueId: 1, issueNumber: 7, issueUrl: "https://github.com/alex/example/issues/7" },
                },
                { cwd },
            )
            const ctx = { cwd } as ExtensionContext
            const plan = (labels: unknown, names: string[]) =>
                (
                    buildIntegrationContext(
                        ctx,
                        {
                            operation: "labelMutation",
                            entityDir: work.directory,
                            ...(labels ? { labels } : {}),
                            current: { provider: "github", names },
                        } as Parameters<typeof buildIntegrationContext>[1],
                        mcpTools,
                    ) as { labelMutation?: unknown }
                ).labelMutation
            // A Kind change from feature to bugfix keeps Area: FE and Perf, and keeps an unrelated label.
            assert.deepEqual(plan(undefined, ["Kind: Feature", "Area: FE", "Perf", "customer"]), {
                provider: "github",
                labels: ["customer", "Area: FE", "Perf", "Kind: Bugfix"],
                changed: true,
                unknown: ["customer"],
            })
            assert.deepEqual(plan({ area: [], perf: ["perf"] }, ["Kind: Bugfix", "Area: FE", "Perf"]), {
                provider: "github",
                labels: ["Perf", "Kind: Bugfix"],
                changed: true,
                unknown: [],
            })
            // A correction that leaves out a group keeps that group's labels.
            assert.deepEqual(plan({ area: [] }, ["Kind: Bugfix", "Area: FE", "Perf"]), {
                provider: "github",
                labels: ["Perf", "Kind: Bugfix"],
                changed: true,
                unknown: [],
            })
            assert.match(JSON.stringify(plan({ nope: [] }, [])), /"state":"invalid"/)
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("returns queue intake labels from the Kind and the selection, on both trackers", async () => {
        const cwd = temporaryProject()
        try {
            prepareWorkflowProject(cwd)
            prepareArtifactPersistence(cwd)
            const ctx = { cwd } as ExtensionContext
            const labels = (input: Parameters<typeof buildIntegrationContext>[1]) =>
                (buildIntegrationContext(ctx, input, mcpTools) as { labels?: unknown }).labels
            assert.deepEqual(labels(queueSelection), ["Kind: Refactor"])
            assert.deepEqual(labels({ ...queueSelection, entity: "epic", kind: "epic" }), ["Kind: Epic"])
            assert.deepEqual(labels({ ...queueSelection, kind: "not set" }), [])
            const write = (tracker: unknown) => writeProjectConfig(cwd, { tracker, forge: forgeConfig() })
            const area = {
                appliesTo: ["deliverable"],
                cardinality: "oneOrMore",
                values: { fe: { name: "Area: FE", color: "#1D76DB", description: "" } },
            }
            write({ ...config("project"), labels: { groups: { ...kindLabels().groups, area } } })
            assert.deepEqual(labels({ ...queueSelection, labels: { area: ["fe"] } }), ["Area: FE", "Kind: Refactor"])
            assert.deepEqual(
                (labels(queueSelection) as { error: unknown }).error,
                { kind: "cardinality", group: "area", cardinality: "oneOrMore", selected: [] },
                "a required group without a value is reported, not dropped",
            )
            assert.match(
                JSON.stringify(labels({ ...queueSelection, labels: { kind: ["bugfix"], area: ["fe"] } })),
                /comes from the item's Kind/,
            )
            write(config("project", false))
            assert.deepEqual(labels(queueSelection), [])
            write({
                ...linearConfig(),
                labels: { groups: { kind: linearKindGroup() } },
            })
            assert.deepEqual(labels(queueSelection), ["Kind: Refactor"], "Linear queue intake applies labels too")
            assert.deepEqual(
                labels({ ...queueSelection, entity: "epic", kind: "epic" }),
                [],
                "a Linear Epic is a Project and carries no Kind label",
            )
            write(linearConfig())
            assert.deepEqual(labels(queueSelection), [])
            assert.equal(labels({ operation: "inspect", target: "current" }), undefined)
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("requires an exact configured Priority or explicit not set", async () => {
        assert.doesNotThrow(() => assertConfiguredPriority(config("project"), "High"))
        assert.doesNotThrow(() => assertConfiguredPriority(config("project"), "not set"))
        assert.throws(() => assertConfiguredPriority(config("project"), "Critical"), /not configured/)
    })

    it("requires complete, distinct Linear status mappings and native priorities", async () => {
        assert.equal(linearPriorityNumber("Urgent"), 1)
        assert.equal(linearPriorityNumber("High"), 2)
        assert.equal(linearPriorityNumber("Medium"), 3)
        assert.equal(linearPriorityNumber("Low"), 4)
        assert.equal(linearPriorityNumber("not set"), 0)
        assert.throws(() => linearPriorityNumber("Critical"), /invalid Linear priority/)
        assert.throws(() => linearPriorityNumber("toString"), /invalid Linear priority/)
        assert.doesNotThrow(() => assertConfiguredPriority(linearConfig(), "High"))
        assert.throws(() => assertConfiguredPriority(linearConfig(), "Critical"), /invalid Linear priority/)

        const cwd = temporaryProject()
        try {
            const write = (tracker: unknown) => writeInvalidIntegrations(cwd, { tracker, forge: forgeConfig() })
            write(linearConfig())
            assert.equal(loadTracker(cwd).provider, "linear")

            // piton checks the table; the loader fails with its message, which names the key and line.
            const duplicateStatuses = linearConfig()
            duplicateStatuses.statuses.issues.todo = duplicateStatuses.statuses.issues.backlog
            write(duplicateStatuses)
            assert.throws(
                () => loadTracker(cwd),
                /invalid .*\.piton\/config\.toml: line \d+, tracker\.statuses\.issues: /,
            )

            const missingProjectCanceled = linearConfig()
            delete (missingProjectCanceled.statuses.projects as Partial<typeof statuses>).canceled
            write(missingProjectCanceled)
            assert.throws(() => loadTracker(cwd), /tracker\.statuses\.projects: missing field `canceled`/)

            write({ ...linearConfig(), repository: { owner: "x", repo: "y" } })
            assert.throws(() => loadTracker(cwd), /invalid .*\.piton\/config\.toml/)

            const { phase, ...withoutPhase } = linearConfig()
            write(withoutPhase)
            const loaded = loadTracker(cwd)
            assert.equal(loaded.provider === "linear" && loaded.phase, undefined)

            write({ ...withoutPhase, phase: { issues: phase.issues } })
            assert.throws(() => loadTracker(cwd), /invalid .*\.piton\/config\.toml/)
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("reads GitHub mappings with their scopes, Phase optional", async () => {
        // piton's config module checks the mappings (distinct options and fields); see its tracker tests.
        const cwd = temporaryProject()
        try {
            const writeConfig = (tracker: unknown) => writeProjectConfig(cwd, { tracker, forge: forgeConfig() })

            const separateScope = config("issue")
            separateScope.fields.priority.field = separateScope.fields.status.field
            writeConfig(separateScope)
            assert.equal(loadTracker(cwd).provider, "github")

            writeConfig(config("project", false))
            assert.equal(loadTracker(cwd).provider, "github")

            const withoutPhase = config("project")
            delete (withoutPhase.fields as { phase?: unknown }).phase
            writeConfig(withoutPhase)
            const loaded = loadTracker(cwd)
            assert.equal(loaded.provider === "github" && loaded.fields.phase, undefined)
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("reads tracker and forge configuration without provider preflight", async () => {
        const cwd = temporaryProject()
        try {
            prepareWorkflowProject(cwd)
            prepareArtifactPersistence(cwd)
            const result = buildIntegrationContext(
                { cwd } as ExtensionContext,
                { operation: "inspect", target: "current" },
                mcpTools,
            )
            assert.deepEqual(result.roles.tracker, {
                state: "enabled",
                provider: "github",
                config: config(),
            })
            assert.deepEqual(result.roles.forge, {
                state: "enabled",
                provider: "github",
                config: forgeConfig(),
            })
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("returns only what each operation uses", async () => {
        const cwd = temporaryProject()
        try {
            const tracker = {
                ...config(),
                labels: {
                    groups: {
                        ...kindLabels().groups,
                        area: {
                            appliesTo: ["epic", "deliverable"],
                            cardinality: "oneOrMore",
                            values: { cli: { name: "Area: CLI", color: "#000000", description: "The CLI." } },
                        },
                        gate: {
                            appliesTo: ["deliverable"],
                            cardinality: "zeroOrMore",
                            values: { plan: { name: "Gate: Plan", color: "#000000", description: "Waits." } },
                        },
                    },
                },
            }
            prepareWorkflowProject(cwd, tracker)
            prepareArtifactPersistence(cwd)
            const entityDir = writeGitHubTaskEntity(cwd)
            const ctx = { cwd } as ExtensionContext
            const inputs: Parameters<typeof buildIntegrationContext>[1][] = [
                { operation: "inspect", target: "current" },
                queueSelection,
                { operation: "adopt", entity: "task" },
                { operation: "initialize", entityDir },
                { operation: "resume", entityDir },
                { operation: "artifactProjection", entityDir },
                { operation: "pullRequest", entityDir },
                { operation: "shippingRead", entityDir },
                { operation: "trackerMutation", entityDir, change: "lifecycle" },
                { operation: "labelMutation", entityDir, current: { provider: "github", names: [] } },
            ]
            const parts = Object.fromEntries(
                inputs.map((input) => {
                    const context = buildIntegrationContext(ctx, input, mcpTools) as Record<string, unknown> & {
                        roles: { tracker?: { config?: object }; forge?: object }
                    }
                    const roles = [
                        ...(context.roles.tracker
                            ? [
                                  context.roles.tracker.config && "labels" in context.roles.tracker.config
                                      ? "tracker+labels"
                                      : "tracker",
                              ]
                            : []),
                        ...(context.roles.forge ? ["forge"] : []),
                    ]
                    const rest = Object.keys(context).filter(
                        (key) => !["state", "operation", "roles", "target", "projectRoot"].includes(key),
                    )
                    return [input.operation, [...roles, ...rest].sort()]
                }),
            )
            assert.deepEqual(parts, {
                inspect: ["forge", "tracker+labels"],
                queueIntake: ["labels", "tracker"],
                adopt: ["labelGroups", "tracker"],
                initialize: ["entity", "forge", "phaseProjection", "tracker"],
                resume: ["entity", "forge", "phaseProjection", "tracker"],
                artifactProjection: ["projection", "tracker"],
                pullRequest: ["entity", "forge"],
                shippingRead: ["entity", "forge"],
                trackerMutation: ["entity", "tracker"],
                labelMutation: ["entity", "labelMutation", "tracker"],
            })

            // Adoption lists each group for the item type by label name, without setup-only fields.
            const { epic: _epic, ...deliverableKinds } = kindLabelNames
            assert.deepEqual(
                buildIntegrationContext(ctx, { operation: "adopt", entity: "task" }, mcpTools).labelGroups,
                {
                    kind: { cardinality: "exactlyOne", values: deliverableKinds },
                    area: { cardinality: "oneOrMore", values: { cli: "Area: CLI" } },
                    gate: { cardinality: "zeroOrMore", values: { plan: "Gate: Plan" } },
                },
            )
            assert.deepEqual(
                buildIntegrationContext(ctx, { operation: "adopt", entity: "epic" }, mcpTools).labelGroups,
                {
                    kind: { cardinality: "exactlyOne", values: { epic: kindLabelNames.epic } },
                    area: { cardinality: "oneOrMore", values: { cli: "Area: CLI" } },
                },
            )
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("renders entity provider bodies with only same-repository GitHub closing references", async () => {
        const cwd = temporaryProject()
        try {
            prepareWorkflowProject(cwd)
            prepareArtifactPersistence(cwd)
            const entityDir = writeGitHubTaskEntity(cwd)
            const ctx = { cwd } as ExtensionContext
            const source = "# Result\n\n## Overview\n\nDelivered.\n\n## Verification\n\nTests passed.\n"
            const trackerBody =
                "# Plan\n\nDelivered.\n\n<details>\n<summary>Full plan</summary>\n\n## Verification\n\nTests passed.\n\n</details>"
            const prBody =
                "## Overview\n\nDelivered.\n\n<hr />\n\n<details>\n<summary>Verification</summary>\n\nTests passed.\n\n</details>"

            assert.equal(
                renderEntityProviderBody(ctx, {
                    entityDir,
                    destination: "tracker",
                    source,
                }),
                trackerBody,
            )
            assert.equal(
                renderEntityProviderBody(ctx, {
                    entityDir,
                    destination: "pullRequest",
                    source,
                }),
                `${prBody}\n\n---\n\nCloses #42`,
            )

            writeProjectConfig(cwd, {
                tracker: config("project"),
                forge: { ...forgeConfig(), repository: { owner: "alex", repo: "other" } },
            })
            assert.equal(
                renderEntityProviderBody(ctx, {
                    entityDir,
                    destination: "pullRequest",
                    source,
                }),
                prBody,
            )
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("returns the rendered PR body as the tool's text, ready to publish unchanged", async () => {
        const cwd = temporaryProject()
        try {
            prepareWorkflowProject(cwd)
            prepareArtifactPersistence(cwd)
            const entityDir = writeGitHubTaskEntity(cwd)
            const ctx = { cwd } as ExtensionContext
            type Tool = Parameters<ExtensionAPI["registerTool"]>[0]
            const tools = new Map<string, Tool>()
            registerIntegrations({
                events: { on: () => () => {} },
                on: () => {},
                registerCommand: () => {},
                registerTool: (tool: Tool) => tools.set(tool.name, tool),
            } as unknown as ExtensionAPI)
            const input = {
                entityDir,
                destination: "pullRequest" as const,
                source: "# Result\n\n## Overview\n\nDelivered.\n\n## Verification\n\nTests passed.\n",
            }
            const result = await tools
                .get("render_provider_body")!
                .execute("render", input, undefined, undefined, withToolExecution(ctx))
            const markdown = renderEntityProviderBody(ctx, input)
            assert.deepEqual(result.content, [{ type: "text", text: markdown }])
            assert.deepEqual(result.details, { markdown })
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("keeps Linear tracker and GitHub forge contexts independent", async () => {
        const cwd = temporaryProject()
        try {
            prepareWorkflowProject(cwd, linearConfig())
            prepareArtifactPersistence(cwd)
            const entityDir = writeLinearTaskEntity(cwd)
            const ctx = { cwd } as ExtensionContext

            const trackerSide = buildIntegrationContext(ctx, queueSelection, mcpTools)
            assert.equal(trackerSide.roles.tracker?.state, "enabled")
            assert.equal(trackerSide.roles.forge, undefined)
            const resumeSide = buildIntegrationContext(ctx, { operation: "resume", entityDir }, mcpTools)
            assert.equal(resumeSide.roles.tracker?.state, "enabled")
            assert.equal(resumeSide.roles.forge?.state, "enabled")
            const forgeSide = buildIntegrationContext(ctx, { operation: "pullRequest", entityDir }, mcpTools)
            assert.equal(forgeSide.roles.tracker, undefined)
            assert.equal(forgeSide.roles.forge?.state, "enabled")
            assert.equal(
                renderEntityProviderBody(ctx, {
                    entityDir,
                    destination: "pullRequest",
                    source: "# Result\n\n## Overview\n\nDelivered.\n\n## Verification\n\nTests passed.\n",
                }),
                "## Overview\n\nDelivered.\n\n<hr />\n\n<details>\n<summary>Verification</summary>\n\nTests passed.\n\n</details>",
            )
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("runs forge steps with the tracker off, and tracker steps with the forge off", async () => {
        const cwd = temporaryProject()
        try {
            prepareWorkflowProject(cwd)
            prepareArtifactPersistence(cwd)
            const entityDir = writeGitHubTaskEntity(cwd)
            const ctx = { cwd } as ExtensionContext
            const source = "# Result\n\n## Overview\n\nDelivered.\n\n## Verification\n\nTests passed.\n"

            writeProjectConfig(cwd, { tracker: { provider: "none" } })
            const forgeSide = buildIntegrationContext(ctx, { operation: "pullRequest", entityDir }, mcpTools)
            assert.equal(forgeSide.roles.forge?.state, "enabled")
            const resumed = buildIntegrationContext(ctx, { operation: "resume", entityDir }, mcpTools)
            assert.equal(resumed.roles.tracker?.state, "disabled")
            assert.throws(
                () => buildIntegrationContext(ctx, { operation: "artifactProjection", entityDir }, mcpTools),
                /needs the tracker, which is turned off/,
            )
            // Without a tracker, the PR body has no closing reference.
            assert.doesNotMatch(
                renderEntityProviderBody(ctx, { entityDir, destination: "pullRequest", source }),
                /Closes/,
            )
            assert.throws(
                () => renderEntityProviderBody(ctx, { entityDir, destination: "tracker", source }),
                /needs the tracker/,
            )
            // A PR body needs no entity: an ad-hoc PR renders the same way, without a closing reference.
            assert.equal(
                renderEntityProviderBody(ctx, { destination: "pullRequest", source }),
                renderEntityProviderBody(ctx, { entityDir, destination: "pullRequest", source }),
            )
            assert.throws(() => renderEntityProviderBody(ctx, { destination: "tracker", source }), /entityDir/)

            writeProjectConfig(cwd, { tracker: config(), forge: { provider: "none" } })
            assert.equal(
                buildIntegrationContext(ctx, { operation: "artifactProjection", entityDir }, mcpTools).roles.tracker
                    ?.state,
                "enabled",
            )
            assert.throws(
                () => buildIntegrationContext(ctx, { operation: "pullRequest", entityDir }, mcpTools),
                /needs the forge, which is turned off/,
            )
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("treats a local Task as tracker-off while the forge steps still run", async () => {
        const cwd = temporaryProject()
        try {
            prepareWorkflowProject(cwd)
            prepareArtifactPersistence(cwd)
            const ctx = { cwd } as ExtensionContext
            const { directory: entityDir } = await initializeLocalTask(
                { title: "Local", slug: "local", request: "Local work.", kind: "chore" },
                { cwd, now: new Date("2026-03-04T00:00:00Z") },
            )
            const resumed = buildIntegrationContext(ctx, { operation: "resume", entityDir }, mcpTools)
            assert.equal(resumed.roles.tracker?.state, "disabled")
            assert.equal(resumed.phaseProjection, undefined)
            assert.equal(
                buildIntegrationContext(ctx, { operation: "pullRequest", entityDir }, mcpTools).roles.forge?.state,
                "enabled",
            )
            for (const operation of ["artifactProjection", "trackerMutation"] as const)
                assert.throws(
                    () =>
                        buildIntegrationContext(
                            ctx,
                            operation === "trackerMutation"
                                ? { operation, entityDir, change: "lifecycle" }
                                : { operation, entityDir },
                            mcpTools,
                        ),
                    /this Task is local/,
                )
            assert.doesNotMatch(
                renderEntityProviderBody(ctx, {
                    entityDir,
                    destination: "pullRequest",
                    source: "# Result\n\n## Overview\n\nDone.\n\n## Verification\n\nRan.\n",
                }),
                /Closes/,
            )
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("renders Epic and Task trackers losslessly without local mutations", async () => {
        const githubEpic = {
            provider: "github",
            repository: { owner: "alex", repo: "example" },
            external: { issueNumber: 7 },
            planComment: { state: "unpublished" },
        }
        const linearEpic = {
            provider: "linear",
            resource: "project",
            external: {
                projectId: "linear-project-uuid",
                createdAt: "2026-10-01T09:00:00.000Z",
            },
            planDocument: { state: "unpublished" },
        }
        for (const provider of ["github", "linear"] as const) {
            for (const entity of ["epic", "task"] as const) {
                const cwd = temporaryProject()
                try {
                    prepareWorkflowProject(cwd, provider === "github" ? config() : linearConfig())
                    prepareArtifactPersistence(cwd)
                    const entityDir =
                        entity === "task"
                            ? provider === "github"
                                ? writeGitHubTaskEntity(cwd)
                                : writeLinearTaskEntity(cwd)
                            : provider === "github"
                              ? writeEntity(cwd, "epic", "7-rollout", githubEpic, "rollout")
                              : writeEntity(cwd, "epic", "20261001-rollout", linearEpic, "rollout")
                    const before = fileSnapshot(cwd)
                    const overview =
                        "## Overview\n\nExplain the work.\n\n### Plan highlights\n\n- Preserve the contract."
                    const details = "## Scope\n\nKeep every detail and `src/example.ts`."
                    const fold =
                        provider === "github"
                            ? `<details>\n<summary>Full plan</summary>\n\n${details}\n\n</details>`
                            : `>>> Full plan\n\n${details}\n\n>>>`
                    assert.equal(
                        renderEntityProviderBody({ cwd } as ExtensionContext, {
                            entityDir,
                            destination: "tracker",
                            source: `# Work\n\n${overview}\n\n${details}\n`,
                        }),
                        `${provider === "github" ? "# Plan\n\nExplain the work.\n\n## Highlights\n\n- Preserve the contract." : "Explain the work.\n\n## Highlights\n\n- Preserve the contract."}\n\n${fold}`,
                    )
                    assert.deepEqual(fileSnapshot(cwd), before)
                } finally {
                    fs.rmSync(cwd, { recursive: true, force: true })
                }
            }
        }
    })

    it("returns the lossless projection contract with the saved plan publication state", async () => {
        const cwd = temporaryProject()
        try {
            prepareWorkflowProject(cwd, linearConfig())
            prepareArtifactPersistence(cwd)
            const entityDir = writeLinearTaskEntity(cwd)
            const projection = buildIntegrationContext(
                { cwd } as ExtensionContext,
                { operation: "artifactProjection", entityDir },
                mcpTools,
            )
            assert.ok(projection.projection)
            assert.match(projection.projection.contentPolicy, /lossless.*never summarize or condense/)
            assert.match(projection.projection.rendering, /tracker_publication.*exact registered document mutation/)
            assert.equal(projection.projection.authority, "local artifact")
            assert.ok("planDocument" in projection.projection)
            assert.deepEqual(projection.projection.planDocument, {
                state: "unpublished",
            })
            assert.equal(projection.roles.tracker?.state, "enabled")
            assert.equal(projection.roles.forge, undefined)
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("permits a personal Project to use issue-scoped fields", async () => {
        const cwd = temporaryProject()
        try {
            writeProjectConfig(cwd, {
                tracker: { ...config("issue"), project: { owner: "alex", ownerType: "user", number: 3 } },
                forge: forgeConfig(),
            })
            assert.equal(loadTracker(cwd).provider, "github")
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("validates native Phase through runtime context", async () => {
        const cwd = temporaryProject()
        try {
            prepareWorkflowProject(cwd)
            prepareArtifactPersistence(cwd)
            const entityDir = writeGitHubTaskEntity(cwd)
            const context = buildIntegrationContext(
                { cwd } as ExtensionContext,
                {
                    operation: "trackerMutation",
                    entityDir,
                    change: "phase",
                    phase: "execution",
                    lifecycle: "inProgress",
                    native: {
                        provider: "github",
                        current: "Planning",
                        field: { name: "Phase", dataType: "SINGLE_SELECT" },
                    },
                },
                mcpTools,
            )
            assert.partialDeepStrictEqual(context, {
                assignment: { provider: "github", field: "Phase", value: "Execution" },
                nativeChanged: true,
                mutation: { tool: "mcp__github__projects_write" },
            })
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("prepares GitHub Phase from the Project item read as the MCP returns it", async () => {
        const cwd = temporaryProject()
        try {
            prepareWorkflowProject(cwd)
            prepareArtifactPersistence(cwd)
            const entityDir = writeGitHubTaskEntity(cwd)
            type Tool = Parameters<ExtensionAPI["registerTool"]>[0]
            const tools = new Map<string, Tool>()
            let publish: ((data: unknown) => void) | undefined
            registerIntegrations({
                events: {
                    on: (event: string, handler: (data: unknown) => void) => {
                        if (event === "mcp:tool-catalog") publish = handler
                        return () => {}
                    },
                },
                on: () => {},
                registerCommand: () => {},
                registerTool: (tool: Tool) => tools.set(tool.name, tool),
            } as unknown as ExtensionAPI)
            publish!(mcpTools)
            const target = {
                server: "github",
                project: { owner: "ALEX", ownerType: "user" as const, number: 3 },
                repository: { owner: "alex", repo: "example" },
                issueNumber: 42,
            }
            const prepare = (native: unknown, phase = "execution") =>
                tools.get("prepare_github_phase_mutation")!.execute(
                    "phase",
                    { entityDir, phase, native },
                    undefined,
                    undefined,
                    withToolExecution({
                        cwd,
                    } as ExtensionContext),
                )
            const update = (value: string | null) => ({
                tool: "mcp__github__projects_write",
                arguments: {
                    method: "update_project_item",
                    owner: "alex",
                    owner_type: "user",
                    project_number: 3,
                    item_owner: "alex",
                    item_repo: "example",
                    issue_number: 42,
                    updated_field: { name: "Phase", value },
                },
            })
            const prepared = await prepare(githubPhaseReads(target))
            // The call is top-level `mutation`, as every prepare tool returns it (#301).
            assert.partialDeepStrictEqual(prepared.details, {
                assignment: { provider: "github", field: "Phase", value: "Execution" },
                nativeChanged: true,
                mutation: update("Execution"),
            })
            // Lifecycle comes from the item's Status.
            const cleared = await prepare(githubPhaseReads(target, null, "Done"), "none")
            assert.partialDeepStrictEqual(cleared.details, {
                assignment: { provider: "github", field: "Phase", value: null },
                nativeChanged: false,
            })
            assert.equal("mutation" in (cleared.details as object), false)
            // Phase shows only In Progress work, and In Progress work always shows one.
            await assert.rejects(prepare(githubPhaseReads(target, null, "Done")), /none/)
            await assert.rejects(prepare(githubPhaseReads(target), "none"), /In Progress/)
            const inReview = await prepare(githubPhaseReads(target, "Execution", "In Review"), "none")
            assert.partialDeepStrictEqual(inReview.details, {
                assignment: { provider: "github", field: "Phase", value: null },
                nativeChanged: true,
                mutation: update(null),
            })
            await assert.rejects(prepare(githubPhaseReads({ ...target, issueNumber: 43 })), /durable issue/)
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("decides plan approval for a Task from its labels, and refuses an Epic", async () => {
        const cwd = temporaryProject()
        try {
            const gate = {
                appliesTo: ["deliverable"],
                cardinality: "zeroOrMore",
                values: { plan: { name: "Gate: Plan", color: "#1D76DB", description: "" } },
            }
            const tracker = config()
            prepareWorkflowProject(cwd, {
                ...tracker,
                labels: { groups: { ...tracker.labels!.groups, gate } },
            })
            prepareArtifactPersistence(cwd)
            const work = writeGitHubTaskEntity(cwd)
            const epic = writeEntity(
                cwd,
                "epic",
                "7-epic",
                {
                    provider: "github",
                    repository: { owner: "alex", repo: "example" },
                    external: { issueNumber: 7 },
                    planComment: { state: "unpublished" },
                },
                "epic",
            )
            type Tool = Parameters<ExtensionAPI["registerTool"]>[0]
            const tools = new Map<string, Tool>()
            registerIntegrations({
                events: { on: () => () => {} },
                on: () => {},
                registerCommand: () => {},
                registerTool: (tool: Tool) => tools.set(tool.name, tool),
            } as unknown as ExtensionAPI)
            const inspect = (entityDir: string, names: string[]) =>
                tools
                    .get("inspect_gate")!
                    .execute(
                        "gate",
                        { entityDir, gate: "plan", current: { provider: "github", names } },
                        undefined,
                        undefined,
                        withToolExecution({ cwd } as ExtensionContext),
                    )
            assert.deepEqual((await inspect(work, ["Kind: Feature", "Gate: Plan"])).details, {
                gate: "awaitApproval",
                label: "Gate: Plan",
            })
            assert.equal(((await inspect(work, ["Kind: Feature"])).details as { gate: string }).gate, "continue")
            await assert.rejects(inspect(epic, ["Gate: Plan"]), /Epic plan always waits/)
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("calculates Linear whole-label updates through runtime context", async () => {
        const cwd = temporaryProject()
        try {
            prepareWorkflowProject(cwd, linearConfig())
            prepareArtifactPersistence(cwd)
            const entityDir = writeLinearTaskEntity(cwd)
            const base = {
                team: null,
                parentId: null,
                isGroup: false,
                groupType: null,
                retiredAt: null,
            }
            const context = buildIntegrationContext(
                { cwd } as ExtensionContext,
                {
                    operation: "trackerMutation",
                    entityDir,
                    change: "phase",
                    phase: "execution",
                    lifecycle: "inProgress",
                    native: {
                        provider: "linear",
                        applied: ["customer", "planning"],
                        catalog: [
                            {
                                ...base,
                                id: "group",
                                name: "Phase",
                                isGroup: true,
                                groupType: "singleSelect",
                            },
                            ...Object.entries(phaseValues).map(([id, name]) => ({
                                ...base,
                                id,
                                name,
                                parentId: "group",
                            })),
                        ],
                    },
                },
                mcpTools,
            )
            assert.partialDeepStrictEqual(context, {
                assignment: { provider: "linear", labels: ["customer", "execution"] },
                nativeChanged: true,
                mutation: {
                    tool: "mcp__linear__save_issue",
                    arguments: { id: "linear-uuid", labels: ["customer", "execution"] },
                },
            })
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("fails loudly with piton's message for invalid configuration, without substituting labels", async () => {
        // piton's config module owns the tracker's rules and their tests; the loader only surfaces its errors.
        const cwd = temporaryProject()
        try {
            const valid = config("project")
            const write = (tracker: unknown) => writeInvalidIntegrations(cwd, { tracker, forge: forgeConfig() })
            const { audit, ...otherKinds } = kindLabelNames
            write({
                ...valid,
                labels: kindLabels({ ...otherKinds, review: audit } as unknown as typeof kindLabelNames),
            })
            assert.throws(() => loadTracker(cwd), /line \d+, tracker\.labels\.groups\.kind\.values: /)
            write({ ...valid, fields: { ...valid.fields, type: { scope: "project", field: "Kind" } } })
            assert.throws(() => loadTracker(cwd), /tracker\.fields\.type: unknown field/)
            // The labels' old shape is an unknown key.
            write({ ...valid, labels: { kind: { epic: "Kind: Epic" } } })
            assert.throws(() => loadTracker(cwd), /tracker\.labels\.kind: unknown field/)
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })
})
