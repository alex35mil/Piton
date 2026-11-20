import * as path from "node:path"

import { withFileMutationQueue, type ExtensionAPI } from "@earendil-works/pi-coding-agent"
import { Type } from "typebox"

import { parse } from "../../../extensions/__lib/schema.js"
import { getMcpToolName, type McpToolCatalog } from "../../../extensions/__lib/mcp.js"
import * as project from "../../../extensions/__lib/project.js"
import { workflowTools } from "../__lib/tools.js"
import {
    ENTITY_METADATA_FILE,
    readEntityStatus,
    resolveEntityDirectory,
    writeEntityStatus,
    type EntityStatus,
} from "../__lib/entity.js"
import { assertArtifactPersistencePrepared } from "../__lib/project-config.js"
import { loadIntegrationRoles, requireTracker, type TrackerConfig } from "./config.js"
import {
    SetWorkflowPhaseSchema,
    SetWorkflowPhaseValidator,
    setWorkflowPhase,
    LinearPhaseInputSchema,
    LinearPhaseInputValidator,
    linearPhaseFacts,
} from "./phase.js"
import { GitHubPhaseInputSchema, GitHubPhaseInputValidator, githubPhaseFacts } from "./github-phase-evidence.js"

async function updateEntity<T extends { status: EntityStatus; metadataChanged: boolean }>(
    cwd: string,
    raw: string,
    name: string,
    operation: (status: EntityStatus, tracker: TrackerConfig, directory: string) => T,
) {
    const root = project.resolveRootDir(cwd)
    const directory = resolveEntityDirectory(root, cwd, raw.replace(/^@/, ""))
    return withFileMutationQueue(path.join(directory, ENTITY_METADATA_FILE), async () => {
        assertArtifactPersistencePrepared(root)
        const tracker = requireTracker(loadIntegrationRoles(root), name)
        const result = operation(readEntityStatus(directory), tracker, directory)
        if (result.metadataChanged) writeEntityStatus(directory, result.status)
        const { status, ...outcome } = result
        return { entity: path.basename(directory), ...(status.phase ? { phase: status.phase } : {}), ...outcome }
    })
}

export function registerWorkflowOperations(pi: ExtensionAPI, mcpTools: McpToolCatalog): void {
    const tools = workflowTools(pi, "integrations")
    const resolveTool = (server: string, operation: string) => getMcpToolName(server, operation, mcpTools)
    const { entityDir, phase, lifecycle } = SetWorkflowPhaseSchema.properties
    const description = (facts: string) =>
        `Set In Progress workflow Phase and calculate reconciliation from ${facts}. Local metadata only. \`metadataChanged\` and \`nativeChanged\` report each side; a changed native assignment carries the registered provider call as top-level \`mutation\`, to run unchanged. Does not change lifecycle or grant approval.`

    const name = "set_github_workflow_phase"
    tools.registerTool({
        name,
        label: name,
        description: description(
            "the actual Project item read, which gives both lifecycle (its Status) and the current Phase",
        ),
        parameters: Type.Object({ entityDir, phase, native: GitHubPhaseInputSchema }, { additionalProperties: false }),
        async execute(_id, params, _signal, _update, ctx) {
            const result = await updateEntity(ctx.cwd, params.entityDir, name, (status, tracker, directory) => {
                const facts = githubPhaseFacts(
                    tracker,
                    { name: path.basename(directory), status },
                    parse(GitHubPhaseInputValidator, params.native),
                    mcpTools,
                )
                return setWorkflowPhase(
                    status,
                    tracker,
                    parse(SetWorkflowPhaseValidator, { ...params, lifecycle: facts.lifecycle, native: facts.native }),
                    resolveTool,
                )
            })
            return { content: [{ type: "text", text: JSON.stringify(result, null, 2) }], details: result }
        },
    })

    const linear = "set_linear_workflow_phase"
    tools.registerTool({
        name: linear,
        label: linear,
        description: description(
            "fresh lifecycle and label catalog and selection facts; omit only genuinely empty native leaf values",
        ),
        parameters: Type.Object(
            { entityDir, phase, lifecycle, native: LinearPhaseInputSchema },
            { additionalProperties: false },
        ),
        async execute(_id, params, _signal, _update, ctx) {
            const result = await updateEntity(ctx.cwd, params.entityDir, linear, (status, tracker) => {
                const native = linearPhaseFacts(parse(LinearPhaseInputValidator, params.native))
                return setWorkflowPhase(
                    status,
                    tracker,
                    parse(SetWorkflowPhaseValidator, { ...params, native }),
                    resolveTool,
                )
            })
            return { content: [{ type: "text", text: JSON.stringify(result, null, 2) }], details: result }
        },
    })
}
