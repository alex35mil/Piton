import * as fs from "node:fs"
import * as path from "node:path"
import { createHash } from "node:crypto"

import type { IncomingRevision, Repository } from "../integrations/forge/github/incoming-pr-context.js"
import { githubSshUrl, headBranchRef, type GitResult } from "./git.js"
import { requireAddress, stateFacts, type CheckoutState, type Workspace } from "./state.js"
import {
    checkoutSourcePath,
    move,
    nativeOperation,
    observe,
    openCheckout,
    requireExact,
    targetOf,
    workspaceFor,
    type Checkout,
} from "./workspace.js"

type Save = (state: CheckoutState) => Promise<void>
type Fixes = Extract<CheckoutState, { phase: "fixes" }>
type Rebasing = Extract<CheckoutState, { phase: "rebasing" }>
type Rebased = Extract<CheckoutState, { phase: "rebased" }>

export async function remoteHead(c: Checkout, repository: Repository, ref: string): Promise<string | null> {
    const full = await headBranchRef(c.git, ref)
    const result = await c.result(["ls-remote", "--exit-code", "--refs", githubSshUrl(repository), full])
    if (result.code === 2) return null
    if (result.code !== 0) throw new Error(`Cannot observe verified branch ${full}: ${result.stderr}`)
    const lines = result.stdout.trim().split("\n")
    const parts = lines[0].split("\t")
    if (lines.length !== 1 || parts[1] !== full || !/^[a-f0-9]{40,64}$/.test(parts[0]))
        throw new Error("Git observation did not return the exact verified branch/full SHA")
    return parts[0]
}

export async function observeTarget(
    c: Checkout,
    state: CheckoutState,
    current: IncomingRevision,
): Promise<CheckoutState> {
    requireAddress(state, current)
    const headRepository = state.revision.head.repository
    if (!headRepository) throw new Error("Missing contributor repository identity")
    const targetSha = await remoteHead(c, state.revision.base.repository, state.revision.base.ref)
    const headSha = await remoteHead(c, headRepository, state.revision.head.ref)
    if (!targetSha || !headSha)
        throw new Error("Verified target or contributor branch is missing; preserve checkout and reassess")
    return { ...state, providerRevision: current, observation: { targetSha, headSha } }
}

function messages(state: CheckoutState, result: GitResult): string[] {
    return [...state.messages, ...[result.stdout.trim(), result.stderr.trim()].filter(Boolean)]
}

/**
 * The private worktree a rebase runs in, under the PR's evidence folder. Nothing roots a shell, editor, or session
 * there, so contributor files it checks out stay inert; the checkout under review is never mid-rebase.
 */
export interface Scratch {
    directory: string
}

/** The scratch worktree's own Git directory, when `directory` is a worktree of the checkout's repository. */
function ownedBy(directory: string, commonGitDir: string): boolean {
    try {
        const link = fs.readFileSync(path.join(directory, ".git"), "utf-8").trim()
        const match = /^gitdir: (.+)$/.exec(link)
        return !!match && path.resolve(directory, match[1]).startsWith(path.join(commonGitDir, "worktrees") + path.sep)
    } catch {
        return false
    }
}

async function commonGitDir(c: Checkout): Promise<string> {
    return fs.realpathSync(path.resolve(c.root, (await c.git(["rev-parse", "--git-common-dir"])).trim()))
}

/** Removes the scratch worktree when it belongs to the checkout's repository; a foreign folder is preserved. */
export async function removeScratch(c: Checkout, scratch: Scratch): Promise<void> {
    if (!fs.existsSync(scratch.directory)) {
        await c.result(["worktree", "prune"])
        return
    }
    if (!ownedBy(scratch.directory, await commonGitDir(c)))
        throw new Error(`Unmanaged folder at ${scratch.directory}; it's preserved. Remove it by hand once inspected`)
    await c.git(["worktree", "remove", "--force", "--force", scratch.directory])
}

async function openScratch(c: Checkout, scratch: Scratch, signal?: AbortSignal): Promise<Checkout> {
    if (!ownedBy(scratch.directory, await commonGitDir(c)))
        throw new Error("The rebase worktree is missing or isn't this repository's; preserve it and investigate")
    return openCheckout(scratch.directory, signal)
}

export async function publishRebase(c: Checkout, state: Rebased, save: Save): Promise<CheckoutState> {
    requireExact(await observe(c, state.workspace.diskCommit), state.workspace, "Rebase publication")
    const repository = state.revision.head.repository
    if (!repository) throw new Error("Missing contributor repository identity")
    const ref = await headBranchRef(c.git, state.revision.head.ref)
    const confirm = async (candidate: Rebased): Promise<CheckoutState> => {
        const published = {
            ...candidate.rebase,
            publication: { kind: "confirmed" as const, headSha: candidate.rebase.resultSha },
        }
        const result: CheckoutState = {
            ...stateFacts(candidate),
            workspace: candidate.workspace,
            baselineSha: published.resultSha,
            phase: "fixes",
            publishedRebase: published,
        }
        await save(result)
        return result
    }
    const observed = await remoteHead(c, repository, state.revision.head.ref)
    if (observed === state.rebase.resultSha) return confirm(state)
    if (observed !== state.rebase.leaseSha) {
        const rejected: Rebased = {
            ...state,
            rebase: { ...state.rebase, publication: { kind: "rejected", observedHead: observed } },
            messages: [
                ...state.messages,
                "Contributor branch changed or disappeared. Lease rejected; local rebased result and remote work preserved.",
            ],
        }
        await save(rejected)
        return rejected
    }
    // Re-check local state after transport reads, before the irreversible operation.
    requireExact(await observe(c, state.workspace.diskCommit), state.workspace, "Rebase publication")
    let attempted: Rebased = { ...state, rebase: { ...state.rebase, publication: { kind: "attempted" } } }
    await save(attempted)
    const pushed = await c.result([
        "push",
        "--porcelain",
        "--no-verify",
        "--recurse-submodules=no",
        "--signed=false",
        `--force-with-lease=${ref}:${state.rebase.leaseSha}`,
        githubSshUrl(repository),
        `${state.rebase.resultSha}:${ref}`,
    ])
    attempted = { ...attempted, messages: messages(attempted, pushed) }
    await save(attempted)
    let after: string | null
    try {
        after = await remoteHead(c, repository, state.revision.head.ref)
    } catch {
        const uncertain = {
            ...attempted,
            messages: [
                ...attempted.messages,
                "Publication outcome is uncertain. Inspect the exact remote head before an approved publishRebase retry; do not repeat the rebase or replace its lease.",
            ],
        }
        await save(uncertain)
        return uncertain
    }
    if (after === state.rebase.resultSha) return confirm(attempted)
    const rejected: Rebased = {
        ...attempted,
        rebase: { ...attempted.rebase, publication: { kind: "rejected", observedHead: after } },
        messages: [
            ...attempted.messages,
            "Rebased head is not confirmed on the remote branch. No retry or lease refresh performed.",
        ],
    }
    await save(rejected)
    return rejected
}

/** The scratch rebase's current progress, for validation and inspection. */
export async function observeScratch(c: Checkout, scratch: Scratch, signal?: AbortSignal) {
    const s = await openScratch(c, scratch, signal)
    const head = (await s.git(["rev-parse", "--verify", "HEAD"])).trim()
    const unmerged = (await s.git(["ls-files", "--unmerged", "-z"]))
        .split("\0")
        .filter(Boolean)
        .map((entry) => {
            const match = /^(\d+) ([a-f0-9]+) ([123])\t([\s\S]+)$/.exec(entry)
            if (!match) throw new Error("Invalid unmerged index entry")
            return { mode: match[1], sha: match[2], stage: Number(match[3]), path: match[4] }
        })
    const entries = await s.git(["ls-files", "--stage", "-z"])
    return {
        scratch: s,
        head,
        unmerged,
        native: nativeOperation(s.gitDir),
        indexFingerprint: createHash("sha256").update(entries).digest("hex"),
        changed: (await s.git(["diff", "--name-only", "-z", "--no-ext-diff", "--no-textconv", "--"]))
            .split("\0")
            .filter(Boolean),
        untracked: (await s.git(["ls-files", "--others", "--exclude-standard", "-z"])).split("\0").filter(Boolean),
    }
}
export type ObservedScratch = Awaited<ReturnType<typeof observeScratch>>

export function validateRebasing(state: Rebasing, observed: ObservedScratch): void {
    const native = observed.native
    if (
        native.kind !== "rebase" ||
        native.sourceSha !== state.rebase.sourceSha ||
        native.targetSha !== state.rebase.targetSha ||
        !state.rebase.checkpoint ||
        native.fingerprint !== state.rebase.checkpoint.nativeFingerprint ||
        observed.indexFingerprint !== state.rebase.checkpoint.indexFingerprint
    )
        throw new Error("Native rebase progress/index changed or is incomplete; preserve and investigate")
}

async function step(
    c: Checkout,
    scratch: Scratch,
    state: Rebasing,
    args: string[],
    save: Save,
    signal?: AbortSignal,
): Promise<CheckoutState> {
    const s = await openScratch(c, scratch, signal)
    const command = await s.result(args)
    const observed = await observeScratch(c, scratch, signal)
    if (
        observed.native.kind === "rebase" &&
        observed.native.sourceSha === state.rebase.sourceSha &&
        observed.native.targetSha === state.rebase.targetSha
    ) {
        const pending: Rebasing = {
            ...state,
            messages: messages(state, command),
            rebase: {
                ...state.rebase,
                checkpoint: {
                    nativeFingerprint: observed.native.fingerprint,
                    indexFingerprint: observed.indexFingerprint,
                },
            },
        }
        await save(pending)
        if (command.code === 1 && observed.unmerged.length) return pending
        throw new Error(`Rebase stopped without a verified conflict outcome; native work preserved. ${command.stderr}`)
    }
    if (command.code !== 0 || observed.native.kind !== "none")
        throw new Error(`Rebase incomplete; preserve native progress and investigate. ${command.stderr}`)
    if (observed.changed.length || observed.untracked.length || observed.unmerged.length)
        throw new Error("Rebase result isn't clean; preserve the scratch worktree and investigate")
    await s.git(["merge-base", "--is-ancestor", state.rebase.targetSha, observed.head])
    return complete(c, scratch, state, observed.head, messages(state, command), save)
}

/** Moves the checkout under review to the rebase result, with held configuration unchanged, then publishes. */
async function complete(
    c: Checkout,
    scratch: Scratch,
    state: Rebasing,
    resultSha: string,
    log: string[],
    save: Save,
): Promise<CheckoutState> {
    const workspace: Workspace = await workspaceFor(c, resultSha, resultSha, state.trustedSha, state.configuration)
    const from = state.workspace.diskCommit
    const completed: Rebased = {
        ...stateFacts(state),
        moving: from,
        workspace,
        phase: "rebased",
        messages: log,
        rebase: {
            sourceSha: state.rebase.sourceSha,
            targetSha: state.rebase.targetSha,
            leaseSha: state.rebase.leaseSha,
            resultSha,
            publication: { kind: "pending" },
        },
    }
    await save(completed)
    const moved = await move(c, from, targetOf(workspace))
    const settled: Rebased = { ...completed, moving: null, messages: [...completed.messages, ...moved] }
    await save(settled)
    await removeScratch(c, scratch)
    return publishRebase(c, settled, save)
}

export async function startRebase(
    c: Checkout,
    scratch: Scratch,
    state: Fixes,
    current: IncomingRevision,
    targetSha: string,
    expectedHeadSha: string,
    save: Save,
    signal?: AbortSignal,
): Promise<CheckoutState> {
    requireExact(await observe(c, state.workspace.diskCommit), state.workspace, "Rebase")
    if (
        !state.observation ||
        state.observation.targetSha !== targetSha ||
        state.observation.headSha !== expectedHeadSha
    )
        throw new Error("Rebase target/source must match the separately observed and approved exact SHAs")
    if (expectedHeadSha !== state.baselineSha || current.head.sha !== expectedHeadSha)
        throw new Error("Contributor branch changed from the managed/provider head; reassess before rebase")
    const fresh = await observeTarget(c, state, current)
    if (fresh.observation?.targetSha !== targetSha || fresh.observation.headSha !== expectedHeadSha)
        throw new Error("Target or contributor tip changed after observation; obtain renewed exact approval")
    await c.git(["var", "GIT_COMMITTER_IDENT"])
    await c.git([
        "fetch",
        "--no-tags",
        "--no-recurse-submodules",
        githubSshUrl(state.revision.base.repository),
        targetSha,
    ])
    if ((await c.git(["cat-file", "-t", targetSha])).trim() !== "commit")
        throw new Error("Observed target is not a commit")
    requireExact(await observe(c, state.workspace.diskCommit), state.workspace, "Rebase")
    if (fs.existsSync(scratch.directory)) await removeScratch(c, scratch)
    await c.git(["worktree", "add", "--detach", "--quiet", scratch.directory, state.baselineSha])
    const pending: Rebasing = {
        ...stateFacts(fresh),
        workspace: state.workspace,
        phase: "rebasing",
        rebase: { sourceSha: state.baselineSha, targetSha, leaseSha: expectedHeadSha, checkpoint: null },
    }
    await save(pending)
    return step(
        c,
        scratch,
        pending,
        [
            "rebase",
            "--merge",
            "--no-fork-point",
            "--no-rebase-merges",
            "--no-autosquash",
            "--no-autostash",
            "--no-update-refs",
            "--no-gpg-sign",
            "--no-verify",
            "--empty=drop",
            "--no-reapply-cherry-picks",
            targetSha,
        ],
        save,
        signal,
    )
}

export async function continueRebase(
    c: Checkout,
    scratch: Scratch,
    state: Rebasing,
    paths: string[],
    save: Save,
    signal?: AbortSignal,
): Promise<CheckoutState> {
    requireExact(await observe(c, state.workspace.diskCommit), state.workspace, "Rebase continuation")
    const observed = await observeScratch(c, scratch, signal)
    validateRebasing(state, observed)
    const s = observed.scratch
    const approved = paths.map((file) => checkoutSourcePath(scratch.directory, file))
    const conflicts = [...new Set(observed.unmerged.map((entry) => entry.path))]
    if (conflicts.some((file) => !approved.includes(file)))
        throw new Error("Approve each pending conflict resolution path before continuing")
    if ([...observed.changed, ...observed.untracked].some((file) => !approved.includes(file)))
        throw new Error(
            "Unexpected edits or untracked additions outside approved resolutions; preserve and investigate",
        )
    if (approved.length) await s.git(["add", "--force", "--all", "--", ...approved])
    const staged = await observeScratch(c, scratch, signal)
    if (staged.unmerged.length || staged.native.kind !== "rebase")
        throw new Error("Unresolved index or missing native rebase; no continuation performed")
    const ready: Rebasing = {
        ...state,
        rebase: {
            ...state.rebase,
            checkpoint: { nativeFingerprint: staged.native.fingerprint, indexFingerprint: staged.indexFingerprint },
        },
    }
    await save(ready)
    return step(c, scratch, ready, ["rebase", "--continue"], save, signal)
}
