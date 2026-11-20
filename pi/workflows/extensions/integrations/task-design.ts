import { createHash } from "node:crypto"
import * as path from "node:path"

import { type Static, Type } from "typebox"

import { compile } from "../../../extensions/__lib/schema.js"
import type { McpToolCatalog } from "../../../extensions/__lib/mcp.js"
import * as project from "../../../extensions/__lib/project.js"
import { type EntityStatus, readEntityStatus, resolveEntityDirectory, writeEntityStatus } from "../__lib/entity.js"
import { assertArtifactPersistencePrepared } from "../__lib/project-config.js"
import { loadIntegrationRoles, requireTracker, type TrackerConfig } from "./config.js"
import { readTrackerIssue, trackerIssueBody } from "./issue-read.js"
import { assertEntityTracker } from "./policy.js"
import { stripPublicationSignature } from "./publication.js"
import { requireTrackerItem } from "./records.js"
import type { ProviderEvidence } from "./resource-id-operations.js"
import { EvidenceSchema } from "./resource-id-tool.js"

// A Task issue's body is the Task's current design. A started Task records a fingerprint of the
// body it last synced from, so a later design change made elsewhere (an Epic session or a sibling
// Task) is detected exactly, and cleanup of the local brief never reads as a change.

/** Ignores the publication signature, line endings, and trailing whitespace. */
export function taskDesignFingerprint(body: string): string {
    const normalized = stripPublicationSignature(body)
        .replace(/\r\n/g, "\n")
        .split("\n")
        .map((line) => line.trimEnd())
        .join("\n")
        .trim()
    return createHash("sha256").update(normalized).digest("hex")
}

export const TaskDesignSchema = Type.Object(
    {
        entityDir: Type.String({ minLength: 1 }),
        evidence: Type.Union([EvidenceSchema], {
            description:
                "A fresh read of this Task's issue: GitHub issue_read get, or Linear get_issue by the stored issue UUID",
        }),
    },
    { additionalProperties: false },
)
export const TaskDesignValidator = compile(TaskDesignSchema)
export type TaskDesignInput = Static<typeof TaskDesignSchema>

// `unrecorded`: the Task has no recorded design yet (it started before designs were tracked);
// record the current one without changing the brief or plan.
export type TaskDesignCheck = { state: "unchanged" } | { state: "unrecorded" } | { state: "changed"; design: string }

/** The design text in a Task issue read, without its publication signature. */
export function taskIssueDesign(
    status: EntityStatus,
    tracker: TrackerConfig,
    evidence: ProviderEvidence,
    tools: McpToolCatalog,
): string {
    const record = requireTrackerItem(status.tracker, "a Task design read")
    return trackerIssueBody(readTrackerIssue(record, tracker, evidence, tools, "Task issue"))
}

function context(cwd: string, entityDir: string) {
    const root = project.resolveRootDir(cwd)
    assertArtifactPersistencePrepared(root)
    const tracker = requireTracker(loadIntegrationRoles(root), "a Task design check")
    const directory = resolveEntityDirectory(root, cwd, entityDir)
    const status = readEntityStatus(directory)
    if (status.entity !== "task") throw new Error("a design check needs a Task")
    assertEntityTracker(status, tracker)
    return { directory, status, tracker }
}

/** Whether the Task issue's design differs from the one the Task last synced from. */
export function inspectTaskDesign(cwd: string, input: TaskDesignInput, tools: McpToolCatalog): TaskDesignCheck {
    const { status, tracker } = context(cwd, input.entityDir)
    if (status.entity !== "task") throw new Error("a design check needs a Task")
    const design = taskIssueDesign(status, tracker, input.evidence, tools)
    if (status.design === undefined) return { state: "unrecorded" }
    return taskDesignFingerprint(design) === status.design ? { state: "unchanged" } : { state: "changed", design }
}

/** Records the Task issue's current design as the one the Task follows. */
export function recordTaskDesign(cwd: string, input: TaskDesignInput, tools: McpToolCatalog) {
    const { directory, status, tracker } = context(cwd, input.entityDir)
    if (status.entity !== "task") throw new Error("a design record needs a Task")
    const design = taskDesignFingerprint(taskIssueDesign(status, tracker, input.evidence, tools))
    if (design === status.design) return { state: "unchanged" as const, entity: path.basename(directory) }
    writeEntityStatus(directory, { ...status, design })
    return { state: "recorded" as const, entity: path.basename(directory) }
}
