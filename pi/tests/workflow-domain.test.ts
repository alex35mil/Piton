import "./host-runtime.ts"
import { writeProjectConfig } from "./config-fixture.ts"
import { loadIntegrationRoles, requireTracker } from "../workflows/extensions/integrations/config.ts"

import assert from "node:assert/strict"
import * as cp from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { describe, it } from "node:test"

import { Type } from "typebox"

import { kindLabels, linearKindGroup, linearPhases, phaseValues } from "./workflow-phase-fixtures.ts"

import { compile, parse, SchemaValidationError } from "../extensions/__lib/schema.ts"
import { entityName, formatBranchName, readyBranch, slugify } from "../workflows/extensions/__lib/domain.ts"
import {
    readEntityStatus,
    readEpicTaskProgress,
    taskParent,
    writeEntityStatus,
} from "../workflows/extensions/__lib/entity.ts"
import { prepareArtifactPersistence } from "../workflows/extensions/__lib/project-config.ts"
import {
    EntityInitializationValidator,
    initializeEntity,
    initializeLocalTask,
    preflightEntity,
} from "../workflows/extensions/init/entity.ts"
import { formatInitializedEntity } from "../workflows/extensions/init/index.ts"
import {
    checkTaskParent,
    CheckTaskParentValidator,
    routeTask,
    RouteTaskValidator,
} from "../workflows/extensions/init/route.ts"
import { restoreTask, RestoreTaskValidator } from "../workflows/extensions/init/restore-task.ts"
import { planPublicationMarker } from "../workflows/extensions/integrations/plan-publication.ts"
import { renderProviderBody } from "../workflows/extensions/integrations/projection.ts"
import { workflowMcpCatalog } from "./mcp-catalog-fixture.ts"
import { githubReads, linearReads } from "./workflow-issue-reads-fixture.ts"

const repository = { owner: "example", repo: "project" }
const forge = { provider: "github", mcpServer: "github", repository }
const statuses = {
    backlog: "Backlog",
    todo: "Todo",
    inProgress: "In Progress",
    inReview: "In Review",
    done: "Done",
    canceled: "Canceled",
}

function git(cwd: string, args: string[]): string {
    return cp.execFileSync("git", args, { cwd, encoding: "utf-8" }).trim()
}

function temporaryRepository(tracker: "github" | "linear" = "github", format = "identifier-title"): string {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), "workflow-domain-"))
    git(directory, ["init", "-b", "main"])
    git(directory, ["config", "user.email", "test@example.com"])
    git(directory, ["config", "user.name", "Test"])
    fs.writeFileSync(path.join(directory, "README.md"), "test\n")
    fs.writeFileSync(path.join(directory, ".gitignore"), "/.piton/.local\n")
    git(directory, ["add", "README.md", ".gitignore"])
    git(directory, ["commit", "-m", "initial"])
    fs.mkdirSync(path.join(directory, ".piton"), { recursive: true })
    writeProjectConfig(directory, {
        branchFormat: format,
        tracker:
            tracker === "github"
                ? {
                      provider: "github",
                      mcpServer: "github",
                      repository,
                      project: { owner: "example", ownerType: "user", number: 3 },
                      fields: {
                          status: { field: "Status", values: statuses },
                          phase: { field: "Phase", values: { ...phaseValues } },
                          priority: {
                              scope: "project",
                              field: "Priority",
                              values: ["High", "Low"],
                          },
                      },
                  }
                : {
                      provider: "linear",
                      mcpServer: "linear",
                      team: "Engineering",
                      phase: linearPhases(),
                      statuses: {
                          issues: statuses,
                          projects: { ...statuses, done: "Completed" },
                      },
                  },
        forge,
    })
    prepareArtifactPersistence(directory)
    return directory
}

function issue(number: number) {
    return {
        issueId: 1000 + number,
        issueNumber: number,
        issueUrl: `https://github.com/example/project/issues/${number}`,
        projectItemId: 2000 + number,
    }
}

function linearIssue(identifier: string, extra: Record<string, string> = {}) {
    return {
        issueId: `${identifier.toLowerCase()}-uuid`,
        identifier,
        issueUrl: `https://linear.app/example/issue/${identifier}/work`,
        ...extra,
    }
}

// A Task's init input from an issue object: its tracker reads, standalone or under the named Epic. `init`
// turns these into the reads `init_task` takes.
type TaskObject = ReturnType<typeof issue> | ReturnType<typeof linearIssue>
const standalone = (title: string, object: TaskObject) => ({
    entity: "task" as const,
    title,
    slug: slugify(title),
    request: title,
    kind: "chore" as const,
    under: undefined as string | undefined,
    object,
})
const epic = (title: string, object: unknown) => ({
    entity: "epic" as const,
    title,
    slug: slugify(title),
    request: title,
    object,
})
// An Epic Task; `kind` must match its plan section's (planTask defaults to `feature`).
const task = (title: string, epicDir: string, object: TaskObject, kind = "feature") => ({
    entity: "task" as const,
    title,
    slug: slugify(title),
    request: title,
    kind,
    under: epicDir as string | undefined,
    object,
})

/** The tracker reads for a Task issue object, its parent the Epic at `under` or none. */
function readsFor(cwd: string, object: TaskObject, under: string | undefined) {
    const epic = under ? readEntityStatus(path.join(cwd, under)) : undefined
    if ("issueNumber" in object) {
        const parent = epic && epic.tracker.provider === "github" ? epic.tracker.external.issueNumber : null
        return githubReads({ number: object.issueNumber, parent })
    }
    // A Linear issue's Project is its own fact; `under` only names the expected Epic.
    const extra = object as { projectId?: string; gitBranchName?: string }
    return linearReads({
        identifier: object.identifier,
        ...(extra.projectId ? { projectId: extra.projectId } : {}),
        ...(extra.gitBranchName ? { gitBranchName: extra.gitBranchName } : {}),
    })
}
// One `## Tasks` section; tagged once issue processing has created or linked its issue, which then
// holds the design. `extra` holds the optional `**Labels:**` and `**Status:**` lines.
const planTask = (title: string, marker?: string, kind = "feature", extra = "") =>
    `### ${marker ? `[${marker}] ` : ""}${title}\n\n**Kind:** \`${kind}\`\n${extra}**Summary:** ${title} summary.\n` +
    (marker ? "" : `\n${title} outcome.\n`)
const epicPlan = (branch: string, target: "epic" | "default", ...tasks: string[]) =>
    `# Epic\n\n## Overview\n\nPlan.\n\n## Task delivery strategy\n\n**Task target:** \`${target}\`\n**Epic branch:** \`${branch}\`\n\nWhy.\n\n## Tasks\n\n${tasks.join("\n")}`

const catalog = workflowMcpCatalog("github", "linear")

async function init(cwd: string, input: unknown) {
    const value = input as Record<string, unknown>
    const taskInput =
        value.entity === "task" && "object" in value
            ? (() => {
                  const { object, under, ...rest } = value as { object: TaskObject; under?: string }
                  return { ...rest, reads: readsFor(cwd, object, under) }
              })()
            : input
    return await initializeEntity(parse(EntityInitializationValidator, taskInput), { cwd, tools: catalog })
}

function writeMetadata(cwd: string, directory: string, status: unknown): void {
    fs.writeFileSync(path.join(cwd, directory, "metadata.json"), `${JSON.stringify(status, null, 2)}\n`)
}

describe("workflow domain", () => {
    it("parses strictly: never strips, converts, or defaults", async () => {
        const validator = compile(
            Type.Object({ name: Type.String(), count: Type.Integer() }, { additionalProperties: false }),
        )
        const value = { name: "a", count: 3 }
        assert.equal(parse(validator, value), value)
        assert.throws(
            () => parse(validator, { ...value, extra: true }),
            (error) => error instanceof SchemaValidationError && /\/extra: unknown field/.test(error.message),
        )
        // An unknown field lists the fields accepted where it appeared, across union branches and nesting.
        assert.throws(
            () => parse(validator, { ...value, extra: true }),
            /\/extra: unknown field \(accepted: name, count\)/,
        )
        const nested = compile(
            Type.Union([
                Type.Object(
                    {
                        kind: Type.Literal("a"),
                        inner: Type.Object({ x: Type.Number() }, { additionalProperties: false }),
                    },
                    { additionalProperties: false },
                ),
                Type.Object({ kind: Type.Literal("b"), y: Type.Number() }, { additionalProperties: false }),
            ]),
        )
        assert.throws(
            () => parse(nested, { kind: "a", inner: { x: 1, z: 2 } }),
            /\/inner\/z: unknown field \(accepted: x\)/,
        )
        assert.throws(() => parse(nested, { kind: "b", y: 1, source: "s" }), /accepted: kind, inner, y/)
        assert.throws(
            () => parse(validator, { ...value, count: "3" }),
            (error) =>
                error instanceof SchemaValidationError &&
                /\/count:/.test(error.message) &&
                error.cause.some((cause) => cause.path === "/count"),
        )
    })

    it("names entities and branches from the tracker reference", async () => {
        assert.equal(slugify("Café workflow!"), "cafe-workflow")
        assert.equal(entityName({ provider: "github", issueNumber: 12 }, "cafe-workflow"), "12-cafe-workflow")
        assert.equal(entityName({ provider: "linear", identifier: "ENG-7" }, "work"), "ENG-7-work")
        assert.equal(
            entityName({ provider: "linear", projectCreatedAt: "2026-10-01T09:00:00Z" }, "rollout"),
            "20261001-rollout",
        )
        assert.throws(
            () => entityName({ provider: "linear", projectCreatedAt: "2026-13-01T00:00:00Z" }, "x"),
            /invalid timestamp/,
        )
        assert.equal(formatBranchName("ENG-7-work", "identifier-title"), "eng-7-work")
        assert.equal(formatBranchName("12-work", "username/identifier-title", "alex"), "alex/12-work")
        assert.throws(() => formatBranchName("12-work", "username/identifier-title"), /configured username/)
        assert.deepEqual(readyBranch("12-work", "main", "generated"), {
            state: "ready",
            name: "12-work",
            start: "main",
            target: "main",
            source: "generated",
        })
    })

    it("accepts only the tracker object that matches each entity", async () => {
        const check = (input: unknown) => EntityInitializationValidator.check(input)
        const base = { entity: "task", title: "Work", slug: "work", request: "Work", kind: "chore" }
        assert.equal(check({ ...base, reads: githubReads({ number: 1 }) }), true)
        assert.equal(check({ ...base, reads: linearReads({ identifier: "ENG-1" }) }), true)
        // The reads decide the issue and parent: a Task never takes them as input.
        assert.equal(check({ ...base, object: issue(1) }), false)
        assert.equal(check({ ...base, reads: githubReads({ number: 1 }), parent: { kind: "none" } }), false)
        assert.equal(check({ ...base, reads: {} }), false)
        assert.equal(
            check(
                epic("Work", {
                    projectId: "p",
                    projectUrl: "https://linear.app/x/project/p",
                    createdAt: "2026-10-01T09:00:00.000Z",
                }),
            ),
            true,
        )
        assert.equal(check({ ...epic("Work", issue(1)), kind: "feature" }), false)
        assert.equal(check({ ...base, kind: "review", reads: githubReads({ number: 1 }) }), false)
        assert.equal(check({ ...base, priority: "High", reads: githubReads({ number: 1 }) }), false)
    })

    it("keeps an adopted item's publication signature out of the brief", async () => {
        const cwd = temporaryRepository()
        try {
            const signed = (footer: string) => ({
                ...standalone("Signed", issue(8)),
                request: `Signed\n\nDo it.\n\n${footer}\n`,
            })
            const created = await init(cwd, signed("<sub>Sent via Pi. Model: x/y.</sub>"))
            const brief = path.join(cwd, created.directory, "brief.md")
            assert.equal(fs.readFileSync(brief, "utf-8"), "# Brief\n\nSigned\n\nDo it.\n")
            fs.rmSync(path.join(cwd, created.directory), { recursive: true })
            git(cwd, ["branch", "-D", created.status.branch.name])
            const linear = await init(cwd, signed("*Sent via Pi. Model: x/y.*"))
            assert.equal(
                fs.readFileSync(path.join(cwd, linear.directory, "brief.md"), "utf-8"),
                "# Brief\n\nSigned\n\nDo it.\n",
            )
            await assert.rejects(
                () => init(cwd, { ...standalone("Only", issue(9)), request: "<sub>Sent via Pi. Model: x/y.</sub>" }),
                /request is empty/,
            )
            assert.equal(fs.existsSync(path.join(cwd, ".piton/.local/tasks/9-only")), false)
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("uses the current local branch as default without a remote", async () => {
        const cwd = temporaryRepository()
        try {
            git(cwd, ["switch", "-c", "other-local-branch"])
            const initialized = await init(cwd, standalone("Local only", issue(5)))
            assert.equal(initialized.status.branch.start, "other-local-branch")
            assert.equal(initialized.status.branch.target, "other-local-branch")
            assert.equal(git(cwd, ["remote"]), "")
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("initializes a GitHub Epic and Task from their issues with a plan marker", async () => {
        const cwd = temporaryRepository()
        try {
            const created = await init(cwd, epic("Shared workflow", issue(12)))
            assert.equal(created.directory, ".piton/.local/epics/12-shared-workflow")
            const epicStatus = created.status
            assert.equal(epicStatus.entity, "epic")
            if (epicStatus.entity !== "epic") assert.fail()
            assert.equal(epicStatus.branch.name, "12-shared-workflow")
            assert.equal(git(cwd, ["rev-parse", epicStatus.branch.name]), git(cwd, ["rev-parse", "main"]))
            assert.deepEqual(epicStatus.tracker, {
                provider: "github",
                repository,
                external: { issueNumber: 12 },
                planComment: { state: "unpublished" },
            })
            // Phase and creation time live on the tracker, never in metadata.
            assert.equal("phase" in epicStatus, false)
            assert.equal("createdAt" in epicStatus, false)
            const epicDir = path.join(cwd, created.directory)
            for (const entry of ["brief.md", "notes.md", "metadata.json", "reviews", "scratch", "tasks"])
                assert.ok(fs.existsSync(path.join(epicDir, entry)), entry)
            for (const entry of ["pending.md", "status.md", "designs"])
                assert.equal(fs.existsSync(path.join(epicDir, entry)), false, entry)
            assert.deepEqual(
                created.resourceIds.map((ids) => [ids.scope.kind, ids.scope.entity]),
                [
                    ["github-issue", "12-shared-workflow"],
                    ["github-project-item", "12-shared-workflow"],
                ],
            )
            assert.match(formatInitializedEntity(created), /Initialized \.piton\/\.local\/epics\/12-shared-workflow/)

            const epicPath = path.join(epicDir, "epic.md")
            const plan = epicPlan(
                "12-shared-workflow",
                "epic",
                planTask("Build it", "#13", "bugfix"),
                planTask("Queued", "#14"),
                planTask("Dropped", "#15", "chore", "**Status:** `canceled`\n"),
                planTask("Later"),
            )
            fs.writeFileSync(epicPath, plan)
            await assert.rejects(
                () => init(cwd, task("Later", created.directory, issue(16))),
                /no Task tagged #16; finish the Epic's issue processing/,
            )
            await assert.rejects(() => init(cwd, task("Dropped", created.directory, issue(15), "chore")), /canceled/)
            await assert.rejects(
                () => init(cwd, task("Build it", created.directory, issue(13), "feature")),
                /gives #13 the Kind bugfix, not feature/,
            )

            const built = await init(cwd, task("Build it", created.directory, issue(13), "bugfix"))
            assert.equal(built.directory, ".piton/.local/epics/12-shared-workflow/tasks/13-build-it")
            assert.equal(built.status.branch.name, "13-build-it")
            assert.equal(built.status.branch.start, "12-shared-workflow")
            // The plan section's Kind must match; initialization never rewrites the plan.
            assert.equal(built.status.entity === "task" && built.status.kind, "bugfix")
            assert.equal(fs.readFileSync(epicPath, "utf-8"), plan)
            assert.deepEqual(readEntityStatus(path.join(cwd, built.directory)), built.status)
            await assert.rejects(
                () => init(cwd, task("Build it", created.directory, issue(13), "bugfix")),
                /no Task tagged #13/,
            )

            const lifecycles = { "#13": "inProgress", "#14": "todo", "#15": "canceled" } as const
            assert.deepEqual(readEpicTaskProgress(epicDir, lifecycles), {
                epic: "#12",
                items: [
                    {
                        state: "initialized",
                        title: "Build it",
                        lifecycle: "inProgress",
                        task: { marker: "#13", directory: built.directory },
                    },
                    { state: "queued", title: "Queued", lifecycle: "todo", task: { marker: "#14" } },
                    { state: "canceled", title: "Dropped", task: { marker: "#15" } },
                    { state: "unpublished", title: "Later" },
                ],
            })
            assert.equal(readEpicTaskProgress(epicDir, { ...lifecycles, "#13": "done" }).items[0].state, "complete")
            assert.throws(() => readEpicTaskProgress(epicDir, { "#13": "done" }), /lifecycle is required for #14/)
            // A Task started in another clone has no local directory.
            assert.deepEqual(readEpicTaskProgress(epicDir, { ...lifecycles, "#14": "inProgress" }).items[1], {
                state: "initialized",
                title: "Queued",
                lifecycle: "inProgress",
                task: { marker: "#14" },
            })

            // The tracker owns the relationship; a lagging local plan doesn't break the Task.
            fs.writeFileSync(epicPath, plan.replace("[#13] ", ""))
            assert.deepEqual(readEntityStatus(path.join(cwd, built.directory)), built.status)
            fs.writeFileSync(epicPath, plan.replace("[#13]", "[TASK-01KDVDNA00]"))
            assert.throws(() => readEpicTaskProgress(epicDir), /invalid Task marker/)
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("routes each new Task from the plan's Task target and never retargets existing Tasks", async () => {
        const cwd = temporaryRepository()
        try {
            const created = await init(cwd, epic("Feature-flag rollout", issue(20)))
            const epicBranch = created.status.branch.name
            const epicPath = path.join(cwd, created.directory, "epic.md")
            const tasks = [planTask("First Task", "#21"), planTask("Second Task", "#22"), planTask("Third Task", "#23")]
            fs.writeFileSync(epicPath, epicPlan(epicBranch, "epic", ...tasks))
            const first = await init(cwd, task("First Task", created.directory, issue(21)))
            assert.equal(first.status.branch.target, epicBranch)

            // Default-target Tasks need nothing from the Epic branch, even after it's deleted.
            fs.writeFileSync(epicPath, epicPlan(epicBranch, "default", ...tasks))
            git(cwd, ["branch", "-D", epicBranch])
            const second = await init(cwd, task("Second Task", created.directory, issue(22)))
            assert.equal(second.status.branch.start, "main")
            assert.equal(readEntityStatus(path.join(cwd, first.directory)).branch.target, epicBranch)

            // An Epic-target Task starts from the Epic branch, so it must exist locally.
            fs.writeFileSync(epicPath, epicPlan(epicBranch, "epic", ...tasks))
            const third = () => init(cwd, task("Third Task", created.directory, issue(23)))
            await assert.rejects(third, /start from the Epic branch 20-feature-flag-rollout; fetch or create it/)
            git(cwd, ["branch", epicBranch, "main"])
            assert.equal((await third()).status.branch.target, epicBranch)
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("requires routing lines that match the Epic", async () => {
        const cwd = temporaryRepository()
        try {
            const created = await init(cwd, epic("Planning only", issue(30)))
            const epicPath = path.join(cwd, created.directory, "epic.md")
            const start = () => init(cwd, task("Ship it", created.directory, issue(31)))
            fs.writeFileSync(epicPath, epicPlan("other-branch", "default", planTask("Ship it", "#31")))
            await assert.rejects(start, /says ``\*\*Epic branch:\*\* `other-branch` ``/)
            fs.writeFileSync(
                epicPath,
                epicPlan("30-planning-only", "default", planTask("Ship it", "#31")).replace(
                    "**Task target:** `default`\n",
                    "",
                ),
            )
            await assert.rejects(start, /needs ``\*\*Task target:\*\* `default` `` or/)
            fs.writeFileSync(epicPath, epicPlan("30-planning-only", "default", planTask("Ship it", "#31")))
            assert.equal((await start()).status.branch.start, "main")
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("names Linear work from its identifier and the Epic from its Project's creation date", async () => {
        const cwd = temporaryRepository("linear", "tracker")
        try {
            const project = {
                projectId: "project-uuid",
                projectUrl: "https://linear.app/example/project/rollout",
                createdAt: "2026-10-01T23:30:00.000-02:00",
            }
            const linearEpic = await init(cwd, epic("Rollout", project))
            assert.equal(linearEpic.directory, ".piton/.local/epics/20261002-rollout")
            assert.equal(linearEpic.status.branch.name, "20261002-rollout")
            assert.equal(linearEpic.status.branch.source, "tracker")
            assert.ok(linearEpic.status.tracker.provider === "linear")
            assert.deepEqual(linearEpic.status.tracker.external, {
                projectId: "project-uuid",
                createdAt: project.createdAt,
            })
            await assert.rejects(
                () => init(cwd, epic("Rollout", { ...project, projectId: "other-uuid" })),
                /entity directory already exists/,
            )
            assert.equal(
                EntityInitializationValidator.check(epic("Later", { ...project, createdAt: "yesterday" })),
                false,
            )
            const { createdAt: _, ...undated } = project
            assert.equal(EntityInitializationValidator.check(epic("Later", undated)), false)
            git(cwd, ["branch", "20261003-other-epic"])
            await assert.rejects(
                () =>
                    init(
                        cwd,
                        epic("Other epic", {
                            ...project,
                            projectId: "other",
                            createdAt: "2026-10-03T08:00:00Z",
                        }),
                    ),
                /workflow branch already exists: 20261003-other-epic/,
            )

            fs.writeFileSync(
                path.join(cwd, linearEpic.directory, "epic.md"),
                epicPlan("20261002-rollout", "epic", planTask("Wire it", "ENG-8")),
            )
            await assert.rejects(
                () =>
                    init(
                        cwd,
                        task(
                            "Wire it",
                            linearEpic.directory,
                            linearIssue("ENG-8", {
                                projectId: "other",
                                gitBranchName: "alex/eng-8",
                            }),
                        ),
                    ),
                /belongs to the Epic Project other, which this clone doesn't have/,
            )
            await assert.rejects(
                () =>
                    init(
                        cwd,
                        task("Wire it", linearEpic.directory, linearIssue("ENG-8", { projectId: "project-uuid" })),
                    ),
                /gitBranchName/,
            )
            const wired = await init(
                cwd,
                task(
                    "Wire it",
                    linearEpic.directory,
                    linearIssue("ENG-8", {
                        projectId: "project-uuid",
                        gitBranchName: "alex/eng-8-wire-it",
                    }),
                ),
            )
            assert.equal(wired.directory, ".piton/.local/epics/20261002-rollout/tasks/ENG-8-wire-it")
            assert.equal(wired.status.branch.name, "alex/eng-8-wire-it")
            assert.equal(wired.resourceIds[0].scope.entity, "ENG-8-wire-it")
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("initializes a local Task without a tracker item under every branch format", async () => {
        const now = new Date("2026-11-17T23:30:00Z")
        const local = { title: "Offline work", slug: "offline-work", request: "Offline work.", kind: "chore" as const }
        for (const [format, branch] of [
            ["identifier-title", "20261117-offline-work"],
            ["username/identifier-title", "alex/20261117-offline-work"],
            ["tracker", "20261117-offline-work"],
        ] as const) {
            const cwd = temporaryRepository("linear")
            try {
                // No tracker at all: a local Task doesn't need one.
                writeProjectConfig(cwd, { branchFormat: format, tracker: { provider: "none" }, username: "alex" })
                prepareArtifactPersistence(cwd)
                const created = await initializeLocalTask(local, { cwd, now })
                assert.equal(created.directory, ".piton/.local/tasks/20261117-offline-work")
                assert.equal(created.status.branch.name, branch)
                assert.equal(created.status.branch.start, "main")
                assert.deepEqual(created.status.tracker, { provider: "local", createdAt: now.toISOString() })
                assert.equal("phase" in created.status, false)
                assert.deepEqual(created.resourceIds, [])
                assert.match(formatInitializedEntity(created), /local/)
                assert.equal(git(cwd, ["rev-parse", "--verify", branch]).length, 40)
                assert.deepEqual(readEntityStatus(path.join(cwd, created.directory)), created.status)
                await assert.rejects(() => initializeLocalTask(local, { cwd, now }), /already exists/)
            } finally {
                fs.rmSync(cwd, { recursive: true, force: true })
            }
        }
    })

    it("refuses tracker-backed initialization with the tracker off, and local Epics or Tasks", async () => {
        const cwd = temporaryRepository()
        try {
            writeProjectConfig(cwd, { tracker: { provider: "none" } })
            await assert.rejects(
                () => init(cwd, standalone("Tracked", issue(3))),
                /needs the tracker, which is turned off/,
            )
            const created = await initializeLocalTask(
                { title: "Local", slug: "local", request: "Local.", kind: "chore" },
                { cwd, now: new Date("2026-01-02T00:00:00Z") },
            )
            const metadata = JSON.parse(fs.readFileSync(path.join(cwd, created.directory, "metadata.json"), "utf-8"))
            const epicDirectory = ".piton/.local/epics/20260102-local"
            fs.mkdirSync(path.join(cwd, epicDirectory), { recursive: true })
            const { kind: _kind, parent: _parent, ...epicMetadata } = metadata
            writeMetadata(cwd, epicDirectory, { ...epicMetadata, entity: "epic" })
            assert.throws(() => readEntityStatus(path.join(cwd, epicDirectory)), /an Epic can.t be local/)
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("uses static formats for Linear issues and rejects the tracker format on GitHub", async () => {
        const linear = temporaryRepository("linear")
        const github = temporaryRepository("github", "tracker")
        try {
            const created = await init(linear, standalone("Static", linearIssue("ENG-9")))
            assert.equal(created.status.branch.name, "eng-9-static")
            assert.equal(created.status.branch.source, "generated")
            assert.throws(
                () =>
                    preflightEntity(
                        {
                            entity: "task",
                            title: "Work",
                            slug: "work",
                            request: "Work",
                            kind: "chore",
                        },
                        { cwd: github },
                    ),
                /requires a Linear tracker/,
            )
            // Linear reads can't initialize a Task on a GitHub tracker.
            await assert.rejects(() => init(github, standalone("Wrong provider", linearIssue("ENG-1"))), /issue_number/)
        } finally {
            fs.rmSync(linear, { recursive: true, force: true })
            fs.rmSync(github, { recursive: true, force: true })
        }
    })

    it("returns the labels to create new work with, from its Kind and selection", async () => {
        const cwd = temporaryRepository()
        try {
            const tracker = requireTracker(loadIntegrationRoles(cwd), "test")
            const area = {
                appliesTo: ["epic", "deliverable"],
                cardinality: "zeroOrMore",
                values: { fe: { name: "Area: FE", color: "#1D76DB", description: "" } },
            }
            writeProjectConfig(cwd, { tracker: { ...tracker, labels: { groups: { ...kindLabels().groups, area } } } })
            const base = { title: "Work", slug: "work", request: "Work" }
            assert.deepEqual(
                preflightEntity({ ...base, entity: "task", kind: "chore", labels: { area: ["fe"] } }, { cwd }).labels,
                ["Area: FE", "Kind: Chore"],
            )
            assert.deepEqual(preflightEntity({ ...base, entity: "epic" }, { cwd }).labels, ["Kind: Epic"])
            assert.throws(
                () => preflightEntity({ ...base, entity: "task", kind: "chore", labels: { area: ["be"] } }, { cwd }),
                /labels: label group "area" has no value "be"/,
            )
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("creates a Linear Epic without a Kind label, and its Deliverables with one", async () => {
        const cwd = temporaryRepository("linear")
        try {
            const tracker = requireTracker(loadIntegrationRoles(cwd), "test")
            writeProjectConfig(cwd, { tracker: { ...tracker, labels: { groups: { kind: linearKindGroup() } } } })
            const base = { title: "Work", slug: "work", request: "Work" }
            assert.deepEqual(preflightEntity({ ...base, entity: "epic" }, { cwd }).labels, [])
            assert.deepEqual(preflightEntity({ ...base, entity: "task", kind: "chore" }, { cwd }).labels, [
                "Kind: Chore",
            ])
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("fails preflight before any tracker create and changes nothing", async () => {
        const cwd = temporaryRepository()
        try {
            const before = git(cwd, ["for-each-ref", "--format=%(refname)"])
            assert.throws(
                () =>
                    preflightEntity(
                        { entity: "task", title: "Orphan", slug: "-", request: "Orphan", kind: "feature" },
                        { cwd },
                    ),
                /slug must contain/,
            )
            assert.equal(git(cwd, ["for-each-ref", "--format=%(refname)"]), before)
            assert.equal(fs.existsSync(path.join(cwd, ".piton/.local/tasks")), false)
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("rejects an existing branch or directory without deleting it", async () => {
        const cwd = temporaryRepository()
        try {
            git(cwd, ["branch", "40-conflict", "main"])
            await assert.rejects(
                () => init(cwd, standalone("Conflict", issue(40))),
                /workflow branch already exists: 40-conflict/,
            )
            assert.ok(git(cwd, ["show-ref", "--verify", "refs/heads/40-conflict"]))
            assert.equal(fs.existsSync(path.join(cwd, ".piton/.local/tasks/40-conflict")), false)

            const first = await init(cwd, standalone("Kept", issue(41)))
            git(cwd, ["branch", "-D", "41-kept"])
            await assert.rejects(() => init(cwd, standalone("Kept", issue(41))), /entity directory already exists/)
            assert.ok(fs.existsSync(path.join(cwd, first.directory, "metadata.json")))
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("derives a Task's parent from its directory and holds each parent to its invariants", async () => {
        const cwd = temporaryRepository("linear")
        try {
            assert.deepEqual(taskParent(path.join(cwd, ".piton/.local/tasks/ENG-1-work")), { kind: "none" })
            assert.deepEqual(taskParent(path.join(cwd, ".piton/.local/epics/20261002-e/tasks/ENG-1-work")), {
                kind: "epic",
                directory: path.join(cwd, ".piton/.local/epics/20261002-e"),
            })
            for (const misplaced of [".piton/.local/epics/ENG-1-work", ".piton/.local/tasks/a/tasks/ENG-1-work"])
                assert.throws(() => taskParent(path.join(cwd, misplaced)), /a Task lives under/)

            // The tracker decides: an issue in a Project is that Epic's Task, so with no local Epic it refuses.
            await assert.rejects(
                () => init(cwd, standalone("Work", linearIssue("ENG-1", { projectId: "project-uuid" }))),
                /belongs to the Epic Project project-uuid, which this clone doesn't have; restore the Epic first/,
            )
            const created = await init(cwd, standalone("Work", linearIssue("ENG-1")))
            assert.equal(created.directory, ".piton/.local/tasks/ENG-1-work")
            const status = created.status
            if (status.entity !== "task") assert.fail("expected a Task")
            assert.deepEqual(status.parent, { kind: "none" })
            const directory = path.join(cwd, created.directory)
            for (const [candidate, error] of [
                [{ ...status, design: "0".repeat(64) }, /a standalone Task has no design/],
                [
                    { ...status, parent: { kind: "epic", epic: { provider: "linear", projectId: "p" } } },
                    /parent is the Epic Project p, but it lives under/,
                ],
            ] as const) {
                writeMetadata(cwd, created.directory, candidate)
                assert.throws(() => readEntityStatus(directory), error)
            }

            // An Epic Task can't be local: its Epic created its issue.
            const rollout = await init(
                cwd,
                epic("Rollout", {
                    projectId: "project-uuid",
                    projectUrl: "https://linear.app/example/project/rollout",
                    createdAt: "2026-10-02T09:00:00.000Z",
                }),
            )
            const local = path.join(rollout.directory, "tasks", "20261002-local")
            fs.mkdirSync(path.join(cwd, local), { recursive: true })
            writeMetadata(cwd, local, {
                ...status,
                slug: "local",
                branch: { ...status.branch, name: "20261002-local" },
                tracker: { provider: "local", createdAt: "2026-10-02T10:00:00.000Z" },
            })
            assert.throws(() => readEntityStatus(path.join(cwd, local)), /an Epic Task can't be local/)
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("rejects metadata that disagrees with its directory, branch, or PR intent", async () => {
        const cwd = temporaryRepository()
        try {
            const created = await init(cwd, standalone("Invariant checks", issue(50)))
            const directory = path.join(cwd, created.directory)
            const status = created.status
            if (status.entity !== "task") assert.fail("expected Task")
            writeEntityStatus(directory, { ...status, kind: "audit" })
            const changed = readEntityStatus(directory)
            assert.equal(changed.entity === "task" ? changed.kind : undefined, "audit")
            const cases: Array<[unknown, RegExp]> = [
                [{ ...status, slug: "Not normalized" }, /slug is not normalized/],
                [{ ...status, slug: "other" }, /directory name does not match/],
                [
                    {
                        ...status,
                        tracker: { ...status.tracker, external: { issueNumber: 51 } },
                    },
                    /directory name does not match/,
                ],
                [{ ...status, branch: { ...status.branch, target: "develop" } }, /branch start\/target/],
                [
                    {
                        ...status,
                        forge: {
                            provider: "github",
                            repository,
                            head: "other",
                            target: "main",
                        },
                    },
                    /forge association/,
                ],
                [{ ...status, state: "inProgress" }, /invalid entity metadata/],
                [{ ...status, id: "TASK-01KDVDNA00" }, /invalid entity metadata/],
            ]
            for (const [candidate, error] of cases) {
                writeMetadata(cwd, created.directory, candidate)
                assert.throws(() => readEntityStatus(directory), error)
            }
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("discovers the recorded default branch from existing work", async () => {
        const cwd = temporaryRepository()
        try {
            await init(cwd, standalone("Earlier", issue(60)))
            git(cwd, ["switch", "-c", "work"])
            git(cwd, ["branch", "another"])
            const later = await init(cwd, standalone("Later", issue(61)))
            assert.equal(later.status.branch.start, "main")
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })
})

describe("Task routing, parent checks, and restore", () => {
    const route = (cwd: string, target: unknown) => routeTask(cwd, parse(RouteTaskValidator, { target }), catalog)
    const queued = { status: "Todo" }
    // A GitHub tracker with the Epic Kind label configured, so a parent issue's labels tell an Epic apart.
    const labelled = (cwd: string) => {
        const tracker = requireTracker(loadIntegrationRoles(cwd), "test")
        writeProjectConfig(cwd, { tracker: { ...tracker, labels: kindLabels() } })
    }
    const reason = (result: unknown) => (result as { reason?: string }).reason
    const strip = <T extends { next?: string }>(result: T) => ({ ...result, next: undefined })

    it("routes GitHub issues by the tracker's parent, asking for each read it needs", async () => {
        const cwd = temporaryRepository()
        try {
            labelled(cwd)
            assert.equal(route(cwd, { kind: "new" }).route, "new")
            const { issue: get, hierarchy } = githubReads({ number: 5 })
            assert.equal(route(cwd, { kind: "issue", issue: get, ...queued }).route, "readRequired")
            assert.equal(route(cwd, { kind: "issue", issue: get, hierarchy, ...queued }).route, "standalone")
            // A sub-issue of an issue that isn't a local Epic needs the parent's labels.
            const sub = githubReads({ number: 5, parent: 9 })
            assert.equal(
                route(cwd, { kind: "issue", issue: sub.issue, hierarchy: sub.hierarchy, ...queued }).route,
                "readRequired",
            )
            assert.equal(
                route(cwd, { kind: "issue", ...githubReads({ number: 5, parent: 9, parentLabels: [] }), ...queued })
                    .route,
                "standalone",
            )
            assert.deepEqual(
                strip(
                    route(cwd, {
                        kind: "issue",
                        ...githubReads({ number: 5, parent: 9, parentLabels: ["Kind: Epic"] }),
                        ...queued,
                    }),
                ),
                { route: "restoreEpic", epic: { provider: "github", issueNumber: 9 }, next: undefined },
            )
            assert.throws(
                () =>
                    route(cwd, {
                        kind: "issue",
                        issue: get,
                        hierarchy: githubReads({ number: 6 }).hierarchy,
                        ...queued,
                    }),
                /issue_number must be 5/,
            )
            // Started in another clone: restore it rather than start it twice. Finished work stops.
            assert.deepEqual(
                strip(route(cwd, { kind: "issue", ...githubReads({ number: 5 }), status: "In Progress" })),
                {
                    route: "restoreTask",
                    parent: { kind: "none" },
                    next: undefined,
                },
            )
            assert.equal(
                reason(route(cwd, { kind: "issue", ...githubReads({ number: 5 }), status: "Done" })),
                "finished",
            )

            // A local Epic: its Tasks route by their plan marker.
            const created = await init(cwd, epic("Rollout", issue(9)))
            const epicPath = path.join(cwd, created.directory, "epic.md")
            const plan = (branch: string) =>
                epicPlan(
                    branch,
                    "default",
                    planTask("Build", "#5", "bugfix"),
                    planTask("Drop", "#6", "chore", "**Status:** `canceled`\n"),
                    planTask("Later"),
                )
            fs.writeFileSync(epicPath, plan(created.status.branch.name))
            const child = (number: number) => ({ kind: "issue", ...githubReads({ number, parent: 9 }), ...queued })
            assert.deepEqual(strip(route(cwd, child(5))), {
                route: "epicTask",
                epicDir: created.directory,
                marker: "#5",
                kind: "bugfix",
                taskTarget: "default",
                next: undefined,
            })
            assert.equal(reason(route(cwd, child(6))), "canceled")
            assert.equal(reason(route(cwd, child(7))), "untagged")
            fs.writeFileSync(epicPath, plan("other"))
            assert.equal(reason(route(cwd, child(5))), "routing")
            fs.writeFileSync(epicPath, plan(created.status.branch.name))

            // Started Tasks resume, by issue or by path; the Task records the tracker's parent.
            const built = await init(cwd, task("Build", created.directory, issue(5), "bugfix"))
            assert.deepEqual(built.status.entity === "task" && built.status.parent, {
                kind: "epic",
                epic: { provider: "github", issueNumber: 9 },
            })
            assert.deepEqual(strip(route(cwd, child(5))), {
                route: "resume",
                entityDir: built.directory,
                next: undefined,
            })
            assert.equal(route(cwd, { kind: "path", path: built.directory }).route, "resume")
            assert.throws(() => route(cwd, { kind: "path", path: created.directory }), /is an Epic/)
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("asks the user when no Epic Kind label tells an Epic from an ordinary parent", async () => {
        const cwd = temporaryRepository()
        try {
            const reads = githubReads({ number: 5, parent: 9, parentLabels: [] })
            assert.equal(reason(route(cwd, { kind: "issue", ...reads, ...queued })), "unclassified")
            await assert.rejects(
                () => init(cwd, { entity: "task", title: "W", slug: "w", request: "W", kind: "chore", reads }),
                /no Epic Kind label/,
            )
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("routes Linear issues by their Project", async () => {
        const cwd = temporaryRepository("linear")
        try {
            assert.equal(
                route(cwd, { kind: "issue", ...linearReads({ identifier: "ENG-1" }), ...queued }).route,
                "standalone",
            )
            const inProject = {
                kind: "issue",
                ...linearReads({ identifier: "ENG-2", projectId: "project-uuid" }),
                ...queued,
            }
            assert.equal(route(cwd, inProject).route, "restoreEpic")
            assert.throws(
                () => route(cwd, { ...inProject, hierarchy: githubReads({ number: 1 }).hierarchy }),
                /for GitHub/,
            )
            const created = await init(
                cwd,
                epic("Rollout", {
                    projectId: "project-uuid",
                    projectUrl: "https://linear.app/example/project/rollout",
                    createdAt: "2026-10-02T09:00:00.000Z",
                }),
            )
            fs.writeFileSync(
                path.join(cwd, created.directory, "epic.md"),
                epicPlan(created.status.branch.name, "epic", planTask("Wire", "ENG-2")),
            )
            const routed = route(cwd, inProject)
            assert.equal(routed.route, "epicTask")
            assert.equal((routed as { taskTarget?: string }).taskTarget, "epic")
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    it("stops a resumed Task whose tracker parent changed", async () => {
        const cwd = temporaryRepository()
        try {
            labelled(cwd)
            const check = (entityDir: string, reads: unknown) =>
                checkTaskParent(cwd, parse(CheckTaskParentValidator, { entityDir, reads }), catalog)
            const solo = await init(cwd, standalone("Solo", issue(11)))
            assert.equal(check(solo.directory, githubReads({ number: 11 })).state, "unchanged")
            const { issue: get } = githubReads({ number: 11 })
            assert.equal(check(solo.directory, { issue: get }).state, "readRequired")
            // Moved under an Epic on the tracker: the Task stops; nothing moves it.
            const moved = check(solo.directory, githubReads({ number: 11, parent: 30, parentLabels: ["Kind: Epic"] }))
            assert.equal(moved.state, "changed")
            assert.match(
                (moved as { detail: string }).detail,
                /started under no parent \(standalone\), but the tracker now shows the Epic #30/,
            )
            assert.ok(fs.existsSync(path.join(cwd, solo.directory)))
            assert.throws(() => check(solo.directory, githubReads({ number: 12 })), /not this Task's issue/)
        } finally {
            fs.rmSync(cwd, { recursive: true, force: true })
        }
    })

    // A clone with an `origin`, and a second clone that pushes a Task branch to it.
    function withRemote(cwd: string): { origin: string; other: string } {
        const origin = fs.mkdtempSync(path.join(os.tmpdir(), "workflow-origin-"))
        git(origin, ["init", "--bare", "-b", "main"])
        git(cwd, ["remote", "add", "origin", origin])
        git(cwd, ["push", "-q", "origin", "main"])
        const other = fs.mkdtempSync(path.join(os.tmpdir(), "workflow-other-"))
        git(other, ["clone", "-q", origin, "."])
        git(other, ["config", "user.email", "test@example.com"])
        git(other, ["config", "user.name", "Test"])
        return { origin, other }
    }
    function pushBranch(other: string, branch: string) {
        git(other, ["switch", "-q", "-c", branch])
        fs.writeFileSync(path.join(other, "work.txt"), `${branch}\n`)
        git(other, ["add", "work.txt"])
        git(other, ["commit", "-q", "-m", "work"])
        git(other, ["push", "-q", "origin", branch])
    }
    const comments = (number: number, bodies: Array<{ id: number; body: string }> = []) => ({
        tool: "mcp__github__issue_read",
        arguments: { method: "get_comments", ...repository, issue_number: number, perPage: 100 },
        response: {
            content: [
                {
                    type: "text",
                    text: JSON.stringify(
                        bodies.map(({ id, body }) => ({
                            id,
                            body,
                            html_url: `https://github.com/example/project/issues/${number}#issuecomment-${id}`,
                        })),
                    ),
                },
            ],
        },
    })

    it("restores a standalone Task from the tracker and its pushed branch, never guessing the slug", async () => {
        const cwd = temporaryRepository()
        const { origin, other } = withRemote(cwd)
        try {
            labelled(cwd)
            // The stored slug was shortened at start: the branch, not the title, names it.
            const reads = githubReads({
                number: 48,
                title: "Merge one thing into another with an optional parent",
                labels: ["Kind: Feature"],
            })
            const restore = (fetched: boolean, pages = [comments(48)]) =>
                restoreTask(cwd, parse(RestoreTaskValidator, { reads, fetched, pages }), catalog)
            assert.deepEqual(await restore(false), { state: "fetchRequired", command: ["git", "fetch", "origin"] })
            await assert.rejects(() => restore(true), /never pushed its work/)
            pushBranch(other, "48-merge-one-thing")
            git(cwd, ["fetch", "-q", "origin"])
            const restored = await restore(true)
            if (restored.state !== "restored") assert.fail("expected a restore")
            assert.equal(restored.directory, ".piton/.local/tasks/48-merge-one-thing")
            assert.equal(restored.branch, "48-merge-one-thing")
            assert.equal(restored.plan, "none")
            const status = readEntityStatus(path.join(cwd, restored.directory))
            assert.deepEqual(status.entity === "task" && [status.kind, status.parent], ["feature", { kind: "none" }])
            assert.equal(
                git(cwd, ["rev-parse", "48-merge-one-thing"]),
                git(cwd, ["rev-parse", "origin/48-merge-one-thing"]),
            )
            assert.equal(fs.existsSync(path.join(cwd, restored.directory, "plan.md")), false)
            await assert.rejects(() => restore(true), /already has a local Task/)
            // Routing finds it now.
            assert.equal(route(cwd, { kind: "issue", ...reads, status: "In Progress" }).route, "resume")

            // A second branch matching the same reference is ambiguous.
            pushBranch(other, "49-one")
            git(other, ["switch", "-q", "main"])
            pushBranch(other, "49-two")
            git(cwd, ["fetch", "-q", "origin"])
            await assert.rejects(
                () =>
                    restoreTask(
                        cwd,
                        parse(RestoreTaskValidator, {
                            reads: githubReads({ number: 49, labels: ["Kind: Chore"] }),
                            fetched: true,
                            pages: [comments(49)],
                        }),
                        catalog,
                    ),
                /several remote branches match 49-/,
            )
            // Without a single Kind label, the user decides.
            await assert.rejects(
                () =>
                    restoreTask(
                        cwd,
                        parse(RestoreTaskValidator, {
                            reads: githubReads({ number: 48 }),
                            fetched: true,
                            pages: [comments(48)],
                        }),
                        catalog,
                    ),
                /already has a local Task/,
            )
        } finally {
            for (const directory of [cwd, origin, other]) fs.rmSync(directory, { recursive: true, force: true })
        }
    })

    it("restores an Epic Task under its local Epic with its published plan", async () => {
        const cwd = temporaryRepository()
        const { origin, other } = withRemote(cwd)
        try {
            labelled(cwd)
            const created = await init(cwd, epic("Rollout", issue(9)))
            fs.writeFileSync(
                path.join(cwd, created.directory, "epic.md"),
                epicPlan(created.status.branch.name, "default", planTask("Build", "#5", "bugfix")),
            )
            pushBranch(other, "5-build")
            git(cwd, ["fetch", "-q", "origin"])
            const plan = "# Plan\n\n## Overview\n\nDo it.\n\n## Steps\n\nOne.\n"
            const source = `${renderProviderBody(plan, { kind: "github-plan-comment" })}\n\n${planPublicationMarker("5b2c8f0e-1d3a-4c6b-9e7f-2a4d6c8e0b1f")}`
            const reads = githubReads({ number: 5, parent: 9 })
            const restored = await restoreTask(
                cwd,
                parse(RestoreTaskValidator, { reads, fetched: true, pages: [comments(5, [{ id: 77, body: source }])] }),
                catalog,
            )
            if (restored.state !== "restored") assert.fail("expected a restore")
            assert.equal(restored.directory, path.join(created.directory, "tasks", "5-build"))
            assert.equal(restored.plan, "restored")
            const status = readEntityStatus(path.join(cwd, restored.directory))
            if (status.entity !== "task" || status.tracker.provider !== "github") assert.fail("expected a GitHub Task")
            assert.equal(status.kind, "bugfix")
            assert.equal(status.tracker.planComment.state, "published")
            assert.match(fs.readFileSync(path.join(cwd, restored.directory, "plan.md"), "utf-8"), /Do it\./)

            // A queued sibling isn't restored: it starts. An Epic this clone lacks refuses.
            await assert.rejects(
                () =>
                    restoreTask(
                        cwd,
                        parse(RestoreTaskValidator, {
                            reads: githubReads({ number: 6, parent: 40, parentLabels: ["Kind: Epic"] }),
                            fetched: true,
                            pages: [],
                        }),
                        catalog,
                    ),
                /restore the Epic first/,
            )
        } finally {
            for (const directory of [cwd, origin, other]) fs.rmSync(directory, { recursive: true, force: true })
        }
    })
})
