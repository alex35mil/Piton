import * as fs from "node:fs"

import * as project from "../../../extensions/__lib/project.js"
import { readEntityStatus, resolveEntityDirectory } from "../__lib/entity.js"
import * as Git from "../__lib/git.js"
import { branchBase } from "../review/diff.js"

export type BeginUserReviewResult =
    | { state: "ready"; base: string; status: string }
    | { state: "uncommitted"; status: string }

export type EndUserReviewResult = { state: "ended"; branch: string; status: string } | { state: "notInReview" }

// Moving HEAD during one of these would leave the operation pointing at the wrong commit.
const OPERATIONS = ["MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD", "rebase-merge", "rebase-apply"]

function deliverableBranch(cwd: string, entityDir: string) {
    const root = project.resolveRootDir(cwd)
    const status = readEntityStatus(resolveEntityDirectory(root, cwd, entityDir))
    if (status.entity === "epic") throw new Error("User review is for a Task branch")
    for (const name of OPERATIONS)
        if (fs.existsSync(Git.run(root, ["rev-parse", "--path-format=absolute", "--git-path", name])))
            throw new Error(`A Git operation is in progress (${name}); finish it first. Nothing changed`)
    const base = branchBase(root, status.branch.target, `refs/heads/${status.branch.name}`)
    if (base.state === "missing") throw new Error(`No merge base with the target ${base.target}`)
    return { root, branch: status.branch.name, base: base.commit }
}

const attachedBranch = (root: string) => Git.tryRun(root, ["symbolic-ref", "--quiet", "--short", "HEAD"])
const shortStatus = (root: string) => Git.run(root, ["status", "--short"])

/**
 * Puts the branch diff in the worktree: HEAD detaches at the merge base with the target, so committed work shows as
 * staged. Only HEAD moves; the index and files stay as they are.
 */
export function beginUserReview(cwd: string, entityDir: string): BeginUserReviewResult {
    const { root, branch, base } = deliverableBranch(cwd, entityDir)
    const attached = attachedBranch(root)
    const head = Git.run(root, ["rev-parse", "HEAD"])
    if (!attached && head === base) return { state: "ready", base, status: shortStatus(root) }
    if (attached !== branch) throw new Error(`User review starts on the branch ${branch}. Nothing changed`)
    if (head === base) return { state: "uncommitted", status: shortStatus(root) }
    Git.run(root, ["update-ref", "--no-deref", "-m", "piton: user review", "HEAD", base])
    return { state: "ready", base, status: shortStatus(root) }
}

/** Returns HEAD to the branch. Only HEAD moves, so edits made during the review stay as uncommitted changes. */
export function endUserReview(cwd: string, entityDir: string): EndUserReviewResult {
    const { root, branch, base } = deliverableBranch(cwd, entityDir)
    const attached = attachedBranch(root)
    if (attached === branch) return { state: "notInReview" }
    if (attached || Git.run(root, ["rev-parse", "HEAD"]) !== base)
        throw new Error(
            `HEAD isn't at the review base ${base} (a commit during the review, or another checkout state); reattaching would drop it. Nothing changed; ask the user`,
        )
    Git.run(root, ["symbolic-ref", "-m", "piton: user review ended", "HEAD", `refs/heads/${branch}`])
    return { state: "ended", branch, status: shortStatus(root) }
}
