import * as fs from "node:fs"
import * as path from "node:path"

import { mcpResponsePayload } from "../../../extensions/__lib/mcp.js"
import type { ProviderEvidence } from "../integrations/resource-id-operations.js"
import { githubSshUrl, headBranchRef } from "./git.js"
import { remoteHead } from "./rebase.js"
import { historicalRebase, stateFacts, type CheckoutState } from "./state.js"
import {
    checkoutSourcePath,
    diskCommitFor,
    matchesFixCommit,
    observe,
    requireExact,
    type Checkout,
    type Observed,
} from "./workspace.js"

type Save = (state: CheckoutState) => Promise<void>
type Editing = Extract<CheckoutState, { phase: "fixes" }>
type Committing = Extract<CheckoutState, { phase: "committingFixes" }>
type Committed = Extract<CheckoutState, { phase: "committedFixes" }>
function open(evidence: ProviderEvidence) {
    const response = mcpResponsePayload(evidence.response)
    if (response.state !== "open" || response.merged !== false)
        throw new Error("Confirm an open unmerged PR before writing its branch")
}
function messageText(message: string): string {
    if (!message.trim() || message.includes("\0")) throw new Error("Approve a nonempty literal commit message")
    return message.endsWith("\n") ? message : message + "\n"
}

/**
 * The maintainer's changes in a fixes draft: tracked edits and new files relative to the files on disk the tool left.
 * HEAD and the index stay at the baseline, so held configuration differs only through the tool's own disk state.
 */
function draft(observed: Observed): string[] {
    return [...new Set([...observed.changed, ...observed.untracked])].sort()
}

/** A fixes draft is an editable disk state: HEAD and the index unchanged, no conflict or Git operation in progress. */
function requireDraftable(observed: Observed, state: Editing | Committing): void {
    if (
        observed.head !== state.workspace.headSha ||
        !observed.detached ||
        observed.indexTree !== state.workspace.indexTree ||
        observed.unmerged.length ||
        observed.native.kind !== "none"
    )
        throw new Error(
            "Unexplained HEAD, index, or Git operation changes; preserve them and investigate before committing",
        )
}

export async function commitFixes(
    c: Checkout,
    state: Editing | Committing,
    paths: string[],
    message: string,
    evidence: ProviderEvidence,
    save: Save,
): Promise<CheckoutState> {
    open(evidence)
    const approved = paths.map((file) => checkoutSourcePath(c.root, file)).sort()
    if (new Set(approved).size !== approved.length) throw new Error("Approve unique exact source paths")
    const text = messageText(message)
    let pending: Committing
    if (state.phase === "committingFixes") {
        if (state.fixes.message !== text || JSON.stringify(state.fixes.paths) !== JSON.stringify(approved))
            throw new Error("Recovery must match the exact recorded commit paths/message")
        pending = state
    } else {
        const observed = await observe(c, state.workspace.diskCommit)
        requireDraftable(observed, state)
        const changed = draft(observed)
        if (!changed.length) throw new Error("No unpublished fixes to commit; skip the commit step")
        if (JSON.stringify(changed) !== JSON.stringify(approved))
            throw new Error("Changed/untracked paths must match the complete approved fix set")
        const held = new Set([...observed.heldChanged, ...observed.heldUntracked])
        switch (state.configuration) {
            case "held":
                if (approved.some((file) => held.has(file)))
                    throw new Error(
                        "A fix touches held configuration, which is on disk at its trusted version; release the PR's configuration first, then redo that edit",
                    )
                break
            case "released":
                break
        }
        for (const file of approved) {
            try {
                if (fs.lstatSync(path.join(c.root, file)).isDirectory())
                    throw new Error("Approve individual source files, not recursive directory staging")
            } catch (error) {
                if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error
            }
        }
        await c.git(["var", "GIT_COMMITTER_IDENT"])
        await c.git(["add", "--force", "--all", "--", ...approved])
        const staged = await observe(c, state.workspace.diskCommit)
        const treeSha = (await c.git(["write-tree"])).trim()
        if (treeSha === state.workspace.indexTree) throw new Error("No actual fix tree change; no empty commit created")
        if (JSON.stringify(draft(staged)) !== JSON.stringify(approved) || staged.native.kind !== "none")
            throw new Error("Fix source changed during staging; preserve it and reassess")
        // After the commit, the files on disk are the committed tree with held configuration as it was.
        const diskCommit = await diskCommitFor(c, treeSha, state.trustedSha, state.configuration)
        pending = {
            ...stateFacts(state),
            workspace: state.workspace,
            phase: "committingFixes",
            publishedRebase: historicalRebase(state),
            fixes: {
                sourceSha: state.baselineSha,
                treeSha,
                message: text,
                paths: approved,
                indexFingerprint: staged.indexFingerprint,
                diskCommit,
            },
        }
        await save(pending)
    }
    let observed = await observe(c, pending.fixes.diskCommit)
    let messages = pending.messages
    if (observed.head === pending.fixes.sourceSha) {
        if (observed.indexFingerprint !== pending.fixes.indexFingerprint || observed.native.kind !== "none")
            throw new Error("Fix commit intent/index changed; preserve and investigate")
        const result = await c.result([
            "commit",
            "--no-verify",
            "--no-gpg-sign",
            "--cleanup=verbatim",
            `--message=${pending.fixes.message}`,
        ])
        messages = [...messages, ...[result.stdout.trim(), result.stderr.trim()].filter(Boolean)]
        observed = await observe(c, pending.fixes.diskCommit)
        if (observed.head === pending.fixes.sourceSha)
            throw new Error(`Fix commit did not complete; intent preserved. ${result.stderr}`)
    }
    if (!observed.head || !matchesFixCommit(observed, pending.fixes))
        throw new Error("Fix commit result does not match recorded parent/tree/message; preserve and investigate")
    // A commit leaves HEAD attached only when it was; detach so the review stays off every branch.
    await c.git(["update-ref", "--no-deref", "HEAD", observed.head])
    const completed: Committed = {
        ...stateFacts(pending),
        workspace: { headSha: observed.head, indexTree: pending.fixes.treeSha, diskCommit: pending.fixes.diskCommit },
        phase: "committedFixes",
        messages,
        publishedRebase: pending.publishedRebase,
        fixes: { ...pending.fixes, resultSha: observed.head, publication: { kind: "pending" } },
    }
    await save(completed)
    requireExact(await observe(c, completed.workspace.diskCommit), completed.workspace, "Fix commit")
    return completed
}

export async function publishFixes(
    c: Checkout,
    state: Committed,
    evidence: ProviderEvidence,
    save: Save,
): Promise<CheckoutState> {
    requireExact(await observe(c, state.workspace.diskCommit), state.workspace, "Fix publication")
    const repository = state.revision.head.repository
    if (!repository) throw new Error("Missing contributor repository identity")
    const ref = await headBranchRef(c.git, state.revision.head.ref)
    const confirm = async (candidate: Committed): Promise<CheckoutState> => {
        const result: Editing = {
            ...stateFacts(candidate),
            workspace: candidate.workspace,
            baselineSha: candidate.fixes.resultSha,
            phase: "fixes",
            publishedRebase: candidate.publishedRebase,
        }
        await save(result)
        return result
    }
    const before = await remoteHead(c, repository, state.revision.head.ref)
    if (before === state.fixes.resultSha) return confirm(state)
    if (before !== state.fixes.sourceSha) {
        const rejected: Committed = {
            ...state,
            fixes: { ...state.fixes, publication: { kind: "rejected", observedHead: before } },
            messages: [
                ...state.messages,
                "Contributor branch changed or disappeared; local fix commit preserved. No push or history rewrite.",
            ],
        }
        await save(rejected)
        return rejected
    }
    open(evidence)
    requireExact(await observe(c, state.workspace.diskCommit), state.workspace, "Fix publication")
    let attempted: Committed = { ...state, fixes: { ...state.fixes, publication: { kind: "attempted" } } }
    await save(attempted)
    const pushed = await c.result([
        "push",
        "--porcelain",
        "--no-verify",
        "--recurse-submodules=no",
        "--signed=false",
        githubSshUrl(repository),
        `${state.fixes.resultSha}:${ref}`,
    ])
    attempted = {
        ...attempted,
        messages: [...attempted.messages, ...[pushed.stdout.trim(), pushed.stderr.trim()].filter(Boolean)],
    }
    await save(attempted)
    let after: string | null
    try {
        after = await remoteHead(c, repository, state.revision.head.ref)
    } catch {
        const uncertain = {
            ...attempted,
            messages: [
                ...attempted.messages,
                "Fix publication is uncertain; inspect the exact remote before an approved retry. Do not recommit or force-push.",
            ],
        }
        await save(uncertain)
        return uncertain
    }
    if (after === state.fixes.resultSha) return confirm(attempted)
    const rejected: Committed = {
        ...attempted,
        fixes: { ...attempted.fixes, publication: { kind: "rejected", observedHead: after } },
        messages: [...attempted.messages, "Fix head not confirmed remotely; no retry or force performed."],
    }
    await save(rejected)
    return rejected
}
