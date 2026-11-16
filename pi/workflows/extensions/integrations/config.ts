import { type Static, Type } from "typebox"

import { compile } from "../../../extensions/__lib/schema.js"
import {
    camelKeys,
    type DataKeys,
    repositoryConfigFile,
    resolveSettings,
    section,
    snakeKeys,
} from "../../../extensions/__lib/settings.js"
import { PRIORITY_NOT_SET } from "../__lib/domain.js"
import {
    assertGitHubLabels,
    assertLinearLabels,
    GitHubLabelsSchema,
    LinearLabelsSchema,
    LinearPhaseGroupsSchema,
} from "./labels.js"
import { linearPriorityNumber } from "./records.js"

const FieldNameSchema = Type.String({ minLength: 1 })

const PriorityOptionSchema = Type.String({ minLength: 1, pattern: "^(?!not set$)[^\\r\\n]+$" })

export const PriorityFieldSchema = Type.Union([
    Type.Object(
        {
            scope: Type.Enum(["issue"], { type: "string" }),
            field: FieldNameSchema,
            values: Type.Array(PriorityOptionSchema, { minItems: 1, uniqueItems: true }),
        },
        { additionalProperties: false },
    ),
    Type.Object(
        {
            scope: Type.Enum(["project"], { type: "string" }),
            field: FieldNameSchema,
            values: Type.Array(PriorityOptionSchema, { minItems: 1, uniqueItems: true }),
        },
        { additionalProperties: false },
    ),
])
export const PriorityFieldValidator = compile(PriorityFieldSchema)
export type PriorityField = Static<typeof PriorityFieldSchema>

export const PhaseValuesSchema = Type.Object(
    {
        planning: FieldNameSchema,
        execution: FieldNameSchema,
        selfReview: FieldNameSchema,
    },
    { additionalProperties: false },
)
export type PhaseValues = Static<typeof PhaseValuesSchema>

export const GitHubPhaseFieldSchema = Type.Object(
    { field: FieldNameSchema, values: PhaseValuesSchema },
    { additionalProperties: false },
)
export type GitHubPhaseField = Static<typeof GitHubPhaseFieldSchema>

const StatusValuesSchema = Type.Object(
    {
        backlog: Type.String({ minLength: 1 }),
        todo: Type.String({ minLength: 1 }),
        inProgress: Type.String({ minLength: 1 }),
        inReview: Type.String({ minLength: 1 }),
        done: Type.String({ minLength: 1 }),
        canceled: Type.String({ minLength: 1 }),
    },
    { additionalProperties: false },
)

const McpServerSchema = Type.String({ minLength: 1, pattern: "^[A-Za-z0-9_.-]+$" })
const GitHubRepositorySchema = Type.Object(
    { owner: Type.String({ minLength: 1 }), repo: Type.String({ minLength: 1 }) },
    { additionalProperties: false },
)
const GitHubRoleProperties = {
    provider: Type.Enum(["github"], { type: "string" }),
    mcpServer: McpServerSchema,
    repository: GitHubRepositorySchema,
}

export const GitHubTrackerSchema = Type.Object(
    {
        ...GitHubRoleProperties,
        project: Type.Object(
            {
                owner: Type.String({ minLength: 1 }),
                ownerType: Type.Enum(["user", "org"], { type: "string" }),
                number: Type.Integer({ minimum: 1 }),
            },
            { additionalProperties: false },
        ),
        // Managed labels are applied only when configured.
        labels: Type.Optional(GitHubLabelsSchema),
        fields: Type.Object(
            {
                status: Type.Object(
                    {
                        field: FieldNameSchema,
                        values: StatusValuesSchema,
                    },
                    { additionalProperties: false },
                ),
                // Phase is visibility only; without it, workflows track no Phase at all.
                phase: Type.Optional(GitHubPhaseFieldSchema),
                priority: PriorityFieldSchema,
            },
            { additionalProperties: false },
        ),
    },
    { additionalProperties: false },
)
export const GitHubTrackerValidator = compile(GitHubTrackerSchema)
export type GitHubTracker = Static<typeof GitHubTrackerSchema>

export const LinearTrackerSchema = Type.Object(
    {
        provider: Type.Enum(["linear"], { type: "string" }),
        mcpServer: McpServerSchema,
        team: Type.String({ minLength: 1 }),
        // Phase is visibility only; without it, workflows track no Phase at all.
        phase: Type.Optional(LinearPhaseGroupsSchema),
        labels: Type.Optional(LinearLabelsSchema),
        statuses: Type.Object(
            {
                issues: StatusValuesSchema,
                projects: StatusValuesSchema,
            },
            { additionalProperties: false },
        ),
    },
    { additionalProperties: false },
)
export const LinearTrackerValidator = compile(LinearTrackerSchema)
export type LinearTracker = Static<typeof LinearTrackerSchema>

export const TrackerConfigSchema = Type.Union([GitHubTrackerSchema, LinearTrackerSchema])
export const TrackerConfigValidator = compile(TrackerConfigSchema)
export type TrackerConfig = Static<typeof TrackerConfigSchema>

export const GitHubForgeSchema = Type.Object(GitHubRoleProperties, { additionalProperties: false })
export type GitHubForge = Static<typeof GitHubForgeSchema>

export const IntegrationConfigSchema = Type.Object(
    { tracker: TrackerConfigSchema, forge: GitHubForgeSchema },
    { additionalProperties: false },
)
export const IntegrationConfigValidator = compile(IntegrationConfigSchema)
export type IntegrationConfig = Static<typeof IntegrationConfigSchema>

export function assertConfiguredPriority(config: TrackerConfig, priority: string): void {
    if (config.provider === "linear") {
        linearPriorityNumber(priority)
        return
    }
    if (priority === PRIORITY_NOT_SET || config.fields.priority.values.includes(priority)) return
    throw new Error(`priority is not configured for the GitHub tracker: ${JSON.stringify(priority)}`)
}

function assertDistinctStatuses(provider: string, scope: string, statuses: Static<typeof StatusValuesSchema>): void {
    const options = Object.values(statuses)
    if (new Set(options).size !== options.length) {
        throw new Error(`${provider} ${scope} Status mappings must use distinct options`)
    }
}

export function assertDistinctPhases(values: PhaseValues): void {
    const options = Object.values(values)
    if (new Set(options).size !== options.length) throw new Error("Phase mappings must use distinct options")
}

function assertGitHubTrackerInvariants(config: GitHubTracker): void {
    if (config.fields.phase) assertDistinctPhases(config.fields.phase.values)
    const statusOptions = Object.values(config.fields.status.values)
    if (new Set(statusOptions).size !== statusOptions.length) {
        throw new Error("GitHub Status mappings must use distinct options")
    }

    assertGitHubLabels(config.labels)

    const projectFieldNames = [
        config.fields.status.field,
        ...(config.fields.phase ? [config.fields.phase.field] : []),
        ...(config.fields.priority.scope === "project" ? [config.fields.priority.field] : []),
    ]
    const duplicateProjectFields = projectFieldNames.filter(
        (field, index) => projectFieldNames.indexOf(field) !== index,
    )
    if (duplicateProjectFields.length > 0) {
        throw new Error(
            `GitHub Project field names must be distinct: ${[...new Set(duplicateProjectFields)].map((field) => JSON.stringify(field)).join(", ")}`,
        )
    }
}

/**
 * Maps whose keys are names, not schema: label group keys, label value keys, and a GitHub label group's values. Their
 * keys are kept as they are when translating between the file's snake_case and the code's camelCase.
 */
export const integrationDataKeys: DataKeys = (at) => {
    const tail = at.slice(-2).join(".")
    return tail === "labels.groups" || (at.length >= 2 && at[at.length - 2] === "*" && at[at.length - 1] === "values")
}

/** `[tracker]` and `[forge]` as the code names their keys. */
export function integrationsFromFile(value: unknown): unknown {
    return camelKeys(value, integrationDataKeys)
}

/** `[tracker]` or `[forge]` as the file names their keys, for setup to write. */
export function integrationsToFile(value: unknown): unknown {
    return snakeKeys(value, integrationDataKeys)
}

// Workflows need both a tracker and a GitHub forge: the nearest `[tracker]` and `[forge]` in piton's configuration
// chain, each whole.
export function loadIntegrationConfig(cwd: string): IntegrationConfig {
    const settings = resolveSettings(cwd)
    const tracker = section(settings.tracker)
    const forge = section(settings.forge)
    if (!tracker || !forge) {
        const missing = [!tracker && "[tracker]", !forge && "[forge]"].filter(Boolean).join(" and ")
        throw new Error(
            `missing required ${missing}: configure a tracker and a forge in ${repositoryConfigFile("shared")} or a parent folder's`,
        )
    }
    const files = [...new Set([tracker.path, forge.path])].join(" and ")
    return validateIntegrationConfig(integrationsFromFile({ tracker: tracker.value, forge: forge.value }), files)
}

export type IntegrationPresence = { kind: "configured" } | { kind: "absent" } | { kind: "unreadable"; error: string }

/** Whether any level of the chain configures integrations, or why that can't be told. */
export function integrationPresence(cwd: string): IntegrationPresence {
    try {
        const settings = resolveSettings(cwd)
        const tracker = section(settings.tracker)
        const forge = section(settings.forge)
        return tracker || forge ? { kind: "configured" } : { kind: "absent" }
    } catch (error) {
        return { kind: "unreadable", error: error instanceof Error ? error.message : String(error) }
    }
}

/** A validator path (`/tracker/fields/status/values/inProgress`) as the file's dotted key. */
function filePath(pointer: string): string {
    // `at` is the path as integrationDataKeys sees it: schema names, `*` for a data key; list indexes don't count.
    const at: string[] = []
    const named: string[] = []
    for (const part of pointer.split("/").filter(Boolean)) {
        if (/^\d+$/.test(part)) {
            named.push(part)
        } else if (integrationDataKeys(at)) {
            named.push(part)
            at.push("*")
        } else {
            named.push(part.replace(/[A-Z]/g, (c) => `_${c.toLowerCase()}`))
            at.push(part)
        }
    }
    return named.length > 0 ? named.join(".") : "(top level)"
}

/** Validates the merged `[tracker]` and `[forge]`; errors name `files`, where they came from. */
function validateIntegrationConfig(value: unknown, files: string): IntegrationConfig {
    if (!IntegrationConfigValidator.check(value)) {
        // Errors name keys as the file has them: `tracker.fields.status.values.in_progress`.
        const details = IntegrationConfigValidator.errors(value)
            .map((error) => `${filePath(error.path)}: ${error.message}`)
            .join("; ")
        throw new Error(`invalid ${files}: ${details}`)
    }
    try {
        if (value.tracker.provider === "github") assertGitHubTrackerInvariants(value.tracker)
        if (value.tracker.provider === "linear") {
            assertLinearLabels(value.tracker.labels, value.tracker.phase)
            assertDistinctStatuses("Linear", "issue", value.tracker.statuses.issues)
            assertDistinctStatuses("Linear", "Project", value.tracker.statuses.projects)
        }
    } catch (error) {
        const message = error instanceof Error ? error.message : String(error)
        throw new Error(`invalid ${files}: ${message}`)
    }

    return value
}
