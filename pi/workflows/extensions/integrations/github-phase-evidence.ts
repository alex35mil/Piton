import { type Static, Type } from "typebox"

import { compile } from "../../../extensions/__lib/schema.js"
import { mcpResponsePayload, type McpToolCatalog } from "../../../extensions/__lib/mcp.js"
import type { LifecycleState } from "../__lib/domain.js"
import type { EntityStatus } from "../__lib/entity.js"
import { lifecycleFromStatus, type TrackerConfig } from "./config.js"
import { itemFieldEntry } from "./github-project-reads.js"
import { requirePhase, type NativePhaseFacts } from "./phase.js"
import { resourceIdsFromRead } from "./resource-id-operations.js"
import { EvidenceSchema } from "./resource-id-tool.js"

export const GitHubPhaseInputSchema = Type.Object(
    {
        item: {
            ...EvidenceSchema,
            description:
                "Unmodified projects_get get_project_item read of this entity's Project item, with field_names including the configured Status and Phase fields. Read it after any Status change in this operation: lifecycle comes from its Status.",
        },
    },
    { additionalProperties: false },
)
export const GitHubPhaseInputValidator = compile(GitHubPhaseInputSchema)
export type GitHubPhaseInput = Static<typeof GitHubPhaseInputSchema>

/**
 * Extracts lifecycle and Phase facts from the actual Project item read of an initialized GitHub entity.
 * The read carries both fields' identity and current values, so no separate field read is needed; setup
 * validates the Phase options, and GitHub rejects a write of a missing one.
 */
export function githubPhaseFacts(
    tracker: TrackerConfig,
    entity: { name: string; status: EntityStatus },
    input: GitHubPhaseInput,
    tools: McpToolCatalog,
): { lifecycle: LifecycleState; native: NativePhaseFacts } {
    const bound = entity.status.tracker
    if (tracker.provider !== "github" || bound.provider !== "github")
        throw new Error("GitHub Phase requires the configured GitHub tracker and a GitHub tracker issue")
    const native = requirePhase(tracker)
    if (native.provider !== "github") throw new Error("GitHub Phase requires the configured GitHub Phase field")
    const phaseName = native.field.field
    const statusName = tracker.fields.status.field
    // Identity is the same check that records the entity's Project item ID.
    resourceIdsFromRead(
        {
            kind: "github-project-item",
            entity: entity.name,
            mcpServer: tracker.mcpServer,
            repository: bound.repository,
            issueNumber: bound.external.issueNumber,
            project: tracker.project,
        },
        input.item,
        tools,
    )
    const requested = input.item.arguments.field_names
    const missing = [statusName, phaseName].filter((name) => !Array.isArray(requested) || !requested.includes(name))
    if (missing.length)
        throw new Error(
            `Read this entity's Project item with field_names including ${[statusName, phaseName].map((name) => JSON.stringify(name)).join(" and ")}; missing ${missing.join(", ")}`,
        )
    const response = mcpResponsePayload(input.item.response)
    const phase = itemFieldEntry(response, phaseName)
    if (phase.state === "absent")
        throw new Error(`Project item read returned no ${phaseName} entry; read the item again with field_names`)
    const status = itemFieldEntry(response, statusName)
    if (status.state === "absent")
        throw new Error(`Project item read returned no ${statusName} entry; read the item again with field_names`)
    if (status.value === null) throw new Error(`Project item has no ${statusName}; set its lifecycle first`)
    return {
        lifecycle: lifecycleFromStatus(tracker.fields.status.values, status.value),
        native: { provider: "github", field: { name: phaseName, dataType: phase.dataType }, current: phase.value },
    }
}
