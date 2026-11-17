import * as fs from "node:fs"
import { Type, type Static } from "typebox"

import { compile, parse } from "../../../extensions/__lib/schema.js"
import { EvidenceSchema } from "../integrations/resource-id-tool.js"
import type { McpToolCatalog } from "../../../extensions/__lib/mcp.js"
import {
    incomingRevision,
    sameIncomingAddress,
    sameIncomingRevision,
    type IncomingRevision,
} from "../integrations/forge/github/incoming-pr-context.js"
import type { GitHubForge } from "../integrations/config.js"

const sha = Type.String({ pattern: "^[a-f0-9]{40,64}$" })
const repository = Type.Object({ owner: Type.String(), repo: Type.String() }, { additionalProperties: false })
const revision = Type.Object(
    {
        repository,
        number: Type.Integer({ minimum: 1 }),
        url: Type.String(),
        kind: Type.Literal("incomingPullRequest"),
        mcpServer: Type.String(),
        base: Type.Object({ repository, ref: Type.String(), sha }, { additionalProperties: false }),
        head: Type.Object(
            { repository: Type.Union([repository, Type.Null()]), ref: Type.String(), sha },
            { additionalProperties: false },
        ),
    },
    { additionalProperties: false },
)
/** Where the review happens: the checkout's root and what it was on before preparation. */
const start = Type.Union([
    Type.Object(
        { kind: Type.Literal("branch"), name: Type.String({ minLength: 1 }), head: sha },
        {
            additionalProperties: false,
        },
    ),
    Type.Object({ kind: Type.Literal("detached"), head: sha }, { additionalProperties: false }),
])
const location = Type.Object({ root: Type.String({ minLength: 1 }), start }, { additionalProperties: false })
/**
 * The state the tool set the checkout to: detached HEAD, the index tree, and the commit whose tree equals the files on
 * disk (the index tree with held configuration at its trusted version). `piton worker delete` reads these three fields.
 */
const workspace = Type.Object({ headSha: sha, indexTree: sha, diskCommit: sha }, { additionalProperties: false })
const intent = { sourceSha: sha, targetSha: sha, leaseSha: sha }
const checkpoint = Type.Object(
    { nativeFingerprint: Type.String(), indexFingerprint: Type.String() },
    { additionalProperties: false },
)
const active = Type.Object(
    { ...intent, checkpoint: Type.Union([checkpoint, Type.Null()]) },
    { additionalProperties: false },
)
const publication = Type.Union([
    Type.Object({ kind: Type.Literal("pending") }, { additionalProperties: false }),
    Type.Object({ kind: Type.Literal("attempted") }, { additionalProperties: false }),
    Type.Object(
        { kind: Type.Literal("rejected"), observedHead: Type.Union([sha, Type.Null()]) },
        { additionalProperties: false },
    ),
    Type.Object({ kind: Type.Literal("confirmed"), headSha: sha }, { additionalProperties: false }),
])
const completed = Type.Object({ ...intent, resultSha: sha, publication }, { additionalProperties: false })
const published = Type.Union([completed, Type.Null()])
const fixIntent = {
    sourceSha: sha,
    treeSha: sha,
    message: Type.String({ minLength: 1 }),
    paths: Type.Array(Type.String({ minLength: 1 }), { minItems: 1, uniqueItems: true }),
    indexFingerprint: Type.String({ pattern: "^[a-f0-9]{64}$" }),
    /** The disk state after the commit: the committed tree with held configuration as it is. */
    diskCommit: sha,
}
export const IncomingCleanupCompletionSchema = Type.Union([
    Type.Object({ kind: Type.Enum(["forgeOnly"], { type: "string" }) }, { additionalProperties: false }),
    Type.Object(
        {
            kind: Type.Enum(["project"], { type: "string" }),
            fieldEvidence: EvidenceSchema,
            itemEvidence: EvidenceSchema,
        },
        { additionalProperties: false },
    ),
])
export type IncomingCleanupCompletion = Static<typeof IncomingCleanupCompletionSchema>
const ConfigurationSchema = Type.Union([Type.Literal("held"), Type.Literal("released")])
export type Configuration = Static<typeof ConfigurationSchema>
const core = {
    revision,
    evidence: EvidenceSchema,
    location,
    reviewBaseSha: sha,
    baselineSha: sha,
    /**
     * Where held configuration on disk comes from: the checkout's starting revision, which the maintainer was already
     * running, so the review never changes the configuration a shell, editor, or session sees.
     */
    trustedSha: sha,
    configuration: ConfigurationSchema,
    /**
     * The disk state a recorded move started from, while the move runs: saved before the checkout changes, cleared
     * once it's done, so an interrupted move is finished or retried rather than guessed.
     */
    moving: Type.Union([sha, Type.Null()]),
    providerRevision: revision,
    messages: Type.Array(Type.String()),
    observation: Type.Union([
        Type.Object({ targetSha: sha, headSha: sha }, { additionalProperties: false }),
        Type.Null(),
    ]),
}
const materialized = { ...core, workspace }
export const CheckoutStateSchema = Type.Union([
    Type.Object({ ...core, phase: Type.Literal("prepared") }, { additionalProperties: false }),
    Type.Object({ ...materialized, phase: Type.Literal("contributorReview") }, { additionalProperties: false }),
    Type.Object(
        { ...materialized, phase: Type.Literal("fixes"), publishedRebase: published },
        { additionalProperties: false },
    ),
    Type.Object({ ...materialized, phase: Type.Literal("rebasing"), rebase: active }, { additionalProperties: false }),
    Type.Object(
        { ...materialized, phase: Type.Literal("rebased"), rebase: completed },
        { additionalProperties: false },
    ),
    Type.Object(
        {
            ...materialized,
            phase: Type.Literal("committingFixes"),
            publishedRebase: published,
            fixes: Type.Object(fixIntent, { additionalProperties: false }),
        },
        { additionalProperties: false },
    ),
    Type.Object(
        {
            ...materialized,
            phase: Type.Literal("committedFixes"),
            publishedRebase: published,
            fixes: Type.Object({ ...fixIntent, resultSha: sha, publication }, { additionalProperties: false }),
        },
        { additionalProperties: false },
    ),
    Type.Object({ ...core, phase: Type.Literal("restored") }, { additionalProperties: false }),
])
export const CheckoutStateValidator = compile(CheckoutStateSchema)
export type CheckoutState = Static<typeof CheckoutStateSchema>
export type Workspace = Static<typeof workspace>
export type Start = Static<typeof start>
export type ActiveRebase = Static<typeof active>
export type CompletedRebase = Static<typeof completed>
export type Materialized = Extract<CheckoutState, { workspace: Workspace }>

export function historicalRebase(state: CheckoutState): CompletedRebase | null {
    return "publishedRebase" in state ? state.publishedRebase : null
}
export function currentPublication(state: CheckoutState) {
    if (state.phase === "committedFixes") return state.fixes.publication
    if (state.phase === "rebased") return state.rebase.publication
    return historicalRebase(state)?.publication ?? null
}

export function stateFacts(state: CheckoutState) {
    const {
        revision,
        evidence,
        location,
        reviewBaseSha,
        baselineSha,
        trustedSha,
        configuration,
        moving,
        providerRevision,
        messages,
        observation,
    } = state
    return {
        revision,
        evidence,
        location,
        reviewBaseSha,
        baselineSha,
        trustedSha,
        configuration,
        moving,
        providerRevision,
        messages,
        observation,
    }
}

/** The record as stored, before it's checked against the current PR: enough to decide who owns it. */
export function readRecordLocation(file: string): { root: string; phase: string } | null {
    if (!fs.existsSync(file)) return null
    const value: unknown = JSON.parse(fs.readFileSync(file, "utf-8"))
    if (!CheckoutStateValidator.check(value))
        throw new Error(
            "Unrecognized PR checkout record (possibly from the earlier nested-checkout layout); it's preserved. Inspect it and remove it by hand when it's no longer needed",
        )
    return { root: value.location.root, phase: value.phase }
}

export function readCheckoutState(
    file: string,
    current: IncomingRevision,
    forge: GitHubForge,
    tools: McpToolCatalog,
): CheckoutState {
    const value: unknown = JSON.parse(fs.readFileSync(file, "utf-8"))
    if (!CheckoutStateValidator.check(value))
        throw new Error("Invalid managed checkout record; preserve it and investigate")
    const state = value
    const original = incomingRevision(forge, state.revision, state.evidence, tools)
    if (!sameIncomingRevision(original, state.revision))
        throw new Error("Original checkout evidence does not match its recorded revision")
    if (
        state.revision.number !== current.number ||
        state.revision.repository.owner !== current.repository.owner ||
        state.revision.repository.repo !== current.repository.repo ||
        !sameIncomingAddress(state.revision, state.providerRevision)
    )
        throw new Error("Managed checkout does not belong to this PR or has inconsistent stored addressing")
    if (
        (state.phase === "committingFixes" || state.phase === "committedFixes") &&
        state.fixes.sourceSha !== state.baselineSha
    )
        throw new Error("Fix commit source does not match the published baseline")
    const integration = historicalRebase(state)
    if (integration && integration.publication.kind !== "confirmed") throw new Error("Invalid confirmed rebase history")
    return state
}

export function requireAddress(state: CheckoutState, current: IncomingRevision): void {
    if (!sameIncomingAddress(state.revision, current))
        throw new Error("PR addressing changed; preserve existing checkout and reassess")
}

export function requireProviderRevision(state: CheckoutState, current: IncomingRevision): void {
    requireAddress(state, current)
    const result =
        state.phase === "rebased"
            ? state.rebase.resultSha
            : state.phase === "committedFixes"
              ? state.fixes.resultSha
              : null
    const expected = result ? [state.baselineSha, result] : [state.baselineSha]
    if (!expected.includes(current.head.sha))
        throw new Error("PR head/revision changed; reassess before mutation. Existing checkout preserved")
    const published = historicalRebase(state) !== null
    const knownPublishedResult = result !== null && current.head.sha === result
    if (!published && !knownPublishedResult && current.base.sha !== state.providerRevision.base.sha)
        throw new Error(
            "Provider base revision changed; observe and reassess before mutation. Existing checkout preserved",
        )
}

export async function saveCheckoutState(file: string, state: CheckoutState): Promise<void> {
    parse(CheckoutStateValidator, state)
    // Caller holds this record's mutation queue for the full operation.
    const temporary = `${file}.tmp`
    await fs.promises.rm(temporary, { force: true })
    await fs.promises.writeFile(temporary, JSON.stringify(state, null, 2) + "\n", { flag: "wx" })
    await fs.promises.rename(temporary, file)
}
