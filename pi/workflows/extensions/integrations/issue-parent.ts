import { type Static, Type } from "typebox"

import { mcpResponsePayload, type McpToolCatalog } from "../../../extensions/__lib/mcp.js"
import type { TrackerConfig } from "./config.js"
import { requireProviderCall } from "./provider-evidence.js"
import type { TaskTrackerParent, TrackerObject } from "./records.js"
import type { ProviderEvidence } from "./resource-id-operations.js"
import { EvidenceSchema } from "./resource-id-tool.js"

// A Task's parent is a tracker fact. This module turns a Task issue's tracker reads into that fact, the
// tracker object, and the facts routing and restore need; every caller (route_task, init, resume, restore)
// uses it, so no caller can pass or infer a parent of its own.

/** The reads that decide a Task issue's parent. GitHub needs `hierarchy`, and `parent` when it has one. */
export const IssueReadsSchema = Type.Object(
    {
        issue: Type.Union([EvidenceSchema], {
            description: "The issue's read: GitHub issue_read get, or Linear get_issue",
        }),
        hierarchy: Type.Optional(
            Type.Union([EvidenceSchema], {
                description: "GitHub: issue_read get_parent for the same issue. Required on GitHub.",
            }),
        ),
        parent: Type.Optional(
            Type.Union([EvidenceSchema], {
                description: "GitHub: issue_read get for the parent issue, when it has one and it isn't a local Epic",
            }),
        ),
    },
    { additionalProperties: false },
)
export type IssueReads = Static<typeof IssueReadsSchema>

/** A provider read the caller still has to run. */
export interface ReadRequest {
    server: string
    operation: string
    arguments: Record<string, unknown>
}

/** What a Task issue's reads say about it. */
export interface IssueFacts {
    object: Extract<TrackerObject, { provider: "github" } | { provider: "linear"; resource: "issue" }>
    /** `#12` or `ENG-12`: the Epic plan marker and directory prefix. */
    label: string
    title: string
    body: string
    labels: string[]
}

export type IssueParentResult =
    | { state: "resolved"; facts: IssueFacts; parent: TaskTrackerParent }
    | { state: "readRequired"; facts: IssueFacts; read: ReadRequest; as: "hierarchy" | "parent" }
    /** A sub-issue of an issue that may or may not be an Epic, with no Epic Kind label configured to tell. */
    | { state: "unclassified"; facts: IssueFacts; parentNumber: number }

/**
 * The Task issue's parent from its reads. `isLocalEpic` says whether a GitHub parent issue is a local Epic;
 * a local Epic is one without a label check, since its metadata already records it as an Epic.
 */
export function issueParent(
    tracker: TrackerConfig,
    reads: IssueReads,
    tools: McpToolCatalog,
    isLocalEpic: (parent: { provider: "github"; issueNumber: number }) => boolean,
): IssueParentResult {
    switch (tracker.provider) {
        case "github":
            return githubParent(tracker, reads, tools, isLocalEpic)
        case "linear":
            if (reads.hierarchy || reads.parent)
                throw new Error("hierarchy and parent reads are for GitHub; a Linear issue's projectId decides")
            return linearParent(tracker, reads.issue, tools)
    }
}

type GitHubTracker = Extract<TrackerConfig, { provider: "github" }>
type LinearTracker = Extract<TrackerConfig, { provider: "linear" }>

function githubParent(
    tracker: GitHubTracker,
    reads: IssueReads,
    tools: McpToolCatalog,
    isLocalEpic: (parent: { provider: "github"; issueNumber: number }) => boolean,
): IssueParentResult {
    const repository = tracker.repository
    const number = reads.issue.arguments.issue_number
    if (typeof number !== "number") throw new Error("issue evidence must name its issue_number")
    const read = (evidence: ProviderEvidence, method: string, issueNumber: number, label: string) => {
        requireProviderCall(
            evidence,
            {
                server: tracker.mcpServer,
                operation: "issue_read",
                expected: { method, ...repository, issue_number: issueNumber },
            },
            tools,
            label,
        )
        return mcpResponsePayload(evidence.response)
    }
    const request = (method: string, issueNumber: number): ReadRequest => ({
        server: tracker.mcpServer,
        operation: "issue_read",
        arguments: { method, ...repository, issue_number: issueNumber },
    })
    const issue = read(reads.issue, "get", number, "issue")
    if (issue.number !== number) throw new Error("issue read is for another issue")
    const issueId = typeof issue.id === "string" ? Number(issue.id) : issue.id
    if (typeof issueId !== "number" || !Number.isSafeInteger(issueId) || issueId < 1)
        throw new Error("issue read lacks its numeric issue ID")
    const url = issue.html_url ?? issue.url
    if (typeof url !== "string" || !url) throw new Error("issue read lacks its URL")
    const facts: IssueFacts = {
        object: { provider: "github", issueId, issueNumber: number, issueUrl: url },
        label: `#${number}`,
        title: text(issue.title, "title"),
        body: typeof issue.body === "string" ? issue.body : "",
        labels: labelNames(issue.labels),
    }

    if (!reads.hierarchy) return { state: "readRequired", facts, read: request("get_parent", number), as: "hierarchy" }
    const hierarchy = read(reads.hierarchy, "get_parent", number, "hierarchy")
    const parent = hierarchy.parent
    if (parent === null) return { state: "resolved", facts, parent: { kind: "none" } }
    const parentNumber = typeof parent === "object" && parent ? (parent as { number?: unknown }).number : undefined
    if (typeof parentNumber !== "number")
        throw new Error("hierarchy read has no parent: expected null or the parent issue")
    const epic = { kind: "epic" as const, epic: { provider: "github" as const, issueNumber: parentNumber } }
    if (isLocalEpic(epic.epic)) return { state: "resolved", facts, parent: epic }

    if (!reads.parent) return { state: "readRequired", facts, read: request("get", parentNumber), as: "parent" }
    const parentIssue = read(reads.parent, "get", parentNumber, "parent issue")
    const epicLabel = tracker.labels?.groups?.kind?.values.epic?.name
    if (!epicLabel) return { state: "unclassified", facts, parentNumber }
    // A sub-issue of an ordinary issue is standalone: only an Epic's sub-issues are its Tasks.
    return {
        state: "resolved",
        facts,
        parent: labelNames(parentIssue.labels).includes(epicLabel) ? epic : { kind: "none" },
    }
}

function linearParent(tracker: LinearTracker, evidence: ProviderEvidence, tools: McpToolCatalog): IssueParentResult {
    requireProviderCall(evidence, { server: tracker.mcpServer, operation: "get_issue", expected: {} }, tools, "issue")
    const issue = mcpResponsePayload(evidence.response)
    const identifier = text(issue.id, "identifier")
    const uuid = text(issue.uuid, "UUID")
    const requested = evidence.arguments.id
    if (requested !== identifier && requested !== uuid)
        throw new Error("Linear issue evidence must be get_issue for this issue's identifier or UUID")
    const projectId = issue.projectId
    if (projectId !== null && projectId !== undefined && (typeof projectId !== "string" || !projectId))
        throw new Error("Linear issue read has an invalid projectId")
    const gitBranchName =
        typeof issue.gitBranchName === "string" && issue.gitBranchName ? issue.gitBranchName : undefined
    const facts: IssueFacts = {
        object: {
            provider: "linear",
            resource: "issue",
            issueId: uuid,
            identifier,
            issueUrl: text(issue.url, "URL"),
            ...(gitBranchName ? { gitBranchName } : {}),
            ...(typeof projectId === "string" ? { projectId } : {}),
        },
        label: identifier,
        title: text(issue.title, "title"),
        body: typeof issue.description === "string" ? issue.description : "",
        labels: labelNames(issue.labels),
    }
    // Every Linear Project is an Epic; an issue in none is standalone.
    return {
        state: "resolved",
        facts,
        parent:
            typeof projectId === "string"
                ? { kind: "epic", epic: { provider: "linear", projectId } }
                : { kind: "none" },
    }
}

function text(value: unknown, name: string): string {
    if (typeof value !== "string" || !value.trim()) throw new Error(`issue read lacks its ${name}`)
    return value
}

function labelNames(value: unknown): string[] {
    const nodes = Array.isArray(value)
        ? value
        : value && typeof value === "object" && Array.isArray((value as { nodes?: unknown }).nodes)
          ? (value as { nodes: unknown[] }).nodes
          : []
    return nodes.flatMap((label) =>
        typeof label === "string"
            ? [label]
            : label && typeof label === "object" && typeof (label as { name?: unknown }).name === "string"
              ? [(label as { name: string }).name]
              : [],
    )
}
