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
import { type LifecycleState, PRIORITY_NOT_SET } from "../__lib/domain.js"
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
export type StatusValues = Static<typeof StatusValuesSchema>

/** The lifecycle a native Status name stands for, through the configured Status values. */
export function lifecycleFromStatus(values: StatusValues, status: string): LifecycleState {
    const match = (Object.entries(values) as Array<[LifecycleState, string]>).find(([, name]) => name === status)
    if (!match)
        throw new Error(
            `Status ${JSON.stringify(status)} is not a configured lifecycle value; configured: ${Object.values(values).join(", ")}`,
        )
    return match[0]
}

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

/** `provider = "none"` and nothing else: turns a role off, also over a farther level's table. */
export const DisabledRoleSchema = Type.Object(
    { provider: Type.Enum(["none"], { type: "string" }) },
    { additionalProperties: false },
)
export type DisabledRole = Static<typeof DisabledRoleSchema>
const TrackerTableValidator = compile(Type.Union([TrackerConfigSchema, DisabledRoleSchema]))
const ForgeTableValidator = compile(Type.Union([GitHubForgeSchema, DisabledRoleSchema]))

/** Why a role is off: no table anywhere in the chain, or a table that turns it off. */
export type RoleOff = { state: "off"; reason: "absent" } | { state: "off"; reason: "disabled"; file: string }
export type Role<T> = { state: "on"; config: T } | RoleOff

/** The project's two integration roles. Each is optional: a step that needs one refuses when it's off. */
export interface IntegrationRoles {
    tracker: Role<TrackerConfig>
    forge: Role<GitHubForge>
}

export type RoleName = "tracker" | "forge"

/** A step that needs a role that's off. Names the role, why it's off, and the step. */
export class RoleUnavailableError extends Error {
    constructor(
        readonly role: RoleName,
        readonly off: RoleOff,
        readonly operation: string,
    ) {
        super(`${operation} needs the ${role}, which ${roleOffReason(role, off)}`)
        this.name = "RoleUnavailableError"
    }
}

/** Why a role is off, as a sentence ending for messages: "the tracker <reason>". */
export function roleOffReason(role: RoleName, off: RoleOff): string {
    switch (off.reason) {
        case "absent":
            return `isn't configured: no [${role}] in ${repositoryConfigFile("shared")} or a parent folder's`
        case "disabled":
            return `is turned off (provider = "none" in ${off.file})`
    }
}

export function requireTracker(roles: IntegrationRoles, operation: string): TrackerConfig {
    const tracker = roles.tracker
    switch (tracker.state) {
        case "on":
            return tracker.config
        case "off":
            throw new RoleUnavailableError("tracker", tracker, operation)
    }
}

export function requireForge(roles: IntegrationRoles, operation: string): GitHubForge {
    const forge = roles.forge
    switch (forge.state) {
        case "on":
            return forge.config
        case "off":
            throw new RoleUnavailableError("forge", forge, operation)
    }
}

/** A role as context results report it: `on` with its config, or `off` with the reason. */
export function roleContext<P extends string, T>(role: RoleName, value: Role<T>, provider: (config: T) => P) {
    switch (value.state) {
        case "on":
            return { state: "enabled" as const, provider: provider(value.config), config: value.config }
        case "off":
            return { state: "disabled" as const, reason: `The ${role} ${roleOffReason(role, value)}.` }
    }
}

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

/**
 * The nearest `[tracker]` and `[forge]` in piton's configuration chain, each whole and each optional. A missing table
 * or `provider = "none"` turns that role off; an invalid table fails, so a typo never silently disables a role.
 */
export function loadIntegrationRoles(cwd: string): IntegrationRoles {
    const settings = resolveSettings(cwd)
    const tracker = section(settings.tracker)
    const forge = section(settings.forge)
    return {
        tracker: tracker
            ? validateTracker(integrationsFromFile(tracker.value), tracker.path)
            : { state: "off", reason: "absent" },
        forge: forge
            ? validateForge(integrationsFromFile(forge.value), forge.path)
            : { state: "off", reason: "absent" },
    }
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

type TableValidator<T> = {
    check(value: unknown): value is T
    errors(value: unknown): { path: string; message: string }[]
}

/** Checks one role's table; errors name the file and keys as the file has them (`tracker.fields.status.values.in_progress`). */
function checkTable<T>(validator: TableValidator<T>, role: RoleName, value: unknown, file: string): T {
    if (validator.check(value)) return value
    const details = validator
        .errors(value)
        .map((error) => `${filePath(`/${role}${error.path}`)}: ${error.message}`)
        .join("; ")
    throw new Error(`invalid ${file}: ${details}`)
}

function validateTracker(value: unknown, file: string): Role<TrackerConfig> {
    const tracker = checkTable(TrackerTableValidator, "tracker", value, file)
    switch (tracker.provider) {
        case "none":
            return { state: "off", reason: "disabled", file }
        case "github":
        case "linear":
            break
    }
    try {
        switch (tracker.provider) {
            case "github":
                assertGitHubTrackerInvariants(tracker)
                break
            case "linear":
                assertLinearLabels(tracker.labels, tracker.phase)
                assertDistinctStatuses("Linear", "issue", tracker.statuses.issues)
                assertDistinctStatuses("Linear", "Project", tracker.statuses.projects)
                break
        }
    } catch (error) {
        const message = error instanceof Error ? error.message : String(error)
        throw new Error(`invalid ${file}: ${message}`)
    }
    return { state: "on", config: tracker }
}

function validateForge(value: unknown, file: string): Role<GitHubForge> {
    const forge = checkTable(ForgeTableValidator, "forge", value, file)
    switch (forge.provider) {
        case "none":
            return { state: "off", reason: "disabled", file }
        case "github":
            return { state: "on", config: forge }
    }
}
