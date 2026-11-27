import * as fs from "node:fs"
import * as path from "node:path"

import { type Static, Type } from "typebox"

import { compile } from "../../../extensions/__lib/schema.js"
import type { McpToolCatalog } from "../../../extensions/__lib/mcp.js"
import * as project from "../../../extensions/__lib/project.js"
import {
    type DeliverableKind,
    EPIC_ROOT,
    type LifecycleState,
    TASK_ROOT,
    trackerReferenceLabel,
} from "../__lib/domain.js"
import { readEntityStatus, resolveEntityDirectory } from "../__lib/entity.js"
import { epicTaskMarker, readEpicRouting, readEpicTasks } from "../__lib/epic-tasks.js"
import * as Git from "../__lib/git.js"
import { assertArtifactPersistencePrepared } from "../__lib/project-config.js"
import { lifecycleFromStatus, loadIntegrationRoles, type TrackerConfig } from "../integrations/config.js"
import { issueParent, IssueReadsSchema, type ReadRequest } from "../integrations/issue-parent.js"
import {
    describeTaskParent,
    type EpicTrackerItem,
    sameTaskParent,
    type TaskTrackerParent,
    trackerReference,
} from "../integrations/records.js"
import { findLocalEpic } from "./entity.js"

// Decides how a `/task` request starts, from the tracker reads it's given and local artifacts, so the model
// never infers whether work is standalone or an Epic's. The tracker decides the parent; local folders only
// say what this clone already has. Reads nothing remote and writes nothing.

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
                        ...IssueReadsSchema.properties,
                        status: Type.Union([OneLineSchema], {
                            description:
                                "The issue's native Status name: the configured Project's Status field on GitHub, the issue's status on Linear",
                        }),
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

export type TaskRouteBlock = "untagged" | "canceled" | "routing" | "unclassified" | "finished"

export type TaskRoute =
    | { route: "resume"; entityDir: string }
    | { route: "epicTask"; epicDir: string; marker: string; kind: DeliverableKind; taskTarget: "default" | "epic" }
    | { route: "restoreEpic"; epic: EpicTrackerItem }
    | { route: "restoreTask"; parent: TaskTrackerParent }
    | { route: "readRequired"; as: "hierarchy" | "parent"; call: ReadRequest }
    | { route: "standalone" }
    | { route: "new"; tracker: "on" | "off" }
    | { route: "blocked"; reason: TaskRouteBlock; detail: string }

const NEXT: Record<TaskRoute["route"], string> = {
    resume: "Resume this Task.",
    epicTask: "Start this Epic Task: adopt its issue and call init_task with the same reads and the plan Kind.",
    restoreEpic: "Restore the Epic (restore_epic), then call route_task again with the same reads.",
    restoreTask: "The Task started in another clone: call restore_task with the same reads.",
    readRequired: "Run this read, then call route_task again with it added under the named key.",
    standalone: "Start a standalone Task: adopt the issue and call init_task with the same reads.",
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
        case "path": {
            const directory = resolveEntityDirectory(root, cwd, target.path)
            const status = readEntityStatus(directory)
            switch (status.entity) {
                case "epic":
                    throw new Error(`${target.path} is an Epic; resume it with /epic`)
                case "task":
                    return { route: "resume", entityDir: path.relative(root, directory) }
            }
            break
        }
        case "new":
            return { route: "new", tracker: roles.tracker.state }
        case "issue":
            if (roles.tracker.state !== "on")
                throw new Error(`routing an issue needs the tracker, which is off: ${roles.tracker.reason}`)
            return routeIssue(root, roles.tracker.config, target, tools)
    }
    throw new Error("unreachable: every Task target routes")
}

type IssueTarget = Extract<RouteTaskInput["target"], { kind: "issue" }>

function routeIssue(root: string, tracker: TrackerConfig, target: IssueTarget, tools: McpToolCatalog): TaskRoute {
    const { issue, hierarchy, parent } = target
    const result = issueParent(
        tracker,
        { issue, hierarchy, parent },
        tools,
        (epic) => findLocalEpic(root, epic) !== undefined,
    )
    const label = result.facts.label
    const started = startedTask(root, label)
    if (started) return { route: "resume", entityDir: started }
    switch (result.state) {
        case "readRequired":
            return { route: "readRequired", as: result.as, call: result.read }
        case "unclassified":
            return {
                route: "blocked",
                reason: "unclassified",
                detail: `${label} is a sub-issue of #${result.parentNumber}, and no Epic Kind label is configured to tell whether #${result.parentNumber} is an Epic; ask the user`,
            }
        case "resolved":
            break
    }

    const lifecycle = lifecycleOf(tracker, target.status)
    switch (lifecycle) {
        case "backlog":
        case "todo":
            break
        case "inProgress":
        case "inReview":
            // Started in another clone: rebuild it from the tracker instead of starting it twice.
            return { route: "restoreTask", parent: result.parent }
        case "done":
        case "canceled":
            return { route: "blocked", reason: "finished", detail: `${label} is ${lifecycle}; it can't be started` }
    }

    const owner = result.parent
    switch (owner.kind) {
        case "none":
            return { route: "standalone" }
        case "epic": {
            const epicDir = findLocalEpic(root, owner.epic)
            return epicDir ? routeEpicTask(root, epicDir, label) : { route: "restoreEpic", epic: owner.epic }
        }
    }
}

function lifecycleOf(tracker: TrackerConfig, status: string): LifecycleState {
    switch (tracker.provider) {
        case "github":
            return lifecycleFromStatus(tracker.fields.status.values, status)
        case "linear":
            return lifecycleFromStatus(tracker.statuses.issues, status)
    }
}

/** The local Task directory, standalone or an Epic's, whose tracker reference is `label`. */
export function startedTask(root: string, label: string): string | undefined {
    const roots = [
        path.join(root, TASK_ROOT),
        ...childDirectories(path.join(root, EPIC_ROOT)).map((epic) => path.join(epic, "tasks")),
    ]
    for (const directory of roots.flatMap(childDirectories)) {
        // A directory is `<reference>-<slug>`: only a matching prefix can be this issue's.
        if (!path.basename(directory).startsWith(`${label.replace(/^#/, "")}-`)) continue
        const status = readEntityStatus(directory)
        // readEntityStatus already checked that the directory mirrors the recorded parent.
        if (status.entity === "task" && trackerReferenceLabel(trackerReference(status.tracker)) === label)
            return path.relative(root, directory)
    }
    return undefined
}

function routeEpicTask(root: string, epicDir: string, marker: string): TaskRoute {
    const directory = path.join(root, epicDir)
    const epic = readEntityStatus(directory)
    if (epic.entity !== "epic") throw new Error(`${epicDir} is not an Epic`)
    const epicPath = path.join(directory, "epic.md")
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
    if (routing.epicBranch !== epic.branch.name)
        return {
            route: "blocked",
            reason: "routing",
            detail: `epic.md names the Epic branch ${routing.epicBranch}, but the Epic's branch is ${epic.branch.name}`,
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
    return { route: "epicTask", epicDir, marker, kind: task.kind, taskTarget: routing.taskTarget }
}

function childDirectories(directory: string): string[] {
    if (!fs.existsSync(directory)) return []
    return fs
        .readdirSync(directory, { withFileTypes: true })
        .filter((entry) => entry.isDirectory())
        .map((entry) => path.join(directory, entry.name))
        .sort()
}

export const CheckTaskParentSchema = Type.Object(
    { entityDir: OneLineSchema, reads: IssueReadsSchema },
    { additionalProperties: false },
)
export const CheckTaskParentValidator = compile(CheckTaskParentSchema)
export type CheckTaskParentInput = Static<typeof CheckTaskParentSchema>

export type TaskParentCheck =
    | { state: "unchanged"; parent: TaskTrackerParent }
    | { state: "readRequired"; as: "hierarchy" | "parent"; call: ReadRequest }
    | { state: "changed"; recorded: TaskTrackerParent; tracker: TaskTrackerParent | "unclassified"; detail: string }

/**
 * Compares a resumed Task's recorded parent with the one its fresh tracker reads give. A Task never changes
 * its parent, so a difference stops the work; nothing here moves or rewrites the Task.
 */
export function checkTaskParent(cwd: string, input: CheckTaskParentInput, tools: McpToolCatalog): TaskParentCheck {
    const root = project.resolveRootDir(cwd)
    assertArtifactPersistencePrepared(root)
    const roles = loadIntegrationRoles(root)
    if (roles.tracker.state !== "on")
        throw new Error(`a parent check needs the tracker, which is off: ${roles.tracker.reason}`)
    const directory = resolveEntityDirectory(root, cwd, input.entityDir)
    const status = readEntityStatus(directory)
    if (status.entity !== "task") throw new Error("a parent check needs a Task")
    if (status.tracker.provider === "local") throw new Error("a local Task has no tracker item to check")
    const result = issueParent(
        roles.tracker.config,
        input.reads,
        tools,
        (epic) => findLocalEpic(root, epic) !== undefined,
    )
    if (result.facts.label !== trackerReferenceLabel(trackerReference(status.tracker)))
        throw new Error(`the reads are for ${result.facts.label}, not this Task's issue`)
    const recorded = status.parent
    switch (result.state) {
        case "readRequired":
            return { state: "readRequired", as: result.as, call: result.read }
        case "unclassified":
            return {
                state: "changed",
                recorded,
                tracker: "unclassified",
                detail: `the issue is now a sub-issue of #${result.parentNumber}, which no Epic Kind label classifies`,
            }
        case "resolved":
            return sameTaskParent(recorded, result.parent)
                ? { state: "unchanged", parent: recorded }
                : {
                      state: "changed",
                      recorded,
                      tracker: result.parent,
                      detail: `the Task started under ${describeTaskParent(recorded)}, but the tracker now shows ${describeTaskParent(result.parent)}`,
                  }
    }
}
