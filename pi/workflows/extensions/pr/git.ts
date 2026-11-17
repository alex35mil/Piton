import * as cp from "node:child_process"
import * as fs from "node:fs"
import {
    FAILING_PROGRAM as FAILING_MERGE_DRIVER,
    GIT_HELPER_CONFIG_PATTERN,
    gitHelperOverride,
    inheritedGitRelocation,
} from "../__lib/git.js"
import type { Repository } from "../integrations/forge/github/incoming-pr-context.js"

const settings = [
    "core.hooksPath=/dev/null",
    "core.fsmonitor=false",
    "core.untrackedCache=false",
    "submodule.recurse=false",
    "push.recurseSubmodules=no",
    "commit.gpgSign=false",
    "tag.gpgSign=false",
    "core.editor=:",
    "rerere.enabled=false",
    "rerere.autoupdate=false",
    "rebase.autoStash=false",
    "rebase.autoSquash=false",
    "rebase.updateRefs=false",
    "rebase.rebaseMerges=false",
    "rebase.instructionFormat=%s",
    "maintenance.auto=false",
    "gc.auto=0",
]

// Tool-owned commits (materialized disk states) get a fixed identity and date, so the same tree always gives the same
// commit and never depends on the maintainer's Git identity.
const TOOL_IDENTITY = {
    GIT_AUTHOR_NAME: "Piton",
    GIT_AUTHOR_EMAIL: "piton@localhost",
    GIT_AUTHOR_DATE: "1970-01-01T00:00:00Z",
    GIT_COMMITTER_NAME: "Piton",
    GIT_COMMITTER_EMAIL: "piton@localhost",
    GIT_COMMITTER_DATE: "1970-01-01T00:00:00Z",
}

// Callers supply repository identities validated against actual configured PR evidence.
export function githubSshUrl(repository: Repository): string {
    return `git@github.com:${repository.owner}/${repository.repo}.git`
}

// Configuration/artifact readers also use Git. Reject relocation before invoking any of them,
// rather than temporarily mutating process.env in a parallel tool runtime.
export function assertGitEnvironment(): void {
    const found = inheritedGitRelocation()
    if (found.length)
        throw new Error(
            `Unset inherited Git relocation/configuration settings before continuing: ${found.join(", ")}. No checkout changes made.`,
        )
}

export interface GitResult {
    code: number
    stdout: string
    stderr: string
}

/**
 * How one Git call runs:
 * - `pathspecs`: `literal` for exact paths the maintainer approved (no globbing or magic), `magic` for the tool's own
 *   held-path pathspecs.
 * - `index`: a private index file the tool owns, instead of the checkout's index.
 * - `toolIdentity`: commit as the tool, for materialized disk states.
 * - `input`: standard input, for `update-index --index-info`.
 */
export interface GitOptions {
    pathspecs?: "literal" | "magic"
    index?: string
    toolIdentity?: boolean
    input?: string
    signal?: AbortSignal
}

function executeResult(
    directory: string,
    args: string[],
    extra: NodeJS.ProcessEnv,
    signal?: AbortSignal,
    input?: string,
): Promise<GitResult> {
    const env: NodeJS.ProcessEnv = {
        ...process.env,
        GIT_OPTIONAL_LOCKS: "0",
        GIT_TERMINAL_PROMPT: "0",
        GIT_EDITOR: ":",
        GIT_SEQUENCE_EDITOR: ":",
        ...extra,
    }
    delete env.GIT_EXTERNAL_DIFF
    return new Promise((resolve, reject) => {
        const child = cp.execFile(
            "git",
            args,
            { cwd: directory, env, encoding: "utf-8", maxBuffer: 64 * 1024 * 1024, signal },
            (error, stdout, stderr) => {
                if (!error) return resolve({ code: 0, stdout, stderr })
                const failed = error as cp.ExecFileException
                if (typeof failed.code !== "number") return reject(error)
                resolve({ code: failed.code, stdout: stdout ?? "", stderr: stderr ?? "" })
            },
        )
        // A command that reads no input can exit before the write lands; its result is what matters.
        child.stdin?.on("error", () => {})
        child.stdin?.end(input ?? "")
    })
}

export function checked(result: GitResult): string {
    if (result.code !== 0)
        throw Object.assign(
            new Error(result.stderr.trim() || result.stdout.trim() || `Git exited ${result.code}`),
            result,
        )
    return result.stdout
}

async function safeSettings(directory: string, signal?: AbortSignal): Promise<string[]> {
    const args = settings.flatMap((value) => ["-c", value])
    const keys = await executeResult(
        directory,
        [...args, "config", "--null", "--name-only", "--get-regexp", GIT_HELPER_CONFIG_PATTERN],
        {},
        signal,
    )
    if (keys.code !== 0 && keys.code !== 1) checked(keys)
    for (const key of keys.stdout.split("\0").filter(Boolean)) {
        if (key.startsWith("merge.") && !fs.existsSync(FAILING_MERGE_DRIVER))
            throw new Error(`Trusted failing merge driver ${FAILING_MERGE_DRIVER} is unavailable`)
        args.push("-c", gitHelperOverride(key, FAILING_MERGE_DRIVER))
    }
    return args
}

/**
 * Git in a checkout of the maintainer's repository (the current checkout, or the tool's scratch rebase worktree),
 * with hooks, fsmonitor, filters, external diff and textconv helpers, custom merge drivers, signing, editors, rerere,
 * maintenance, and submodule recursion disabled.
 */
export async function gitResult(directory: string, args: string[], options: GitOptions = {}): Promise<GitResult> {
    assertGitEnvironment()
    const mode = options.pathspecs ?? "literal"
    const prefix = mode === "literal" ? ["--literal-pathspecs"] : []
    const env: NodeJS.ProcessEnv = {
        ...(options.index ? { GIT_INDEX_FILE: options.index } : {}),
        ...(options.toolIdentity ? TOOL_IDENTITY : {}),
    }
    return executeResult(
        directory,
        [...prefix, ...(await safeSettings(directory, options.signal)), ...args],
        env,
        options.signal,
        options.input,
    )
}

export async function git(directory: string, args: string[], options: GitOptions = {}): Promise<string> {
    return checked(await gitResult(directory, args, options))
}

/** A Git call bound to one directory and abort signal. */
export type Git = (args: string[], options?: Omit<GitOptions, "signal">) => Promise<string>
export type GitWithResult = (args: string[], options?: Omit<GitOptions, "signal">) => Promise<GitResult>

export function bindGit(directory: string, signal?: AbortSignal): { git: Git; result: GitWithResult } {
    return {
        git: (args, options) => git(directory, args, { ...options, signal }),
        result: (args, options) => gitResult(directory, args, { ...options, signal }),
    }
}

/** `true` when Git exits 0, `false` when it exits 1; anything else throws. */
export async function quiet(run: GitWithResult, args: string[], options?: Omit<GitOptions, "signal">) {
    const result = await run(args, options)
    if (result.code === 0) return true
    if (result.code === 1) return false
    checked(result)
    return false
}

export async function headBranchRef(run: Git, ref: string): Promise<string> {
    const full = `refs/heads/${ref}`
    await run(["check-ref-format", full])
    return full
}
