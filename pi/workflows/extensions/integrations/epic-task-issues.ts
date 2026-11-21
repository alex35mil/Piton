import * as fs from "node:fs"
import * as path from "node:path"

import { type Static, Type } from "typebox"

import { compile } from "../../../extensions/__lib/schema.js"
import {
    getMcpToolName,
    mcpResponsePayload,
    requireMcpTool,
    type McpToolCatalog,
} from "../../../extensions/__lib/mcp.js"
import * as project from "../../../extensions/__lib/project.js"
import { type LifecycleState, PRIORITY_NOT_SET } from "../__lib/domain.js"
import {
    readEntityStatus,
    readEpicTaskDirectories,
    resolveEntityDirectory,
    type EntityStatus,
} from "../__lib/entity.js"
import { assertAcceptedTaskList } from "../__lib/epic-acceptance.js"
import { type EpicTask, epicTaskMarker, readEpicTasks, tagEpicTask } from "../__lib/epic-tasks.js"
import { assertArtifactPersistencePrepared } from "../__lib/project-config.js"
import {
    assertConfiguredPriority,
    type GitHubTracker,
    lifecycleFromStatus,
    type LinearTracker,
    loadIntegrationRoles,
    requireTracker,
    type TrackerConfig,
} from "./config.js"
import { registeredCredentialEnvironment } from "./mcp.js"
import { assertEntityTracker } from "./policy.js"
import { linearPriorityNumber } from "./records.js"
import { linearClientFor, type LinearClient } from "./tracker/linear.js"
import { LabelSelectionFailure, selectedLabelNames } from "./label-selection.js"
import { EvidenceSchema } from "./resource-id-tool.js"

const OneLineSchema = Type.String({ minLength: 1, pattern: "^[^\\r\\n]+$" })

// Fresh native facts shared by a child issue on either tracker. Omit status or priority
// only for an observed empty leaf.
const childFacts = {
    title: OneLineSchema,
    status: Type.Optional(
        Type.String({
            minLength: 1,
            pattern: "^[^\\r\\n]+$",
            description: "Native Status name; omit when empty or off the board",
        }),
    ),
    priority: Type.Optional(
        Type.String({
            minLength: 1,
            pattern: "^[^\\r\\n]+$",
            description: "Native Priority name; omit when empty",
        }),
    ),
}

// A GitHub sub-issue of the Epic with its configured Project item fields.
export const GitHubEpicChildSchema = Type.Object(
    {
        marker: Type.String({ pattern: "^#[1-9][0-9]*$", description: "`#N`" }),
        issueId: Type.Integer({ minimum: 1, description: "The numeric issue ID (`id`), not the number" }),
        ...childFacts,
        onBoard: Type.Boolean({ description: "The issue has an item in the configured Project" }),
        closed: Type.Boolean({ description: "The issue state is closed" }),
    },
    { additionalProperties: false },
)
export type GitHubEpicChild = Static<typeof GitHubEpicChildSchema>

// A Linear issue in the Epic's Project.
export const LinearEpicChildSchema = Type.Object(
    {
        marker: Type.String({ pattern: "^[A-Z][A-Z0-9]*-[1-9][0-9]*$", description: "`ENG-N`" }),
        ...childFacts,
    },
    { additionalProperties: false },
)
export type LinearEpicChild = Static<typeof LinearEpicChildSchema>

export const EpicChildrenSchema = Type.Union([
    Type.Object(
        {
            provider: Type.Enum(["github"], { type: "string" }),
            issues: Type.Array(GitHubEpicChildSchema, {
                description:
                    "Every sub-issue of the Epic, in `get_sub_issues` order across all pages, with its configured Project Status and Priority",
            }),
        },
        { additionalProperties: false },
    ),
    Type.Object(
        {
            provider: Type.Enum(["linear"], { type: "string" }),
            issues: Type.Array(LinearEpicChildSchema, {
                description: "Every issue in the Epic's Project, with complete pagination, in any order",
            }),
        },
        { additionalProperties: false },
    ),
])
export type EpicChildren = Static<typeof EpicChildrenSchema>

type ChildFacts = GitHubEpicChild | LinearEpicChild

export const PlanEpicTaskIssuesSchema = Type.Object(
    {
        entityDir: Type.String({ minLength: 1 }),
        epicPriority: Type.String({
            minLength: 1,
            description: "The Epic's native Priority, read fresh, or `not set`",
        }),
        children: EpicChildrenSchema,
    },
    { additionalProperties: false },
)
export const PlanEpicTaskIssuesValidator = compile(PlanEpicTaskIssuesSchema)
export type PlanEpicTaskIssuesInput = Static<typeof PlanEpicTaskIssuesSchema>

export const RecordEpicTaskIssueSchema = Type.Object(
    {
        entityDir: Type.String({ minLength: 1 }),
        title: OneLineSchema,
        marker: Type.String({ minLength: 1 }),
        // The clear-success create call; omitted when tagging a matched existing child.
        evidence: Type.Optional(EvidenceSchema),
    },
    { additionalProperties: false },
)
export const RecordEpicTaskIssueValidator = compile(RecordEpicTaskIssueSchema)
export type RecordEpicTaskIssueInput = Static<typeof RecordEpicTaskIssueSchema>

type Mutation = { tool: string; arguments: Record<string, unknown> }
type PendingTask = Extract<EpicTask, { issue: "pending" }>
export type EpicTaskIssueStep =
    | { title: string; effect: "create"; mutation: Mutation }
    | { title: string; marker: string; effect: "tag" }
    | {
          title: string
          marker: string
          effect: "addToProject" | "setStatus" | "setPriority" | "cancel" | "close" | "reorder"
          mutation: Mutation
      }
export type EpicTaskIssuePlan =
    | { state: "complete" }
    | { state: "steps-needed"; steps: EpicTaskIssueStep[] }
    | { state: "blocked"; steps: EpicTaskIssueStep[]; task: string; reason: string }

interface Context {
    root: string
    directory: string
    status: EntityStatus
    tracker: TrackerConfig
    tools: McpToolCatalog
}

function context(cwd: string, entityDir: string, tools: McpToolCatalog): Context {
    const root = project.resolveRootDir(cwd)
    assertArtifactPersistencePrepared(root)
    const tracker = requireTracker(loadIntegrationRoles(root), "Task issue processing")
    const directory = resolveEntityDirectory(root, cwd, entityDir)
    const status = readEntityStatus(directory)
    if (status.entity !== "epic") throw new Error("Task issue processing requires an Epic")
    assertEntityTracker(status, tracker)
    const epicPath = path.join(directory, "epic.md")
    if (!fs.existsSync(epicPath)) throw new Error("Epic has no epic.md")
    // Only an accepted Task list creates, links, or cancels issues.
    assertAcceptedTaskList(status, fs.readFileSync(epicPath, "utf-8"))
    return { root, directory, status, tracker, tools }
}

// Reconciles the whole accepted plan against fresh tracker facts. It keeps no saved
// progress, so a first run and every retry apply the same rules to each Task in order.
export function planEpicTaskIssues(
    cwd: string,
    input: PlanEpicTaskIssuesInput,
    tools: McpToolCatalog,
): EpicTaskIssuePlan {
    const ctx = context(cwd, input.entityDir, tools)
    assertConfiguredPriority(ctx.tracker, input.epicPriority)
    const tasks = readEpicTasks(path.join(ctx.directory, "epic.md"))
    const children = input.children
    const tracker = ctx.tracker
    const mismatch = () => new Error(`children are ${children.provider} issues, but the tracker is ${tracker.provider}`)
    switch (tracker.provider) {
        case "github":
            if (children.provider !== "github") throw mismatch()
            return reconcile(ctx, tasks, children.issues, githubCalls(ctx, tracker, input.epicPriority))
        case "linear":
            if (children.provider !== "linear") throw mismatch()
            return reconcile(ctx, tasks, children.issues, linearCalls(ctx, tracker, input.epicPriority))
    }
}

function reconcile<C extends ChildFacts>(
    ctx: Context,
    tasks: EpicTask[],
    issues: C[],
    native: NativeCalls<C>,
): EpicTaskIssuePlan {
    const children = new Map<string, C>()
    for (const child of issues) {
        if (children.has(child.marker)) throw new Error(`children list repeats ${child.marker}`)
        children.set(child.marker, child)
    }
    const r: Reconciliation<C> = {
        children,
        planned: new Set(tasks.flatMap((task) => epicTaskMarker(task) ?? [])),
        started: readEpicTaskDirectories(ctx.directory),
        native,
        lifecycle: lifecycleOf(ctx),
    }
    const steps: EpicTaskIssueStep[] = []
    for (const task of tasks) {
        try {
            steps.push(...reconcileTask(task, r))
        } catch (error) {
            const reason = error instanceof Error ? error.message : String(error)
            return { state: "blocked", steps, task: task.title, reason }
        }
    }
    // Creates append new children, so the order is settled only once every Task is.
    if (steps.length === 0) steps.push(...native.order(tasks, issues))
    return steps.length ? { state: "steps-needed", steps } : { state: "complete" }
}

interface Reconciliation<C extends ChildFacts> {
    children: Map<string, C>
    planned: Set<string>
    started: Map<string, string>
    native: NativeCalls<C>
    lifecycle: (status: string | undefined) => LifecycleState | undefined
}

function reconcileTask<C extends ChildFacts>(task: EpicTask, r: Reconciliation<C>): EpicTaskIssueStep[] {
    if (task.issue === "pending") {
        // A child whose marker the plan already carries belongs to another Task.
        const matches = [...r.children.values()].filter(
            (child) => !r.planned.has(child.marker) && child.title === task.title,
        )
        if (matches.length > 1)
            throw new Error(`several child issues match this title: ${matches.map((m) => m.marker).join(", ")}`)
        if (matches.length === 0) return [{ title: task.title, effect: "create", mutation: r.native.create(task) }]
        // Check the match before tagging, so a started issue is never linked.
        const marker = matches[0].marker
        const repairs = queuedChild(task.title, marker, r)
        r.planned.add(marker)
        return [{ title: task.title, marker, effect: "tag" }, ...repairs]
    }
    const marker = task.marker
    if (task.canceled) {
        const child = requireChild(marker, r)
        const lifecycle = r.lifecycle(child.status)
        // A started Task can be canceled too; its branch, PR, and local directory stay as they are.
        switch (lifecycle) {
            case "canceled":
                return r.native.closeIfOpen(task.title, child)
            case "backlog":
            case "todo":
            case "inProgress":
            case "inReview":
                return r.native.cancel(task.title, marker)
            case "done":
                throw new Error(`${marker} is ${child.status}; a Done Task can't be canceled`)
            case undefined:
                throw new Error(`${marker} is off the board; add it to the board before canceling it`)
        }
    }
    const child = requireChild(marker, r)
    // The tracker decides whether a Task has started; it may have started in another clone.
    const lifecycle = r.lifecycle(child.status)
    if (lifecycle === "inProgress" || lifecycle === "inReview" || lifecycle === "done") return []
    if (r.started.has(marker))
        throw new Error(`${marker} has a local Task directory but its issue is ${child.status ?? "off the board"}`)
    return queuedChild(task.title, marker, r)
}

// A tagged Task that hasn't started: a child of this Epic, with its board setup
// completed, still queued or already canceled.
function queuedChild<C extends ChildFacts>(title: string, marker: string, r: Reconciliation<C>): EpicTaskIssueStep[] {
    const child = requireChild(marker, r)
    const lifecycle = r.lifecycle(child.status)
    // Canceled on the tracker is a terminal outcome; never pull it back into Todo.
    if (lifecycle === "canceled") return []
    if (lifecycle !== undefined && lifecycle !== "backlog" && lifecycle !== "todo")
        throw new Error(`${marker} is ${child.status} and can't be linked as a queued Task`)
    return r.native.repairBoard(title, child)
}

function requireChild<C extends ChildFacts>(marker: string, r: Reconciliation<C>): C {
    const child = r.children.get(marker)
    if (!child) throw new Error(`${marker} is not a child issue of this Epic`)
    return child
}

function lifecycleOf(ctx: Context) {
    const values = ctx.tracker.provider === "github" ? ctx.tracker.fields.status.values : ctx.tracker.statuses.issues
    return (status: string | undefined): LifecycleState | undefined =>
        status === undefined ? undefined : lifecycleFromStatus(values, status)
}

interface NativeCalls<C extends ChildFacts> {
    // The new issue's body is the Task's design; from then on the issue holds it.
    create(task: PendingTask): Mutation
    repairBoard(title: string, child: C): EpicTaskIssueStep[]
    cancel(title: string, marker: string): EpicTaskIssueStep[]
    closeIfOpen(title: string, child: C): EpicTaskIssueStep[]
    // Puts the children in plan order; runs only once every Task is settled.
    order(tasks: readonly EpicTask[], children: readonly C[]): EpicTaskIssueStep[]
}

/** One move toward the target order: put `marker` directly before or after another child. */
export type ChildMove = { marker: string } & ({ before: string } | { after: string })

/**
 * The child order the plan asks for: its Tasks that aren't canceled, in section order, then every
 * other child (canceled Tasks and issues outside the plan) in its current relative order.
 */
export function planChildOrder(tasks: readonly EpicTask[], current: readonly string[]): string[] {
    const present = new Set(current)
    const leading = tasks.flatMap((task) => {
        switch (task.issue) {
            case "pending":
                return []
            case "tagged":
                return !task.canceled && present.has(task.marker) ? [task.marker] : []
        }
    })
    const placed = new Set(leading)
    return [...leading, ...current.filter((marker) => !placed.has(marker))]
}

/**
 * Moves that turn `current` into `target`, a permutation of it: walk the target and move each child
 * that isn't already in place, before the first child or after its predecessor. A matching order
 * needs none.
 */
export function childMoves(current: readonly string[], target: readonly string[]): ChildMove[] {
    const order = [...current]
    const moves: ChildMove[] = []
    target.forEach((marker, index) => {
        if (order[index] === marker) return
        const from = order.indexOf(marker)
        if (from < index) throw new Error(`target order repeats or adds ${marker}`)
        moves.push(index === 0 ? { marker, before: order[0] } : { marker, after: target[index - 1] })
        order.splice(from, 1)
        order.splice(index, 0, marker)
    })
    return moves
}

function mcpCall(ctx: Context, operation: string, args: Record<string, unknown>): Mutation {
    return { tool: getMcpToolName(ctx.tracker.mcpServer, operation, ctx.tools), arguments: args }
}

function githubCalls(ctx: Context, tracker: GitHubTracker, epicPriority: string): NativeCalls<GitHubEpicChild> {
    const call = (operation: string, args: Record<string, unknown>) => mcpCall(ctx, operation, args)
    if (ctx.status.tracker.provider !== "github") throw new Error("Epic is not bound to a GitHub issue")
    const epicNumber = ctx.status.tracker.external.issueNumber
    const repository = tracker.repository
    const board = {
        owner: tracker.project.owner,
        owner_type: tracker.project.ownerType,
        project_number: tracker.project.number,
    }
    const issueNumber = (marker: string) => Number(marker.slice(1))
    const item = (marker: string) => ({
        ...board,
        item_owner: repository.owner,
        item_repo: repository.repo,
        issue_number: issueNumber(marker),
    })
    const setField = (marker: string, name: string, value: string) =>
        call("projects_write", { method: "update_project_item", ...item(marker), updated_field: { name, value } })
    const priority = tracker.fields.priority
    const setPriority = (marker: string): Mutation =>
        priority.scope === "project"
            ? setField(marker, priority.field, epicPriority)
            : call("issue_write", {
                  method: "update",
                  ...repository,
                  issue_number: issueNumber(marker),
                  issue_fields: [{ field_name: priority.field, field_option_name: epicPriority }],
              })
    const close = (title: string, marker: string): EpicTaskIssueStep => ({
        title,
        marker,
        effect: "close",
        mutation: call("issue_write", {
            method: "update",
            ...repository,
            issue_number: issueNumber(marker),
            state: "closed",
            state_reason: "not_planned",
        }),
    })
    return {
        create: (task) => {
            const labels = taskLabels(tracker, task)
            return call("issue_write", {
                method: "create",
                ...repository,
                title: task.title,
                body: task.design,
                ...(labels.length > 0 ? { labels } : {}),
                parent_issue_number: epicNumber,
            })
        },
        repairBoard(title, child) {
            const steps: EpicTaskIssueStep[] = []
            const marker = child.marker
            if (!child.onBoard)
                steps.push({
                    title,
                    marker,
                    effect: "addToProject",
                    mutation: call("projects_write", {
                        method: "add_project_item",
                        ...board,
                        item_owner: repository.owner,
                        item_repo: repository.repo,
                        item_type: "issue",
                        issue_number: issueNumber(marker),
                    }),
                })
            // Automations may add the issue first, e.g. as Backlog with no Priority.
            if (child.status === undefined || child.status === tracker.fields.status.values.backlog)
                steps.push({
                    title,
                    marker,
                    effect: "setStatus",
                    mutation: setField(marker, tracker.fields.status.field, tracker.fields.status.values.todo),
                })
            if (child.priority === undefined && epicPriority !== PRIORITY_NOT_SET)
                steps.push({ title, marker, effect: "setPriority", mutation: setPriority(marker) })
            return steps
        },
        cancel: (title, marker) => [
            {
                title,
                marker,
                effect: "cancel",
                mutation: setField(marker, tracker.fields.status.field, tracker.fields.status.values.canceled),
            },
            // A closed sub-issue counts toward GitHub's sub-issue progress.
            close(title, marker),
        ],
        // Finishes an interrupted cancel whose Status write landed before the close.
        closeIfOpen: (title, child) => (child.closed ? [] : [close(title, child.marker)]),
        // GitHub lists sub-issues in link order until they are reprioritized.
        order(tasks, children) {
            const current = children.map((child) => child.marker)
            const byMarker = new Map(children.map((child) => [child.marker, child]))
            const titles = new Map(tasks.flatMap((task) => [[epicTaskMarker(task), task.title] as const]))
            const issueId = (marker: string) => byMarker.get(marker)!.issueId
            return childMoves(current, planChildOrder(tasks, current)).map((move) => ({
                title: titles.get(move.marker) ?? byMarker.get(move.marker)!.title,
                marker: move.marker,
                effect: "reorder" as const,
                mutation: call("sub_issue_write", {
                    method: "reprioritize",
                    ...repository,
                    issue_number: epicNumber,
                    sub_issue_id: issueId(move.marker),
                    ...("before" in move ? { before_id: issueId(move.before) } : { after_id: issueId(move.after) }),
                }),
            }))
        },
    }
}

function linearCalls(ctx: Context, tracker: LinearTracker, epicPriority: string): NativeCalls<LinearEpicChild> {
    const call = (operation: string, args: Record<string, unknown>) => mcpCall(ctx, operation, args)
    if (ctx.status.tracker.provider !== "linear" || ctx.status.tracker.resource !== "project")
        throw new Error("Epic is not bound to a Linear Project")
    const projectId = ctx.status.tracker.external.projectId
    const statuses = tracker.statuses.issues
    return {
        create: (task) => {
            const labels = taskLabels(tracker, task)
            return call("save_issue", {
                team: tracker.team,
                title: task.title,
                description: task.design,
                project: projectId,
                state: statuses.todo,
                priority: linearPriorityNumber(epicPriority),
                ...(labels.length > 0 ? { labels } : {}),
            })
        },
        repairBoard(title, child) {
            const backlog = child.status === undefined || child.status === statuses.backlog
            const priority = child.priority === undefined && epicPriority !== PRIORITY_NOT_SET
            if (!backlog && !priority) return []
            return [
                {
                    title,
                    marker: child.marker,
                    effect: backlog ? "setStatus" : "setPriority",
                    mutation: call("save_issue", {
                        id: child.marker,
                        ...(backlog ? { state: statuses.todo } : {}),
                        ...(priority ? { priority: linearPriorityNumber(epicPriority) } : {}),
                    }),
                },
            ]
        },
        cancel: (title, marker) => [
            { title, marker, effect: "cancel", mutation: call("save_issue", { id: marker, state: statuses.canceled }) },
        ],
        closeIfOpen: () => [],
        // Linear's MCP tools have no way to order issues in a Project.
        order: () => [],
    }
}

/** A Task issue's labels: its `kind` label and its `**Labels:**` selection, checked against the configuration. */
function taskLabels(tracker: TrackerConfig, task: EpicTask): string[] {
    try {
        return selectedLabelNames(tracker.labels, "deliverable", task.kind, task.labels)
    } catch (error) {
        if (error instanceof LabelSelectionFailure)
            throw new Error(`Epic Task ${JSON.stringify(task.title)}: ${error.message}`)
        throw error
    }
}

export const OrderLinearEpicIssuesSchema = Type.Object(
    { entityDir: Type.String({ minLength: 1 }) },
    { additionalProperties: false },
)
export const OrderLinearEpicIssuesValidator = compile(OrderLinearEpicIssuesSchema)
export type OrderLinearEpicIssuesInput = Static<typeof OrderLinearEpicIssuesSchema>

export type LinearEpicOrder = { state: "unchanged" } | { state: "ordered"; moved: string[] }

export interface LinearEpicOrderOptions {
    projectTrusted: boolean
    fetch?: typeof fetch
    env?: NodeJS.ProcessEnv
}

interface LinearProjectIssue {
    id: string
    marker: string
    sortOrder: number
}

/**
 * Puts a Linear Epic's Project issues in plan order, the order Manual ordering shows. Linear's MCP
 * tools can't read or set an issue's position, so this uses Linear's GraphQL API with the registered
 * Linear MCP server's credential. It keeps no state: a rerun reads fresh positions and finishes.
 */
export async function orderLinearEpicIssues(
    cwd: string,
    input: OrderLinearEpicIssuesInput,
    tools: McpToolCatalog,
    options: LinearEpicOrderOptions,
): Promise<LinearEpicOrder> {
    const ctx = context(cwd, input.entityDir, tools)
    if (ctx.tracker.provider !== "linear" || ctx.status.tracker.provider !== "linear")
        throw new Error(
            "order_linear_epic_issues orders only Linear Epics; GitHub Epics reorder through plan_epic_task_issues",
        )
    if (ctx.status.tracker.resource !== "project") throw new Error("Epic is not bound to a Linear Project")
    const tasks = readEpicTasks(path.join(ctx.directory, "epic.md"))
    const credential = registeredCredentialEnvironment(cwd, ctx.tracker.mcpServer, "linear", options.projectTrusted)
    const client = linearClientFor(credential, options)
    const issues = await projectIssues(client, ctx.status.tracker.external.projectId)
    const order = issues.map((issue) => issue.marker)
    const moves = childMoves(order, planChildOrder(tasks, order))
    if (moves.length === 0) return { state: "unchanged" }
    const positions = new Map(issues.map((issue) => [issue.marker, issue.sortOrder]))
    const ids = new Map(issues.map((issue) => [issue.marker, issue.id]))
    for (const move of moves) {
        order.splice(order.indexOf(move.marker), 1)
        const at = "before" in move ? order.indexOf(move.before) : order.indexOf(move.after) + 1
        order.splice(at, 0, move.marker)
        const previous = at > 0 ? positions.get(order[at - 1])! : undefined
        const next = at < order.length - 1 ? positions.get(order[at + 1])! : undefined
        const sortOrder = previous === undefined ? next! - 1 : next === undefined ? previous + 1 : (previous + next) / 2
        await client.graphql(
            `mutation OrderEpicIssue($id: String!, $sortOrder: Float!) {
                issueUpdate(id: $id, input: { sortOrder: $sortOrder }) { success }
            }`,
            { id: ids.get(move.marker)!, sortOrder },
        )
        positions.set(move.marker, sortOrder)
    }
    return { state: "ordered", moved: moves.map((move) => move.marker) }
}

// Every issue in the Project, ascending by its manual position.
async function projectIssues(client: LinearClient, projectId: string): Promise<LinearProjectIssue[]> {
    const issues: LinearProjectIssue[] = []
    let after: string | null = null
    do {
        const data = await client.graphql(
            `query EpicIssueOrder($id: String!, $after: String) {
                project(id: $id) {
                    issues(first: 100, after: $after, includeArchived: false) {
                        nodes { id identifier sortOrder }
                        pageInfo { hasNextPage endCursor }
                    }
                }
            }`,
            { id: projectId, after },
        )
        const connection = (data.project as { issues?: unknown } | null)?.issues as
            | { nodes?: unknown; pageInfo?: { hasNextPage?: unknown; endCursor?: unknown } }
            | undefined
        if (!connection || !Array.isArray(connection.nodes))
            throw new Error("Linear returned no issues for the Epic's Project")
        for (const node of connection.nodes as Array<Record<string, unknown>>) {
            if (
                typeof node.id !== "string" ||
                typeof node.identifier !== "string" ||
                typeof node.sortOrder !== "number"
            )
                throw new Error("Linear returned an issue without id, identifier, or sortOrder")
            issues.push({ id: node.id, marker: node.identifier, sortOrder: node.sortOrder })
        }
        const page = connection.pageInfo
        after = page?.hasNextPage === true && typeof page.endCursor === "string" ? page.endCursor : null
    } while (after)
    return issues.sort((left, right) => left.sortOrder - right.sortOrder)
}

// Tags one untagged plan Task with its issue's marker: a clear-success create, or the
// single matching existing child the plan reconciliation reported.
export function recordEpicTaskIssue(cwd: string, input: RecordEpicTaskIssueInput, tools: McpToolCatalog) {
    const ctx = context(cwd, input.entityDir, tools)
    const epicPath = path.join(ctx.directory, "epic.md")
    const task = readEpicTasks(epicPath).find((candidate) => candidate.title === input.title)
    if (!task) throw new Error(`Epic has no Task titled ${JSON.stringify(input.title)}`)
    if (input.evidence) {
        const created = createdMarker(ctx, task, input.evidence)
        if (created !== input.marker) throw new Error(`create evidence returned ${created}, not ${input.marker}`)
    }
    const result = tagEpicTask(epicPath, input.title, input.marker)
    return { state: result, title: input.title, marker: input.marker }
}

function createdMarker(ctx: Context, task: EpicTask, evidence: Static<typeof EvidenceSchema>): string {
    const tracker = ctx.tracker
    const operation = tracker.provider === "github" ? "issue_write" : "save_issue"
    requireMcpTool(evidence.tool, tracker.mcpServer, operation, ctx.tools, "Task issue create evidence")
    const args = evidence.arguments
    if (args.title !== task.title) throw new Error("create evidence is for another Task title")
    const response = mcpResponsePayload(evidence.response)
    if (tracker.provider === "github") {
        const parent = ctx.status.tracker.provider === "github" ? ctx.status.tracker.external.issueNumber : undefined
        if (args.method !== "create" || args.parent_issue_number !== parent)
            throw new Error("create evidence is not a sub-issue create under this Epic")
        const url = typeof response.url === "string" ? response.url : response.html_url
        const number = typeof url === "string" ? /\/issues\/([1-9][0-9]*)$/.exec(url)?.[1] : undefined
        if (!number) throw new Error("create response lacks the new issue's URL")
        return `#${number}`
    }
    const projectId =
        ctx.status.tracker.provider === "linear" && ctx.status.tracker.resource === "project"
            ? ctx.status.tracker.external.projectId
            : undefined
    if (Object.hasOwn(args, "id") || args.project !== projectId)
        throw new Error("create evidence is not an issue create in this Epic's Project")
    const identifier = response.identifier ?? response.id
    if (typeof identifier !== "string" || !/^[A-Z][A-Z0-9]*-[1-9][0-9]*$/.test(identifier))
        throw new Error("create response lacks the new issue's identifier")
    return identifier
}
