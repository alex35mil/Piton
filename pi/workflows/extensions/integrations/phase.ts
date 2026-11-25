import { type Static, Type } from "typebox"

import { compile } from "../../../extensions/__lib/schema.js"
import { type LifecycleState, type Phase, projectedPhase } from "../__lib/domain.js"
import type { EntityType } from "../__lib/domain.js"
import type { EntityStatus } from "../__lib/entity.js"
import type { GitHubPhaseField, LinearTracker, TrackerConfig } from "./config.js"
import type { LinearPhaseGroup, LinearPhaseGroups } from "./labels.js"
import { requireTrackerItem, type TrackerRecord } from "./records.js"

const NameSchema = Type.String({ minLength: 1 })
const NullableNameSchema = Type.Union([NameSchema, Type.Null()])

// Normalized facts from complete, fresh native reads; never cached provider state.
export const NativePhaseFactsSchema = Type.Union([
    Type.Object(
        {
            provider: Type.Literal("github"),
            field: Type.Object({ name: NameSchema, dataType: NameSchema }, { additionalProperties: false }),
            current: NullableNameSchema,
        },
        { additionalProperties: false },
    ),
    Type.Object(
        {
            provider: Type.Literal("linear"),
            catalog: Type.Array(
                Type.Object(
                    {
                        id: NameSchema,
                        name: NameSchema,
                        team: NullableNameSchema,
                        parentId: NullableNameSchema,
                        isGroup: Type.Boolean(),
                        groupType: Type.Union([Type.Literal("singleSelect"), Type.Literal("multiSelect"), Type.Null()]),
                        retiredAt: NullableNameSchema,
                    },
                    { additionalProperties: false },
                ),
            ),
            applied: Type.Array(NameSchema, { uniqueItems: true }),
        },
        { additionalProperties: false },
    ),
])
export const NativePhaseFactsValidator = compile(NativePhaseFactsSchema)
export type NativePhaseFacts = Static<typeof NativePhaseFactsSchema>
type LinearFacts = Extract<NativePhaseFacts, { provider: "linear" }>

/**
 * The native Phase to show: one while In Progress, `none` to clear it otherwise. A string enum, not a
 * nullable union, so every harness keeps the choice in the model-facing schema.
 */
export const TargetPhaseSchema = Type.Enum(["planning", "execution", "selfReview", "none"], {
    type: "string",
    description:
        "The Phase to show: planning, execution, or selfReview while the item is In Progress; none otherwise, which clears it.",
})
export type TargetPhase = Static<typeof TargetPhaseSchema>

export function targetPhase(target: TargetPhase): Phase | null {
    switch (target) {
        case "planning":
        case "execution":
        case "selfReview":
            return target
        case "none":
            return null
    }
}

export const LinearPhaseInputSchema = Type.Object(
    {
        catalog: Type.Array(
            Type.Object(
                {
                    id: NameSchema,
                    name: NameSchema,
                    team: Type.Optional(NameSchema),
                    parentId: Type.Optional(NameSchema),
                    isGroup: Type.Boolean(),
                    groupType: Type.Optional(Type.Enum(["singleSelect", "multiSelect"], { type: "string" })),
                    retiredAt: Type.Optional(NameSchema),
                },
                { additionalProperties: false },
            ),
        ),
        applied: Type.Array(NameSchema, { uniqueItems: true }),
    },
    { additionalProperties: false },
)
export const LinearPhaseInputValidator = compile(LinearPhaseInputSchema)

/** Absence denotes the observed native empty value, never missing overall read evidence. */
export function linearPhaseFacts(input: Static<typeof LinearPhaseInputSchema>): NativePhaseFacts {
    return {
        provider: "linear",
        catalog: input.catalog.map((label) => ({
            ...label,
            team: label.team ?? null,
            parentId: label.parentId ?? null,
            groupType: label.groupType ?? null,
            retiredAt: label.retiredAt ?? null,
        })),
        applied: input.applied,
    }
}

/** How the configured tracker shows Phase natively, when it does. */
export type PhaseRepresentation =
    | { provider: "github"; field: GitHubPhaseField }
    | { provider: "linear"; groups: LinearPhaseGroups }

export type TrackerPhase = { state: "configured"; representation: PhaseRepresentation } | { state: "notConfigured" }

/** Phase is optional configuration; without it, workflows track no Phase, locally or natively. */
export function trackerPhase(tracker: TrackerConfig): TrackerPhase {
    switch (tracker.provider) {
        case "github":
            return tracker.fields.phase
                ? { state: "configured", representation: { provider: "github", field: tracker.fields.phase } }
                : { state: "notConfigured" }
        case "linear":
            return tracker.phase
                ? { state: "configured", representation: { provider: "linear", groups: tracker.phase } }
                : { state: "notConfigured" }
    }
}

export const PHASE_NOT_CONFIGURED = "Phase isn't configured for this tracker; skip every Phase step"

export function requirePhase(tracker: TrackerConfig): PhaseRepresentation {
    const phase = trackerPhase(tracker)
    switch (phase.state) {
        case "configured":
            return phase.representation
        case "notConfigured":
            throw new Error(PHASE_NOT_CONFIGURED)
    }
}

/** How the tracker shows the entity's Phase, when it does. The Phase itself lives only on the tracker. */
export function phaseProjectionContract(tracker: TrackerConfig, entity: EntityType) {
    const configured = trackerPhase(tracker)
    switch (configured.state) {
        case "notConfigured":
            return { state: "notConfigured" as const, instruction: PHASE_NOT_CONFIGURED }
        case "configured":
            break
    }
    const native = configured.representation
    const representation =
        native.provider === "github"
            ? { provider: native.provider, field: native.field }
            : { provider: native.provider, labels: linearGroup(native.groups, entity) }
    return {
        state: "configured" as const,
        representation,
        authority:
            "native Phase shows only In Progress work; read fresh native lifecycle and Phase, never cached facts",
    }
}

function linearGroup(groups: LinearPhaseGroups, entity: EntityType): LinearPhaseGroup {
    return entity === "epic" ? groups.projects : groups.issues
}

/** The entity's Linear Phase group for its label kind, when Phase is configured. */
export function linearPhaseGroup(groups: LinearPhaseGroups | undefined, entity: EntityType) {
    return groups ? linearGroup(groups, entity) : undefined
}

export function validateGitHubPhaseField(
    configured: GitHubPhaseField,
    facts: Extract<NativePhaseFacts, { provider: "github" }>,
): void {
    // GitHub spells the type SINGLE_SELECT in GraphQL and single_select through the MCP. Setup validates
    // the options, and GitHub rejects a write of a missing one, so runtime checks only identity and type.
    if (facts.field.name !== configured.field || facts.field.dataType.toLowerCase() !== "single_select") {
        throw new Error("configured GitHub Phase requires its exact Project single-select field")
    }
}

export function resolveLinearPhaseGroup(tracker: LinearTracker, entity: EntityType, catalog: LinearFacts["catalog"]) {
    if (!tracker.phase) throw new Error(PHASE_NOT_CONFIGURED)
    const configured = linearGroup(tracker.phase, entity)
    const team = configured.scope === "workspace" ? null : tracker.team
    const groups = catalog.filter((label) => label.name === configured.group.name && label.team === team)
    if (groups.length !== 1 || !groups[0].isGroup || groups[0].retiredAt !== null) {
        throw new Error("Linear Phase requires one active group in the exact configured scope")
    }
    const group = groups[0]
    // Both label kinds can be multi-select. A Project label group without a type is single-select
    // by Linear's schema contract; an issue label group always reports its type.
    const singleSelect = group.groupType === "singleSelect" || (entity === "epic" && group.groupType === null)
    if (!singleSelect) {
        throw new Error(`Linear ${entity === "epic" ? "Project" : "issue"} Phase requires a single-select group`)
    }
    const children = catalog.filter((label) => label.parentId === group.id)
    const values = Object.fromEntries(
        Object.entries(configured.values).map(([phase, { name }]) => {
            const matches = children.filter((label) => label.name === name)
            if (
                matches.length !== 1 ||
                matches[0].isGroup ||
                matches[0].retiredAt !== null ||
                matches[0].team !== team
            ) {
                throw new Error(`Linear Phase requires one active child in its configured group: ${name}`)
            }
            return [phase, matches[0].id]
        }),
    ) as Record<Phase, string>
    return { groupId: group.id, childIds: children.map((label) => label.id), values }
}

/** Resolves a configured server's operation to its registered MCP tool name; throws when it isn't registered. */
export type McpToolResolver = (server: string, operation: string) => string

/** The registered provider call that applies a changed Phase assignment, run unchanged. */
export interface PhaseProviderCall {
    tool: string
    arguments: Record<string, unknown>
}

/** The tracker item a Phase assignment applies to. */
export interface PhaseItem {
    entity: EntityType
    tracker: TrackerRecord
}

export function phaseItem(status: EntityStatus, operation: string): PhaseItem {
    return { entity: status.entity, tracker: requireTrackerItem(status.tracker, operation) }
}

/** The absolute native Phase assignment: a GitHub field value, or a Linear issue's or Project's full label set. */
export type PhaseAssignment =
    | { provider: "github"; field: string; value: string | null }
    | { provider: "linear"; labels: string[] }

/**
 * Only a changed native assignment carries the call that applies it, as top-level `mutation` like every other
 * prepared provider call; an unchanged one has nothing to run.
 */
export type PhaseMutation =
    | { assignment: PhaseAssignment; nativeChanged: false }
    | { assignment: PhaseAssignment; nativeChanged: true; mutation: PhaseProviderCall }

function phaseMutation(
    assignment: PhaseAssignment,
    changed: boolean,
    mutation: () => PhaseProviderCall,
): PhaseMutation {
    return changed ? { assignment, nativeChanged: true, mutation: mutation() } : { assignment, nativeChanged: false }
}

export function planPhaseMutation(
    tracker: TrackerConfig,
    item: PhaseItem,
    phase: Phase | null,
    lifecycle: LifecycleState,
    facts: NativePhaseFacts,
    resolveTool: McpToolResolver,
): PhaseMutation {
    const native = requirePhase(tracker)
    const call = (operation: string, args: Record<string, unknown>): PhaseProviderCall => ({
        tool: resolveTool(tracker.mcpServer, operation),
        arguments: args,
    })
    const projected = projectedPhase(lifecycle, phase)
    if (
        tracker.provider === "github" &&
        native.provider === "github" &&
        facts.provider === "github" &&
        item.tracker.provider === "github"
    ) {
        validateGitHubPhaseField(native.field, facts)
        const value = projected === null ? null : native.field.values[projected]
        const field = native.field.field
        const { repository, external } = item.tracker
        return phaseMutation({ provider: "github", field, value }, facts.current !== value, () =>
            call("projects_write", {
                method: "update_project_item",
                owner: tracker.project.owner,
                owner_type: tracker.project.ownerType,
                project_number: tracker.project.number,
                item_owner: repository.owner,
                item_repo: repository.repo,
                issue_number: external.issueNumber,
                updated_field: { name: field, value },
            }),
        )
    }
    if (
        tracker.provider === "linear" &&
        native.provider === "linear" &&
        facts.provider === "linear" &&
        item.tracker.provider === "linear"
    ) {
        const record = item.tracker
        const group = resolveLinearPhaseGroup(tracker, item.entity, facts.catalog)
        const managed = new Set(group.childIds)
        const labels = facts.applied.filter((id) => !managed.has(id))
        if (projected !== null) labels.push(group.values[projected])
        const current = new Set(facts.applied)
        const changed = labels.length !== current.size || labels.some((id) => !current.has(id))
        return phaseMutation({ provider: "linear", labels }, changed, () => {
            // Only `id` and the full label set: the write changes nothing but labels.
            switch (record.resource) {
                case "project":
                    return call("save_project", { id: record.external.projectId, labels })
                case "task-issue":
                case "gig-issue":
                    return call("save_issue", { id: record.external.issueId, labels })
            }
        })
    }
    throw new Error("native Phase facts do not match the configured tracker provider")
}
