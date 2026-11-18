import * as fs from "node:fs"
import * as path from "node:path"
import { INCOMING_CHECKOUT_TOOLS } from "../../../pr/operations.js"
import { isDeepStrictEqual } from "node:util"

import {
    mcpResponsePayload,
    requireMcpTool,
    resolveMcpTool,
    type McpToolCatalog,
} from "../../../../../extensions/__lib/mcp.js"
import * as project from "../../../../../extensions/__lib/project.js"
import { PROJECT_LOCAL_ROOT } from "../../../__lib/domain.js"
import { assertArtifactPersistencePrepared, assertWorkerArtifactsLinked } from "../../../__lib/project-config.js"
import {
    loadIntegrationRoles,
    requireForge,
    type Role,
    roleOffReason,
    type GitHubForge,
    type TrackerConfig,
} from "../../config.js"
import type { ProviderEvidence } from "../../resource-id-operations.js"

export interface Repository {
    owner: string
    repo: string
}

export interface IncomingPullRequest {
    repository: Repository
    number: number
    url: string
}

export interface IncomingRevision extends IncomingPullRequest {
    kind: "incomingPullRequest"
    mcpServer: string
    base: { repository: Repository; ref: string; sha: string }
    head: { repository: Repository | null; ref: string; sha: string }
}

export function parseIncomingReference(reference: string, defaultRepository: Repository): IncomingPullRequest {
    const value = reference.trim()
    const short = /^#([1-9][0-9]*)$/.exec(value)
    if (short) return pullRequest(defaultRepository, short[1])
    const url = /^https:\/\/github\.com\/([^/?#]+)\/([^/?#]+)\/pull\/([1-9][0-9]*)\/?(?:[?#].*)?$/i.exec(value)
    if (!url) throw new Error("Use #NN or an HTTPS github.com/<owner>/<repo>/pull/<number> URL")
    const selected = pullRequest({ owner: url[1], repo: url[2] }, url[3])
    const repository = normalizeRepository(defaultRepository)
    if (!isDeepStrictEqual(selected.repository, repository)) {
        throw new Error(`Incoming PR must target the configured repository ${repository.owner}/${repository.repo}`)
    }
    return selected
}

function pullRequest(repository: Repository, rawNumber: string): IncomingPullRequest {
    const normalized = normalizeRepository(repository)
    const number = Number(rawNumber)
    if (!Number.isSafeInteger(number) || number < 1) throw new Error("PR number must be a positive safe integer")
    return {
        repository: normalized,
        number,
        url: `https://github.com/${normalized.owner}/${normalized.repo}/pull/${number}`,
    }
}

function normalizeRepository(repository: Repository): Repository {
    if (!/^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$/i.test(repository.owner)) throw new Error("Invalid GitHub repository owner")
    if (!/^[a-z0-9_.-]+$/i.test(repository.repo) || [".", ".."].includes(repository.repo)) {
        throw new Error("Invalid GitHub repository name")
    }
    return { owner: repository.owner.toLowerCase(), repo: repository.repo.toLowerCase() }
}

export function incomingPullRequestContext(cwd: string, reference: string, tools: McpToolCatalog) {
    const root = project.resolveRootDir(cwd)
    assertArtifactPersistencePrepared(root)
    assertWorkerArtifactsLinked(root)
    const roles = loadIntegrationRoles(root)
    const forge = requireForge(roles, "incoming PR handling")
    const selected = parseIncomingReference(reference, forge.repository)
    const board = incomingBoard(roles.tracker, selected, tools)
    return {
        state: "enabled" as const,
        operation: "incomingPullRequest" as const,
        project: board,
        roles: { forge: { state: "enabled" as const, provider: "github" as const, config: forge } },
        incomingPullRequest: {
            ...selected,
            localDirectory: incomingLocalDirectory(selected),
            reviewsDirectory: incomingReviewsDirectory(selected),
            read: {
                ...resolveMcpTool(forge.mcpServer, "pull_request_read", tools),
                arguments: { method: "get", ...selected.repository, pullNumber: selected.number },
            },
        },
        safety: incomingSafety(),
    }
}

/** The incoming PR's Project board: a GitHub tracker's Project, or disabled with the reason. */
function incomingBoard(tracker: Role<TrackerConfig>, selected: IncomingPullRequest, tools: McpToolCatalog) {
    switch (tracker.state) {
        case "off":
            return {
                state: "disabled" as const,
                reason: `The tracker ${roleOffReason("tracker", tracker)}; forge-only inspection remains available.`,
            }
        case "on":
            break
    }
    const configured = tracker.config
    switch (configured.provider) {
        case "linear":
            return {
                state: "disabled" as const,
                reason: "The tracker isn't a GitHub Project; forge-only inspection remains available.",
            }
        case "github":
            return {
                state: "enabled" as const,
                mcpServer: configured.mcpServer,
                project: configured.project,
                status: configured.fields.status,
                lookup: {
                    ...resolveMcpTool(configured.mcpServer, "projects_list", tools),
                    arguments: {
                        method: "list_project_items",
                        owner: configured.project.owner,
                        owner_type: configured.project.ownerType,
                        project_number: configured.project.number,
                        query: `repo:${selected.repository.owner}/${selected.repository.repo} is:pr`,
                        field_names: [configured.fields.status.field],
                    },
                },
            }
    }
}

function incomingSafety() {
    return {
        approval:
            "Conversational approval of exact content, verdict, operation and revision; existing tool permissions still apply.",
        checkout: {
            tools: INCOMING_CHECKOUT_TOOLS,
            orchestration:
                "Discover and use the named registered checkout operations, which work in the current checkout (normally the PR's worker): preparation and fetch, contributor-review setup, validated transitions, held-configuration release and hold, restore, and approved standard rebase with immediate SHA-pinned --force-with-lease PR update. Configuration that tools and agents load (the piton tools registry) stays at the checkout's starting version on disk unless its release is approved. GitHub fetch/remote-tip observation/push use SSH with existing authentication; configure no HTTPS credentials and change no global Git configuration. Saved unpublished rebase results recover through approved publish_incoming_pr_rebase with their original lease, without another rebase. Approved commit_incoming_pr_fixes and separate publish_incoming_pr_fixes preserve additive history and advance the managed baseline; fixes publication is non-forced. An exact request to ship and merge includes shared repository merge, confirmed Project Done and restoring the checkout without another cleanup question. Keep private evidence and contributor branches. Never reconstruct this Git orchestration in Bash or fall back after refusal/unavailability.",
            scale: [
                { score: 1, label: "malicious", meaning: "Clear malicious behavior found." },
                { score: 2, label: "suspicious", meaning: "Strong suspicious indicators." },
                { score: 3, label: "inconclusive", meaning: "Inconclusive or insufficient inspection." },
                { score: 4, label: "concerns", meaning: "No clear malicious behavior, but unresolved concerns." },
                {
                    score: 5,
                    label: "safe",
                    meaning: "No malicious indicators found in the inspected contribution.",
                },
            ],
            assessment:
                "Before materializing source, present Checkout safety: N/5 (label), using the scale's label; for example, Checkout safety: 5/5 (safe). Include exact-revision evidence and coverage limitations. A 5 is not a guarantee.",
            approval:
                "Every score requires explicit checkout approval for the exact revision/path. No automatic threshold or durable approval token.",
            phases: "Contributor-only review first (HEAD at the merge base, the PR staged); then the clean contributor head for approved fixes. Approved standard rebase and conflict resolution run in a private scratch worktree and immediately publish using the pinned lease, before later approved uncommitted fixes and maintainer testing. Restore returns the checkout to its starting branch and revision.",
            rebase: "Observe actual target/contributor tips separately from unchanged provider evidence. Approval covers exact standard rebase plus immediate PR update, with consent/access established first. Never use plain --force, refresh a rejected lease automatically, or execute contributor code. Later fix commits/push, testing, publication and GitHub merge retain separate approvals.",
        },
        project:
            "Use incomingPullRequestProject for scoped native Project plans. Existing membership is preserved; explicit missing intake chooses Backlog/Todo, review start sets In Review and keeps it through contributor/fix activity until confirmed merge/Done or unmerged close/Canceled. Do not infer activity or set In Progress/Phase.",
        execution:
            "Approved checkout and static fixes are supported. Git hooks, filters and helpers stay disabled, and tool and agent configuration stays at its trusted version on disk. Editors and language servers can still run project code when files open; call such changes out in the safety assessment. Executing contributor code needs separate command and appropriately isolated environment approval. Report unavailable runtime checks as not run; a checkout is not a sandbox.",
        identity:
            "Review/merge use the base repository; contributor-branch writes require the actual head repository/ref, consent and effective access.",
        publication:
            "Call configured registered MCP tools directly; never bypass denial/failure with HTTP, gh or another client. Existing code-owned attribution applies.",
    }
}

export function incomingLocalDirectory(selected: IncomingPullRequest): string {
    const checked = pullRequest(selected.repository, String(selected.number))
    return path.join(PROJECT_LOCAL_ROOT, "pull-requests", String(checked.number))
}

export function incomingRevision(
    forge: GitHubForge,
    selected: IncomingPullRequest,
    evidence: ProviderEvidence,
    tools: McpToolCatalog,
): IncomingRevision {
    const identity = pullRequest(selected.repository, String(selected.number))
    const args = evidence.arguments
    requireMcpTool(evidence.tool, forge.mcpServer, "pull_request_read", tools, "Incoming PR evidence")
    if (
        args.method !== "get" ||
        args.pullNumber !== selected.number ||
        typeof args.owner !== "string" ||
        typeof args.repo !== "string" ||
        !isDeepStrictEqual(normalizeRepository({ owner: args.owner, repo: args.repo }), selected.repository)
    )
        throw new Error("Incoming PR evidence must be the exact configured pull_request_read:get call")
    const response = mcpResponsePayload(evidence.response)
    const observed = parseIncomingReference(text(response.html_url, "PR URL"), forge.repository)
    if (!isDeepStrictEqual(observed, identity) || response.number !== identity.number)
        throw new Error("PR evidence does not match the selected repository/number")
    const base = object(response.base, "PR base")
    const head = object(response.head, "PR head")
    const baseRepository = repositoryFromResponse(base.repo)
    if (!isDeepStrictEqual(baseRepository, selected.repository))
        throw new Error("PR base repository does not match the selected repository")
    return {
        ...identity,
        kind: "incomingPullRequest",
        mcpServer: forge.mcpServer,
        base: { repository: baseRepository, ref: text(base.ref, "base ref"), sha: sha(base.sha) },
        head: {
            repository: head.repo == null ? null : repositoryFromResponse(head.repo),
            ref: text(head.ref, "head ref"),
            sha: sha(head.sha),
        },
    }
}

function repositoryFromResponse(value: unknown): Repository {
    const repo = object(value, "PR repository")
    const parts = text(repo.full_name, "repository full_name").split("/")
    if (parts.length !== 2) throw new Error("PR repository full_name must be owner/repository")
    const normalized = normalizeRepository({ owner: parts[0], repo: parts[1] })
    if (repo.name !== undefined && text(repo.name, "repository name").toLowerCase() !== normalized.repo) {
        throw new Error("PR repository full_name does not match name")
    }
    if (repo.owner !== undefined) {
        const owner = object(repo.owner, "repository owner")
        if (text(owner.login, "owner login").toLowerCase() !== normalized.owner) {
            throw new Error("PR repository full_name does not match owner")
        }
    }
    return normalized
}

export function sameIncomingRevision(left: IncomingRevision, right: IncomingRevision): boolean {
    return isDeepStrictEqual(left, right)
}

// Checkout association survives its own approved head rewrite; reviewer identity remains exact.
export function sameIncomingAddress(left: IncomingRevision, right: IncomingRevision): boolean {
    return (
        left.number === right.number &&
        left.mcpServer === right.mcpServer &&
        left.repository.owner === right.repository.owner &&
        left.repository.repo === right.repository.repo &&
        left.base.repository.owner === right.base.repository.owner &&
        left.base.repository.repo === right.base.repository.repo &&
        left.base.ref === right.base.ref &&
        left.head.repository?.owner === right.head.repository?.owner &&
        left.head.repository?.repo === right.head.repository?.repo &&
        left.head.ref === right.head.ref
    )
}

function incomingReviewsDirectory(selected: IncomingPullRequest): string {
    return path.join(incomingLocalDirectory(selected), "reviews")
}

export function incomingRevisionDirectory(revision: IncomingRevision): string {
    return path.join(incomingReviewsDirectory(revision), `${sha(revision.base.sha)}-${sha(revision.head.sha)}`)
}

// Evidence and responses must stay in this PR's ignored local area, including through symlinks.
// The one allowed link is a worker's `.piton/.local` pointing at its source checkout's directory.
export function resolveIncomingLocalPath(root: string, selected: IncomingPullRequest, raw: string): string {
    const local = path.resolve(root, incomingLocalDirectory(selected))
    const resolved = path.resolve(root, raw.replace(/^@/, ""))
    if (resolved !== local && !resolved.startsWith(local + path.sep))
        throw new Error("PR evidence path must be inside the selected PR's private directory")
    const linkedLocalRoot = path.join(root, PROJECT_LOCAL_ROOT)
    let current = root
    for (const part of path.relative(root, resolved).split(path.sep)) {
        current = path.join(current, part)
        let link: boolean
        try {
            link = fs.lstatSync(current).isSymbolicLink()
        } catch (error) {
            if ((error as NodeJS.ErrnoException).code === "ENOENT") break
            throw error
        }
        if (!link) continue
        if (current === linkedLocalRoot && isDirectory(current)) continue
        throw new Error("PR evidence paths must not traverse symlinks")
    }
    return resolved
}

function isDirectory(file: string): boolean {
    try {
        return fs.statSync(file).isDirectory()
    } catch {
        return false
    }
}

function object(value: unknown, label: string): Record<string, unknown> {
    if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error(`Missing or invalid ${label}`)
    return value as Record<string, unknown>
}
function text(value: unknown, label: string): string {
    if (typeof value !== "string" || !value.trim() || /[\r\n\0]/.test(value))
        throw new Error(`Missing or invalid ${label}`)
    return value
}
function sha(value: unknown): string {
    if (typeof value !== "string" || !/^[a-f0-9]{40,64}$/i.test(value))
        throw new Error("Missing or invalid full PR commit SHA")
    return value.toLowerCase()
}
