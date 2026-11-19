import type { EntityStatus } from "../../../__lib/entity.js"
import { loadIntegrationRoles } from "../../config.js"
import { registeredCredentialEnvironment } from "../../mcp.js"
import { sameRepository } from "../../policy.js"

/** The closing link between a recorded PR and its tracker issue, after a best-effort link. */
export type IssueLink =
    | { state: "linked"; issueNumber: number }
    | { state: "unconfirmed"; issueNumber: number }
    | { state: "not-applicable" }
    | { state: "failed"; reason: string }

export interface IssueLinkOptions {
    projectTrusted: boolean
    fetch?: typeof fetch
    env?: NodeJS.ProcessEnv
}

interface LinkRead {
    repository: {
        issue: { id: string }
        pullRequest: {
            id: string
            closingIssuesReferences: { nodes: Array<{ number: number; repository: { nameWithOwner: string } }> }
        }
    }
}

/**
 * A safety net under the PR body's `Closes #N`, which GitHub occasionally doesn't turn into a closing link. Links the
 * PR to the entity's same-repository GitHub issue with `addCloseIssueReferences` (the official MCP server has no tool
 * for it), then confirms the link from the PR's closing references. Idempotent, and never throws: recording the PR
 * must not depend on it.
 */
export async function linkPullRequestIssue(
    cwd: string,
    status: EntityStatus,
    pullRequestNumber: number,
    options: IssueLinkOptions,
): Promise<IssueLink> {
    try {
        const roles = loadIntegrationRoles(cwd)
        const tracker = status.tracker
        if (
            roles.forge.state !== "on" ||
            roles.tracker.state !== "on" ||
            roles.tracker.config.provider !== "github" ||
            tracker.provider !== "github" ||
            !sameRepository(tracker.repository, roles.forge.config.repository)
        )
            return { state: "not-applicable" }
        const { repository, mcpServer } = roles.forge.config
        const issueNumber = tracker.external.issueNumber
        const credential = registeredCredentialEnvironment(cwd, mcpServer, "github", options.projectTrusted)
        const token = (options.env ?? process.env)[credential]
        if (!token) return { state: "failed", reason: `environment variable ${credential} is not set` }

        // Errors carry only GitHub's messages or a fetch error name, never the request or its credential.
        const graphql = async <T>(query: string, variables: Record<string, unknown>): Promise<T> => {
            let response: Response
            try {
                response = await (options.fetch ?? fetch)("https://api.github.com/graphql", {
                    method: "POST",
                    headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json" },
                    body: JSON.stringify({ query, variables }),
                })
            } catch (error) {
                throw new Error(`network request failed (${error instanceof Error ? error.name : "unknown error"})`)
            }
            const body = (await response.json().catch(() => ({}))) as { data?: T; errors?: Array<{ message?: string }> }
            if (!response.ok || body.errors?.length || !body.data)
                throw new Error(body.errors?.map((error) => error.message).join("; ") || `HTTP ${response.status}`)
            return body.data
        }
        // A workflow PR closes one issue; 50 closing references is ample.
        const read = async () => {
            const { repository: found } = await graphql<LinkRead>(
                `
                    query ($owner: String!, $repo: String!, $issue: Int!, $pr: Int!) {
                        repository(owner: $owner, name: $repo) {
                            issue(number: $issue) {
                                id
                            }
                            pullRequest(number: $pr) {
                                id
                                closingIssuesReferences(first: 50) {
                                    nodes {
                                        number
                                        repository {
                                            nameWithOwner
                                        }
                                    }
                                }
                            }
                        }
                    }
                `,
                { ...repository, issue: issueNumber, pr: pullRequestNumber },
            )
            const name = `${repository.owner}/${repository.repo}`.toLowerCase()
            return {
                issueId: found.issue.id,
                pullRequestId: found.pullRequest.id,
                linked: found.pullRequest.closingIssuesReferences.nodes.some(
                    (node) => node.number === issueNumber && node.repository.nameWithOwner.toLowerCase() === name,
                ),
            }
        }

        const before = await read()
        if (before.linked) return { state: "linked", issueNumber }
        // A failed request may still have applied; the read-back decides.
        await graphql(
            `
                mutation ($issue: ID!, $pr: ID!) {
                    addCloseIssueReferences(input: { issueId: $issue, pullRequestIds: [$pr] }) {
                        clientMutationId
                    }
                }
            `,
            { issue: before.issueId, pr: before.pullRequestId },
        ).catch(() => undefined)
        return (await read()).linked ? { state: "linked", issueNumber } : { state: "unconfirmed", issueNumber }
    } catch (error) {
        return { state: "failed", reason: error instanceof Error ? error.message : String(error) }
    }
}
