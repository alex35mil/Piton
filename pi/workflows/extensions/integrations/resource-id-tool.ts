import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent"
import { Type } from "typebox"
import { compile, parse } from "../../../extensions/__lib/schema.js"
import { workflowTools } from "../__lib/tools.js"

import type { McpToolCatalog } from "../../../extensions/__lib/mcp.js"
import { isResourceIdsKind, OperationResourceIdsMemo, type ResourceIds } from "./resource-ids.js"
import { linkPullRequestIssue } from "./forge/github/issue-link.js"
import { findPullRequest } from "./forge/github/find-pull-request.js"
import { beginPullRequestCreation, recordPullRequestCreation } from "./creation.js"
import {
    evidenceResourceKind,
    invalidateStaleResourceIds,
    recordResourceIdsEvidence,
    selectPullRequestResourceIds,
    resolveResourceIdsContext,
    resolveResourceIds,
} from "./resource-id-operations.js"

const ResourceSchema = Type.Enum(
    ["github-issue", "github-project-item", "linear-project", "linear-issue", "github-pull-request"],
    { type: "string" },
)
const common = { entityDir: Type.String({ minLength: 1 }), resource: ResourceSchema }
export const EvidenceSchema = Type.Object(
    {
        tool: Type.String({ minLength: 1 }),
        arguments: Type.Record(Type.String(), Type.Unknown()),
        response: Type.Unknown({ description: "Actual provider JSON or complete native MCP result." }),
    },
    { additionalProperties: false },
)
export const ResourceIdsToolSchema = Type.Union([
    Type.Object(
        { entityDir: common.entityDir, action: Type.Literal("beginCreation") },
        { additionalProperties: false },
    ),
    Type.Object(
        { entityDir: common.entityDir, action: Type.Literal("recordCreation"), evidence: EvidenceSchema },
        { additionalProperties: false },
    ),
    Type.Object(
        {
            entityDir: common.entityDir,
            resource: Type.Literal("github-pull-request"),
            action: Type.Literal("select"),
            evidence: EvidenceSchema,
        },
        { additionalProperties: false },
    ),
    Type.Object({ ...common, action: Type.Literal("resolve") }, { additionalProperties: false }),
    // The resource kind comes from the evidence's tool and method.
    Type.Object(
        { entityDir: common.entityDir, action: Type.Literal("record"), evidence: EvidenceSchema },
        { additionalProperties: false },
    ),
    Type.Object(
        { entityDir: common.entityDir, action: Type.Literal("invalidate"), evidence: EvidenceSchema },
        { additionalProperties: false },
    ),
])
export const ResourceIdsToolValidator = compile(ResourceIdsToolSchema)

// The PR is recorded either way; the link only reports how it went.
async function linkIssue(ctx: ExtensionContext, entityDir: string, ids: ResourceIds) {
    if (!isResourceIdsKind(ids, "github-pull-request")) throw new Error("issue linking requires PR resource IDs")
    const status = resolveResourceIdsContext(ctx.cwd, entityDir, "github-pull-request").status
    return linkPullRequestIssue(ctx.cwd, status, ids.value.number, { projectTrusted: ctx.isProjectTrusted() })
}

export function registerResourceIds(pi: ExtensionAPI, memo: OperationResourceIdsMemo, tools: McpToolCatalog): void {
    const toolsApi = workflowTools(pi, "integrations")
    const [begin, creation, select, resolve, record, invalidate] = ResourceIdsToolSchema.anyOf
    const operations = [
        ["begin_resource_creation", "beginCreation", begin],
        ["record_resource_creation", "recordCreation", creation],
        ["select_pull_request_resource", "select", select],
        ["resolve_resource_ids", "resolve", resolve],
        ["record_resource_ids", "record", record],
        ["invalidate_resource_ids", "invalidate", invalidate],
    ] as const
    for (const [name, action, schema] of operations) {
        const parameters = Type.Omit(schema, action === "select" ? ["action", "resource"] : ["action"])
        const validator = compile(parameters)
        toolsApi.registerTool({
            name,
            label: name,
            description: `${name.replaceAll("_", " ")} for one initialized entity.${action === "record" || action === "invalidate" ? " The resource kind comes from the evidence's tool and method; a rejection lists the accepted ones." : ""} Never execute or authorize provider calls. Record actual clear-success mutation/read evidence. Missing/stale IDs and uncertain outcomes require agent investigation through configured provider tools. Identity, lifecycle and merge state are not disposable cache values.`,
            parameters,
            async execute(_toolCallId, params, _signal, _onUpdate, ctx) {
                const input = parse(ResourceIdsToolValidator, {
                    ...parse(validator, params),
                    action,
                    ...(action === "select" ? { resource: "github-pull-request" } : {}),
                })
                const entityDir = input.entityDir.replace(/^@/, "")
                const result = await (async () => {
                    if (input.action === "beginCreation") return beginPullRequestCreation(ctx.cwd, entityDir, tools)
                    if (input.action === "recordCreation") {
                        const recorded = await recordPullRequestCreation(ctx.cwd, entityDir, input.evidence, tools)
                        for (const ids of recorded.resourceIds) memo.record(ids)
                        const [pullRequest] = recorded.resourceIds
                        return { ...recorded, issueLink: await linkIssue(ctx, entityDir, pullRequest) }
                    }
                    if (input.action === "resolve")
                        return resolveResourceIds(resolveResourceIdsContext(ctx.cwd, entityDir, input.resource), memo)
                    if (input.action === "select") {
                        const context = resolveResourceIdsContext(ctx.cwd, entityDir, input.resource)
                        const resourceIds = await selectPullRequestResourceIds(context, input.evidence, memo, tools)
                        return {
                            state: "resolved" as const,
                            source: "user-selection" as const,
                            resourceIds,
                            issueLink: await linkIssue(ctx, entityDir, resourceIds),
                        }
                    }
                    const resource = evidenceResourceKind(ctx.cwd, entityDir, input.evidence, input.action, tools)
                    const context = resolveResourceIdsContext(ctx.cwd, entityDir, resource)
                    if (input.action === "invalidate")
                        return invalidateStaleResourceIds(context, input.evidence, memo, tools)
                    const resourceIds = await recordResourceIdsEvidence(context, input.evidence, memo, tools)
                    return {
                        state: "resolved" as const,
                        source: "evidence" as const,
                        resourceIds,
                        // A PR recorded after rediscovery gets the same closing link as a created one.
                        ...(isResourceIdsKind(resourceIds, "github-pull-request")
                            ? { issueLink: await linkIssue(ctx, entityDir, resourceIds) }
                            : {}),
                    }
                })()
                return { content: [{ type: "text" as const, text: JSON.stringify(result, null, 2) }], details: result }
            },
        })
    }
    toolsApi.registerTool({
        name: "find_pull_request",
        label: "find_pull_request",
        description:
            "Find the entity's PR after an uncertain create or missing IDs: PRs linked to its GitHub issue first, then PRs with the saved head and target. A match needs the saved repository, head, and target and the local branch head commit. Returns found (with source), ambiguous, or none, each with rejected candidates. Read-only; record a found PR with record_resource_ids from a pull_request_read get.",
        parameters: FindSchema,
        async execute(_toolCallId, params, _signal, _onUpdate, ctx) {
            const entityDir = parse(FindValidator, params).entityDir.replace(/^@/, "")
            const status = resolveResourceIdsContext(ctx.cwd, entityDir, "github-pull-request").status
            const result = await findPullRequest(ctx.cwd, status, { projectTrusted: ctx.isProjectTrusted() })
            return { content: [{ type: "text" as const, text: JSON.stringify(result, null, 2) }], details: result }
        },
    })
}

const FindSchema = Type.Object({ entityDir: common.entityDir }, { additionalProperties: false })
const FindValidator = compile(FindSchema)
