import { type Static, Type } from "typebox"

import { compile } from "../../../extensions/__lib/schema.js"
import {
    type EntityType,
    LINEAR_IDENTIFIER_PATTERN,
    PRIORITY_NOT_SET,
    TIMESTAMP_PATTERN,
    type TrackerReference,
} from "../__lib/domain.js"
import { type PlanAnnouncement, PlanAnnouncementSchema } from "./plan-announcement.js"
import { assertPlanCommentUrl, type PlanComment, PlanCommentSchema } from "./plan-comment.js"
import { assertPlanDocumentUrl, type PlanDocument, PlanDocumentSchema } from "./plan-document.js"

const LINEAR_PRIORITIES = {
    Urgent: 1,
    High: 2,
    Medium: 3,
    Low: 4,
    [PRIORITY_NOT_SET]: 0,
} as const
export type LinearPriority = keyof typeof LINEAR_PRIORITIES

export function linearPriorityNumber(priority: string): number {
    if (!Object.hasOwn(LINEAR_PRIORITIES, priority)) {
        throw new Error(`invalid Linear priority ${JSON.stringify(priority)}`)
    }
    return LINEAR_PRIORITIES[priority as LinearPriority]
}

const OneLineSchema = Type.String({ minLength: 1, pattern: "^[^\\r\\n]+$" })
export const LinearIdentifierSchema = Type.String({
    pattern: LINEAR_IDENTIFIER_PATTERN,
})
const TimestampSchema = Type.String({
    pattern: TIMESTAMP_PATTERN,
    description: "ISO 8601 timestamp",
})
const GitHubRepositorySchema = Type.Object(
    { owner: OneLineSchema, repo: OneLineSchema },
    { additionalProperties: false },
)

// The tracker object identified by create or adoption evidence. It exists before the
// local entity and names it; initialization takes it whether new or adopted.
export const GitHubTrackerObjectSchema = Type.Object(
    {
        issueId: Type.Integer({ minimum: 1 }),
        issueNumber: Type.Integer({ minimum: 1 }),
        issueUrl: OneLineSchema,
        projectItemId: Type.Optional(Type.Integer({ minimum: 1, maximum: Number.MAX_SAFE_INTEGER })),
    },
    { additionalProperties: false },
)
export const GitHubTrackerObjectValidator = compile(GitHubTrackerObjectSchema)
export const LinearProjectObjectSchema = Type.Object(
    {
        projectId: OneLineSchema,
        projectUrl: OneLineSchema,
        createdAt: Type.String({
            pattern: TIMESTAMP_PATTERN,
            description: "The Linear Project's createdAt; it dates the Epic's name",
        }),
    },
    { additionalProperties: false },
)
export const LinearProjectObjectValidator = compile(LinearProjectObjectSchema)
// A Task's Linear issue. `projectId` is the read fact: present exactly when the issue is in a Project,
// which for a Task means its parent Epic's.
export const LinearIssueObjectSchema = Type.Object(
    {
        issueId: OneLineSchema,
        identifier: LinearIdentifierSchema,
        issueUrl: OneLineSchema,
        gitBranchName: Type.Optional(OneLineSchema),
        projectId: Type.Optional(
            Type.String({
                minLength: 1,
                pattern: "^[^\\r\\n]+$",
                description: "The issue's Project UUID, when it's in a Project; omit it when the issue has none",
            }),
        ),
    },
    { additionalProperties: false },
)
export const LinearIssueObjectValidator = compile(LinearIssueObjectSchema)
export type GitHubTrackerObject = Static<typeof GitHubTrackerObjectSchema>
export type LinearProjectObject = Static<typeof LinearProjectObjectSchema>
export type LinearIssueObject = Static<typeof LinearIssueObjectSchema>
export type TrackerObject =
    | ({ provider: "github" } & GitHubTrackerObject)
    | ({ provider: "linear"; resource: "project" } & LinearProjectObject)
    | ({ provider: "linear"; resource: "issue" } & LinearIssueObject)

export const GitHubTrackerRecordSchema = Type.Object(
    {
        provider: Type.Literal("github"),
        repository: GitHubRepositorySchema,
        external: Type.Object({ issueNumber: Type.Integer({ minimum: 1 }) }, { additionalProperties: false }),
        planComment: PlanCommentSchema,
        // Present while a plan update's announcement is in flight.
        planAnnouncement: Type.Optional(PlanAnnouncementSchema),
    },
    { additionalProperties: false },
)
export const LinearProjectRecordSchema = Type.Object(
    {
        provider: Type.Literal("linear"),
        resource: Type.Literal("project"),
        external: Type.Object(
            { projectId: OneLineSchema, createdAt: TimestampSchema },
            { additionalProperties: false },
        ),
        planDocument: PlanDocumentSchema,
        // Present while a plan update's announcement is in flight.
        planAnnouncement: Type.Optional(PlanAnnouncementSchema),
    },
    { additionalProperties: false },
)
// A Task's Linear issue: identity only. Its parent Epic's Project, when it has one, lives in the Task's `parent`.
export const LinearIssueRecordSchema = Type.Object(
    {
        provider: Type.Literal("linear"),
        resource: Type.Literal("issue"),
        external: Type.Object(
            { issueId: OneLineSchema, identifier: LinearIdentifierSchema },
            { additionalProperties: false },
        ),
        planDocument: PlanDocumentSchema,
        planAnnouncement: Type.Optional(PlanAnnouncementSchema),
    },
    { additionalProperties: false },
)
export const TrackerRecordSchema = Type.Union([
    GitHubTrackerRecordSchema,
    LinearProjectRecordSchema,
    LinearIssueRecordSchema,
])
export const TrackerRecordValidator = compile(TrackerRecordSchema)

/**
 * A Task's parent as the tracker records it: an Epic, by its tracker item, or none. The tracker decides it
 * (a GitHub sub-issue of an Epic-labelled issue, a Linear issue in a Project); the local directory mirrors it.
 * Sub-tasks would add a Task parent here.
 */
export const TaskTrackerParentSchema = Type.Union([
    Type.Object(
        {
            kind: Type.Literal("epic"),
            epic: Type.Union([
                Type.Object(
                    { provider: Type.Literal("github"), issueNumber: Type.Integer({ minimum: 1 }) },
                    { additionalProperties: false },
                ),
                Type.Object(
                    { provider: Type.Literal("linear"), projectId: OneLineSchema },
                    { additionalProperties: false },
                ),
            ]),
        },
        { additionalProperties: false },
    ),
    Type.Object({ kind: Type.Literal("none") }, { additionalProperties: false }),
])
export type TaskTrackerParent = Static<typeof TaskTrackerParentSchema>
export type EpicTrackerItem = Extract<TaskTrackerParent, { kind: "epic" }>["epic"]

/** Whether `record` is the Epic tracker item `epic` names. */
export function isEpicTrackerItem(record: EntityTracker, epic: EpicTrackerItem): boolean {
    switch (epic.provider) {
        case "github":
            return record.provider === "github" && record.external.issueNumber === epic.issueNumber
        case "linear":
            return (
                record.provider === "linear" &&
                record.resource === "project" &&
                record.external.projectId === epic.projectId
            )
    }
}

/** Whether two parents are the same tracker fact. */
export function sameTaskParent(left: TaskTrackerParent, right: TaskTrackerParent): boolean {
    switch (left.kind) {
        case "none":
            return right.kind === "none"
        case "epic": {
            if (right.kind !== "epic") return false
            const [a, b] = [left.epic, right.epic]
            switch (a.provider) {
                case "github":
                    return b.provider === "github" && a.issueNumber === b.issueNumber
                case "linear":
                    return b.provider === "linear" && a.projectId === b.projectId
            }
        }
    }
}

export function describeTaskParent(parent: TaskTrackerParent): string {
    switch (parent.kind) {
        case "none":
            return "no parent (standalone)"
        case "epic":
            switch (parent.epic.provider) {
                case "github":
                    return `the Epic #${parent.epic.issueNumber}`
                case "linear":
                    return `the Epic Project ${parent.epic.projectId}`
            }
    }
}
export type TrackerRecord = Static<typeof TrackerRecordSchema>
export type GitHubTrackerRecord = Static<typeof GitHubTrackerRecordSchema>
export type LinearTrackerRecord = Exclude<TrackerRecord, GitHubTrackerRecord>

// A local Task: no tracker item. Its creation time names it, and every tracker step refuses for it.
export const LocalTrackerRecordSchema = Type.Object(
    { provider: Type.Literal("local"), createdAt: TimestampSchema },
    { additionalProperties: false },
)
export type LocalTrackerRecord = Static<typeof LocalTrackerRecordSchema>

/** An entity's tracker binding: its tracker item, or none for a local Task. */
export const EntityTrackerSchema = Type.Union([...TrackerRecordSchema.anyOf, LocalTrackerRecordSchema])
export type EntityTracker = TrackerRecord | LocalTrackerRecord

/** The entity's tracker item, for a step that works on it; a local Task has none. */
export function requireTrackerItem(tracker: EntityTracker, operation: string): TrackerRecord {
    switch (tracker.provider) {
        case "local":
            throw new Error(`${operation} needs a tracker item, and this Task is local (it has none)`)
        case "github":
        case "linear":
            return tracker
    }
}

export const ForgeRecordSchema = Type.Object(
    {
        provider: Type.Literal("github"),
        repository: GitHubRepositorySchema,
        head: OneLineSchema,
        target: OneLineSchema,
    },
    { additionalProperties: false },
)
export type ForgeRecord = Static<typeof ForgeRecordSchema>

export function trackerReference(record: EntityTracker | TrackerObject): TrackerReference {
    if (record.provider === "local") return { provider: "local", createdAt: record.createdAt }
    if (record.provider === "github")
        return {
            provider: "github",
            issueNumber: "external" in record ? record.external.issueNumber : record.issueNumber,
        }
    if (record.resource === "project")
        return {
            provider: "linear",
            projectCreatedAt: "external" in record ? record.external.createdAt : record.createdAt,
        }
    return {
        provider: "linear",
        identifier: "external" in record ? record.external.identifier : record.identifier,
    }
}

// The entity's plan-publication state on its tracker item: a GitHub comment or a Linear document.
export function trackerPlanPublication(record: TrackerRecord): PlanComment | PlanDocument {
    return record.provider === "github" ? record.planComment : record.planDocument
}

export function trackerRecordFromObject(
    object: TrackerObject,
    repository: { owner: string; repo: string } | undefined,
): TrackerRecord {
    switch (object.provider) {
        case "github":
            if (!repository) throw new Error("GitHub tracker record requires the configured repository")
            return {
                provider: "github",
                repository: { ...repository },
                external: { issueNumber: object.issueNumber },
                planComment: { state: "unpublished" },
            }
        case "linear":
            switch (object.resource) {
                case "project":
                    return {
                        provider: "linear",
                        resource: "project",
                        external: {
                            projectId: object.projectId,
                            createdAt: object.createdAt,
                        },
                        planDocument: { state: "unpublished" },
                    }
                case "issue":
                    return {
                        provider: "linear",
                        resource: "issue",
                        external: {
                            issueId: object.issueId,
                            identifier: object.identifier,
                        },
                        planDocument: { state: "unpublished" },
                    }
            }
    }
}

/**
 * The record's own invariants and its fit with the entity type. A Task's fit with its parent (local only
 * when standalone, a Linear Project only under an Epic) is checked where the parent is known.
 */
export function assertTrackerRecordInvariants(
    entity: EntityType,
    tracker: EntityTracker,
    fail: (message: string) => never,
): void {
    if (tracker.provider === "local") {
        // An Epic creates its Tasks' issues on the tracker, so it always needs one.
        switch (entity) {
            case "epic":
                return fail("an Epic can't be local; it needs a tracker item")
            case "task":
                return
        }
    }
    if (tracker.provider === "github") {
        const comment = tracker.planComment
        if (comment.state === "published")
            assertPlanCommentUrl(tracker.repository, tracker.external.issueNumber, comment.id, comment.url)
        assertPlanAnnouncementState(tracker.planAnnouncement, comment.state, fail)
        return
    }
    const expected = linearResourceFor(entity)
    if (tracker.resource !== expected) fail(`Linear tracker resource must be ${expected} for ${entity}`)
    if (tracker.planDocument.state === "published") assertPlanDocumentUrl(tracker.planDocument.url)
    assertPlanAnnouncementState(tracker.planAnnouncement, tracker.planDocument.state, fail)
}

/** The Linear resource an entity is: an Epic is a Project, a Task an issue. */
export function linearResourceFor(entity: EntityType): "project" | "issue" {
    switch (entity) {
        case "epic":
            return "project"
        case "task":
            return "issue"
    }
}

// An update in flight needs a published plan.
function assertPlanAnnouncementState(
    announcement: PlanAnnouncement | undefined,
    publication: (PlanComment | PlanDocument)["state"],
    fail: (message: string) => never,
): void {
    if (!announcement) return
    switch (announcement.state) {
        case "plan-update":
        case "announcing":
            if (publication !== "published") fail("a plan announcement requires a published plan")
            return
    }
}
