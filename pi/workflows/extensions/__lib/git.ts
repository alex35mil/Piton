import * as cp from "node:child_process"

import { runProgram, type Program, type RunOptions, type RunOutcome } from "../../../extensions/__lib/run.js"

/** Runs a program from pi's process: piton's daemon decides where in a sandboxed session (`runProgram`). */
export type ProgramRunner = (program: Program, options: RunOptions) => Promise<RunOutcome>

/** What happened to a deleted branch's `[branch "<name>"]` settings in the repository's configuration. */
export type BranchSettings =
    /** It had none: nothing was asked. */
    | { kind: "none" }
    /** They were removed, with Git's control files open. */
    | { kind: "removed" }
    /** They're still there: the removal was declined or failed. The branch itself is deleted. */
    | { kind: "kept"; reason: string }

/**
 * Deletes the local branch `branch` in `root`, which must still be at `commit` and checked out in no worktree, then
 * its settings. The ref goes with `git update-ref -d` in pi's own process (its sandbox, in a sandboxed session), which
 * never asks and refuses when the branch has moved. The settings live in `.git/config`, which pi's sandbox keeps
 * closed, so only when the branch has some does the daemon remove them with Git's control files open, after asking the
 * user (or as a saved entry says). Throws when the branch can't be deleted; once it resolves, the branch is gone.
 */
export async function deleteBranch(
    root: string,
    branch: string,
    commit: string,
    reason: string,
    runner: ProgramRunner = runProgram,
): Promise<BranchSettings> {
    const checkedOut = run(root, ["worktree", "list", "--porcelain"])
        .split(/\r?\n/)
        .includes(`branch refs/heads/${branch}`)
    if (checkedOut) throw new Error(`branch ${branch} is checked out in a worktree`)
    run(root, ["update-ref", "-d", `refs/heads/${branch}`, commit])
    if (!hasBranchSettings(root, branch)) return { kind: "none" }
    const removed = await runner(
        { kind: "exec", argv: ["git", "config", "--local", "--remove-section", `branch.${branch}`] },
        { cwd: root, where: { kind: "widened", grants: { allow_git_control_files: true } }, reason },
    )
    const failure = runFailure(removed)
    return failure === undefined ? { kind: "removed" } : { kind: "kept", reason: failure }
}

/** Whether the repository's own configuration has settings for `branch` (`branch.<name>.<key>`). */
function hasBranchSettings(root: string, branch: string): boolean {
    const result = cp.spawnSync("git", ["config", "--local", "--name-only", "--get-regexp", "^branch\\."], {
        cwd: root,
        encoding: "utf-8",
    })
    // `--get-regexp` exits 1 when nothing matches.
    if (result.status === 1) return false
    if (result.status !== 0)
        throw new Error(`git config --get-regexp failed: ${(result.stderr || result.stdout).trim()}`)
    // A key is `branch.<name>.<key>`; the name may hold dots, the key never does.
    return result.stdout
        .split(/\r?\n/)
        .some(
            (key) =>
                key.lastIndexOf(".") > "branch.".length && key.slice("branch.".length, key.lastIndexOf(".")) === branch,
        )
}

/** Why a run didn't succeed, for an error message; `undefined` when it exited 0. */
export function runFailure(outcome: RunOutcome): string | undefined {
    switch (outcome.kind) {
        case "exited":
            return outcome.code === 0 ? undefined : outcome.output.trim() || `exited with code ${outcome.code}`
        case "timedOut":
            return "timed out"
        case "notRun":
            return `not run: ${outcome.reason}`
    }
}

// Inherited variables that relocate Git's repository, index, or configuration away from the working directory.
export const GIT_RELOCATION_VARIABLES = [
    "GIT_DIR",
    "GIT_COMMON_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
] as const

export function inheritedGitRelocation(env: NodeJS.ProcessEnv = process.env): string[] {
    return GIT_RELOCATION_VARIABLES.filter((key) => env[key] !== undefined)
}

// A trusted program that always fails, substituted for configured helpers that must not run.
export const FAILING_PROGRAM = "/usr/bin/false"

// Repository configuration that runs programs during content conversion or merging.
export const GIT_HELPER_CONFIG_PATTERN = "^(filter\\..*\\.(clean|smudge|process|required)|merge\\..*\\.driver)$"

/**
 * The `-c` override that disables one configured helper key found by `GIT_HELPER_CONFIG_PATTERN`.
 * A disabled custom merge driver must report a conflict rather than silently accept ours, so it
 * becomes `failingDriver`.
 */
export function gitHelperOverride(key: string, failingDriver: string): string {
    const value = key.startsWith("merge.") ? failingDriver : key.endsWith(".required") ? "false" : ""
    return `${key}=${value}`
}

export function run(cwd: string, args: string[]): string {
    const result = cp.spawnSync("git", args, { cwd, encoding: "utf-8" })
    if (result.status !== 0) {
        throw new Error(`git ${args.join(" ")} failed: ${(result.stderr || result.stdout).trim()}`)
    }
    return result.stdout.trim()
}

export function tryRun(cwd: string, args: string[]): string {
    const result = cp.spawnSync("git", args, { cwd, encoding: "utf-8" })
    return result.status === 0 ? result.stdout.trim() : ""
}

export function succeeds(cwd: string, args: string[]): boolean {
    return cp.spawnSync("git", args, { cwd, encoding: "utf-8" }).status === 0
}
