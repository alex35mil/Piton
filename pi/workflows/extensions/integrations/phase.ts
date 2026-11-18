import { type Static, Type } from "typebox"

import { compile, parse } from "../../../extensions/__lib/schema.js"
import { LifecycleStateSchema, PhaseSchema, type LifecycleState, type Phase, projectedPhase } from "../__lib/domain.js"
import type { EntityType } from "../__lib/domain.js"
import type { EntityStatus } from "../__lib/entity.js"
import type { GitHubPhaseField, LinearTracker, TrackerConfig } from "./config.js"
import type { LinearPhaseGroup, LinearPhaseGroups } from "./labels.js"
import { assertEntityTracker } from "./policy.js"

const NameSchema = Type.String({ minLength: 1 })
const NullableNameSchema = Type.Union([NameSchema, Type.Null()])

// Normalized facts from complete, fresh native reads; never cached provider state.
export const NativePhaseFactsSchema = Type.Union([
    Type.Object(
        {
            provider: Type.Literal("github"),
            field: Type.Object(
                {
                    name: NameSchema,
                    dataType: NameSchema,
                    options: Type.Array(NameSchema, { uniqueItems: true }),
                },
                { additionalProperties: false },
            ),
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

export const SetWorkflowPhaseSchema = Type.Object(
    { entityDir: NameSchema, phase: PhaseSchema, lifecycle: LifecycleStateSchema, native: NativePhaseFactsSchema },
    { additionalProperties: false },
)
export const SetWorkflowPhaseValidator = compile(SetWorkflowPhaseSchema)
export type SetWorkflowPhaseInput = Static<typeof SetWorkflowPhaseSchema>

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

/** The metadata `phase` property a new entity starts with: none when Phase isn't configured. */
export function initialPhase(tracker: TrackerConfig, phase: Phase): { phase?: Phase } {
    const configured = trackerPhase(tracker)
    switch (configured.state) {
        case "configured":
            return { phase }
        case "notConfigured":
            return {}
    }
}

export function requirePhase(tracker: TrackerConfig): PhaseRepresentation {
    const phase = trackerPhase(tracker)
    switch (phase.state) {
        case "configured":
            return phase.representation
        case "notConfigured":
            throw new Error(PHASE_NOT_CONFIGURED)
    }
}

export function setWorkflowPhase(status: EntityStatus, tracker: TrackerConfig, input: SetWorkflowPhaseInput) {
    parse(SetWorkflowPhaseValidator, input)
    assertEntityTracker(status, tracker)
    if (input.lifecycle !== "inProgress") throw new Error("setting workflow Phase requires fresh native In Progress")
    const phaseMutation = planPhaseMutation(tracker, status.entity, input.phase, input.lifecycle, input.native)
    return { status: { ...status, phase: input.phase }, changed: status.phase !== input.phase, phaseMutation }
}

/** `phase` is the entity's metadata Phase: absent until set, and always absent without configured Phase. */
export function phaseProjectionContract(phase: Phase | undefined, tracker: TrackerConfig, entity: EntityType) {
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
        phase: phase ?? null,
        inProgress: phase === undefined ? null : projectedPhase("inProgress", phase),
        otherwise: null,
        representation,
        ...(phase === undefined ? { next: "metadata has no Phase yet; set it with the Phase tool" } : {}),
        authority: "read fresh native lifecycle; do not infer it from metadata or cached facts",
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
    // GitHub spells the type SINGLE_SELECT in GraphQL and single_select through the MCP.
    if (facts.field.name !== configured.field || facts.field.dataType.toLowerCase() !== "single_select") {
        throw new Error("configured GitHub Phase requires its exact Project single-select field")
    }
    for (const name of Object.values(configured.values)) {
        if (!facts.field.options.includes(name)) throw new Error(`GitHub Phase option is missing: ${name}`)
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

export function planPhaseMutation(
    tracker: TrackerConfig,
    entity: EntityType,
    phase: Phase | undefined,
    lifecycle: LifecycleState,
    facts: NativePhaseFacts,
) {
    const native = requirePhase(tracker)
    // Outside In Progress native Phase is cleared, so a metadata Phase isn't needed to clear it.
    let projected: Phase | null = null
    if (phase !== undefined) projected = projectedPhase(lifecycle, phase)
    else if (lifecycle === "inProgress")
        throw new Error("metadata has no Phase yet; set it with the Phase tool instead of projecting it")
    if (native.provider === "github" && facts.provider === "github") {
        validateGitHubPhaseField(native.field, facts)
        const value = projected === null ? null : native.field.values[projected]
        return {
            provider: "github" as const,
            field: native.field.field,
            value,
            changed: facts.current !== value,
        }
    }
    if (tracker.provider === "linear" && native.provider === "linear" && facts.provider === "linear") {
        const group = resolveLinearPhaseGroup(tracker, entity, facts.catalog)
        const managed = new Set(group.childIds)
        const labels = facts.applied.filter((id) => !managed.has(id))
        if (projected !== null) labels.push(group.values[projected])
        const current = new Set(facts.applied)
        return {
            provider: "linear" as const,
            labels,
            changed: labels.length !== current.size || labels.some((id) => !current.has(id)),
        }
    }
    throw new Error("native Phase facts do not match the configured tracker provider")
}
