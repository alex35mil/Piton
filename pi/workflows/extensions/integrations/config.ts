import { type Static, Type } from "typebox"

import {
    camelKeys,
    type DataKeys,
    repositoryConfigFile,
    type ResolvedRole,
    resolveSettings,
    section,
    snakeKeys,
    validateTable,
} from "../../../extensions/__lib/settings.js"
import { type LifecycleState, PRIORITY_NOT_SET } from "../__lib/domain.js"
import { GitHubLabelsSchema, LinearLabelsSchema, LinearPhaseGroupsSchema } from "./labels.js"
import { linearPriorityNumber } from "./records.js"

/*
 * The shapes of `[tracker]` and `[forge]` with the code's key names: the types the code reads, and the input schemas of
 * the setup tools that produce them. piton's config module owns their rules (which values are valid, which must be
 * distinct, the reserved label groups): `loadIntegrationRoles` reads tables it validated, and setup checks a table with
 * `validateIntegration` before changing anything at a provider. Keep these to shape: types, keys, and enums.
 */

const FieldNameSchema = Type.String()

export const PriorityFieldSchema = Type.Union([
    Type.Object(
        {
            scope: Type.Enum(["issue"], { type: "string" }),
            field: FieldNameSchema,
            values: Type.Array(Type.String()),
        },
        { additionalProperties: false },
    ),
    Type.Object(
        {
            scope: Type.Enum(["project"], { type: "string" }),
            field: FieldNameSchema,
            values: Type.Array(Type.String()),
        },
        { additionalProperties: false },
    ),
])
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
        backlog: FieldNameSchema,
        todo: FieldNameSchema,
        inProgress: FieldNameSchema,
        inReview: FieldNameSchema,
        done: FieldNameSchema,
        canceled: FieldNameSchema,
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

const McpServerSchema = Type.String()
const GitHubRepositorySchema = Type.Object(
    { owner: Type.String(), repo: Type.String() },
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
                owner: Type.String(),
                ownerType: Type.Enum(["user", "org"], { type: "string" }),
                number: Type.Integer(),
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
export type GitHubTracker = Static<typeof GitHubTrackerSchema>

export const LinearTrackerSchema = Type.Object(
    {
        provider: Type.Enum(["linear"], { type: "string" }),
        mcpServer: McpServerSchema,
        team: Type.String(),
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
export type LinearTracker = Static<typeof LinearTrackerSchema>

export const TrackerConfigSchema = Type.Union([GitHubTrackerSchema, LinearTrackerSchema])
export type TrackerConfig = Static<typeof TrackerConfigSchema>

export const GitHubForgeSchema = Type.Object(GitHubRoleProperties, { additionalProperties: false })
export type GitHubForge = Static<typeof GitHubForgeSchema>

/** `provider = "none"` and nothing else: turns a role off, also over a farther level's table. */
export const DisabledRoleSchema = Type.Object(
    { provider: Type.Enum(["none"], { type: "string" }) },
    { additionalProperties: false },
)
export type DisabledRole = Static<typeof DisabledRoleSchema>

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
 * The tracker and forge as piton resolves them: each the nearest table in its configuration chain, validated there (the
 * only implementation of their schema and rules). A missing table or `provider = "none"` turns that role off; an invalid
 * table fails with piton's message naming its key and line, so a typo never silently disables a role.
 */
export function loadIntegrationRoles(cwd: string): IntegrationRoles {
    const settings = resolveSettings(cwd)
    return {
        tracker: role<TrackerConfig>(section(settings.tracker)),
        forge: role<GitHubForge>(section(settings.forge)),
    }
}

/** A resolved role with the code's key names. Its config is valid: piton checked it. */
function role<T>(resolved: ResolvedRole): Role<T> {
    switch (resolved.state) {
        case "on":
            return { state: "on", config: integrationsFromFile(resolved.config) as T }
        case "off":
            switch (resolved.reason) {
                case "absent":
                    return { state: "off", reason: "absent" }
                case "disabled":
                    return { state: "off", reason: "disabled", file: resolved.path }
            }
    }
}

/**
 * A tracker or forge table piton accepts, from setup's values (the code's key names), checked as the file would be read
 * before anything changes at a provider. Throws piton's message, which names the key at fault.
 */
export function validateIntegration<T>(table: "tracker" | "forge", value: T): T {
    return integrationsFromFile(validateTable(table, integrationsToFile(value))) as T
}

export type IntegrationPresence = { kind: "configured" } | { kind: "absent" } | { kind: "unreadable"; error: string }

/**
 * Whether any level of the chain configures integrations, or why that can't be told. A role turned off
 * (`provider = "none"`) counts as configured: the repository uses workflows without that role.
 */
export function integrationPresence(cwd: string): IntegrationPresence {
    try {
        const settings = resolveSettings(cwd)
        const roles = [section(settings.tracker), section(settings.forge)]
        const configured = roles.some((value) => {
            switch (value.state) {
                case "on":
                    return true
                case "off":
                    return value.reason === "disabled"
            }
        })
        return configured ? { kind: "configured" } : { kind: "absent" }
    } catch (error) {
        return { kind: "unreadable", error: error instanceof Error ? error.message : String(error) }
    }
}
