import * as fs from "node:fs"
import * as path from "node:path"

import { type Static, Type } from "typebox"

import { compile } from "../../../extensions/__lib/schema.js"
import { mcpResponsePayload, type McpToolCatalog } from "../../../extensions/__lib/mcp.js"
import * as project from "../../../extensions/__lib/project.js"
import { type DeliverableKind, EPIC_ROOT, TASK_ROOT, trackerReferenceLabel } from "../__lib/domain.js"
import {
    type EntityStatus,
    readEntityStatus,
    resolveEntityDirectory,
    type TaskParent,
    taskParent,
} from "../__lib/entity.js"
import { epicTaskMarker, readEpicRouting, readEpicTasks } from "../__lib/epic-tasks.js"
import * as Git from "../__lib/git.js"
import { assertArtifactPersistencePrepared } from "../__lib/project-config.js"
import { loadIntegrationRoles, type TrackerConfig } from "../integrations/config.js"
import { requireProviderCall } from "../integrations/provider-evidence.js"
import { trackerReference } from "../integrations/records.js"
import type { ProviderEvidence } from "../integrations/resource-id-operations.js"
import { EvidenceSchema } from "../integrations/resource-id-tool.js"

// Decides how a `/task` request starts, from local artifacts and the provider reads it's given, so the
// model never infers whether work is standalone or an Epic's. Reads nothing remote and writes nothing.

const OneLineSchema = Type.String({ minLength: 1, pattern: "^[^\\r\\n]+$" })

export const RouteTaskSchema = Type.Object(
    {
        target: Type.Union(
            [
                Type.Object(
                    {
                        kind: Type.Enum(["path"], { type: "string" }),
                        path: Type.Union([OneLineSchema], { description: "An existing Task directory" }),
                    },
                    { additionalProperties: false },
                ),
                Type.Object(
                    {
                        kind: Type.Enum(["issue"], { type: "string" }),
                        issue: Type.Union([EvidenceSchema], {
                            description: "The issue's read: GitHub issue_read get, or Linear get_issue",
                        }),
                        hierarchy: Type.Optional(
                            Type.Union([EvidenceSchema], {
                                description:
                                    "GitHub only, when route_task asks for it: issue_read get_parent for the same issue",
                            }),
                        ),
                        parent: Type.Optional(
                            Type.Union([EvidenceSchema], {
                                description:
                                    "GitHub only, when route_task asks for it: issue_read get for the parent issue",
                            }),
                        ),
                    },
                    { additionalProperties: false },
                ),
                Type.Object({ kind: Type.Enum(["new"], { type: "string" }) }, { additionalProperties: false }),
            ],
            { description: "What the request names: a Task path, a tracker issue, or new work with no issue" },
        ),
    },
    { additionalProperties: false },
)
export const RouteTaskValidator = compile(RouteTaskSchema)
export type RouteTaskInput = Static<typeof RouteTaskSchema>

/** The parent to pass to a Task's init tool. */
export type RoutedParent = { kind: "epic"; epicDir: string } | { kind: "none" }

export type TaskRouteBlock = "untagged" | "canceled" | "routing" | "unclassified"

export type TaskRoute =
    | { route: "resume"; entityDir: string; parent: RoutedParent }
    | {
          route: "epicTask"
          parent: { kind: "epic"; epicDir: string }
          marker: string
          kind: DeliverableKind
          taskTarget: "default" | "epic"
      }
    | {
          route: "restoreEpic"
          epic: { provider: "github"; issueNumber: number } | { provider: "linear"; projectId: string }
      }
    | { route: "readHierarchy"; call: ProviderCallRequest }
    | { route: "readParent"; call: ProviderCallRequest }
    | { route: "standalone"; parent: { kind: "none" } }
    | { route: "new"; tracker: "on" | "off" }
    | { route: "blocked"; reason: TaskRouteBlock; detail: string }

interface ProviderCallRequest {
    server: string
    operation: string
    arguments: Record<string, unknown>
}

const NEXT: Record<TaskRoute["route"], string> = {
    resume: "Resume this Task.",
    epicTask: "Start this Epic Task: adopt its issue and call the init tool with this parent and Kind.",
    restoreEpic: "Restore the Epic (restore_epic), then call route_task again with the same reads.",
    readHierarchy: "Run this read, then call route_task again with it as hierarchy.",
    readParent: "Run this read, then call route_task again with it as parent, keeping hierarchy.",
    standalone: "Start a standalone Task: adopt the issue and call the init tool with parent none.",
    new: "Start new standalone work: preflight_task and create the issue, or init_local_task with the tracker off.",
    blocked: "Stop and tell the user why.",
}

export function routeTask(cwd: string, input: RouteTaskInput, tools: McpToolCatalog): TaskRoute & { next: string } {
    const route = decide(cwd, input, tools)
    return { ...route, next: NEXT[route.route] }
}

function decide(cwd: string, input: RouteTaskInput, tools: McpToolCatalog): TaskRoute {
    const root = project.resolveRootDir(cwd)
    assertArtifactPersistencePrepared(root)
    const roles = loadIntegrationRoles(root)
    const target = input.target
    switch (target.kind) {
        case "path":
            return routePath(root, cwd, target.path)
        case "new":
            return { route: "new", tracker: roles.tracker.state }
        case "issue":
            if (roles.tracker.state !== "on")
                throw new Error(`routing an issue needs the tracker, which is off: ${roles.tracker.reason}`)
            return routeIssue(root, roles.tracker.config, target, tools)
    }
}

function routePath(root: string, cwd: string, raw: string): TaskRoute {
    const directory = resolveEntityDirectory(root, cwd, raw)
    const status = readEntityStatus(directory)
    switch (status.entity) {
        case "epic":
            throw new Error(`${raw} is an Epic; resume it with /epic`)
        case "task":
            return { route: "resume", entityDir: path.relative(root, directory), parent: routedParent(root, directory) }
    }
}

function routedParent(root: string, taskDirectory: string): RoutedParent {
    const parent: TaskParent = taskParent(taskDirectory)
    switch (parent.kind) {
        case "none":
            return { kind: "none" }
        case "epic":
            return { kind: "epic", epicDir: path.relative(root, parent.directory) }
    }
}

type IssueTarget = Extract<RouteTaskInput["target"], { kind: "issue" }>

function routeIssue(root: string, tracker: TrackerConfig, target: IssueTarget, tools: McpToolCatalog): TaskRoute {
    switch (tracker.provider) {
        case "github":
            return routeGitHubIssue(root, tracker, target, tools)
        case "linear":
            if (target.hierarchy || target.parent)
                throw new Error("hierarchy and parent reads are for GitHub; a Linear issue's projectId decides")
            return routeLinearIssue(root, tracker, target.issue, tools)
    }
}

type GitHubTracker = Extract<TrackerConfig, { provider: "github" }>
type LinearTracker = Extract<TrackerConfig, { provider: "linear" }>

function routeGitHubIssue(root: string, tracker: GitHubTracker, target: IssueTarget, tools: McpToolCatalog): TaskRoute {
    const repository = tracker.repository
    const number = target.issue.arguments.issue_number
    if (typeof number !== "number") throw new Error("issue evidence must name its issue_number")
    const read = (evidence: ProviderEvidence, method: string, issueNumber: number, label: string) => {
        requireProviderCall(
            evidence,
            {
                server: tracker.mcpServer,
                operation: "issue_read",
                expected: { method, ...repository, issue_number: issueNumber },
            },
            tools,
            label,
        )
        return mcpResponsePayload(evidence.response)
    }
    const issue = read(target.issue, "get", number, "issue")
    if (issue.number !== number) throw new Error("issue read is for another issue")

    const started = startedTask(root, `#${number}`)
    if (started) return started

    if (!target.hierarchy)
        return {
            route: "readHierarchy",
            call: {
                server: tracker.mcpServer,
                operation: "issue_read",
                arguments: { method: "get_parent", ...repository, issue_number: number },
            },
        }
    const hierarchy = read(target.hierarchy, "get_parent", number, "hierarchy")
    const parent = hierarchy.parent
    if (parent === null) return { route: "standalone", parent: { kind: "none" } }
    if (
        typeof parent !== "object" ||
        parent === undefined ||
        typeof (parent as { number?: unknown }).number !== "number"
    )
        throw new Error("hierarchy read has no parent: expected null or the parent issue")
    const parentNumber = (parent as { number: number }).number

    const epic = localEpic(
        root,
        (status) => status.tracker.provider === "github" && status.tracker.external.issueNumber === parentNumber,
    )
    if (epic) return routeEpicTask(root, epic, `#${number}`)

    if (!target.parent)
        return {
            route: "readParent",
            call: {
                server: tracker.mcpServer,
                operation: "issue_read",
                arguments: { method: "get", ...repository, issue_number: parentNumber },
            },
        }
    const parentIssue = read(target.parent, "get", parentNumber, "parent issue")
    const epicLabel = tracker.labels?.groups?.kind?.values.epic?.name
    if (!epicLabel)
        return {
            route: "blocked",
            reason: "unclassified",
            detail: `#${number} is a sub-issue of #${parentNumber}, and no Epic Kind label is configured to tell whether #${parentNumber} is an Epic; ask the user`,
        }
    return labelNames(parentIssue.labels).includes(epicLabel)
        ? { route: "restoreEpic", epic: { provider: "github", issueNumber: parentNumber } }
        : { route: "standalone", parent: { kind: "none" } }
}

function routeLinearIssue(
    root: string,
    tracker: LinearTracker,
    evidence: ProviderEvidence,
    tools: McpToolCatalog,
): TaskRoute {
    requireProviderCall(evidence, { server: tracker.mcpServer, operation: "get_issue", expected: {} }, tools, "issue")
    const issue = mcpResponsePayload(evidence.response)
    const requested = evidence.arguments.id
    if (typeof requested !== "string" || (requested !== issue.id && requested !== issue.uuid))
        throw new Error("Linear issue evidence must be get_issue for this issue's identifier or UUID")
    const identifier = issue.id
    if (typeof identifier !== "string" || !identifier) throw new Error("Linear issue read lacks its identifier")
    const started = startedTask(root, identifier)
    if (started) return started
    const projectId = issue.projectId
    // Every Linear Project is an Epic; an issue in none is standalone.
    if (projectId === null || projectId === undefined) return { route: "standalone", parent: { kind: "none" } }
    if (typeof projectId !== "string" || !projectId) throw new Error("Linear issue read has an invalid projectId")
    const epic = localEpic(
        root,
        (status) =>
            status.tracker.provider === "linear" &&
            status.tracker.resource === "project" &&
            status.tracker.external.projectId === projectId,
    )
    return epic
        ? routeEpicTask(root, epic, identifier)
        : { route: "restoreEpic", epic: { provider: "linear", projectId } }
}

function labelNames(value: unknown): string[] {
    if (!Array.isArray(value)) return []
    return value.flatMap((label) =>
        typeof label === "string"
            ? [label]
            : label && typeof label === "object" && typeof (label as { name?: unknown }).name === "string"
              ? [(label as { name: string }).name]
              : [],
    )
}

interface LocalEpic {
    directory: string
    status: Extract<EntityStatus, { entity: "epic" }>
}

function localEpic(
    root: string,
    matches: (status: Extract<EntityStatus, { entity: "epic" }>) => boolean,
): LocalEpic | undefined {
    for (const directory of childDirectories(path.join(root, EPIC_ROOT))) {
        const status = readEntityStatus(directory)
        if (status.entity === "epic" && matches(status)) return { directory, status }
    }
    return undefined
}

/** The local Task, standalone or an Epic's, whose tracker reference is `label`. */
function startedTask(root: string, label: string): TaskRoute | undefined {
    const roots = [
        path.join(root, TASK_ROOT),
        ...childDirectories(path.join(root, EPIC_ROOT)).map((epic) => path.join(epic, "tasks")),
    ]
    for (const directory of roots.flatMap(childDirectories)) {
        // A directory is `<reference>-<slug>`: only a matching prefix can be this issue's.
        if (!path.basename(directory).startsWith(`${label.replace(/^#/, "")}-`)) continue
        const status = readEntityStatus(directory)
        if (trackerReferenceLabel(trackerReference(status.tracker)) !== label) continue
        return { route: "resume", entityDir: path.relative(root, directory), parent: routedParent(root, directory) }
    }
    return undefined
}

function routeEpicTask(root: string, epic: LocalEpic, marker: string): TaskRoute {
    const epicDir = path.relative(root, epic.directory)
    const epicPath = path.join(epic.directory, "epic.md")
    if (!fs.existsSync(epicPath))
        return { route: "blocked", reason: "untagged", detail: `${epicDir} has no epic.md yet; resume the Epic first` }
    const task = readEpicTasks(epicPath).find((candidate) => epicTaskMarker(candidate) === marker)
    if (!task || task.issue !== "tagged")
        return {
            route: "blocked",
            reason: "untagged",
            detail: `the Epic's plan has no Task tagged ${marker}; resume the Epic to finish its issue processing`,
        }
    if (task.canceled) return { route: "blocked", reason: "canceled", detail: `Epic Task ${marker} is canceled` }
    let routing: ReturnType<typeof readEpicRouting>
    try {
        routing = readEpicRouting(epicPath)
    } catch (error) {
        return { route: "blocked", reason: "routing", detail: error instanceof Error ? error.message : String(error) }
    }
    if (routing.epicBranch !== epic.status.branch.name)
        return {
            route: "blocked",
            reason: "routing",
            detail: `epic.md names the Epic branch ${routing.epicBranch}, but the Epic's branch is ${epic.status.branch.name}`,
        }
    if (
        routing.taskTarget === "epic" &&
        !Git.succeeds(root, ["show-ref", "--verify", `refs/heads/${routing.epicBranch}`])
    )
        return {
            route: "blocked",
            reason: "routing",
            detail: `Epic-target Tasks start from ${routing.epicBranch}; fetch or create it locally first`,
        }
    return {
        route: "epicTask",
        parent: { kind: "epic", epicDir },
        marker,
        kind: task.kind,
        taskTarget: routing.taskTarget,
    }
}

function childDirectories(directory: string): string[] {
    if (!fs.existsSync(directory)) return []
    return fs
        .readdirSync(directory, { withFileTypes: true })
        .filter((entry) => entry.isDirectory())
        .map((entry) => path.join(directory, entry.name))
        .sort()
}
