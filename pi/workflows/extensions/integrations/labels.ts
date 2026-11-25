import { type Static, Type } from "typebox"

import { type DeliverableKind, DeliverableKindSchema, type EntityType } from "../__lib/domain.js"

/** Workflow items that carry labels: Epics, and Deliverables (Tasks and Gigs). */
export const LabelItemTypeSchema = Type.Enum(["epic", "deliverable"], { type: "string" })
export type LabelItemType = Static<typeof LabelItemTypeSchema>

export function labelItemType(entity: EntityType): LabelItemType {
    switch (entity) {
        case "epic":
            return "epic"
        case "task":
        case "gig":
            return "deliverable"
    }
}

export const CardinalitySchema = Type.Enum(["exactlyOne", "zeroOrOne", "oneOrMore", "zeroOrMore"], {
    type: "string",
})
export type Cardinality = Static<typeof CardinalitySchema>

/*
 * The shapes of `tracker.labels` and Linear's `tracker.phase`: types and setup tool inputs. piton's config module owns
 * their rules (key and color formats, distinct names, the reserved groups); see `config.ts`.
 */

const LabelKeySchema = Type.String()
const LabelNameSchema = Type.String()

/** One tracker label as setup provisions it. Runtime reads only `name`. */
export const LabelDefinitionSchema = Type.Object(
    {
        name: LabelNameSchema,
        color: Type.String({ description: 'A color like "#1D76DB"' }),
        description: Type.String(),
    },
    { additionalProperties: false },
)
export type LabelDefinition = Static<typeof LabelDefinitionSchema>

const LabelValuesSchema = Type.Record(LabelKeySchema, LabelDefinitionSchema, { additionalProperties: false })

const groupProperties = {
    appliesTo: Type.Array(LabelItemTypeSchema),
    cardinality: CardinalitySchema,
    values: LabelValuesSchema,
}

export const GitHubLabelGroupSchema = Type.Object(groupProperties, { additionalProperties: false })
export type GitHubLabelGroup = Static<typeof GitHubLabelGroupSchema>

export const LinearLabelScopeSchema = Type.Enum(["workspace", "team"], { type: "string" })
export type LinearLabelScope = Static<typeof LinearLabelScopeSchema>

export const LinearLabelGroupSchema = Type.Object(
    {
        ...groupProperties,
        scope: LinearLabelScopeSchema,
        // Absent: the values are Linear labels with no native group.
        nativeGroup: Type.Optional(LabelDefinitionSchema),
    },
    { additionalProperties: false },
)
export type LinearLabelGroup = Static<typeof LinearLabelGroupSchema>

export const GitHubLabelEntrySchema = Type.Object({ name: LabelNameSchema }, { additionalProperties: false })
export type GitHubLabelEntry = Static<typeof GitHubLabelEntrySchema>

export const LinearLabelCatalogSchema = Type.Enum(["issue", "project"], { type: "string" })
export type LinearLabelCatalog = Static<typeof LinearLabelCatalogSchema>

export const LinearLabelEntrySchema = Type.Object(
    { name: LabelNameSchema, catalog: LinearLabelCatalogSchema, scope: LinearLabelScopeSchema },
    { additionalProperties: false },
)
export type LinearLabelEntry = Static<typeof LinearLabelEntrySchema>

const GroupsSchema = <T extends typeof GitHubLabelGroupSchema | typeof LinearLabelGroupSchema>(group: T) =>
    Type.Optional(Type.Record(LabelKeySchema, group, { additionalProperties: false }))

export const GitHubLabelsSchema = Type.Object(
    {
        groups: GroupsSchema(GitHubLabelGroupSchema),
        // Labels outside the configuration that the user chose to keep.
        unmanaged: Type.Optional(
            Type.Object({ keep: Type.Array(GitHubLabelEntrySchema) }, { additionalProperties: false }),
        ),
        // Retired (archived) labels that are accounted for, so runtime doesn't report them.
        retired: Type.Optional(Type.Array(GitHubLabelEntrySchema)),
    },
    { additionalProperties: false },
)
export type GitHubLabels = Static<typeof GitHubLabelsSchema>

export const LinearLabelsSchema = Type.Object(
    {
        groups: GroupsSchema(LinearLabelGroupSchema),
        unmanaged: Type.Optional(
            Type.Object({ keep: Type.Array(LinearLabelEntrySchema) }, { additionalProperties: false }),
        ),
        retired: Type.Optional(Type.Array(LinearLabelEntrySchema)),
    },
    { additionalProperties: false },
)
export type LinearLabels = Static<typeof LinearLabelsSchema>

const PhaseLabelValuesSchema = Type.Object(
    {
        planning: LabelDefinitionSchema,
        execution: LabelDefinitionSchema,
        selfReview: LabelDefinitionSchema,
    },
    { additionalProperties: false },
)

/** A Linear Phase label group: always single-select, with one child per Phase. */
export const LinearPhaseGroupSchema = Type.Object(
    {
        scope: LinearLabelScopeSchema,
        group: LabelDefinitionSchema,
        values: PhaseLabelValuesSchema,
    },
    { additionalProperties: false },
)
export type LinearPhaseGroup = Static<typeof LinearPhaseGroupSchema>

export const LinearPhaseGroupsSchema = Type.Object(
    { issues: LinearPhaseGroupSchema, projects: LinearPhaseGroupSchema },
    { additionalProperties: false },
)
export type LinearPhaseGroups = Static<typeof LinearPhaseGroupsSchema>

/** The reserved group whose value comes from the item's workflow Kind. */
export const KIND_GROUP_KEY = "kind"

/**
 * The reserved group whose values mark a Task or Gig with gates: points where the workflow stops for
 * the user's approval. The user sets them on the tracker item; the workflow only reads them.
 */
export const GATE_GROUP_KEY = "gate"

/** The gates the workflow knows how to honor; config may name only these (piton's config module checks it). */
export const GateSchema = Type.Enum(["plan"], { type: "string" })
export type Gate = Static<typeof GateSchema>

export type WorkflowKind = "epic" | DeliverableKind

const DELIVERABLE_KINDS: readonly DeliverableKind[] = DeliverableKindSchema.enum

function kindValuesFor(itemType: LabelItemType): readonly WorkflowKind[] {
    switch (itemType) {
        case "epic":
            return ["epic"]
        case "deliverable":
            return DELIVERABLE_KINDS
    }
}

/**
 * A group's values for one item type, as `[valueKey, definition]` pairs. A group that doesn't apply
 * to the item type has none; only `kind` splits its values by item type.
 */
export function groupValuesFor(
    key: string,
    group: GitHubLabelGroup | LinearLabelGroup,
    itemType: LabelItemType,
): [string, LabelDefinition][] {
    if (!group.appliesTo.includes(itemType)) return []
    const entries = Object.entries(group.values)
    if (key !== KIND_GROUP_KEY) return entries
    const allowed = new Set<string>(kindValuesFor(itemType))
    return entries.filter(([value]) => allowed.has(value))
}

/** Where a label lives: one GitHub repository, or a Linear catalog at workspace or team level. */
export type LabelLocation =
    | { catalog: "repository"; scope: "repository" }
    | { catalog: LinearLabelCatalog; scope: LinearLabelScope }

/** A label as the unknown-label rule sees it; managed labels are excluded before it's asked. */
export interface UnknownLabelCandidate {
    name: string
    location: LabelLocation
    /** The label's native Linear group, when it has one. */
    parentName: string | null
}

export interface UnknownLabelLists {
    keep: readonly { name: string; location: LabelLocation }[]
    retired: readonly { name: string; location: LabelLocation }[]
}

const sameLocation = (a: LabelLocation, b: LabelLocation) => a.catalog === b.catalog && a.scope === b.scope

/**
 * The one rule for an unknown label, shared by setup and runtime: a label that isn't managed is
 * unknown unless it, or the native group it belongs to, is on the keep list, or it's on the retired
 * list. Setup lists exactly these labels, so runtime never mentions one setup won't ask about.
 */
export function isUnknownLabel(label: UnknownLabelCandidate, lists: UnknownLabelLists): boolean {
    const listed = (entries: UnknownLabelLists["keep"], name: string) =>
        entries.some(
            (entry) => sameLocation(entry.location, label.location) && entry.name.toLowerCase() === name.toLowerCase(),
        )
    if (listed(lists.keep, label.name) || listed(lists.retired, label.name)) return false
    return label.parentName === null || !listed(lists.keep, label.parentName)
}
