import { mcpResponsePayload, type McpToolCatalog } from "../../../extensions/__lib/mcp.js"
import type { TrackerConfig } from "./config.js"
import { requireProviderCall } from "./provider-evidence.js"
import { stripPublicationSignature } from "./publication.js"
import type { TrackerRecord } from "./records.js"
import type { ProviderEvidence } from "./resource-id-operations.js"

/** A validated fresh read of a Deliverable's own tracker issue. */
export type TrackerIssueRead =
    | { provider: "github"; response: Record<string, unknown>; label: string }
    | { provider: "linear"; response: Record<string, unknown>; label: string }

/**
 * Validates a fresh read of the entity's own tracker issue: GitHub `issue_read` `get` by its
 * repository and number, or Linear `get_issue` by its stored issue UUID. An Epic's Linear Project
 * is not an issue. `label` names the read in errors.
 */
export function readTrackerIssue(
    record: TrackerRecord,
    tracker: TrackerConfig,
    evidence: ProviderEvidence,
    tools: McpToolCatalog,
    label: string,
): TrackerIssueRead {
    const response = mcpResponsePayload(evidence.response)
    const server = tracker.mcpServer
    switch (record.provider) {
        case "github": {
            const expected = { method: "get", ...record.repository, issue_number: record.external.issueNumber }
            requireProviderCall(evidence, { server, operation: "issue_read", expected }, tools, label)
            if (response.number !== record.external.issueNumber) throw new Error(`${label} read is for another issue`)
            return { provider: "github", response, label }
        }
        case "linear": {
            switch (record.resource) {
                case "project":
                    throw new Error(`${label} needs a Linear issue, not a Project`)
                case "issue":
                    break
            }
            const expected = { id: record.external.issueId }
            requireProviderCall(evidence, { server, operation: "get_issue", expected }, tools, label)
            if (response.uuid !== record.external.issueId) throw new Error(`${label} read is for another issue`)
            return { provider: "linear", response, label }
        }
    }
}

/** The issue's current one-line title. */
export function trackerIssueTitle(read: TrackerIssueRead): string {
    const value = read.response.title
    if (typeof value !== "string" || !value.trim() || /[\r\n]/.test(value))
        throw new Error(`${read.label} read has no one-line title`)
    return value.trim()
}

/** The issue's body (GitHub) or description (Linear), without its publication signature. */
export function trackerIssueBody(read: TrackerIssueRead): string {
    const value = (() => {
        switch (read.provider) {
            case "github":
                return read.response.body
            case "linear":
                return read.response.description
        }
    })()
    return stripPublicationSignature(typeof value === "string" ? value : "").trim()
}
