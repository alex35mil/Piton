import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent"
import { type Static, Type } from "typebox"

import { compile, parse } from "../../../extensions/__lib/schema.js"
import * as project from "../../../extensions/__lib/project.js"
import * as Git from "../__lib/git.js"
import { readProjectPolicies } from "../__lib/project-config.js"
import { workflowTools } from "../__lib/tools.js"
import {
    fence,
    MAX_EMBEDDED_FILE_BYTES,
    REVIEW_AUTHORITY_INSTRUCTIONS,
    REVIEW_GIT_INSTRUCTIONS,
    truncate,
} from "./context.js"
import { commitDiff, commitRangeDiff, workingTreeDiff } from "./diff.js"
import { reviewAgentOutputInstructions, renderReviewDocument } from "./findings.js"
import { reviewerEnvironment } from "./model.js"
import { executeReviewRound } from "./round.js"

const RefSchema = Type.String({
    minLength: 1,
    pattern: "^[^-\\s][^\\s]*$",
    description: "A Git revision: a branch, tag, or commit. Must not start with '-'.",
})
const strict = { additionalProperties: false } as const
const kind = <K extends string>(value: K) => Type.Enum([value], { type: "string" })

/** What a standalone review looks at. Each variant carries only what it needs. */
export const ReviewTargetSchema = Type.Union([
    Type.Object({ kind: kind("uncommitted") }, strict),
    Type.Object({ kind: kind("commit"), commit: RefSchema }, strict),
    Type.Object({ kind: kind("range"), base: RefSchema, head: Type.Optional(RefSchema) }, strict),
    Type.Object({ kind: kind("branch"), base: RefSchema }, strict),
    Type.Object(
        {
            kind: kind("paths"),
            paths: Type.Array(Type.String({ minLength: 1 }), { minItems: 1, uniqueItems: true }),
        },
        strict,
    ),
])
export type ReviewTarget = Static<typeof ReviewTargetSchema>

export const StandaloneReviewSchema = Type.Object(
    {
        target: ReviewTargetSchema,
        context: Type.Optional(
            Type.String({
                description:
                    "What the change is meant to do: requirements, intent, decisions, or earlier findings to verify. The reviewer treats it as the requirement.",
            }),
        ),
        instructions: Type.Optional(Type.String({ description: "Extra review instructions for this run" })),
        model: Type.Optional(Type.String({ description: "Optional pi model pattern for the reviewer" })),
    },
    strict,
)
export const StandaloneReviewValidator = compile(StandaloneReviewSchema)
export type StandaloneReviewInput = Static<typeof StandaloneReviewSchema>

/** A target resolved against the repository: refs checked, paths inside the repository. */
type ResolvedTarget =
    | { kind: "uncommitted" }
    | { kind: "commit"; commit: string }
    | { kind: "range"; base: string; head: string }
    | { kind: "branch"; base: string; mergeBase: string }
    | { kind: "paths"; paths: string[] }

function verifyRef(root: string, ref: string): string {
    if (ref.startsWith("-")) throw new Error(`invalid revision ${JSON.stringify(ref)}`)
    const commit = Git.tryRun(root, ["rev-parse", "--verify", "--quiet", "--end-of-options", `${ref}^{commit}`])
    if (!commit) throw new Error(`unknown revision ${JSON.stringify(ref)}`)
    return ref
}

export function resolveReviewTarget(root: string, target: ReviewTarget): ResolvedTarget {
    switch (target.kind) {
        case "uncommitted":
            return target
        case "commit":
            return { kind: "commit", commit: verifyRef(root, target.commit) }
        case "range":
            return { kind: "range", base: verifyRef(root, target.base), head: verifyRef(root, target.head ?? "HEAD") }
        case "branch": {
            // Any revision works as the base (a branch, `origin/main`, a tag, a SHA): the merge base comes from it.
            const base = verifyRef(root, target.base)
            const mergeBase = Git.tryRun(root, ["merge-base", "--end-of-options", base, "HEAD"])
            if (!mergeBase) throw new Error(`the current branch has no merge base with ${JSON.stringify(base)}`)
            return { kind: "branch", base, mergeBase }
        }
        case "paths":
            return {
                kind: "paths",
                paths: target.paths.map((raw) => {
                    const absolute = path.resolve(root, raw)
                    const relative = path.relative(root, absolute)
                    if (relative.startsWith("..") || path.isAbsolute(relative))
                        throw new Error(`review path is outside the repository: ${raw}`)
                    if (!fs.existsSync(absolute)) throw new Error(`review path does not exist: ${raw}`)
                    return relative || "."
                }),
            }
    }
}

function describeTarget(target: ResolvedTarget): string {
    switch (target.kind) {
        case "uncommitted":
            return "uncommitted work: staged, unstaged, and untracked changes against HEAD"
        case "commit":
            return `commit ${target.commit}`
        case "range":
            return `commits ${target.base}..${target.head}`
        case "branch":
            return `the current branch since its merge base with ${target.base}, through the working tree`
        case "paths":
            return `these files as they are: ${target.paths.join(", ")}`
    }
}

/** The target's changes for the prompt, and whether untracked files may be part of it. */
async function targetChanges(root: string, target: ResolvedTarget): Promise<{ changes: string; untracked: boolean }> {
    switch (target.kind) {
        case "uncommitted":
            return {
                changes: await workingTreeDiff(root, "HEAD", "Uncommitted changes against HEAD: staged and unstaged."),
                untracked: true,
            }
        case "commit":
            return { changes: await commitDiff(root, target.commit), untracked: false }
        case "range":
            return {
                changes: await commitRangeDiff(
                    root,
                    target.base,
                    target.head,
                    `Changes from ${target.base} to ${target.head}.`,
                ),
                untracked: false,
            }
        case "branch":
            return {
                changes: await workingTreeDiff(
                    root,
                    target.mergeBase,
                    `Changes from the merge base with ${target.base} (${target.mergeBase}) to the working tree: committed, staged, and unstaged.`,
                ),
                untracked: true,
            }
        case "paths":
            return { changes: "", untracked: false }
    }
}

export async function buildStandaloneReviewPrompt(root: string, input: StandaloneReviewInput): Promise<string> {
    const target = resolveReviewTarget(root, input.target)
    const { changes, untracked } = await targetChanges(root, target)
    const policies = readProjectPolicies(root)
    return [
        "# Review request",
        "",
        `Review target: ${describeTarget(target)}`,
        "",
        "You are an independent senior code review agent running in a fresh context.",
        "You are read-only. Do not modify files. Do not ask the user questions directly.",
        "Follow all rules supplied by the rules extension. If reviewing files that may match path-scoped rules, read those files first so the rules extension can load applicable rules. A violated rule is a broken requirement.",
        ...REVIEW_AUTHORITY_INSTRUCTIONS,
        "This review isn't part of an Epic, Task, or Gig. Don't ask for plans, results, branches, or workflow artifacts.",
        REVIEW_GIT_INSTRUCTIONS,
        untracked
            ? "The diff leaves out untracked files. Read each untracked file that git status lists (`??`) with read; they are part of the change."
            : "",
        target.kind === "paths"
            ? "Read each listed path with read (directories with ls and find) and review it as it is."
            : "Read the surrounding code the change touches before reporting a finding.",
        input.context
            ? "The stated context below is the requirement: check the change against it, and report a break of it as blocking."
            : "No requirement was stated: review for correctness, regressions, security, and maintainability.",
        "A finding is blocking when the change, as it is, would contain a defect, would break a stated requirement or active rule, or needs a decision only the user can make. Everything else is a note.",
        "It is valid to report no findings. Do not invent a finding to justify the review.",
        "For edge-case, race/concurrency, and failure-mode findings, apply every active evidence and credible-complexity rule. A finding that omits evidence required by those rules is invalid, must be omitted, and must not affect signoff.",
        input.instructions ? `\nExtra instructions:\n${input.instructions}` : "",
        "",
        reviewAgentOutputInstructions(),
        policies ? ["", "# Project Policies (policies.md)", "", policies].join("\n") : "",
        input.context
            ? ["", "# Stated context", "", fence(truncate(input.context, MAX_EMBEDDED_FILE_BYTES), "markdown")].join(
                  "\n",
              )
            : "",
        changes ? ["", "# Changes", "", changes].join("\n") : "",
    ]
        .filter(Boolean)
        .join("\n")
}

/**
 * Runs one review of `target` with the shared reviewer and returns its rendered findings. Needs no entity, tracker, or
 * forge, and keeps nothing: the round's files live in a temporary directory removed afterwards.
 */
export async function runStandaloneReview(
    pi: Pick<ExtensionAPI, "getSettings" | "getCommands">,
    ctx: { cwd: string; modelRegistry: Parameters<typeof reviewerEnvironment>[1] },
    input: StandaloneReviewInput,
    options: {
        signal?: AbortSignal
        onUpdate?: (text: string) => void
        execute?: typeof executeReviewRound
    } = {},
): Promise<string> {
    const root = project.resolveRootDir(ctx.cwd)
    const prompt = await buildStandaloneReviewPrompt(root, input)
    const directory = await fs.promises.mkdtemp(path.join(os.tmpdir(), "pi-standalone-review-"))
    try {
        const { report } = await (options.execute ?? executeReviewRound)({
            environment: reviewerEnvironment(pi, ctx.modelRegistry),
            cwd: root,
            directory,
            additionalFiles: [],
            prompt,
            signal: options.signal,
            model: input.model,
            agentName: "standalone-review",
            progressLabel: "Review agent",
            toolset: "filesAndGit",
            onUpdate: options.onUpdate,
        }).catch((error: unknown) => {
            // The round's files are about to be removed: keep the reason and any unvalidated output, drop the paths.
            const message = error instanceof Error ? error.message : String(error)
            throw new Error(message.replace(/\n\nArtifacts:\n(?:- .*\n?)*$/, ""))
        })
        return renderReviewDocument(report).trimEnd()
    } finally {
        await fs.promises.rm(directory, { recursive: true, force: true })
    }
}

export function registerStandaloneReview(pi: ExtensionAPI): void {
    const tools = workflowTools(pi, "reviews")
    tools.registerTool({
        name: "review",
        label: "review",
        description:
            "Run one independent read-only review of uncommitted work, one commit, a commit range, the current branch against a base, or a set of paths, optionally against stated requirements. Needs no Epic, Task, or Gig, and no tracker or forge. Returns the findings and signoff; saves nothing.",
        parameters: StandaloneReviewSchema,
        async execute(_id, params, signal, onUpdate, ctx) {
            const input = parse(StandaloneReviewValidator, params)
            const text = await runStandaloneReview(pi, ctx, input, {
                signal,
                onUpdate: (update) => onUpdate?.({ content: [{ type: "text", text: update }], details: undefined }),
            })
            return { content: [{ type: "text" as const, text }], details: undefined }
        },
    })
}
