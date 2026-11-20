import * as path from "node:path"

import { withFileMutationQueue } from "@earendil-works/pi-coding-agent"

import {
    getMcpToolName,
    mcpResponsePayload,
    requireMcpTool,
    type McpToolCatalog,
} from "../../../extensions/__lib/mcp.js"
import * as project from "../../../extensions/__lib/project.js"
import {
    type EntityStatus,
    ENTITY_METADATA_FILE,
    readEntityStatus,
    resolveEntityDirectory,
    writeEntityStatus,
} from "../__lib/entity.js"
import { assertArtifactPersistencePrepared } from "../__lib/project-config.js"
import { readReviewCycle, unfinishedCycleReason } from "../__lib/review-rounds.js"
import { type IntegrationRoles, loadIntegrationRoles, requireForge } from "./config.js"
import { readTrackerIssue, trackerIssueTitle } from "./issue-read.js"
import { assertEntityTracker } from "./policy.js"
import { gitHubResourceNumber, parseResourceIds, recordResourceIds } from "./resource-ids.js"
import { resolveResourceIdsContext, type ProviderEvidence } from "./resource-id-operations.js"

const CREATE_OPERATION = "create_pull_request"

// The create call's required arguments; the returned call and the evidence check share them.
function pullRequestCreateArguments(
    repository: { owner: string; repo: string },
    head: string,
    target: string,
    title: string,
) {
    return { owner: repository.owner, repo: repository.repo, head, base: target, title }
}

/** Whether PR creation changed the stored title to the tracker issue's current one. */
export type PullRequestTitleSync = { state: "unchanged" } | { state: "updated"; from: string; to: string }

// The PR title: the tracker issue's current title from a fresh read, since the issue may have been
// retitled after initialization. Without an issue to read (a local Gig, or the tracker role off),
// the stored title stands and evidence is refused.
function currentTitle(
    status: EntityStatus,
    roles: IntegrationRoles,
    evidence: ProviderEvidence | undefined,
    tools: McpToolCatalog,
): string {
    const stored = () => {
        if (evidence) throw new Error("PR creation takes no tracker issue read: this entity has no issue to read")
        return status.title
    }
    const record = status.tracker
    switch (record.provider) {
        case "local":
            return stored()
        case "github":
        case "linear":
            break
    }
    switch (roles.tracker.state) {
        case "off":
            return stored()
        case "on":
            break
    }
    const tracker = roles.tracker.config
    assertEntityTracker(status, tracker)
    if (!evidence) throw new Error("PR creation needs a fresh read of the tracker issue, for its current title")
    return trackerIssueTitle(readTrackerIssue(record, tracker, evidence, tools, "tracker issue"))
}

// PR creation: save the immutable repository, head, and target intent and return the exact
// create call, then record the created PR's number and URL. Tracker objects are created
// before the entity exists. A new PR needs the newest final review cycle finished; an existing
// PR needs no local review state, so a fresh clone can still update and merge it. The title
// follows the tracker issue and is saved with the intent, so recording and shipping use it too.
export async function beginPullRequestCreation(
    cwd: string,
    entityDir: string,
    evidence: ProviderEvidence | undefined,
    tools: McpToolCatalog,
) {
    const root = project.resolveRootDir(cwd)
    assertArtifactPersistencePrepared(root)
    const roles = loadIntegrationRoles(root)
    const forge = requireForge(roles, "PR creation")
    const directory = resolveEntityDirectory(root, cwd, entityDir)
    return withFileMutationQueue(path.join(directory, ENTITY_METADATA_FILE), async () => {
        const status = readEntityStatus(directory)
        if (status.entity === "epic") throw new Error("Epics never create pull requests")
        if (status.forge) throw new Error("PR intent already exists; reconcile it instead of creating a replacement")
        const cycle = readReviewCycle(directory, "final")
        switch (cycle.state) {
            case "none":
            case "awaitingVerification":
                throw new Error(`a new PR requires a finished final review: ${unfinishedCycleReason(cycle, "final")}`)
            case "finished":
                break
        }
        const tool = getMcpToolName(forge.mcpServer, CREATE_OPERATION, tools)
        const title = currentTitle(status, roles, evidence, tools)
        const titleSync: PullRequestTitleSync =
            title === status.title ? { state: "unchanged" } : { state: "updated", from: status.title, to: title }
        writeEntityStatus(directory, {
            ...status,
            title,
            forge: {
                provider: "github",
                repository: { ...forge.repository },
                head: status.branch.name,
                target: status.branch.target,
            },
        })
        return {
            entity: path.basename(directory),
            state: "intent" as const,
            mutation: {
                tool,
                arguments: pullRequestCreateArguments(
                    forge.repository,
                    status.branch.name,
                    status.branch.target,
                    title,
                ),
            },
            title: titleSync,
            authorization:
                "checkpoint only; add the rendered body as arguments.body and run mutation unchanged under the existing submission approval and permissions",
        }
    })
}

export async function recordPullRequestCreation(
    cwd: string,
    entityDir: string,
    evidence: ProviderEvidence,
    tools: McpToolCatalog,
) {
    const { status, scope, directory } = resolveResourceIdsContext(cwd, entityDir, "github-pull-request")
    if (scope.kind !== "github-pull-request") throw new Error("PR recording requires prepared forge intent")
    const response = mcpResponsePayload(evidence.response)
    requireMcpTool(evidence.tool, scope.mcpServer, CREATE_OPERATION, tools, "PR creation evidence")
    const expected = pullRequestCreateArguments(scope.repository, scope.head, scope.target, status.title)
    for (const [key, value] of Object.entries(expected))
        if (JSON.stringify(evidence.arguments[key]) !== JSON.stringify(value))
            throw new Error(`creation evidence ${key} does not match the intended request`)
    if (typeof response.url !== "string" || !response.url)
        throw new Error("creation response omits required identity; preserve the intent and reconcile")
    const url = response.url
    const ids = parseResourceIds({ scope, value: { number: gitHubResourceNumber(url, scope.repository, "pull"), url } })
    await recordResourceIds(directory, [ids])
    return { entity: path.basename(directory), resourceIds: [ids] }
}
