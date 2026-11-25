import "./host-runtime.ts"
import { writeProjectConfig } from "./config-fixture.ts"

import assert from "node:assert/strict"
import * as cp from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { describe, it } from "node:test"

import { workflowMcpCatalog } from "./mcp-catalog-fixture.ts"
import { kindLabels, linearKindGroup, linearPhases, phaseValues } from "./workflow-phase-fixtures.ts"
import { acceptEpicTasks } from "../workflows/extensions/__lib/epic-acceptance.ts"
import { readEntityStatus, writeEntityStatus } from "../workflows/extensions/__lib/entity.ts"
import { epicTaskList, parseEpicRouting, parseEpicTasks } from "../workflows/extensions/__lib/epic-tasks.ts"
import { prepareArtifactPersistence } from "../workflows/extensions/__lib/project-config.ts"
import { initializeEntity } from "../workflows/extensions/init/entity.ts"
import { restoreEpic, restoreEpicPlanSource } from "../workflows/extensions/init/restore.ts"
import { renderProviderBody, restoreProviderSource } from "../workflows/extensions/integrations/projection.ts"
import {
    childMoves,
    type EpicTaskIssueStep,
    type GitHubEpicChild,
    type LinearEpicChild,
    orderLinearEpicIssues,
    planChildOrder,
    planEpicTaskIssues,
    recordEpicTaskIssue,
} from "../workflows/extensions/integrations/epic-task-issues.ts"
import { inspectTaskDesign, recordTaskDesign } from "../workflows/extensions/integrations/task-design.ts"

const repository = { owner: "example", repo: "app" }
const statuses = {
    backlog: "Backlog",
    todo: "Todo",
    inProgress: "In Progress",
    inReview: "In Review",
    done: "Done",
    canceled: "Canceled",
}
const tools = workflowMcpCatalog("github", "linear")

function git(cwd: string, ...args: string[]) {
    return cp.execFileSync("git", args, { cwd, encoding: "utf-8" }).trim()
}

function repo(tracker: "github" | "linear", labels?: unknown) {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), "epic-task-issues-"))
    git(root, "init", "-qb", "main")
    git(root, "config", "user.email", "test@example.com")
    git(root, "config", "user.name", "Test")
    fs.writeFileSync(path.join(root, ".gitignore"), "/.piton/.local\n")
    git(root, "add", ".")
    git(root, "commit", "-qm", "base")
    fs.mkdirSync(path.join(root, ".piton"))
    writeProjectConfig(root, {
        branchFormat: "identifier-title",
        tracker:
            tracker === "github"
                ? {
                      provider: "github",
                      mcpServer: "github",
                      repository,
                      project: { owner: "example", ownerType: "user", number: 3 },
                      labels: labels ?? kindLabels(),
                      fields: {
                          status: { field: "Status", values: statuses },
                          phase: { field: "Phase", values: { ...phaseValues } },
                          priority: { scope: "project", field: "Priority", values: ["High", "Low"] },
                      },
                  }
                : {
                      provider: "linear",
                      mcpServer: "linear",
                      team: "Engineering",
                      phase: linearPhases(),
                      ...(labels ? { labels } : {}),
                      statuses: { issues: statuses, projects: statuses },
                  },
        forge: { provider: "github", mcpServer: "github", repository },
    })
    prepareArtifactPersistence(root)
    const epic = initializeEntity(
        {
            entity: "epic",
            title: "Rollout",
            slug: "rollout",
            request: "Roll out",
            object:
                tracker === "github"
                    ? { issueId: 100, issueNumber: 10, issueUrl: "https://github.com/example/app/issues/10" }
                    : {
                          projectId: "project-uuid",
                          projectUrl: "https://linear.app/example/project/rollout",
                          createdAt: "2026-10-01T09:00:00.000Z",
                      },
        },
        { cwd: root },
    )
    const epicPath = path.join(root, epic.directory, "epic.md")
    // Writes epic.md with its Task list accepted, as accept_epic_tasks leaves it.
    const write = (content: string) => {
        fs.writeFileSync(epicPath, content)
        const status = readEntityStatus(path.join(root, epic.directory))
        writeEntityStatus(path.join(root, epic.directory), {
            ...status,
            acceptedTasks: epicTaskList(parseEpicTasks(content)),
        } as typeof status)
    }
    return { root, directory: epic.directory, epicPath, branch: epic.status.branch.name, write }
}

// `extra` holds the optional `**Labels:**` and `**Status:**` lines. Only an untagged Task carries its design.
const section = (title: string, marker?: string, extra = "") =>
    `### ${marker ? `[${marker}] ` : ""}${title}\n\n**Kind:** \`feature\`\n${extra}**Summary:** ${title} summary.\n` +
    (marker ? "" : `\n${title} outcome.\n`)
const plan = (...sections: string[]) => `# Epic\n\n## Overview\n\nPlan.\n\n## Tasks\n\n${sections.join("\n")}`
// A queued GitHub sub-issue; its numeric issue ID is derived from the number.
const child = (marker: string, title: string, facts: Partial<GitHubEpicChild> = {}): GitHubEpicChild => ({
    marker,
    issueId: 1000 + Number(marker.slice(1)),
    title,
    onBoard: true,
    closed: false,
    status: "Todo",
    priority: "High",
    ...facts,
})
const linearChild = (marker: string, title: string, facts: Partial<LinearEpicChild> = {}): LinearEpicChild => ({
    marker,
    title,
    status: "Todo",
    priority: "High",
    ...facts,
})
const github = (...issues: GitHubEpicChild[]) => ({ provider: "github" as const, issues })
const linear = (...issues: LinearEpicChild[]) => ({ provider: "linear" as const, issues })

describe("Epic Task format", () => {
    it("parses a tagged Task's summary and an untagged Task's multi-paragraph design as one Task each", () => {
        const tasks = parseEpicTasks(
            [
                "# Epic",
                "",
                "### Not a Task",
                "",
                "Context.",
                "",
                "## Tasks",
                "",
                "### [#12] Build the parser",
                "",
                "**Kind:** `bugfix`",
                "**Summary:** Parse the input.",
                "",
                "### Wire the CLI",
                "",
                "**Kind:** `chore`",
                "**Summary:** Expose the parser.",
                "",
                "Status: a design paragraph that reads like a field.",
                "",
                "First paragraph.",
                "",
                "1. nested",
                "2. list",
                "",
                "```markdown",
                "### Not a heading",
                "## Not a section",
                "```",
                "",
                "## Delivery",
                "",
                "Not part of a Task.",
            ].join("\n"),
        )
        assert.deepEqual(tasks[0], {
            issue: "tagged",
            title: "Build the parser",
            marker: "#12",
            kind: "bugfix",
            labels: {},
            summary: "Parse the input.",
            canceled: false,
        })
        assert.equal(tasks[1].issue, "pending")
        assert.equal(tasks[1].summary, "Expose the parser.")
        assert.match(
            tasks[1].issue === "pending" ? tasks[1].design : "",
            /^Status: a design[\s\S]*First paragraph\.[\s\S]*### Not a heading[\s\S]*```$/,
        )
    })

    it("rejects malformed Tasks", () => {
        assert.throws(() => parseEpicTasks(plan("### A\n\nNo kind.\n")), /first body line must be ``\*\*Kind/)
        assert.throws(
            () => parseEpicTasks(plan("### A\n\n**Kind:** `epic`\n**Summary:** A.\n\nText.\n")),
            /invalid Kind/,
        )
        assert.throws(
            () => parseEpicTasks(plan("### A\n\n**Kind:** chore\n**Summary:** A.\n\nText.\n")),
            /one code span/,
        )
        assert.throws(() => parseEpicTasks(plan("### A\n\n**Kind:** `chore`\n\nText.\n")), /needs a one-line/)
        assert.throws(
            () => parseEpicTasks(plan("### A\n\n**Kind:** `chore`\n**Summary:**\n\nText.\n")),
            /needs a one-line/,
        )
        assert.throws(
            () => parseEpicTasks(plan("### A\n\n**Kind:** `chore`\n**Summary:** A.\n")),
            /design after `\*\*Summary:\*\*` is empty/,
        )
        // A tagged Task's design lives in its issue; the Epic keeps only the summary.
        assert.throws(
            () => parseEpicTasks(plan("### [#1] A\n\n**Kind:** `chore`\n**Summary:** A.\n\nOld design.\n")),
            /design belongs in the Task issue/,
        )
        assert.throws(
            () => parseEpicTasks(plan(section("A", undefined, "**Status:** `canceled`\n"))),
            /already has an issue/,
        )
        assert.throws(() => parseEpicTasks(plan(section("A", "#1", "**Status:** `done`\n"))), /only allowed Status/)
        assert.throws(() => parseEpicTasks(plan(section("A", "#1", "**Status:** canceled\n"))), /only allowed Status/)
        // The plain `Name: value` form is rejected with the bold form as the hint.
        for (const old of [
            "### A\n\nKind: chore\nSummary: A.\n\nText.\n",
            "### A\n\n**Kind:** `chore`\nLabels: area=fe\n**Summary:** A.\n\nText.\n",
            "### [#1] A\n\n**Kind:** `chore`\nStatus: canceled\n**Summary:** A.\n",
            "### A\n\n**Kind:** `chore`\nSummary: A.\n\nText.\n",
        ])
            assert.throws(
                () => parseEpicTasks(plan(old)),
                /are written `\*\*(Kind|Labels|Status|Summary):\*\* <value>`/,
            )
        assert.throws(() => parseEpicTasks(plan(section("A"), section("A"))), /duplicate Task title/)
        assert.throws(() => parseEpicTasks(plan(section("A", "#1"), section("B", "#1"))), /duplicate Task marker #1/)
        assert.throws(() => parseEpicTasks(plan(section("[x] A"))), /invalid Task marker/)
        assert.throws(() => parseEpicTasks(`# Epic\n\n${section("Stray")}`), /outside `## Tasks`/)
        // A plan without `## Tasks` simply has no Tasks.
        assert.deepEqual(parseEpicTasks("# Epic\n\n## Ordered Tasks\n\n1. **A** — a.\n"), [])
        const canceled = parseEpicTasks(plan(section("A", "#1", "**Status:** `canceled`\n")))[0]
        assert.equal(canceled.issue === "tagged" && canceled.canceled, true)
    })
})

describe("Epic Task issue processing", () => {
    it("creates untagged Tasks, tags them, and finishes on a rerun", () => {
        const { root, directory, epicPath, write } = repo("github")
        try {
            write(plan(section("Parser"), section("CLI")))
            const first = planEpicTaskIssues(
                root,
                { entityDir: directory, epicPriority: "High", children: github() },
                tools,
            )
            assert.equal(first.state, "steps-needed")
            const steps = first.state === "steps-needed" ? first.steps : []
            assert.deepEqual(
                steps.map((step) => [step.title, step.effect]),
                [
                    ["Parser", "create"],
                    ["CLI", "create"],
                ],
            )
            const create = steps[0].effect === "create" ? steps[0].mutation : undefined
            assert.deepEqual(create?.arguments, {
                method: "create",
                ...repository,
                title: "Parser",
                body: "Parser outcome.",
                labels: ["Kind: Feature"],
                parent_issue_number: 10,
            })
            const evidence = {
                tool: create!.tool,
                arguments: create!.arguments,
                response: { id: "5", url: "https://github.com/example/app/issues/11" },
            }
            assert.deepEqual(
                recordEpicTaskIssue(root, { entityDir: directory, title: "Parser", marker: "#11", evidence }, tools),
                {
                    state: "tagged",
                    title: "Parser",
                    marker: "#11",
                },
            )
            assert.throws(
                () => recordEpicTaskIssue(root, { entityDir: directory, title: "CLI", marker: "#12", evidence }, tools),
                /another Task title/,
            )
            assert.throws(
                () => recordEpicTaskIssue(root, { entityDir: directory, title: "CLI", marker: "#11" }, tools),
                /already contains Task marker #11/,
            )
            assert.equal(
                recordEpicTaskIssue(root, { entityDir: directory, title: "Parser", marker: "#11" }, tools).state,
                "unchanged",
            )
            // Tagging drops the design, now in the issue body, and keeps the fields and summary.
            assert.equal(fs.readFileSync(epicPath, "utf-8"), plan(section("Parser", "#11"), section("CLI")))

            // The session stopped after #12 was created but before its tag; the rerun's
            // title lookup tags it instead of creating a duplicate.
            const retry = planEpicTaskIssues(
                root,
                {
                    entityDir: directory,
                    epicPriority: "High",
                    children: github(child("#11", "Parser"), child("#12", "CLI")),
                },
                tools,
            )
            assert.deepEqual(retry, { state: "steps-needed", steps: [{ title: "CLI", marker: "#12", effect: "tag" }] })
            recordEpicTaskIssue(root, { entityDir: directory, title: "CLI", marker: "#12" }, tools)
            const done = planEpicTaskIssues(
                root,
                {
                    entityDir: directory,
                    epicPriority: "High",
                    children: github(child("#11", "Parser"), child("#12", "CLI")),
                },
                tools,
            )
            assert.deepEqual(done, { state: "complete" })
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    it("skips children already tagged in the plan and stops on ambiguous titles", () => {
        const { root, directory, write } = repo("github")
        try {
            // #11 was renamed in the plan but kept its original issue title.
            write(plan(section("Parser core", "#11"), section("Parser")))
            const result = planEpicTaskIssues(
                root,
                { entityDir: directory, epicPriority: "High", children: github(child("#11", "Parser")) },
                tools,
            )
            assert.equal(result.state === "steps-needed" && result.steps[0].effect, "create")
            const ambiguous = planEpicTaskIssues(
                root,
                {
                    entityDir: directory,
                    epicPriority: "High",
                    children: github(child("#11", "Parser"), child("#20", "Parser"), child("#21", "Parser")),
                },
                tools,
            )
            assert.equal(ambiguous.state, "blocked")
            assert.match(ambiguous.state === "blocked" ? ambiguous.reason : "", /#20, #21/)
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    it("repairs partial board setup and checks the lifecycle of a title match", () => {
        const { root, directory, write } = repo("github")
        try {
            write(plan(section("Parser", "#11"), section("CLI")))
            const repair = planEpicTaskIssues(
                root,
                {
                    entityDir: directory,
                    epicPriority: "High",
                    children: github(
                        child("#11", "Parser", { onBoard: false, status: undefined, priority: undefined }),
                    ),
                },
                tools,
            )
            assert.equal(repair.state, "steps-needed")
            assert.deepEqual(repair.state === "steps-needed" ? repair.steps.map((step) => step.effect) : [], [
                "addToProject",
                "setStatus",
                "setPriority",
                "create",
            ])
            // Auto-added as Backlog with no Priority (Project automation): moved to Todo with the Epic's Priority.
            const automated = planEpicTaskIssues(
                root,
                {
                    entityDir: directory,
                    epicPriority: "High",
                    children: github(child("#11", "Parser", { status: "Backlog", priority: undefined })),
                },
                tools,
            )
            const automatedSteps = automated.state === "steps-needed" ? automated.steps : []
            assert.deepEqual(
                automatedSteps.map((step) => step.effect),
                ["setStatus", "setPriority", "create"],
            )
            assert.deepEqual(
                automatedSteps[0].effect === "setStatus" && automatedSteps[0].mutation.arguments.updated_field,
                { name: "Status", value: "Todo" },
            )
            // A Priority already set on the Task is kept, and Todo needs no change.
            const kept = planEpicTaskIssues(
                root,
                {
                    entityDir: directory,
                    epicPriority: "High",
                    children: github(child("#11", "Parser", { status: "Todo", priority: "Low" })),
                },
                tools,
            )
            assert.deepEqual(kept.state === "steps-needed" ? kept.steps.map((step) => step.effect) : [], ["create"])
            // A Task canceled on the tracker is never pulled back into Todo.
            const canceled = planEpicTaskIssues(
                root,
                {
                    entityDir: directory,
                    epicPriority: "High",
                    children: github(child("#11", "Parser", { closed: true, status: "Canceled", priority: undefined })),
                },
                tools,
            )
            assert.deepEqual(canceled.state === "steps-needed" ? canceled.steps.map((step) => step.effect) : [], [
                "create",
            ])

            const started = planEpicTaskIssues(
                root,
                {
                    entityDir: directory,
                    epicPriority: "High",
                    children: github(child("#11", "Parser"), child("#13", "CLI", { status: "In Progress" })),
                },
                tools,
            )
            assert.equal(started.state, "blocked")
            assert.equal(started.state === "blocked" && started.task, "CLI")
            // A started title match is never tagged.
            assert.deepEqual(started.state === "blocked" ? started.steps : undefined, [])
            const missing = planEpicTaskIssues(
                root,
                { entityDir: directory, epicPriority: "High", children: github() },
                tools,
            )
            assert.match(missing.state === "blocked" ? missing.reason : "", /#11 is not a child issue/)
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    it("cancels queued and started Tasks, never a Done one, and finishes an interrupted cancel", () => {
        const { root, directory, write } = repo("github")
        try {
            write(plan(section("Dropped", "#14", "**Status:** `canceled`\n")))
            const run = (facts: Partial<GitHubEpicChild>) =>
                planEpicTaskIssues(
                    root,
                    { entityDir: directory, epicPriority: "High", children: github(child("#14", "Dropped", facts)) },
                    tools,
                )
            const queued = run({ status: "Todo" })
            assert.deepEqual(queued.state === "steps-needed" ? queued.steps.map((step) => step.effect) : [], [
                "cancel",
                "close",
            ])
            const interrupted = run({ status: "Canceled", closed: false })
            assert.deepEqual(interrupted.state === "steps-needed" ? interrupted.steps.map((step) => step.effect) : [], [
                "close",
            ])
            assert.deepEqual(run({ status: "Canceled", closed: true }), { state: "complete" })
            for (const status of ["In Progress", "In Review"]) {
                const started = run({ status })
                assert.deepEqual(started.state === "steps-needed" ? started.steps.map((step) => step.effect) : [], [
                    "cancel",
                    "close",
                ])
            }
            const done = run({ status: "Done", closed: true })
            assert.equal(done.state, "blocked")
            assert.match(done.state === "blocked" ? done.reason : "", /Done Task can't be canceled/)
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    it("moves a Linear queued Task from Backlog to Todo and fills a missing Priority", () => {
        const { root, directory, write } = repo("linear")
        try {
            write(plan(section("Parser", "ENG-4")))
            const run = (facts: Partial<LinearEpicChild>) =>
                planEpicTaskIssues(
                    root,
                    {
                        entityDir: directory,
                        epicPriority: "High",
                        children: linear(linearChild("ENG-4", "Parser", facts)),
                    },
                    tools,
                )
            const backlog = run({ status: "Backlog", priority: undefined })
            assert.deepEqual(
                backlog.state === "steps-needed" &&
                    backlog.steps[0].effect !== "tag" &&
                    backlog.steps[0].mutation.arguments,
                {
                    id: "ENG-4",
                    state: "Todo",
                    priority: 2,
                },
            )
            assert.deepEqual(run({ status: "Todo", priority: "Low" }), { state: "complete" })
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    it("creates Task issues with the kind label and the Task's Labels selection on both trackers", () => {
        const area = {
            appliesTo: ["deliverable"],
            cardinality: "oneOrMore",
            values: { fe: { name: "Area: FE", color: "#1D76DB", description: "" } },
        }
        for (const provider of ["github", "linear"] as const) {
            const groups = {
                kind: provider === "linear" ? linearKindGroup() : kindLabels().groups.kind,
                area: { ...area, ...(provider === "linear" ? { scope: "workspace" } : {}) },
            }
            const { root, directory, write } = repo(provider, { groups })
            try {
                write(plan(section("Parser", undefined, "**Labels:** `area=fe`\n")))
                const children = provider === "github" ? github() : linear()
                const result = planEpicTaskIssues(root, { entityDir: directory, epicPriority: "High", children }, tools)
                const create =
                    result.state === "steps-needed" && result.steps[0].effect === "create" ? result.steps[0] : undefined
                assert.deepEqual(create?.mutation.arguments.labels, ["Area: FE", "Kind: Feature"], provider)

                write(plan(section("Parser")))
                const blocked = planEpicTaskIssues(
                    root,
                    { entityDir: directory, epicPriority: "High", children },
                    tools,
                )
                assert.equal(blocked.state, "blocked", `${provider}: a required group needs a value`)
                assert.match(blocked.state === "blocked" ? blocked.reason : "", /label group "area" is oneOrMore/)
            } finally {
                fs.rmSync(root, { recursive: true, force: true })
            }
        }
    })

    it("creates Linear issues in the Epic's Project with Todo and Priority", () => {
        const { root, directory, epicPath, write } = repo("linear")
        try {
            write(plan(section("Parser"), section("Dropped", "ENG-3", "**Status:** `canceled`\n")))
            const result = planEpicTaskIssues(
                root,
                { entityDir: directory, epicPriority: "High", children: linear(linearChild("ENG-3", "Dropped")) },
                tools,
            )
            assert.equal(result.state, "steps-needed")
            const steps = result.state === "steps-needed" ? result.steps : []
            assert.deepEqual(steps[0].effect === "create" && steps[0].mutation.arguments, {
                team: "Engineering",
                title: "Parser",
                description: "Parser outcome.",
                project: "project-uuid",
                state: "Todo",
                priority: 2,
            })
            assert.deepEqual(steps[1].effect === "cancel" && steps[1].mutation.arguments, {
                id: "ENG-3",
                state: "Canceled",
            })
            const create = steps[0].effect === "create" ? steps[0].mutation : undefined
            assert.equal(
                recordEpicTaskIssue(
                    root,
                    {
                        entityDir: directory,
                        title: "Parser",
                        marker: "ENG-4",
                        evidence: { tool: create!.tool, arguments: create!.arguments, response: { id: "ENG-4" } },
                    },
                    tools,
                ).state,
                "tagged",
            )
            assert.match(fs.readFileSync(epicPath, "utf-8"), /^### \[ENG-4\] Parser$/m)
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    it("orders GitHub sub-issues to match the plan once every Task is settled", () => {
        const { root, directory, write } = repo("github")
        // Applies reorder steps to a sub-issue list as GitHub's reprioritize does.
        const apply = (issues: GitHubEpicChild[], steps: readonly EpicTaskIssueStep[]) => {
            const order = [...issues]
            for (const step of steps) {
                if (step.effect !== "reorder") throw new Error(`unexpected ${step.effect} step`)
                const args = step.mutation.arguments
                const moved = order.splice(
                    order.findIndex((issue) => issue.issueId === args.sub_issue_id),
                    1,
                )[0]
                const anchor = order.findIndex((issue) => issue.issueId === (args.before_id ?? args.after_id))
                order.splice(args.before_id === undefined ? anchor + 1 : anchor, 0, moved)
            }
            return order
        }
        try {
            // The issue's example: four existing issues linked first, then six created after them,
            // while the plan lists the six new Tasks first.
            const fresh = ["A", "B", "C", "D", "E", "F"].map((title, index) => child(`#${21 + index}`, title))
            const existing = ["W", "X", "Y", "Z"].map((title, index) => child(`#${11 + index}`, title))
            write(plan(...[...fresh, ...existing].map((issue) => section(issue.title, issue.marker))))
            const run = (issues: GitHubEpicChild[]) =>
                planEpicTaskIssues(
                    root,
                    { entityDir: directory, epicPriority: "High", children: github(...issues) },
                    tools,
                )
            const linked = [...existing, ...fresh]
            const result = run(linked)
            assert.equal(result.state, "steps-needed")
            const steps = result.state === "steps-needed" ? result.steps : []
            assert.ok(steps.every((step) => step.effect === "reorder"))
            assert.deepEqual(steps[0].effect === "reorder" && steps[0].mutation, {
                tool: "mcp__github__sub_issue_write",
                arguments: {
                    method: "reprioritize",
                    ...repository,
                    issue_number: 10,
                    sub_issue_id: 1021,
                    before_id: 1011,
                },
            })
            const ordered = apply(linked, steps)
            assert.deepEqual(
                ordered.map((issue) => issue.title),
                ["A", "B", "C", "D", "E", "F", "W", "X", "Y", "Z"],
            )
            assert.deepEqual(run(ordered), { state: "complete" })

            // While a Task still needs its issue, ordering waits for the run that settles it.
            write(plan(section("New"), ...[...fresh, ...existing].map((issue) => section(issue.title, issue.marker))))
            const pending = run(linked)
            assert.deepEqual(pending.state === "steps-needed" ? pending.steps.map((step) => step.effect) : [], [
                "create",
            ])
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    it("puts canceled Tasks and children outside the plan after the plan's Tasks, in their current order", () => {
        assert.deepEqual(
            planChildOrder(
                parseEpicTasks(
                    plan(section("A", "#3"), section("Dropped", "#1", "**Status:** `canceled`\n"), section("B", "#2")),
                ),
                ["#9", "#1", "#2", "#8", "#3"],
            ),
            ["#3", "#2", "#9", "#1", "#8"],
        )
        assert.deepEqual(childMoves(["#1", "#2", "#3"], ["#1", "#2", "#3"]), [])
        assert.deepEqual(childMoves(["#2", "#1", "#3"], ["#1", "#2", "#3"]), [{ marker: "#1", before: "#2" }])
        assert.deepEqual(childMoves(["#1", "#3", "#2"], ["#1", "#2", "#3"]), [{ marker: "#2", after: "#1" }])
    })

    it("returns no Linear reorder steps, and rejects children from the other tracker", () => {
        const { root, directory, write } = repo("linear")
        try {
            write(plan(section("A", "ENG-2"), section("B", "ENG-1")))
            const run = (children: ReturnType<typeof linear> | ReturnType<typeof github>) =>
                planEpicTaskIssues(root, { entityDir: directory, epicPriority: "High", children }, tools)
            assert.deepEqual(run(linear(linearChild("ENG-1", "B"), linearChild("ENG-2", "A"))), { state: "complete" })
            assert.throws(() => run(github()), /children are github issues, but the tracker is linear/)
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })

    it("orders a Linear Epic's Project issues through sortOrder with the registered server's credential", async () => {
        const { root, directory, write } = repo("linear")
        fs.writeFileSync(
            path.join(root, ".piton/mcp.json"),
            JSON.stringify({
                mcpServers: {
                    linear: { url: "https://mcp.linear.app/mcp", headers: { Authorization: "Bearer ${ORDER_KEY}" } },
                },
            }),
        )
        // A fake Linear GraphQL API over one Project's issues, paged two at a time.
        const issues = new Map<string, number>()
        const writes: Array<{ id: string; sortOrder: number }> = []
        const authorizations = new Set<string>()
        const options = {
            projectTrusted: true,
            env: { ORDER_KEY: "linear-key" } as NodeJS.ProcessEnv,
            fetch: (async (_url: string, init: RequestInit) => {
                authorizations.add((init.headers as Record<string, string>).Authorization)
                const { query, variables } = JSON.parse(String(init.body))
                if (query.includes("issueUpdate")) {
                    writes.push(variables)
                    issues.set(variables.id, variables.sortOrder)
                    return Response.json({ data: { issueUpdate: { success: true } } })
                }
                assert.equal(variables.id, "project-uuid")
                const all = [...issues.keys()]
                const start = variables.after ? Number(variables.after) : 0
                const nodes = all.slice(start, start + 2).map((id) => ({
                    id,
                    identifier: id.replace("uuid-", ""),
                    sortOrder: issues.get(id),
                }))
                const hasNextPage = start + 2 < all.length
                return Response.json({
                    data: {
                        project: {
                            issues: {
                                nodes,
                                pageInfo: { hasNextPage, endCursor: hasNextPage ? String(start + 2) : null },
                            },
                        },
                    },
                })
            }) as unknown as typeof fetch,
        }
        const order = () => orderLinearEpicIssues(root, { entityDir: directory }, tools, options)
        const sorted = () =>
            [...issues.entries()].sort((left, right) => left[1] - right[1]).map(([id]) => id.replace("uuid-", ""))
        try {
            // The plan's first Task sits last, and its second sits behind an issue outside the plan.
            write(
                plan(
                    section("A", "ENG-3"),
                    section("B", "ENG-1"),
                    section("Dropped", "ENG-4", "**Status:** `canceled`\n"),
                ),
            )
            for (const [marker, position] of [
                ["ENG-4", 1],
                ["ENG-9", 2],
                ["ENG-1", 3],
                ["ENG-3", 4],
            ] as const)
                issues.set(`uuid-${marker}`, position)
            assert.deepEqual(await order(), { state: "ordered", moved: ["ENG-3", "ENG-1"] })
            // ENG-3 moved to the front, below the first position; ENG-1 between ENG-3 and ENG-4.
            assert.deepEqual(writes, [
                { id: "uuid-ENG-3", sortOrder: 0 },
                { id: "uuid-ENG-1", sortOrder: 0.5 },
            ])
            assert.deepEqual(sorted(), ["ENG-3", "ENG-1", "ENG-4", "ENG-9"])
            assert.deepEqual([...authorizations], ["linear-key"])
            writes.length = 0
            assert.deepEqual(await order(), { state: "unchanged" })
            assert.deepEqual(writes, [])
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
        const other = repo("github")
        try {
            other.write(plan(section("A", "#1")))
            await assert.rejects(
                orderLinearEpicIssues(other.root, { entityDir: other.directory }, tools, options),
                /orders only Linear Epics/,
            )
        } finally {
            fs.rmSync(other.root, { recursive: true, force: true })
        }
    })

    it("accepts only a reviewed Task list, and processes issues only for the accepted one", () => {
        const { root, directory, epicPath } = repo("github")
        const epicDir = path.join(root, directory)
        const reviewed = plan(section("Parser"), section("CLI"))
        // A saved plan round as review_epic leaves it, with the text it reviewed.
        const planRound = (
            round: number,
            signoff: "approved" | "blocked",
            candidate: string,
            pass: Record<string, unknown> = { kind: "review" },
        ) => {
            const roundDir = path.join(epicDir, "reviews", `plan-${round.toString().padStart(3, "0")}`)
            fs.mkdirSync(roundDir, { recursive: true })
            fs.writeFileSync(path.join(roundDir, "pass.json"), JSON.stringify(pass))
            fs.writeFileSync(path.join(roundDir, "outcome.json"), JSON.stringify({ exitCode: 0, signoff }))
            fs.writeFileSync(path.join(roundDir, "candidate.md"), candidate)
        }
        const processIssues = () =>
            planEpicTaskIssues(root, { entityDir: directory, epicPriority: "High", children: github() }, tools)
        try {
            fs.writeFileSync(epicPath, reviewed)
            assert.throws(() => acceptEpicTasks(epicDir), /no plan review has run/)
            // An Epic never accepted can't create issues.
            assert.throws(processIssues, /isn't accepted yet/)
            planRound(1, "blocked", reviewed)
            assert.throws(() => acceptEpicTasks(epicDir), /plan-001 blocked/)
            planRound(2, "approved", reviewed, { kind: "verification", of: 1 })
            // A Task added after the review isn't accepted.
            fs.writeFileSync(epicPath, plan(section("Parser"), section("CLI"), section("Docs")))
            assert.throws(() => acceptEpicTasks(epicDir), /changed after plan-002[\s\S]*Task 3: added "Docs"/)
            // Rewording a design or summary keeps the reviewed list.
            fs.writeFileSync(
                epicPath,
                reviewed.replace("CLI outcome.", "CLI outcome, reworded.").replace("CLI summary.", "CLI, reworded."),
            )
            assert.equal(acceptEpicTasks(epicDir).state, "accepted")
            assert.equal(processIssues().state, "steps-needed")

            // After acceptance, a reorder or a Kind change refuses until it is reviewed and accepted.
            fs.writeFileSync(epicPath, plan(section("CLI"), section("Parser")))
            assert.throws(processIssues, /differs from the accepted one[\s\S]*Task 1: "Parser"/)
            fs.writeFileSync(
                epicPath,
                reviewed.replace("### CLI\n\n**Kind:** `feature`", "### CLI\n\n**Kind:** `chore`"),
            )
            assert.throws(processIssues, /Task 2: "CLI" · feature · planned → "CLI" · chore · planned/)
            // Tagging a Task with its issue marker and editing design text need no acceptance.
            fs.writeFileSync(epicPath, plan(section("Parser", "#11"), `${section("CLI")}\nMore detail.\n`))
            assert.equal(
                planEpicTaskIssues(
                    root,
                    { entityDir: directory, epicPriority: "High", children: github(child("#11", "Parser")) },
                    tools,
                ).state,
                "steps-needed",
            )
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })
})

const token = "0f6c1b8e-6a51-4d2c-9f3e-2b7a4c5d6e7f"
const accepted = (branch: string) =>
    [
        "# Rollout",
        "",
        "## Overview",
        "",
        "Why it matters.",
        "",
        "### Plan highlights",
        "",
        "- Ship it.",
        "",
        "## Task delivery strategy",
        "",
        "**Task target:** `default`",
        `**Epic branch:** \`${branch}\``,
        "",
        "Independent Tasks.",
        "",
        "## Tasks",
        "",
        "### [#11] Parser",
        "",
        "**Kind:** `feature`",
        "**Summary:** Parse the input.",
        "",
        "### CLI",
        "",
        "**Kind:** `chore`",
        "**Summary:** Wire it.",
        "",
        "1. nested",
        "   - list",
        "",
        "```md",
        "## not a section",
        "```",
        "",
    ].join("\n")

describe("Epic routing and restore", () => {
    it("parses routing lines and rejects missing or invalid ones", () => {
        assert.deepEqual(parseEpicRouting(accepted("10-rollout")), { taskTarget: "default", epicBranch: "10-rollout" })
        assert.throws(() => parseEpicRouting("# E\n\n## Tasks\n"), /no `## Task delivery strategy`/)
        assert.throws(
            () => parseEpicRouting(accepted("x").replace("`default`", "`main`")),
            /needs ``\*\*Task target:\*\* `default` `` or/,
        )
        assert.throws(
            () => parseEpicRouting(accepted("x").replace("`default`", "default")),
            /needs ``\*\*Task target:\*\* `default` `` or/,
        )
        assert.throws(
            () => parseEpicRouting(accepted("x").replace("**Epic branch:** `x`\n", "")),
            /needs ``\*\*Epic branch:\*\*/,
        )
        assert.throws(
            () => parseEpicRouting(accepted("x").replace("**Task target:** `default`", "Task target: default")),
            /are written `\*\*Task target:\*\* <value>`/,
        )
    })

    it("round-trips accepted epic.md through GitHub and Linear plan rendering", () => {
        const source = accepted("10-rollout")
        for (const kind of ["github-plan-comment", "linear-plan-document"] as const) {
            const footer =
                kind === "github-plan-comment" ? "<sub>Sent via Pi. Model: a/b.</sub>" : "*Sent via Pi. Model: a/b.*"
            // Restore drops the publication marker and attribution; Linear documents need neither.
            const marked = `${renderProviderBody(source, { kind })}\n\n<!-- plan-publication:tok -->\n\n${footer}`
            assert.equal(restoreProviderSource(marked, { kind }, "Rollout"), source)
            assert.equal(restoreProviderSource(renderProviderBody(source, { kind }), { kind }, "Rollout"), source)
        }
        // Linear never returns a `+++` fold.
        assert.throws(
            () =>
                restoreProviderSource(
                    renderProviderBody(source, { kind: "linear-plan-document" }).replaceAll(">>>", "+++"),
                    { kind: "linear-plan-document" },
                    "R",
                ),
            /no Full plan fold/,
        )
    })

    it("restores a GitHub Epic in a clone without its local files", () => {
        const { root, directory, branch } = repo("github")
        try {
            const source = accepted(branch)
            const body = `${renderProviderBody(source, { kind: "github-plan-comment" })}\n\n<!-- plan-publication:${token} -->`
            const comment = (id: number, text: string) => ({
                id,
                body: text,
                html_url: `https://github.com/example/app/issues/10#issuecomment-${id}`,
            })
            const page = (comments: unknown[]) => ({
                tool: "mcp__github__issue_read",
                arguments: { method: "get_comments", ...repository, issue_number: 10, page: 1, perPage: 100 },
                response: comments,
            })
            const epic = {
                tool: "mcp__github__issue_read",
                arguments: { method: "get", ...repository, issue_number: 10 },
                response: {
                    id: 100,
                    number: 10,
                    title: "Rollout",
                    body: "Roll out\n\n<sub>Sent via Pi. Model: x/y.</sub>",
                    html_url: "https://github.com/example/app/issues/10",
                    created_at: "2026-10-01T09:00:00Z",
                },
            }
            const pages = [page([comment(7, "discussion"), comment(8, body)])]

            // An existing local Epic for the same issue blocks restore under any slug.
            assert.throws(() => restoreEpic(root, { slug: "other", epic, pages }, tools), /already exists/)
            const local = path.join(root, directory)
            assert.deepEqual(restoreEpicPlanSource(root, { entityDir: directory, pages }, tools).source, source)

            fs.rmSync(local, { recursive: true })
            assert.throws(
                () =>
                    restoreEpic(
                        root,
                        { slug: "rollout", epic, pages: [page([comment(8, body), comment(9, body)])] },
                        tools,
                    ),
                /found 2 published plans/,
            )
            assert.throws(() => restoreEpic(root, { slug: "rollout", epic, pages: [page([])] }, tools), /found 0/)
            const noTasks = `${renderProviderBody(source.split("\n## Tasks")[0], { kind: "github-plan-comment" })}\n\n<!-- plan-publication:${token} -->`
            assert.throws(
                () => restoreEpic(root, { slug: "rollout", epic, pages: [page([comment(8, noTasks)])] }, tools),
                /no `## Tasks` section/,
            )
            assert.equal(fs.existsSync(local), false)

            // A leftover directory without metadata is never overwritten or removed.
            fs.mkdirSync(local, { recursive: true })
            fs.writeFileSync(path.join(local, "notes.md"), "keep")
            assert.throws(() => restoreEpic(root, { slug: "rollout", epic, pages }, tools), /already exists/)
            assert.equal(fs.readFileSync(path.join(local, "notes.md"), "utf-8"), "keep")
            fs.rmSync(local, { recursive: true })

            // The slug only names the directory; the branch comes from the published plan.
            const restored = restoreEpic(root, { slug: "renamed", epic, pages }, tools)
            assert.equal(restored.directory, ".piton/.local/epics/10-renamed")
            const status = readEntityStatus(path.join(root, restored.directory))
            assert.equal(status.branch.name, branch)
            assert.equal("phase" in status, false)
            assert.deepEqual(status.tracker.provider === "github" && status.tracker.planComment, {
                state: "published",
                token,
                id: 8,
                url: "https://github.com/example/app/issues/10#issuecomment-8",
            })
            assert.equal(fs.readFileSync(path.join(root, restored.directory, "epic.md"), "utf-8"), source)
            // The issue body's publication signature stays out of the brief.
            assert.equal(
                fs.readFileSync(path.join(root, restored.directory, "brief.md"), "utf-8"),
                "# Brief\n\nRoll out\n",
            )
            // The published plan's Task list was accepted before it was published.
            assert.deepEqual(status.entity === "epic" && status.acceptedTasks, [
                { title: "Parser", kind: "feature", labels: {}, status: "planned" },
                { title: "CLI", kind: "chore", labels: {}, status: "planned" },
            ])

            // A restored clone has no Task directories; started Tasks come from the tracker.
            const children = github(
                child("#11", "Parser", { status: "In Progress" }),
                child("#12", "CLI", { status: "Done" }),
            )
            // Tagging drops CLI's design, as record_epic_task_issue does.
            const tagged = `${source.slice(0, source.indexOf("### CLI"))}### [#12] CLI\n\n**Kind:** \`chore\`\n**Summary:** Wire it.\n`
            fs.writeFileSync(path.join(root, restored.directory, "epic.md"), tagged)
            assert.deepEqual(
                planEpicTaskIssues(root, { entityDir: restored.directory, epicPriority: "High", children }, tools),
                { state: "complete" },
            )
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })
})

describe("Linear Epic restore", () => {
    it("restores a Linear Epic from its Project read and attached plan document", () => {
        const { root, directory, branch } = repo("linear")
        try {
            const source = accepted(branch).replace("[#11]", "[ENG-11]")
            const body = `${renderProviderBody(source, { kind: "linear-plan-document" })}\n\n*Sent via Pi. Model: a/b.*`
            const project = {
                tool: "mcp__linear__get_project",
                arguments: { query: "project-uuid" },
                response: {
                    id: "P-ENG-1",
                    uuid: "project-uuid",
                    name: "Rollout",
                    description: "Roll out",
                    url: "https://linear.app/example/project/rollout",
                    createdAt: "2026-10-01T09:00:00.000Z",
                },
            }
            const document = (id: string, content: string, title = "Plan") => ({
                id,
                title,
                url: `https://linear.app/example/document/${id === "document-uuid" ? "plan-slug" : id}`,
                content,
                archivedAt: null,
                issue: null,
                project: { id: "P-ENG-1" },
            })
            // Plan documents are fully read; documents with other titles aren't candidates.
            const plans = (...documents: ReturnType<typeof document>[]) => [
                project,
                {
                    tool: "mcp__linear__list_documents",
                    arguments: {
                        limit: 50,
                        includeArchived: true,
                        orderBy: "createdAt",
                        fields: ["id", "title", "url", "issue", "project"],
                        projectId: "project-uuid",
                    },
                    response: {
                        documents: [document("notes", "Notes", "Notes"), ...documents].map((item) => ({
                            ...item,
                            content: undefined,
                        })),
                        hasNextPage: false,
                    },
                },
                ...documents.map((item) => ({
                    tool: "mcp__linear__get_document",
                    arguments: { id: item.id },
                    response: item,
                })),
            ]
            const pages = plans(document("document-uuid", body))
            fs.rmSync(path.join(root, directory), { recursive: true })
            assert.throws(
                () =>
                    restoreEpic(
                        root,
                        {
                            slug: "rollout",
                            epic: project,
                            pages: plans(document("document-uuid", body), document("second", body)),
                        },
                        tools,
                    ),
                /found 2 published plans/,
            )
            // A Plan document without a Full plan fold isn't a published plan.
            assert.throws(
                () =>
                    restoreEpic(
                        root,
                        { slug: "rollout", epic: project, pages: plans(document("draft", "Draft only.")) },
                        tools,
                    ),
                /found 0 published plans/,
            )
            const restored = restoreEpic(root, { slug: "rollout", epic: project, pages }, tools)
            assert.equal(restored.directory, ".piton/.local/epics/20261001-rollout")
            const status = readEntityStatus(path.join(root, restored.directory))
            assert.equal(status.branch.name, branch)
            assert.deepEqual(status.tracker.provider === "linear" && status.tracker.planDocument, {
                state: "published",
                id: "document-uuid",
                url: "https://linear.app/example/document/plan-slug",
            })
            assert.equal(fs.readFileSync(path.join(root, restored.directory, "epic.md"), "utf-8"), source)
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })
})

describe("Task design", () => {
    it("detects a changed issue body against the design a started Task last synced from", () => {
        for (const provider of ["github", "linear"] as const) {
            const { root, directory, branch, write } = repo(provider)
            try {
                const marker = provider === "github" ? "#11" : "ENG-11"
                write(
                    `# Epic\n\n## Task delivery strategy\n\n**Task target:** \`default\`\n**Epic branch:** \`${branch}\`\n\n## Tasks\n\n${section("Parser", marker)}`,
                )
                const task = initializeEntity(
                    {
                        entity: "task",
                        title: "Parser",
                        slug: "parser",
                        request: "Parse the input.",
                        parentEpicDir: directory,
                        object:
                            provider === "github"
                                ? {
                                      issueId: 110,
                                      issueNumber: 11,
                                      issueUrl: "https://github.com/example/app/issues/11",
                                  }
                                : {
                                      issueId: "issue-uuid",
                                      identifier: "ENG-11",
                                      issueUrl: "https://linear.app/example/issue/ENG-11",
                                      projectId: "project-uuid",
                                  },
                    },
                    { cwd: root },
                )
                const read = (body: string) =>
                    provider === "github"
                        ? {
                              tool: "mcp__github__issue_read",
                              arguments: { method: "get", ...repository, issue_number: 11 },
                              response: { number: 11, body },
                          }
                        : {
                              tool: "mcp__linear__get_issue",
                              arguments: { id: "issue-uuid" },
                              response: { uuid: "issue-uuid", id: "ENG-11", description: body },
                          }
                const check = (body: string) =>
                    inspectTaskDesign(root, { entityDir: task.directory, evidence: read(body) }, tools)
                const footer =
                    provider === "github" ? "<sub>Sent via Pi. Model: a/b.</sub>" : "*Sent via Pi. Model: a/b.*"
                const original = `Parse the input.\n\n- Keep errors.\n\n${footer}`

                // A Task started before designs were tracked records the current one first.
                assert.deepEqual(check(original), { state: "unrecorded" })
                const record = (body: string) =>
                    recordTaskDesign(root, { entityDir: task.directory, evidence: read(body) }, tools).state
                assert.equal(record(original), "recorded")
                assert.equal(record(original), "unchanged")

                // The signature, line endings, and trailing whitespace aren't design changes.
                assert.deepEqual(
                    check("Parse the input.  \r\n\r\n- Keep errors.\r\n"),
                    { state: "unchanged" },
                    provider,
                )
                const redesigned = `Parse the input as a stream.\n\n- Keep errors.\n\n${footer}`
                assert.deepEqual(
                    check(redesigned),
                    { state: "changed", design: "Parse the input as a stream.\n\n- Keep errors." },
                    provider,
                )
                assert.equal(record(redesigned), "recorded")
                assert.deepEqual(check(redesigned), { state: "unchanged" })

                // Only a read of this Task's own issue counts.
                const other =
                    provider === "github"
                        ? { ...read(original), arguments: { method: "get", ...repository, issue_number: 12 } }
                        : { ...read(original), arguments: { id: "other-uuid" } }
                assert.throws(
                    () => inspectTaskDesign(root, { entityDir: task.directory, evidence: other }, tools),
                    /argument (issue_number|id) must be/,
                )
                assert.throws(
                    () => inspectTaskDesign(root, { entityDir: directory, evidence: read(original) }, tools),
                    /needs a Task/,
                )
            } finally {
                fs.rmSync(root, { recursive: true, force: true })
            }
        }
    })
})
