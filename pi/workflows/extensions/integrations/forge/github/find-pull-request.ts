import type { EntityStatus } from "../../../__lib/entity.js"
import * as project from "../../../../../extensions/__lib/project.js"
import * as Git from "../../../__lib/git.js"
import { loadIntegrationRoles } from "../../config.js"
import { closingIssueNumber, forgeGraphql, type IssueLinkOptions } from "./issue-link.js"

export interface PullRequestCandidate {
    number: number
    url: string
    state: "open" | "closed" | "merged"
}

export interface RejectedCandidate {
    pullRequest: PullRequestCandidate
    mismatches: string[]
}

export type PullRequestSearch =
    | {
          state: "found"
          source: "linked" | "head-base"
          pullRequest: PullRequestCandidate
          rejected: RejectedCandidate[]
      }
    | { state: "ambiguous"; matches: PullRequestCandidate[]; rejected: RejectedCandidate[] }
    | { state: "none"; rejected: RejectedCandidate[] }

interface PullRequestNode {
    number: number
    url: string
    state: "OPEN" | "CLOSED" | "MERGED"
    headRefName: string
    baseRefName: string
    headRefOid: string
    headRepository: { nameWithOwner: string } | null
    baseRepository: { nameWithOwner: string } | null
}

interface Connection {
    nodes: PullRequestNode[]
    pageInfo: { hasNextPage: boolean; endCursor: string | null }
}

const FIELDS = `
    nodes {
        number
        url
        state
        headRefName
        baseRefName
        headRefOid
        headRepository { nameWithOwner }
        baseRepository { nameWithOwner }
    }
    pageInfo { hasNextPage endCursor }
`

/**
 * Finds the entity's PR after an uncertain create or lost IDs. The PRs linked to its GitHub issue come first; PRs with
 * the saved head and target branches are the fallback. A PR matches only with the saved repository as head and base
 * repository, the saved head and target, and the local branch head as its head commit. Read-only.
 */
export async function findPullRequest(
    cwd: string,
    status: EntityStatus,
    options: IssueLinkOptions,
): Promise<PullRequestSearch> {
    const intent = status.forge
    if (!intent) throw new Error("finding the PR requires saved PR intent; prepare the PR with begin_resource_creation")
    const roles = loadIntegrationRoles(cwd)
    if (roles.forge.state !== "on") throw new Error("finding the PR requires the forge")
    const graphql = forgeGraphql(cwd, roles.forge.config.mcpServer, options)
    const repository = `${intent.repository.owner}/${intent.repository.repo}`.toLowerCase()
    const headSha = Git.run(project.resolveRootDir(cwd), [
        "rev-parse",
        "--verify",
        `refs/heads/${intent.head}^{commit}`,
    ]).toLowerCase()
    const issueNumber = closingIssueNumber(roles, status)

    const pages = async (
        query: string,
        variables: Record<string, unknown>,
        select: (data: { repository: Record<string, unknown> }) => Connection,
    ) => {
        const nodes: PullRequestNode[] = []
        let after: string | null = null
        do {
            const data = await graphql<{ repository: Record<string, unknown> }>(query, {
                ...intent.repository,
                ...variables,
                after,
            })
            const connection = select(data)
            nodes.push(...connection.nodes)
            after = connection.pageInfo.hasNextPage ? connection.pageInfo.endCursor : null
        } while (after)
        return nodes
    }
    const rejected = new Map<number, RejectedCandidate>()
    const matches = (nodes: PullRequestNode[]) =>
        nodes.flatMap((node) => {
            const pullRequest = {
                number: node.number,
                url: node.url,
                state: node.state === "OPEN" ? "open" : node.state === "MERGED" ? "merged" : "closed",
            } as const
            const mismatches = [
                node.headRepository?.nameWithOwner.toLowerCase() !== repository && "head repository",
                node.baseRepository?.nameWithOwner.toLowerCase() !== repository && "base repository",
                node.headRefName !== intent.head && "head",
                node.baseRefName !== intent.target && "base",
                node.headRefOid.toLowerCase() !== headSha && "head commit",
            ].filter((mismatch): mismatch is string => typeof mismatch === "string")
            if (mismatches.length) rejected.set(node.number, { pullRequest, mismatches })
            return mismatches.length ? [] : [pullRequest]
        })

    let source: "linked" | "head-base" = "linked"
    let found =
        issueNumber === undefined
            ? []
            : matches(
                  await pages(
                      `query ($owner: String!, $repo: String!, $issue: Int!, $after: String) {
                          repository(owner: $owner, name: $repo) {
                              issue(number: $issue) {
                                  closedByPullRequestsReferences(first: 50, after: $after, includeClosedPrs: true) { ${FIELDS} }
                              }
                          }
                      }`,
                      { issue: issueNumber },
                      (data) =>
                          (data.repository.issue as { closedByPullRequestsReferences: Connection })
                              .closedByPullRequestsReferences,
                  ),
              )
    if (!found.length) {
        source = "head-base"
        found = matches(
            await pages(
                `query ($owner: String!, $repo: String!, $head: String!, $base: String!, $after: String) {
                    repository(owner: $owner, name: $repo) {
                        pullRequests(first: 50, after: $after, headRefName: $head, baseRefName: $base) { ${FIELDS} }
                    }
                }`,
                { head: intent.head, base: intent.target },
                (data) => data.repository.pullRequests as Connection,
            ),
        )
    }
    const others = [...rejected.values()].filter(
        (entry) => !found.some((match) => match.number === entry.pullRequest.number),
    )
    if (found.length === 1) return { state: "found", source, pullRequest: found[0], rejected: others }
    if (found.length > 1) return { state: "ambiguous", matches: found, rejected: others }
    return { state: "none", rejected: others }
}
