import * as fs from "node:fs"
import * as path from "node:path"

import { withFileMutationQueue, truncateHead, type ExtensionAPI } from "@earendil-works/pi-coding-agent"
import { Type, type Static } from "typebox"
import { compile, parse } from "../../../extensions/__lib/schema.js"
import { workflowTools } from "../__lib/tools.js"

import { EvidenceSchema } from "../integrations/resource-id-tool.js"
import type { McpToolCatalog } from "../../../extensions/__lib/mcp.js"
import {
    incomingPullRequestContext,
    incomingRevision,
    sameIncomingAddress,
    resolveIncomingLocalPath,
    type IncomingRevision,
} from "../integrations/forge/github/incoming-pr-context.js"
import type { GitHubForge } from "../integrations/config.js"
import { assertGitEnvironment, githubSshUrl } from "./git.js"
import {
    readCheckoutState,
    readRecordLocation,
    requireProviderRevision,
    saveCheckoutState,
    stateFacts,
    historicalRebase,
    currentPublication,
    IncomingCleanupCompletionSchema,
    type CheckoutState,
    type Configuration,
    type Materialized,
} from "./state.js"
import {
    checkoutRoot,
    heldChanges,
    isWorkTree,
    matches,
    matchesFixCommit,
    move,
    observe,
    observeStart,
    openCheckout,
    recordedTarget,
    requireExact,
    settle,
    targetOf,
    treeOf,
    workspaceFor,
    type Checkout,
    type Observed,
    type Target,
} from "./workspace.js"
import {
    continueRebase,
    observeScratch,
    observeTarget,
    publishRebase,
    removeScratch,
    startRebase,
    validateRebasing,
    type Scratch,
} from "./rebase.js"
import { commitFixes, publishFixes } from "./fixes.js"
import { INCOMING_CHECKOUT_TOOLS } from "./operations.js"
import { requireCompletion } from "./cleanup.js"

const common = {
    reference: Type.String({ minLength: 1, description: "Selected GitHub PR URL or #NN" }),
    evidence: EvidenceSchema,
}
const fullSha = Type.String({ pattern: "^[a-fA-F0-9]{40,64}$" })
export const IncomingCheckoutSchema = Type.Union([
    Type.Object(
        {
            ...common,
            operation: Type.Enum(
                [
                    "prepare",
                    "checkout",
                    "inspect",
                    "beginFixes",
                    "releaseConfiguration",
                    "holdConfiguration",
                    "observeTarget",
                    "publishRebase",
                    "publishFixes",
                    "restore",
                ],
                { type: "string" },
            ),
        },
        { additionalProperties: false },
    ),
    Type.Object(
        { ...common, operation: Type.Literal("rebase"), targetSha: fullSha, expectedHeadSha: fullSha },
        { additionalProperties: false },
    ),
    Type.Object(
        {
            ...common,
            operation: Type.Literal("continueRebase"),
            resolutionPaths: Type.Array(Type.String({ minLength: 1 }), {
                uniqueItems: true,
                description:
                    "Exact approved conflict-resolution paths, relative to the rebase worktree; the tool stages only these paths.",
            }),
        },
        { additionalProperties: false },
    ),
    Type.Object(
        {
            ...common,
            operation: Type.Literal("commitFixes"),
            paths: Type.Array(Type.String({ minLength: 1 }), { minItems: 1, uniqueItems: true }),
            message: Type.String({ minLength: 1 }),
        },
        { additionalProperties: false },
    ),
    Type.Object(
        { ...common, operation: Type.Literal("cleanup"), completion: IncomingCleanupCompletionSchema },
        { additionalProperties: false },
    ),
])
export const IncomingCheckoutValidator = compile(IncomingCheckoutSchema)
export type IncomingCheckoutInput = Static<typeof IncomingCheckoutSchema>
type Operation = IncomingCheckoutInput["operation"]

const DESCRIPTION =
    "for the selected incoming PR, in the current checkout, under its exact operation approval. Review leaves HEAD at the merge base with the PR head staged; files that make tools run or instruct agents (the piton tools registry: mise, direnv, Nix, devbox, AGENTS.md, .agents, .pi, .piton, editor project config) stay at the checkout's starting version on disk unless their release is approved. Refuses to overwrite staged, unstaged, untracked, or ignored work and never stashes. Rebase runs in a private scratch worktree; rebase/continuation create replacement commits and push with an SHA-pinned lease on success, so obtain approval for that exact set and contributor consent/access first. Fix commits are additive and separately approved; fixes publication pushes without force. Restore returns the checkout to its starting branch and revision only when nothing would be lost. Cleanup is part of an exact request to ship and merge, requires confirmed merge/Project Done, and restores; it never deletes a checkout. GitHub fetch/observation/push use SSH with existing authentication. No contributor-code execution, hooks, filters, unapproved commits, GitHub publication/merge, arbitrary Git commands or plain force push."

export function registerIncomingCheckout(pi: ExtensionAPI, tools: McpToolCatalog): void {
    const toolsApi = workflowTools(pi, "pull_requests")
    const [simple, rebase, continuation, commit, cleanup] = IncomingCheckoutSchema.anyOf
    const operations = [
        ["prepare", simple],
        ["checkout", simple],
        ["inspect", simple],
        ["beginFixes", simple],
        ["releaseConfiguration", simple],
        ["holdConfiguration", simple],
        ["observeTarget", simple],
        ["publishRebase", simple],
        ["publishFixes", simple],
        ["rebase", rebase],
        ["continueRebase", continuation],
        ["commitFixes", commit],
        ["restore", simple],
        ["cleanup", cleanup],
    ] as const
    for (const [operation, schema] of operations) {
        const name = INCOMING_CHECKOUT_TOOLS[operation]
        const parameters = Type.Omit(schema, ["operation"])
        const validator = compile(parameters)
        toolsApi.registerTool({
            name,
            label: name,
            description: `${name.replaceAll("_", " ")} ${DESCRIPTION}`,
            parameters,
            async execute(_id, params, signal, _update, ctx) {
                const input = parse(IncomingCheckoutValidator, { ...parse(validator, params), operation })
                assertGitEnvironment()
                const root = await checkoutRoot(ctx.cwd, signal)
                const context = incomingPullRequestContext(root, input.reference, tools)
                const forge = context.roles.forge.config
                const revision = incomingRevision(forge, context.incomingPullRequest, input.evidence, tools)
                const local = (name: string) =>
                    resolveIncomingLocalPath(
                        root,
                        revision,
                        path.join(context.incomingPullRequest.localDirectory, name),
                    )
                const stateFile = local("checkout.json")
                return withFileMutationQueue(stateFile, async () => {
                    fs.mkdirSync(path.dirname(stateFile), { recursive: true })
                    const c = await openCheckout(root, signal)
                    const env: Environment = {
                        c,
                        root,
                        local,
                        stateFile,
                        scratch: { directory: local("rebase") },
                        revision,
                        forge,
                        tools,
                        signal,
                    }
                    let result: unknown
                    try {
                        result = await operate(env, input)
                    } catch (error) {
                        const saved = fs.existsSync(stateFile) ? safeRead(env) : null
                        throw new Error(
                            `${error instanceof Error ? error.message : String(error)}\nCheckout preserved. Recorded phase: ${saved?.phase ?? "none"}. Publication facts: ${JSON.stringify(saved ? currentPublication(saved) : null)}. Inspect with ${INCOMING_CHECKOUT_TOOLS.inspect}; do not retry using Bash.`,
                        )
                    }
                    const output = truncateHead(JSON.stringify(result, null, 2))
                    return {
                        content: [
                            {
                                type: "text",
                                text:
                                    output.content +
                                    (output.truncated
                                        ? `\nOutput truncated; complete checkout state: ${local("checkout-inspection.json")}`
                                        : ""),
                            },
                        ],
                        details: result,
                    }
                })
            },
        })
    }
}

interface Environment {
    c: Checkout
    root: string
    local: (name: string) => string
    stateFile: string
    scratch: Scratch
    revision: IncomingRevision
    forge: GitHubForge
    tools: McpToolCatalog
    signal?: AbortSignal
}

function safeRead(env: Environment): CheckoutState | null {
    try {
        return readCheckoutState(env.stateFile, env.revision, env.forge, env.tools)
    } catch {
        return null
    }
}

/**
 * Who owns the PR's record, which a worker shares with its source through the linked `.piton/.local`:
 * - `none`: no record yet, or a restored one; preparation starts fresh.
 * - `here`: this checkout's review.
 * - `elsewhere`: another live checkout's review; every operation here is refused.
 * - `orphaned`: the recorded checkout no longer exists (a deleted worker); preparation takes the record over.
 */
type Ownership =
    | { kind: "none" }
    | { kind: "here"; state: CheckoutState }
    | { kind: "elsewhere"; root: string }
    | { kind: "orphaned"; root: string }

function ownership(env: Environment): Ownership {
    const recorded = readRecordLocation(env.stateFile)
    if (!recorded) return { kind: "none" }
    const root = path.resolve(recorded.root)
    const realRoot = fs.existsSync(root) ? fs.realpathSync(root) : null
    if (realRoot === env.root) {
        const state = readCheckoutState(env.stateFile, env.revision, env.forge, env.tools)
        return state.phase === "restored" ? { kind: "none" } : { kind: "here", state }
    }
    if (recorded.phase === "restored") return { kind: "none" }
    if (realRoot && isWorkTree(realRoot)) return { kind: "elsewhere", root }
    return { kind: "orphaned", root }
}

async function persist(env: Environment, state: CheckoutState): Promise<void> {
    await saveCheckoutState(env.stateFile, state)
}

async function operate(env: Environment, input: IncomingCheckoutInput) {
    const owner = ownership(env)
    switch (owner.kind) {
        case "elsewhere":
            throw new Error(
                `PR #${env.revision.number} is under review in ${owner.root}. Restore or finish it there, or delete that worker, before reviewing it here`,
            )
        case "orphaned":
            if (input.operation !== "prepare")
                throw new Error(
                    `PR #${env.revision.number}'s review was in ${owner.root}, which no longer exists. Prepare it again here to take the review over`,
                )
            await takeOver(env)
            return prepare(env, input)
        case "none":
            if (input.operation === "cleanup") return cleanupWithoutReview(env, input)
            if (input.operation !== "prepare")
                throw new Error("No review in this checkout: use approved preparation first")
            return prepare(env, input)
        case "here":
            return continueHere(env, owner.state, input)
    }
}

/** Evidence stays; only the stale record and an orphaned scratch worktree of this repository go. */
async function takeOver(env: Environment): Promise<void> {
    if (fs.existsSync(env.scratch.directory)) await removeScratch(env.c, env.scratch)
    else await env.c.result(["worktree", "prune"])
    await fs.promises.rm(env.stateFile)
}

async function prepare(env: Environment, input: IncomingCheckoutInput) {
    const { c, revision } = env
    if (!revision.head.repository)
        throw new Error("Missing head repository identity; cannot fetch the contributor head")
    const { start, status } = await observeStart(c)
    if (status.length)
        throw new Error(
            `The checkout has work a review would mix with or overwrite. Commit, move, or finish it first; nothing was stashed or changed:\n${status.slice(0, 20).join("\n")}`,
        )
    for (const side of [revision.base, revision.head]) {
        await c.git(["fetch", "--no-tags", "--no-recurse-submodules", githubSshUrl(side.repository!), side.sha])
        if ((await c.git(["cat-file", "-t", side.sha])).trim() !== "commit")
            throw new Error("Fetched PR revision is not a commit")
    }
    const reviewBaseSha = (await c.git(["merge-base", revision.base.sha, revision.head.sha])).trim()
    if (!/^[a-f0-9]{40,64}$/.test(reviewBaseSha)) throw new Error("Cannot establish contributor review merge-base")
    const state: CheckoutState = {
        revision,
        evidence: input.evidence,
        location: { root: env.root, start },
        reviewBaseSha,
        baselineSha: revision.head.sha,
        trustedSha: start.head,
        configuration: "held",
        moving: null,
        providerRevision: revision,
        observation: null,
        messages: [],
        phase: "prepared",
    }
    await persist(env, state)
    return inspect(env, state)
}

/** Finishes or rolls back a recorded move that an interruption left behind, before anything else. */
async function recover(env: Environment, state: CheckoutState): Promise<CheckoutState> {
    if (!state.moving) return state
    const startTree = await treeOf(env.c, state.location.start.head)
    const target = recordedTarget(state, startTree)
    if (!target) return { ...state, moving: null }
    const observed = await observe(env.c, target.diskCommit)
    const onDisk = !observed.changed.length && !observed.untracked.length && !observed.unmerged.length
    if (onDisk) {
        // The switch finished: point the index, HEAD, and branch at the target.
        const messages = await settle(env.c, target)
        const settled = { ...state, moving: null, messages: [...state.messages, ...messages] } as CheckoutState
        await persist(env, settled)
        return settled
    }
    const from = await observe(env.c, state.moving)
    if (!from.changed.length && !from.untracked.length && !from.unmerged.length) {
        // The switch never ran: move again from the recorded start.
        const messages = await move(env.c, state.moving, target)
        const moved = { ...state, moving: null, messages: [...state.messages, ...messages] } as CheckoutState
        await persist(env, moved)
        return moved
    }
    throw new Error(
        "An interrupted checkout move left files that match neither its start nor its target; preserve them and investigate",
    )
}

async function continueHere(env: Environment, saved: CheckoutState, input: IncomingCheckoutInput) {
    const { c } = env
    let state = await recover(env, saved)
    const save = (value: CheckoutState) => persist(env, value)
    switch (input.operation) {
        case "inspect":
            return inspect(env, state)
        case "observeTarget":
            state = await observeTarget(c, state, env.revision)
            await save(state)
            return inspect(env, state)
        case "prepare":
            // Repeated preparation never resets a review in progress.
            return inspect(env, state)
        case "cleanup":
            requireCompletion(env.root, env.revision, state, input.evidence, input.completion, env.tools)
            return restore(env, state, "cleanup")
        case "restore":
            return restore(env, state, "restore")
        case "checkout":
        case "beginFixes":
        case "releaseConfiguration":
        case "holdConfiguration":
        case "rebase":
        case "continueRebase":
        case "publishRebase":
        case "commitFixes":
        case "publishFixes":
            break
    }
    requireProviderRevision(state, env.revision)
    switch (input.operation) {
        case "checkout":
            return startReview(env, state)
        case "beginFixes":
            return beginFixes(env, state)
        case "releaseConfiguration":
            return configure(env, state, "released")
        case "holdConfiguration":
            return configure(env, state, "held")
        case "rebase":
            if (state.phase !== "fixes") throw new Error("Rebase requires a clean contributor-head fixes state")
            state = await startRebase(
                c,
                env.scratch,
                state,
                env.revision,
                input.targetSha.toLowerCase(),
                input.expectedHeadSha.toLowerCase(),
                save,
                env.signal,
            )
            return inspect(env, state)
        case "continueRebase":
            if (state.phase !== "rebasing") throw new Error("No managed native rebase to continue")
            state = await continueRebase(c, env.scratch, state, input.resolutionPaths, save, env.signal)
            return inspect(env, state)
        case "publishRebase":
            if (state.phase !== "rebased") throw new Error("No completed unpublished rebase to reconcile")
            state = await publishRebase(c, state, save)
            return inspect(env, state)
        case "commitFixes":
            if (state.phase !== "fixes" && state.phase !== "committingFixes")
                throw new Error("Fix commit requires an approved draft or its recorded commit intent")
            state = await commitFixes(c, state, input.paths, input.message, input.evidence, save)
            return inspect(env, state)
        case "publishFixes":
            if (state.phase !== "committedFixes") throw new Error("No recorded unpublished additive fix commit")
            state = await publishFixes(c, state, input.evidence, save)
            return inspect(env, state)
    }
}

/**
 * Runs a recorded move: saves the target state with its starting disk state first, then moves, then clears it. When
 * Git refuses the switch, nothing on disk changed: HEAD, the index, and the record go back to `current`.
 */
async function transition(
    env: Environment,
    from: string,
    next: CheckoutState,
    target: Target,
    current: { state: CheckoutState; target: Target },
): Promise<CheckoutState> {
    const pending = { ...next, moving: from } as CheckoutState
    await persist(env, pending)
    let messages: string[]
    try {
        messages = await move(env.c, from, target)
    } catch (error) {
        if ((error as { refused?: boolean }).refused) {
            await settle(env.c, current.target)
            await persist(env, current.state)
        }
        throw error
    }
    const done = { ...next, moving: null, messages: [...next.messages, ...messages] } as CheckoutState
    await persist(env, done)
    return done
}

async function startReview(env: Environment, state: CheckoutState) {
    switch (state.phase) {
        case "contributorReview":
            return inspect(env, state)
        case "prepared":
            break
        case "fixes":
        case "rebasing":
        case "rebased":
        case "committingFixes":
        case "committedFixes":
        case "restored":
            throw new Error("Cannot return to contributor review over a fix draft or rebase")
    }
    const start = await observeStart(env.c)
    if (start.status.length || start.start.head !== state.location.start.head)
        throw new Error(
            `The checkout changed since preparation; restore it to ${state.location.start.head} with no work in progress, then prepare again:\n${start.status.slice(0, 20).join("\n")}`,
        )
    const workspace = await workspaceFor(
        env.c,
        state.reviewBaseSha,
        state.revision.head.sha,
        state.trustedSha,
        state.configuration,
    )
    const next: CheckoutState = { ...stateFacts(state), workspace, phase: "contributorReview" }
    // The move detaches HEAD first, so the starting branch stays where it is whatever the switch does.
    const back = startTarget(state, await treeOf(env.c, state.location.start.head))
    return inspect(
        env,
        await transition(env, state.location.start.head, next, targetOf(workspace), { state, target: back }),
    )
}

function startTarget(state: CheckoutState, startTree: string): Target {
    const start = state.location.start
    return {
        head: start.head,
        indexTree: startTree,
        diskCommit: start.head,
        branch: start.kind === "branch" ? start.name : null,
    }
}

async function beginFixes(env: Environment, state: CheckoutState) {
    switch (state.phase) {
        case "fixes":
            return inspect(env, state)
        case "contributorReview":
            break
        case "prepared":
            throw new Error("Contributor review must finish before beginning fixes")
        case "rebasing":
        case "rebased":
        case "committingFixes":
        case "committedFixes":
        case "restored":
            throw new Error("Complete the approved commit/rebase/publication before beginning another fix draft")
    }
    requireExact(await observe(env.c, state.workspace.diskCommit), state.workspace, "Beginning fixes")
    // HEAD moves to the PR head; the index and the files on disk already are the PR head (with held configuration).
    const workspace = { ...state.workspace, headSha: state.revision.head.sha }
    await env.c.git(["update-ref", "--no-deref", "HEAD", workspace.headSha])
    const next: CheckoutState = { ...stateFacts(state), workspace, phase: "fixes", publishedRebase: null }
    await persist(env, next)
    return inspect(env, next)
}

async function configure(env: Environment, state: CheckoutState, configuration: Configuration) {
    switch (state.phase) {
        case "contributorReview":
        case "fixes":
            break
        case "prepared":
        case "rebasing":
        case "rebased":
        case "committingFixes":
        case "committedFixes":
        case "restored":
            throw new Error("Change held configuration only during contributor review or fixes")
    }
    if (state.configuration === configuration) return inspect(env, state)
    const observed = await observe(env.c, state.workspace.diskCommit)
    if (!sameDraftState(observed, state))
        throw new Error("HEAD, the index, or a Git operation changed; preserve them and investigate")
    if (observed.heldChanged.length || observed.heldUntracked.length)
        throw new Error(
            `Held configuration has edits; they're preserved. Move them aside first: ${JSON.stringify([...observed.heldChanged, ...observed.heldUntracked])}`,
        )
    const indexCommit = state.phase === "contributorReview" ? state.revision.head.sha : state.workspace.headSha
    const disk = await workspaceFor(env.c, state.workspace.headSha, indexCommit, state.trustedSha, configuration)
    const workspace = { ...state.workspace, diskCommit: disk.diskCommit }
    const next = { ...state, configuration, workspace } as CheckoutState
    // Fix-draft edits outside held configuration stay: the two disk states differ only in held paths, and the switch
    // carries other local edits across.
    return inspect(
        env,
        await transition(env, state.workspace.diskCommit, next, targetOf(workspace), {
            state,
            target: targetOf(state.workspace),
        }),
    )
}

function sameDraftState(observed: Observed, state: Materialized): boolean {
    return (
        observed.head === state.workspace.headSha &&
        observed.detached &&
        observed.indexTree === state.workspace.indexTree &&
        !observed.unmerged.length &&
        observed.native.kind === "none"
    )
}

async function restore(env: Environment, state: CheckoutState, reason: "restore" | "cleanup") {
    const start = state.location.start
    const restoredState: CheckoutState = { ...stateFacts(state), phase: "restored" }
    switch (state.phase) {
        case "prepared": {
            // Nothing moved yet: the checkout is still at its start.
            await persist(env, {
                ...restoredState,
                messages: [...state.messages, `Review ended by ${reason} before checkout; nothing to restore.`],
            })
            return inspect(env, restoredState)
        }
        case "rebasing":
            throw new Error("A rebase is in progress; finish or resolve it before restoring")
        case "rebased":
            throw new Error("A completed rebase isn't published yet; publish or reconcile it before restoring")
        case "committingFixes":
        case "committedFixes":
            throw new Error("A fix commit isn't published yet; publish it before restoring")
        case "restored":
            return inspect(env, state)
        case "contributorReview":
        case "fixes":
            break
    }
    requireExact(await observe(env.c, state.workspace.diskCommit), state.workspace, "Restore")
    if (fs.existsSync(env.scratch.directory)) await removeScratch(env.c, env.scratch)
    const target = startTarget(state, await treeOf(env.c, start.head))
    const done = await transition(
        env,
        state.workspace.diskCommit,
        {
            ...restoredState,
            messages: [...state.messages, `Restored by ${reason} to the starting ${start.kind} ${start.head}.`],
        },
        target,
        { state, target: targetOf(state.workspace) },
    )
    const after = await observeStart(env.c)
    if (after.status.length || after.start.head !== start.head)
        throw new Error(`Restore didn't return to a clean start: ${after.status.slice(0, 20).join("\n")}`)
    return inspect(env, done)
}

async function cleanupWithoutReview(env: Environment, input: Extract<IncomingCheckoutInput, { operation: "cleanup" }>) {
    requireCompletion(env.root, env.revision, null, input.evidence, input.completion, env.tools)
    return { phase: "absent", cleanup: { state: "notNeeded" }, root: env.root }
}

const save = (file: string, content: string) =>
    withFileMutationQueue(file, () => fs.promises.writeFile(file, content, "utf-8"))

async function inspect(env: Environment, state: CheckoutState) {
    const { c, local, root } = env
    const location = (name: string) => ({ relative: path.relative(root, local(name)), absolute: local(name) })
    const paths = {
        root: { relative: "", absolute: root },
        record: location("checkout.json"),
        inspection: location("checkout-inspection.json"),
        contributionDiff: location("checkout-contribution.diff"),
        fixesDiff: location("checkout-fixes.diff"),
        publishedFixesDiff: location("checkout-published-fixes.diff"),
        rebaseDiff: location("checkout-rebase.diff"),
        conflictsDiff: location("checkout-conflicts.diff"),
        untracked: location("checkout-untracked.json"),
        rebaseWorktree: location("rebase"),
    }
    const diff = (args: string[]) => c.git(["diff", "--binary", "--no-ext-diff", "--no-textconv", ...args, "--"])
    const contribution = await diff([state.reviewBaseSha, state.revision.head.sha])
    const heldInContribution = await heldChanges(c, state.reviewBaseSha, state.revision.head.sha)
    let git: Observed | null = null
    let phaseValid = true
    let fixes = ""
    let publishedFixes = ""
    let conflicts = ""
    let rebaseObservation: Awaited<ReturnType<typeof observeScratch>> | null = null
    const historical = historicalRebase(state)
    switch (state.phase) {
        case "prepared":
        case "restored":
            break
        case "contributorReview":
        case "fixes":
        case "rebasing":
        case "rebased":
        case "committingFixes":
        case "committedFixes": {
            const disk = state.phase === "committingFixes" ? state.fixes.diskCommit : state.workspace.diskCommit
            git = await observe(c, disk)
            phaseValid = phaseMatches(state, git)
            if (state.phase === "fixes" || state.phase === "committingFixes" || state.phase === "committedFixes") {
                fixes = await c.git([
                    "diff",
                    "--binary",
                    "--no-ext-diff",
                    "--no-textconv",
                    state.workspace.diskCommit,
                    "--",
                ])
                publishedFixes = await diff([historical?.resultSha ?? state.revision.head.sha, state.baselineSha])
            }
            if (state.phase === "rebasing") {
                try {
                    rebaseObservation = await observeScratch(c, env.scratch, env.signal)
                    validateRebasing(state, rebaseObservation)
                    if (rebaseObservation.unmerged.length)
                        conflicts = await rebaseObservation.scratch.git([
                            "diff",
                            "--cc",
                            "--no-ext-diff",
                            "--no-textconv",
                            "--",
                        ])
                } catch {
                    phaseValid = false
                }
            }
            break
        }
    }
    const rebase = state.phase === "rebasing" || state.phase === "rebased" ? state.rebase : historical
    const integrationHead =
        state.phase === "rebasing"
            ? rebaseObservation?.head
            : state.phase === "rebased"
              ? state.rebase.resultSha
              : historical?.resultSha
    const rebased = rebase && integrationHead ? await diff([rebase.sourceSha, integrationHead]) : ""
    await save(paths.contributionDiff.absolute, contribution)
    await save(paths.fixesDiff.absolute, fixes)
    await save(paths.publishedFixesDiff.absolute, publishedFixes)
    await save(paths.rebaseDiff.absolute, rebased)
    await save(paths.conflictsDiff.absolute, conflicts)
    await save(paths.untracked.absolute, JSON.stringify(git?.untracked ?? [], null, 2) + "\n")
    const result = {
        paths,
        revision: state.revision,
        providerRevision: env.revision,
        location: state.location,
        reviewBaseSha: state.reviewBaseSha,
        baselineSha: state.baselineSha,
        trustedSha: state.trustedSha,
        configuration: state.configuration,
        heldConfiguration: {
            changedByPr: heldInContribution,
            onDisk: state.configuration === "held" ? "trusted (the checkout's starting version)" : "the PR's",
        },
        observation: state.observation,
        phase: state.phase,
        phaseValid,
        rebase,
        conflictPaths: rebaseObservation
            ? [...new Set(rebaseObservation.unmerged.map((entry) => path.join(env.scratch.directory, entry.path)))]
            : [],
        fixCommit: state.phase === "committingFixes" || state.phase === "committedFixes" ? state.fixes : null,
        messages: state.messages,
        providerAddressMatches: sameIncomingAddress(state.revision, env.revision),
        providerHeadMatchesBaseline: env.revision.head.sha === state.baselineSha,
        git,
        limitations: [
            "The staged diff is the contribution against its merge base with the target, not a rebase onto the latest target.",
            "Git can't inspect unsaved editor buffers; confirm before phase changes.",
            "Editors and language servers can run project code (build scripts, proc macros, linter and formatter config) when files open; Piton can't prevent that.",
            "Submodules, filter, and LFS assets aren't fetched. Rebase conflicts use ours=target plus already replayed commits, theirs=the contributor commit being replayed.",
        ],
    }
    await save(paths.inspection.absolute, JSON.stringify(result, null, 2) + "\n")
    return result
}

function phaseMatches(state: Materialized, observed: Observed): boolean {
    switch (state.phase) {
        case "contributorReview":
        case "rebasing":
        case "rebased":
        case "committedFixes":
            return matches(observed, state.workspace)
        case "fixes":
            return sameDraftState(observed, state)
        case "committingFixes":
            // Staged for the recorded commit, before or after the commit itself.
            return (
                observed.indexTree === state.fixes.treeSha &&
                (observed.head === state.fixes.sourceSha || matchesFixCommit(observed, state.fixes)) &&
                !observed.changed.length &&
                !observed.untracked.length &&
                observed.native.kind === "none"
            )
    }
}
