import * as cp from "node:child_process"

import * as Git from "../__lib/git.js"
import { fence, truncate } from "./context.js"

/** The largest diff a reviewer prompt embeds; the reviewer reads the rest with `git_read`. */
export const MAX_DIFF_BYTES = 180 * 1024

/** Where the branch's own changes start: the merge base with the target, remote-tracking when it exists. */
type BranchBase = { state: "found"; ref: string; commit: string } | { state: "missing"; target: string }

function branchBase(cwd: string, target: string): BranchBase {
    for (const ref of [`refs/remotes/origin/${target}`, `refs/heads/${target}`]) {
        if (!Git.succeeds(cwd, ["rev-parse", "--verify", "--quiet", `${ref}^{commit}`])) continue
        const commit = Git.tryRun(cwd, ["merge-base", ref, "HEAD"])
        if (commit) return { state: "found", ref, commit }
    }
    return { state: "missing", target }
}

/**
 * Everything the branch changes: the diff from the merge base with the target to the working tree,
 * so locally committed steps are reviewed with uncommitted work. `git status` names untracked files.
 */
export async function branchDiff(cwd: string, target: string): Promise<string> {
    const base = branchBase(cwd, target)
    switch (base.state) {
        case "found":
            return workingTreeDiff(
                cwd,
                base.commit,
                `Changes from the merge base with ${base.ref} (${base.commit}) to the working tree: committed, staged, and unstaged.`,
            )
        case "missing":
            return workingTreeDiff(
                cwd,
                "HEAD",
                `No merge base with target ${base.target} was found, so this shows only uncommitted changes against HEAD. Use git_read to inspect the branch's commits.`,
            )
    }
}

/** The diff from `from` to the working tree, with `git status` for untracked files, truncated for a prompt. */
export async function workingTreeDiff(cwd: string, from: string, heading: string): Promise<string> {
    const status = await git(cwd, ["status", "--short"])
    const stat = await git(cwd, ["diff", "--stat", from])
    const diff = await git(cwd, ["diff", from])
    return truncate(
        [
            heading,
            "## git status --short",
            fence(status || "(clean)", "text"),
            `## git diff --stat ${from}`,
            fence(stat || "(empty)", "text"),
            `## git diff ${from}`,
            fence(diff || "(empty)", "diff"),
        ].join("\n\n"),
        MAX_DIFF_BYTES,
    )
}

/** The changes between two commits, truncated for a prompt. */
export async function commitRangeDiff(cwd: string, from: string, to: string, heading: string): Promise<string> {
    const log = await git(cwd, ["log", "--oneline", `${from}..${to}`])
    const stat = await git(cwd, ["diff", "--stat", from, to])
    const diff = await git(cwd, ["diff", from, to])
    return truncate(
        [
            heading,
            `## git log --oneline ${from}..${to}`,
            fence(log || "(no commits)", "text"),
            `## git diff --stat ${from} ${to}`,
            fence(stat || "(empty)", "text"),
            `## git diff ${from} ${to}`,
            fence(diff || "(empty)", "diff"),
        ].join("\n\n"),
        MAX_DIFF_BYTES,
    )
}

/** One commit with its message and changes, truncated for a prompt. */
export async function commitDiff(cwd: string, commit: string): Promise<string> {
    const show = await git(cwd, ["show", "--stat", "--patch", "--format=fuller", commit])
    return truncate([`## git show ${commit}`, fence(show, "diff")].join("\n\n"), MAX_DIFF_BYTES)
}

async function git(cwd: string, args: string[]): Promise<string> {
    return new Promise((resolve) => {
        cp.execFile("git", args, { cwd, maxBuffer: 16 * 1024 * 1024 }, (error, stdout, stderr) => {
            if (error) resolve((stdout || stderr || String(error)).trim())
            else resolve(stdout.trim())
        })
    })
}
