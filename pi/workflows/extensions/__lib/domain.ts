import { type Static, Type } from "typebox"
import { compile } from "../../../extensions/__lib/schema.js"
import { PITON_DIR } from "../../../extensions/__lib/settings.js"

// Everything under the local root is machine-local and never committed.
export const PROJECT_LOCAL_ROOT = `${PITON_DIR}/.local`
export const EPIC_ROOT = `${PROJECT_LOCAL_ROOT}/epics`
export const GIG_ROOT = `${PROJECT_LOCAL_ROOT}/gigs`

export const EntityTypeSchema = Type.Enum(["epic", "task", "gig"], {
    type: "string",
})
export type EntityType = Static<typeof EntityTypeSchema>

export const BranchFormatSchema = Type.Enum(["identifier-title", "username/identifier-title", "tracker"], {
    type: "string",
})
export type BranchFormat = Static<typeof BranchFormatSchema>
export type GeneratedBranchFormat = Exclude<BranchFormat, "tracker">

export const DeliverableKindSchema = Type.Enum(["feature", "bugfix", "research", "refactor", "audit", "chore"], {
    type: "string",
})
export const DeliverableKindValidator = compile(DeliverableKindSchema)
export type DeliverableKind = Static<typeof DeliverableKindSchema>

export const QueueStateSchema = Type.Enum(["backlog", "todo"], {
    type: "string",
})
export type QueueState = Static<typeof QueueStateSchema>

export const InitializedLifecycleStateSchema = Type.Enum(["inProgress", "inReview", "done", "canceled"], {
    type: "string",
})
export type InitializedLifecycleState = Static<typeof InitializedLifecycleStateSchema>

export type FinishedLifecycleState = Extract<InitializedLifecycleState, "done" | "canceled">

export function isFinishedLifecycleState(state: InitializedLifecycleState): state is FinishedLifecycleState {
    return state === "done" || state === "canceled"
}

export const LifecycleStateSchema = Type.Enum(["backlog", "todo", "inProgress", "inReview", "done", "canceled"], {
    type: "string",
})
export type LifecycleState = Static<typeof LifecycleStateSchema>

export const PhaseSchema = Type.Enum(["planning", "execution", "selfReview"], {
    type: "string",
})
export const PhaseValidator = compile(PhaseSchema)
export type Phase = Static<typeof PhaseSchema>

export function projectedPhase(lifecycle: LifecycleState, phase: Phase): Phase | null {
    return lifecycle === "inProgress" ? phase : null
}

export const PRIORITY_NOT_SET = "not set" as const
export const PrioritySchema = Type.String({
    minLength: 1,
    pattern: "^[^\\r\\n]+$",
})
export const PriorityValidator = compile(PrioritySchema)
export type Priority = Static<typeof PrioritySchema>

// The tracker object is the entity's identity. Its reference names the directory and
// generated branches: a GitHub issue number, a Linear issue identifier, or, for a Linear
// Project, which has no number, its creation date so Epics still sort chronologically.
export const LINEAR_IDENTIFIER_PATTERN = "^[A-Z][A-Z0-9]*-[1-9][0-9]*$"
export const TIMESTAMP_PATTERN = "^[0-9]{4}-[0-9]{2}-[0-9]{2}T[^\\r\\n]+$"
// A local Gig has no tracker object; its creation date names it, like a Linear Project.
export type TrackerReference =
    | { provider: "github"; issueNumber: number }
    | { provider: "linear"; identifier: string }
    | { provider: "linear"; projectCreatedAt: string }
    | { provider: "local"; createdAt: string }

export function trackerReferenceLabel(reference: TrackerReference): string | undefined {
    switch (reference.provider) {
        case "github":
            return `#${reference.issueNumber}`
        case "linear":
            return "identifier" in reference ? reference.identifier : undefined
        case "local":
            return undefined
    }
}

export function entityName(reference: TrackerReference, slug: string): string {
    switch (reference.provider) {
        case "github":
            return `${reference.issueNumber}-${slug}`
        case "linear":
            if ("identifier" in reference) return `${reference.identifier}-${slug}`
            return `${datePrefix(reference.projectCreatedAt)}-${slug}`
        case "local":
            return `${datePrefix(reference.createdAt)}-${slug}`
    }
}

// The UTC calendar date of an ISO timestamp as YYYYMMDD.
export function datePrefix(timestamp: string): string {
    const date = new Date(timestamp)
    if (!new RegExp(TIMESTAMP_PATTERN).test(timestamp) || Number.isNaN(date.getTime()))
        throw new Error(`invalid timestamp ${JSON.stringify(timestamp)}`)
    return date.toISOString().slice(0, 10).replaceAll("-", "")
}

export const ReadyBranchContractSchema = Type.Object(
    {
        state: Type.Literal("ready"),
        name: Type.String({ minLength: 1 }),
        start: Type.String({ minLength: 1 }),
        target: Type.String({ minLength: 1 }),
        source: Type.Union([Type.Literal("generated"), Type.Literal("tracker")]),
    },
    { additionalProperties: false },
)
export type ReadyBranchContract = Static<typeof ReadyBranchContractSchema>

export function slugify(value: string): string {
    const slug = value
        .normalize("NFKD")
        .toLowerCase()
        .replace(/[^a-z0-9]+/g, "-")
        .replace(/^-+|-+$/g, "")
    if (!slug) throw new Error("slug must contain at least one ASCII letter or number")
    return slug
}

export function normalizePriority(value: string): Priority {
    const priority = value.trim()
    if (!PriorityValidator.check(priority)) {
        throw new Error(`priority must be one explicit configured value or ${JSON.stringify(PRIORITY_NOT_SET)}`)
    }
    return priority
}

export function validateDeliverableKind(value: string): DeliverableKind {
    if (!DeliverableKindValidator.check(value)) {
        throw new Error(`invalid Deliverable kind ${JSON.stringify(value)}`)
    }
    return value
}

export function formatBranchName(name: string, format: GeneratedBranchFormat, username?: string): string {
    const branch = name.toLowerCase()
    switch (format) {
        case "identifier-title":
            return branch
        case "username/identifier-title":
            return `${requireUsername(username)}/${branch}`
    }
}

export function readyBranch(
    name: string,
    start: string,
    source: "generated" | "tracker",
    target = start,
): ReadyBranchContract {
    return { state: "ready", name, start, target, source }
}

function requireUsername(username: string | undefined): string {
    if (!username) throw new Error("branch format requires configured username")
    return username
}
