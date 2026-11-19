import "./host-runtime.ts"
import { writeProjectConfig } from "./config-fixture.ts"

import assert from "node:assert/strict"
import * as cp from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { describe, it } from "node:test"

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent"

import { phaseValues } from "./workflow-phase-fixtures.ts"
import { runDeliveryCleanup } from "../workflows/extensions/deliverable/cleanup.ts"
import registerDeliverable, { githubMergeProof } from "../workflows/extensions/deliverable/index.ts"
import {
    prepareMergeResolutionComment,
    renderMergeResolutionComment,
} from "../workflows/extensions/deliverable/merge-resolution.ts"
import { beginUserReview, endUserReview } from "../workflows/extensions/deliverable/user-review.ts"
import { fixtureToolName, workflowMcpCatalog } from "./mcp-catalog-fixture.ts"
import { prepareArtifactPersistence } from "../workflows/extensions/__lib/project-config.ts"
import { initializeEntity } from "../workflows/extensions/init/entity.ts"

const repo = { owner: "example", repo: "project" }
const statuses = {
    backlog: "Backlog",
    todo: "Todo",
    inProgress: "In Progress",
    inReview: "In Review",
    done: "Done",
    canceled: "Canceled",
}

function git(cwd: string, args: string[]): string {
    return cp.execFileSync("git", args, { cwd, encoding: "utf-8" }).trim()
}

function repository(withOrigin = true) {
    const parent = fs.mkdtempSync(path.join(os.tmpdir(), "workflow-cleanup-"))
    const root = path.join(parent, "work")
    const origin = path.join(parent, "origin.git")
    fs.mkdirSync(root)
    if (withOrigin) git(parent, ["init", "--bare", origin])
    git(root, ["init", "-b", "main"])
    git(root, ["config", "user.email", "test@example.com"])
    git(root, ["config", "user.name", "Test"])
    fs.writeFileSync(path.join(root, "README.md"), "base\n")
    fs.writeFileSync(path.join(root, ".gitignore"), "/.piton/.local\n")
    git(root, ["add", "README.md", ".gitignore"])
    git(root, ["commit", "-m", "initial"])
    if (withOrigin) {
        git(root, ["remote", "add", "origin", origin])
        git(root, ["push", "-u", "origin", "main"])
    }
    fs.mkdirSync(path.join(root, ".piton"), { recursive: true })
    writeProjectConfig(root, {
        branchFormat: "identifier-title",
        tracker: {
            provider: "github",
            mcpServer: "github",
            repository: repo,
            project: { owner: "example", ownerType: "user", number: 3 },
            fields: {
                status: { field: "Status", values: statuses },
                phase: { field: "Phase", values: phaseValues },
                priority: { scope: "project", field: "Priority", values: ["High"] },
            },
        },
        forge: { provider: "github", mcpServer: "github", repository: repo },
    })
    prepareArtifactPersistence(root)
    return { parent, root, origin }
}

function issue(number: number) {
    return {
        issueId: 1000 + number,
        issueNumber: number,
        issueUrl: `https://github.com/example/project/issues/${number}`,
    }
}

// Records PR intent and the created PR #7 the way submission does.
function recordPullRequest(root: string, directory: string) {
    const metadataPath = path.join(root, directory, "metadata.json")
    const metadata = JSON.parse(fs.readFileSync(metadataPath, "utf-8"))
    metadata.phase = "selfReview"
    metadata.forge = {
        provider: "github",
        repository: repo,
        head: metadata.branch.name,
        target: metadata.branch.target,
    }
    fs.writeFileSync(metadataPath, `${JSON.stringify(metadata, null, 2)}\n`)
    const entity = path.basename(directory)
    fs.writeFileSync(
        path.join(root, directory, "resource-ids.json"),
        JSON.stringify({
            entity,
            resourceIds: [
                {
                    scope: {
                        entity,
                        kind: "github-pull-request",
                        mcpServer: "github",
                        repository: repo,
                        head: metadata.branch.name,
                        target: metadata.branch.target,
                    },
                    value: { number: 7, url: "https://github.com/example/project/pull/7" },
                },
            ],
        }),
    )
    return { directory, branch: metadata.branch.name as string, target: metadata.branch.target as string }
}

function submittedGig(root: string, withPullRequest = true) {
    const gig = initializeEntity(
        {
            entity: "gig",
            title: "Cleanup",
            slug: "cleanup",
            request: "Clean the merged branch",
            kind: "chore",
            object: issue(5),
        },
        { cwd: root },
    )
    if (withPullRequest) return recordPullRequest(root, gig.directory)
    return { directory: gig.directory, branch: gig.status.branch.name, target: gig.status.branch.target }
}

function commitBranch(root: string, branch: string, push = true) {
    git(root, ["switch", branch])
    fs.writeFileSync(path.join(root, "result.txt"), "reviewed result\n")
    git(root, ["add", "result.txt"])
    git(root, ["commit", "-m", "reviewed result"])
    if (push) git(root, ["push", "-u", "origin", branch])
}

function mergeProof(gig: { branch: string; target: string }, headCommitSha: string) {
    return {
        kind: "github-pr" as const,
        source: "pull_request_read:get" as const,
        repository: repo,
        pullRequestNumber: 7,
        merged: true as const,
        mergedAt: "2026-01-02T04:00:00Z",
        head: gig.branch,
        headCommitSha,
        base: gig.target,
    }
}

function squashMerge(root: string, gig: { branch: string; target: string }): string {
    const headCommitSha = git(root, ["rev-parse", gig.branch])
    git(root, ["switch", gig.target])
    git(root, ["merge", "--squash", gig.branch])
    git(root, ["commit", "--allow-empty", "-m", "squash reviewed result"])
    git(root, ["push", "origin", gig.target])
    git(root, ["switch", gig.branch])
    return headCommitSha
}

function cloneBranchExists(root: string, branch: string): boolean {
    return cp.spawnSync("git", ["show-ref", "--verify", `refs/heads/${branch}`], { cwd: root }).status === 0
}

describe("Deliverable branch cleanup", () => {
    it("registers the cleanup, merge-resolution, and user-review tools under the Deliverable extension", () => {
        const tools: string[] = []
        registerDeliverable({
            on() {},
            events: { on: () => () => {} },
            registerTool: (tool: { name: string }) => tools.push(tool.name),
        } as unknown as ExtensionAPI)
        assert.deepEqual(tools, [
            "cleanup_delivery_branch",
            "prepare_merge_resolution_comment",
            "begin_user_review",
            "end_user_review",
        ])
    })

    it("cleans a squash-merged branch in the current clone from verified PR evidence", () => {
        const { parent, root } = repository()
        try {
            const gig = submittedGig(root)
            commitBranch(root, gig.branch)
            const headCommitSha = squashMerge(root, gig)
            fs.writeFileSync(path.join(root, "outside-git-notes.txt"), "keep me\n")
            fs.appendFileSync(path.join(root, "README.md"), "working-tree note\n")

            const input = { entityDir: gig.directory, merge: mergeProof(gig, headCommitSha) }
            const wrong: Array<[Record<string, unknown>, RegExp]> = [
                [{ headCommitSha: "0".repeat(40) }, /head commit does not match/],
                [{ repository: { owner: "other", repo: "project" } }, /repository does not match/],
                [{ base: "other" }, /head\/base does not match/],
                [{ head: "other" }, /head\/base does not match/],
                [{ pullRequestNumber: 8 }, /locally recorded PR/],
            ]
            for (const [overrides, error] of wrong)
                assert.throws(
                    () =>
                        runDeliveryCleanup(
                            { ...input, merge: { ...input.merge, ...overrides } as never },
                            { cwd: root },
                        ),
                    error,
                )
            assert.equal(cloneBranchExists(root, gig.branch), true)
            const applied = runDeliveryCleanup(input, { cwd: root })

            assert.equal(applied.entity, "5-cleanup")
            assert.equal(applied.targetCommit, git(root, ["rev-parse", "FETCH_HEAD"]))
            assert.equal(applied.deletedBranch, gig.branch)
            assert.equal(git(root, ["branch", "--show-current"]), gig.target)
            assert.equal(cloneBranchExists(root, gig.branch), false)
            assert.equal(git(root, ["ls-remote", "--heads", "origin", gig.branch]).includes(gig.branch), true)
            assert.equal(fs.existsSync(path.join(root, gig.directory, "resource-ids.json")), true)
            assert.equal(fs.readFileSync(path.join(root, "outside-git-notes.txt"), "utf-8"), "keep me\n")
            assert.match(fs.readFileSync(path.join(root, "README.md"), "utf-8"), /working-tree note/)
        } finally {
            fs.rmSync(parent, { recursive: true, force: true })
        }
    })

    it("cleans a Deliverable whose only commit is empty after its squash merge", () => {
        const { parent, root } = repository()
        try {
            const gig = submittedGig(root)
            git(root, ["switch", gig.branch])
            git(root, ["commit", "--allow-empty", "-m", "report-only delivery"])
            git(root, ["push", "-u", "origin", gig.branch])
            const headCommitSha = squashMerge(root, gig)
            const applied = runDeliveryCleanup(
                { entityDir: gig.directory, merge: mergeProof(gig, headCommitSha) },
                { cwd: root },
            )
            assert.equal(applied.deletedBranch, gig.branch)
            assert.equal(cloneBranchExists(root, gig.branch), false)
        } finally {
            fs.rmSync(parent, { recursive: true, force: true })
        }
    })

    it("requires recorded PR intent and IDs before cleanup", () => {
        const { parent, root } = repository()
        try {
            const gig = submittedGig(root, false)
            commitBranch(root, gig.branch)
            const sha = git(root, ["rev-parse", gig.branch])
            assert.throws(
                () => runDeliveryCleanup({ entityDir: gig.directory, merge: mergeProof(gig, sha) }, { cwd: root }),
                /stored PR intent/,
            )
            assert.equal(cloneBranchExists(root, gig.branch), true)
        } finally {
            fs.rmSync(parent, { recursive: true, force: true })
        }
    })

    it("derives cleanup proof from exact PR-get evidence for the recorded PR", () => {
        const { parent, root } = repository()
        try {
            const gig = submittedGig(root)
            commitBranch(root, gig.branch)
            const sha = git(root, ["rev-parse", gig.branch])
            const catalog = workflowMcpCatalog("github")
            const args = { method: "get", owner: "example", repo: "project", pullNumber: 7 }
            const pr = {
                number: 7,
                html_url: "https://github.com/example/project/pull/7",
                state: "closed",
                draft: false,
                merged: true,
                merged_at: "2026-01-02T04:00:00Z",
                base: { ref: gig.target, repo: { full_name: "example/project" } },
                head: { ref: gig.branch, sha, repo: { full_name: "example/project" } },
            }
            const evidence = (response: unknown, overrides: Record<string, unknown> = {}) => ({
                entityDir: gig.directory,
                evidence: {
                    tool: fixtureToolName("github", "pull_request_read"),
                    arguments: { ...args, ...overrides },
                    response,
                },
            })
            assert.deepEqual(githubMergeProof(root, evidence(pr), catalog), mergeProof(gig, sha))
            assert.throws(
                () => githubMergeProof(root, evidence({ ...pr, merged: false, state: "open" }), catalog),
                /does not show a merged PR/,
            )
            assert.throws(
                () =>
                    githubMergeProof(
                        root,
                        evidence(
                            { ...pr, number: 8, html_url: "https://github.com/example/project/pull/8" },
                            { pullNumber: 8 },
                        ),
                        catalog,
                    ),
                /locally recorded PR/,
            )
            assert.throws(
                () => githubMergeProof(root, evidence({ ...pr, head: { ...pr.head, sha: "0".repeat(40) } }), catalog),
                /expected head/,
            )
            assert.throws(
                () =>
                    githubMergeProof(
                        root,
                        {
                            ...evidence(pr),
                            evidence: { ...evidence(pr).evidence, tool: "mcp__other__pull_request_read" },
                        },
                        catalog,
                    ),
                /merge evidence rejected/,
            )
        } finally {
            fs.rmSync(parent, { recursive: true, force: true })
        }
    })

    it("preserves a merged branch when origin is unavailable", () => {
        const { parent, root } = repository(false)
        try {
            const gig = submittedGig(root)
            commitBranch(root, gig.branch, false)
            const sha = git(root, ["rev-parse", gig.branch])
            assert.throws(
                () => runDeliveryCleanup({ entityDir: gig.directory, merge: mergeProof(gig, sha) }, { cwd: root }),
                /origin/,
            )
            assert.equal(git(root, ["branch", "--show-current"]), gig.branch)
            assert.equal(cloneBranchExists(root, gig.branch), true)
        } finally {
            fs.rmSync(parent, { recursive: true, force: true })
        }
    })

    it("keeps Epic branches permanent", () => {
        const { parent, root } = repository()
        try {
            const epic = initializeEntity(
                {
                    entity: "epic",
                    title: "Permanent branch",
                    slug: "permanent-branch",
                    request: "Keep it",
                    object: issue(9),
                },
                { cwd: root },
            )
            assert.throws(
                () =>
                    runDeliveryCleanup(
                        {
                            entityDir: epic.directory,
                            merge: mergeProof({ branch: epic.status.branch.name, target: "main" }, "0".repeat(40)),
                        },
                        { cwd: root },
                    ),
                /Epic branches are permanent/,
            )
        } finally {
            fs.rmSync(parent, { recursive: true, force: true })
        }
    })

    it("preserves the branch when the local target is ahead or diverged", () => {
        for (const diverged of [false, true]) {
            const { parent, root } = repository()
            try {
                const gig = submittedGig(root)
                commitBranch(root, gig.branch)
                const sha = squashMerge(root, gig)
                git(root, ["switch", gig.target])
                fs.writeFileSync(path.join(root, "unpublished.txt"), "local work\n")
                git(root, ["add", "unpublished.txt"])
                git(root, ["commit", "-m", "unpublished target work"])
                const localTarget = git(root, ["rev-parse", gig.target])
                if (diverged) {
                    const other = path.join(parent, "other")
                    git(parent, ["clone", "-b", "main", path.join(parent, "origin.git"), other])
                    git(other, ["config", "user.name", "Other"])
                    git(other, ["config", "user.email", "other@example.com"])
                    git(other, ["commit", "--allow-empty", "-m", "another actor advanced origin"])
                    git(other, ["push", "origin", "main"])
                }
                git(root, ["switch", gig.branch])
                assert.throws(
                    () => runDeliveryCleanup({ entityDir: gig.directory, merge: mergeProof(gig, sha) }, { cwd: root }),
                    diverged ? /git merge.*failed/ : /does not match freshly fetched origin/,
                )
                assert.equal(cloneBranchExists(root, gig.branch), true)
                assert.equal(git(root, ["rev-parse", gig.target]), localTarget)
            } finally {
                fs.rmSync(parent, { recursive: true, force: true })
            }
        }
    })

    it("cleans up an Epic-target Task on the synchronized Epic branch and leaves main untouched", () => {
        const { parent, root } = repository()
        try {
            const mainCommit = git(root, ["rev-parse", "main"])
            const epic = initializeEntity(
                { entity: "epic", title: "Umbrella", slug: "umbrella", request: "Deliver Tasks", object: issue(10) },
                { cwd: root },
            )
            const epicBranch = epic.status.branch.name
            fs.writeFileSync(
                path.join(root, epic.directory, "epic.md"),
                `# Epic\n\n## Task delivery strategy\n\nTask target: epic\nEpic branch: ${epicBranch}\n\n## Tasks\n\n### [#11] Child\n\nKind: feature\nSummary: Implement it.\n`,
            )
            git(root, ["push", "origin", epicBranch])
            const task = initializeEntity(
                {
                    entity: "task",
                    title: "Child",
                    slug: "child",
                    request: "Deliver the child",
                    parentEpicDir: epic.directory,
                    object: issue(11),
                },
                { cwd: root },
            )
            const recorded = recordPullRequest(root, task.directory)
            commitBranch(root, recorded.branch)
            const sha = squashMerge(root, recorded)
            const result = runDeliveryCleanup(
                { entityDir: task.directory, merge: mergeProof(recorded, sha) },
                { cwd: root },
            )
            assert.equal(result.target, epicBranch)
            assert.equal(git(root, ["branch", "--show-current"]), epicBranch)
            assert.equal(git(root, ["rev-parse", "main"]), mainCommit)
            assert.equal(cloneBranchExists(root, epicBranch), true)
            assert.equal(cloneBranchExists(root, recorded.branch), false)
        } finally {
            fs.rmSync(parent, { recursive: true, force: true })
        }
    })

    it("accepts rebase delivery evidence without requiring source-commit ancestry", () => {
        const { parent, root } = repository()
        try {
            const gig = submittedGig(root)
            commitBranch(root, gig.branch)
            const headCommitSha = git(root, ["rev-parse", gig.branch])
            git(root, ["switch", gig.target])
            git(root, ["commit", "--allow-empty", "-m", "advance target before rebase"])
            git(root, ["cherry-pick", headCommitSha])
            assert.notEqual(git(root, ["rev-parse", "HEAD"]), headCommitSha)
            git(root, ["push", "origin", gig.target])
            git(root, ["switch", gig.branch])
            const input = { entityDir: gig.directory, merge: mergeProof(gig, headCommitSha) }
            for (const missingFacts of [
                { merged: false },
                { merged: undefined },
                { mergedAt: "" },
                { mergedAt: undefined },
            ]) {
                assert.throws(() =>
                    runDeliveryCleanup(
                        { ...input, merge: { ...input.merge, ...missingFacts } as never },
                        { cwd: root },
                    ),
                )
                assert.equal(cloneBranchExists(root, gig.branch), true)
            }
            runDeliveryCleanup(input, { cwd: root })
            assert.equal(cloneBranchExists(root, gig.branch), false)
            assert.equal(git(root, ["branch", "--show-current"]), gig.target)
        } finally {
            fs.rmSync(parent, { recursive: true, force: true })
        }
    })

    it("preserves the branch and an outside-Git file when switching would overwrite it", () => {
        const { parent, root } = repository()
        try {
            const gig = submittedGig(root)
            commitBranch(root, gig.branch)
            const sha = squashMerge(root, gig)
            git(root, ["switch", gig.target])
            fs.writeFileSync(path.join(root, "target-only.txt"), "remote target\n")
            git(root, ["add", "target-only.txt"])
            git(root, ["commit", "-m", "target update"])
            git(root, ["push", "origin", gig.target])
            git(root, ["reset", "--hard", "HEAD~1"])
            git(root, ["switch", gig.branch])
            fs.writeFileSync(path.join(root, "target-only.txt"), "outside Git\n")
            assert.throws(
                () => runDeliveryCleanup({ entityDir: gig.directory, merge: mergeProof(gig, sha) }, { cwd: root }),
                /git (switch|merge)/,
            )
            assert.equal(cloneBranchExists(root, gig.branch), true)
            assert.equal(fs.readFileSync(path.join(root, "target-only.txt"), "utf-8"), "outside Git\n")
        } finally {
            fs.rmSync(parent, { recursive: true, force: true })
        }
    })
})

describe("Deliverable merge-resolution comment", () => {
    const catalog = workflowMcpCatalog("github")
    const summary = "`README.md` conflicted; kept both lines, branch first."

    // A submitted Gig whose branch and target both changed the same README line.
    function conflicted() {
        const fixture = repository()
        const { root } = fixture
        const gig = submittedGig(root)
        git(root, ["switch", gig.branch])
        fs.writeFileSync(path.join(root, "README.md"), "branch\n")
        git(root, ["commit", "-am", "branch change"])
        git(root, ["push", "-u", "origin", gig.branch])
        git(root, ["switch", gig.target])
        fs.writeFileSync(path.join(root, "README.md"), "target\n")
        git(root, ["commit", "-am", "target change"])
        git(root, ["push", "origin", gig.target])
        git(root, ["switch", gig.branch])
        git(root, ["fetch", "origin"])
        return { ...fixture, gig }
    }

    function resolve(root: string, content: string) {
        cp.spawnSync("git", ["merge", "--no-ff", "--no-commit", "origin/main"], { cwd: root })
        fs.writeFileSync(path.join(root, "README.md"), content)
        git(root, ["add", "README.md"])
        git(root, ["commit", "--no-edit"])
        return git(root, ["rev-parse", "HEAD"])
    }

    it("renders the pushed resolution with the summary, folded remerge-diff, and marker", () => {
        const { parent, root, gig } = conflicted()
        try {
            const prepare = () => prepareMergeResolutionComment(root, { entityDir: gig.directory, summary }, catalog)
            assert.throws(prepare, /doesn't end in a merge commit/)
            const merge = resolve(root, "branch\ntarget\n")
            assert.throws(prepare, /push it before recording/)
            git(root, ["push", "origin", gig.branch])
            const prepared = prepare()
            assert.equal(prepared.merge, merge)
            assert.equal(prepared.call.tool, fixtureToolName("github", "add_issue_comment"))
            const { body, ...address } = prepared.call.arguments
            assert.deepEqual(address, { ...repo, issue_number: 7 })
            assert.ok(body.startsWith("**Merge conflict resolved**"))
            assert.ok(body.includes(summary))
            assert.ok(body.includes(`\`${merge.slice(0, 12)}\``))
            assert.match(body, /<details>\n<summary>Conflict resolution<\/summary>\n\n```diff\n[\s\S]*^-<<<<<<< /m)
            assert.match(body, /^->>>>>>> /m)
            assert.ok(body.endsWith(`<!-- merge-resolution:${merge} -->`))
        } finally {
            fs.rmSync(parent, { recursive: true, force: true })
        }
    })

    it("refuses a merge without conflicts, a merge of another branch, and a missing PR record", () => {
        const { parent, root, gig } = conflicted()
        try {
            const prepare = (directory = gig.directory) =>
                prepareMergeResolutionComment(root, { entityDir: directory, summary }, catalog)
            // Git's own resolution leaves nothing to record.
            git(root, ["switch", "-c", "side", "origin/main~1"])
            fs.writeFileSync(path.join(root, "side.txt"), "side\n")
            git(root, ["add", "side.txt"])
            git(root, ["commit", "-m", "side"])
            git(root, ["switch", gig.branch])
            git(root, ["merge", "--no-ff", "--no-edit", "side"])
            git(root, ["push", "origin", gig.branch])
            assert.throws(prepare, /isn't on refs\/remotes\/origin\/main/)
            git(root, ["push", "origin", "side:main", "--force"])
            git(root, ["fetch", "origin"])
            assert.throws(prepare, /resolved no conflicts/)

            const unsubmitted = submittedGigWithoutPr(root)
            assert.throws(() => prepare(unsubmitted), /forge intent|recorded PR/)
        } finally {
            fs.rmSync(parent, { recursive: true, force: true })
        }
    })

    it("leaves out an oversized diff and names the local command instead", () => {
        const merge = "a".repeat(40)
        const body = renderMergeResolutionComment({
            summary,
            target: "main",
            targetSha: "b".repeat(40),
            merge,
            remergeDiff: "+x".repeat(40_000),
        })
        assert.doesNotMatch(body, /```diff/)
        assert.ok(body.includes(`git show --remerge-diff ${merge}`))
        assert.ok(body.endsWith(`<!-- merge-resolution:${merge} -->`))
        assert.match(
            renderMergeResolutionComment({ summary, target: "main", targetSha: merge, merge, remergeDiff: "+```" }),
            /````diff/,
        )
    })
})

// A second Gig in the same repository with no PR intent recorded.
function submittedGigWithoutPr(root: string): string {
    return initializeEntity(
        {
            entity: "gig",
            title: "Other",
            slug: "other",
            request: "Other work",
            kind: "chore",
            object: issue(6),
        },
        { cwd: root },
    ).directory
}

describe("Deliverable user review", () => {
    // A Gig branch one commit ahead of main, which moved on after the branch started.
    function reviewing() {
        const { parent, root } = repository()
        const gig = submittedGig(root, false)
        const base = git(root, ["rev-parse", "main"])
        git(root, ["switch", gig.branch])
        fs.writeFileSync(path.join(root, "step.txt"), "step\n")
        git(root, ["add", "step.txt"])
        git(root, ["commit", "-m", "step"])
        const head = git(root, ["rev-parse", "HEAD"])
        git(root, ["switch", "main"])
        fs.writeFileSync(path.join(root, "upstream.txt"), "upstream\n")
        git(root, ["add", "upstream.txt"])
        git(root, ["commit", "-m", "upstream"])
        git(root, ["push", "origin", "main"])
        git(root, ["switch", gig.branch])
        return { root, gig, base, head, close: () => fs.rmSync(parent, { recursive: true, force: true }) }
    }
    const attached = (root: string) =>
        cp.spawnSync("git", ["symbolic-ref", "-q", "HEAD"], { cwd: root }).stdout.toString().trim()

    it("stages the branch diff over the merge base, keeps edits, and returns to the branch", () => {
        const f = reviewing()
        try {
            fs.appendFileSync(path.join(f.root, "README.md"), "uncommitted\n")
            assert.equal(beginUserReview(f.root, f.gig.directory).state, "ready")
            assert.equal(git(f.root, ["rev-parse", "HEAD"]), f.base)
            assert.equal(attached(f.root), "")
            assert.equal(git(f.root, ["diff", "--cached", "--name-only"]), "step.txt")
            assert.equal(fs.existsSync(path.join(f.root, "upstream.txt")), false)
            assert.equal(beginUserReview(f.root, f.gig.directory).state, "ready", "Repeated begin is a no-op")
            fs.appendFileSync(path.join(f.root, "step.txt"), "review edit\n")

            assert.equal(endUserReview(f.root, f.gig.directory).state, "ended")
            assert.equal(attached(f.root), `refs/heads/${f.gig.branch}`)
            assert.equal(git(f.root, ["rev-parse", "HEAD"]), f.head)
            assert.equal(git(f.root, ["diff", "--name-only"]), "README.md\nstep.txt")
            assert.equal(git(f.root, ["diff", "--cached", "--name-only"]), "")
            assert.deepEqual(endUserReview(f.root, f.gig.directory), { state: "notInReview" })
        } finally {
            f.close()
        }
    })

    it("refuses to end after a commit during the review, changing nothing", () => {
        const f = reviewing()
        try {
            beginUserReview(f.root, f.gig.directory)
            git(f.root, ["commit", "-qm", "commit during review"])
            const moved = git(f.root, ["rev-parse", "HEAD"])
            assert.throws(() => endUserReview(f.root, f.gig.directory), /review base/)
            assert.equal(git(f.root, ["rev-parse", "HEAD"]), moved)
        } finally {
            f.close()
        }
    })

    it("refuses to begin off the branch or during a Git operation", () => {
        const f = reviewing()
        try {
            git(f.root, ["switch", "main"])
            assert.throws(() => beginUserReview(f.root, f.gig.directory), /starts on the branch/)
            git(f.root, ["switch", f.gig.branch])
            fs.writeFileSync(path.join(f.root, ".git", "MERGE_HEAD"), `${f.base}\n`)
            assert.throws(() => beginUserReview(f.root, f.gig.directory), /MERGE_HEAD/)
            assert.equal(git(f.root, ["rev-parse", "HEAD"]), f.head)
        } finally {
            f.close()
        }
    })

    it("changes nothing when the branch has no commits ahead", () => {
        const { parent, root } = repository()
        try {
            const gig = submittedGig(root, false)
            git(root, ["switch", gig.branch])
            const head = git(root, ["rev-parse", "HEAD"])
            assert.equal(beginUserReview(root, gig.directory).state, "uncommitted")
            assert.equal(attached(root), `refs/heads/${gig.branch}`)
            assert.equal(git(root, ["rev-parse", "HEAD"]), head)
        } finally {
            fs.rmSync(parent, { recursive: true, force: true })
        }
    })
})
