import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { createHash, randomUUID } from "node:crypto"

import { bindGit, git as runGit, type Git, type GitWithResult } from "./git.js"
import { heldPathspecs } from "./held.js"
import type { CheckoutState, Configuration, Start, Workspace } from "./state.js"

/** One checkout of the maintainer's repository the tool operates in: the current checkout or the scratch worktree. */
export interface Checkout {
    root: string
    gitDir: string
    git: Git
    result: GitWithResult
}

export async function openCheckout(root: string, signal?: AbortSignal): Promise<Checkout> {
    const bound = bindGit(root, signal)
    const gitDir = (await bound.git(["rev-parse", "--absolute-git-dir"])).trim()
    return { root, gitDir, ...bound }
}

/** The checkout's top folder, resolved like the tool's recorded locations. */
export async function checkoutRoot(cwd: string, signal?: AbortSignal): Promise<string> {
    return fs.realpathSync((await runGit(cwd, ["rev-parse", "--show-toplevel"], { signal })).trim())
}

export function isWorkTree(root: string): boolean {
    return fs.existsSync(path.join(root, ".git"))
}

const lines = (text: string) => text.split("\0").filter(Boolean)

/** A private index file, removed after `use`. */
async function privateIndex<T>(_c: Checkout, seed: string | null, use: (index: string) => Promise<T>): Promise<T> {
    const index = path.join(os.tmpdir(), `piton-pr-${process.pid}-${randomUUID()}.index`)
    try {
        // Start from the real index so `read-tree -m` keeps its file stat information and refreshes are cheap.
        if (seed) await fs.promises.copyFile(seed, index).catch(() => undefined)
        return await use(index)
    } finally {
        await fs.promises.rm(index, { force: true })
    }
}

async function readTree(c: Checkout, index: string, treeish: string): Promise<void> {
    const merged = await c.result(["read-tree", "-m", treeish], { index })
    if (merged.code !== 0) await c.git(["read-tree", treeish], { index })
}

/** Held paths whose content differs between two revisions. */
export async function heldChanges(c: Checkout, from: string, to: string): Promise<string[]> {
    return lines(
        await c.git(["diff", "--no-renames", "--name-only", "-z", from, to, "--", ...heldPathspecs()], {
            pathspecs: "magic",
        }),
    )
}

/**
 * The commit whose tree is what the tool puts on disk for `treeish`: the same tree with every held path taken from
 * `trustedSha` (present there, or absent when it isn't), unless configuration is released. Tool identity and a fixed
 * date make it the same commit for the same tree.
 */
export async function diskCommitFor(
    c: Checkout,
    treeish: string,
    trustedSha: string,
    configuration: Configuration,
): Promise<string> {
    const tree = await privateIndex(c, null, async (index) => {
        await c.git(["read-tree", treeish], { index })
        switch (configuration) {
            case "released":
                break
            case "held": {
                const specs = heldPathspecs()
                const target = lines(await c.git(["ls-files", "-z", "--", ...specs], { index, pathspecs: "magic" }))
                if (target.length)
                    await c.git(["update-index", "-z", "--force-remove", "--stdin"], {
                        index,
                        input: target.map((file) => file + "\0").join(""),
                    })
                // `ls-tree` takes no glob pathspecs; list the trusted revision's held entries through its own index.
                const trusted = await privateIndex(c, null, async (trustedIndex) => {
                    await c.git(["read-tree", trustedSha], { index: trustedIndex })
                    return c.git(["ls-files", "--stage", "-z", "--", ...specs], {
                        index: trustedIndex,
                        pathspecs: "magic",
                    })
                })
                const entries = lines(trusted).map((entry) => {
                    const match = /^(\d+) ([a-f0-9]+) 0\t([\s\S]+)$/.exec(entry)
                    if (!match) throw new Error("Unexpected Git index entry for held configuration")
                    return `${match[1]} ${match[2]}\t${match[3]}`
                })
                if (entries.length) await updateIndex(c, index, entries)
                break
            }
        }
        return (await c.git(["write-tree"], { index })).trim()
    })
    return (await c.git(["commit-tree", tree, "-m", "piton: incoming PR files on disk"], { toolIdentity: true })).trim()
}

async function updateIndex(c: Checkout, index: string, entries: string[]): Promise<void> {
    await c.git(["update-index", "-z", "--index-info"], { index, input: entries.map((entry) => entry + "\0").join("") })
}

export async function treeOf(c: Checkout, commitish: string): Promise<string> {
    return (await c.git(["rev-parse", `${commitish}^{tree}`])).trim()
}

export async function workspaceFor(
    c: Checkout,
    head: string,
    indexCommit: string,
    trustedSha: string,
    configuration: Configuration,
): Promise<Workspace> {
    return {
        headSha: head,
        indexTree: await treeOf(c, indexCommit),
        diskCommit: await diskCommitFor(c, indexCommit, trustedSha, configuration),
    }
}

export function nativeOperation(gitDir: string) {
    const progress = path.join(gitDir, "rebase-merge")
    if (exists(progress)) {
        if (!fs.lstatSync(progress).isDirectory()) throw new Error("Unsafe native rebase directory")
        const read = (name: string) => regular(path.join(progress, name)).toString("utf-8").trim()
        return {
            kind: "rebase" as const,
            sourceSha: read("orig-head"),
            targetSha: read("onto"),
            fingerprint: treeFingerprint(progress),
        }
    }
    for (const name of ["rebase-apply", "MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD", "sequencer", "BISECT_LOG"])
        if (exists(path.join(gitDir, name))) return { kind: "other" as const, name }
    return { kind: "none" as const }
}
export type NativeOperation = ReturnType<typeof nativeOperation>

function exists(file: string): boolean {
    try {
        fs.lstatSync(file)
        return true
    } catch (error) {
        if ((error as NodeJS.ErrnoException).code === "ENOENT") return false
        throw error
    }
}
function regular(file: string): Buffer {
    if (!fs.lstatSync(file).isFile()) throw new Error(`Unsafe Git progress metadata: ${file}`)
    return fs.readFileSync(file)
}
function treeFingerprint(directory: string): string {
    const hash = createHash("sha256")
    const walk = (current: string) => {
        for (const name of fs.readdirSync(current).sort()) {
            const file = path.join(current, name)
            const stat = fs.lstatSync(file)
            hash.update(path.relative(directory, file) + "\0")
            if (stat.isDirectory()) walk(file)
            else hash.update(regular(file))
        }
    }
    walk(directory)
    return hash.digest("hex")
}

async function commitMetadata(c: Checkout, head: string) {
    const raw = await c.git(["cat-file", "commit", head])
    const separator = raw.indexOf("\n\n")
    if (separator < 0) throw new Error("Invalid Git commit metadata")
    const headers = raw.slice(0, separator).split("\n")
    return {
        tree: headers.find((line) => line.startsWith("tree "))?.slice(5) ?? null,
        parents: headers.filter((line) => line.startsWith("parent ")).map((line) => line.slice(7)),
        message: raw.slice(separator + 2),
    }
}

/**
 * The checkout as it is: HEAD, the index, and the files on disk compared with `diskCommit` (the state the tool set),
 * through a private index so the real one is untouched.
 */
export async function observe(c: Checkout, diskCommit: string) {
    const headResult = await c.result(["rev-parse", "--verify", "--quiet", "HEAD"])
    const head = headResult.code === 0 ? headResult.stdout.trim() : null
    const branchResult = await c.result(["symbolic-ref", "--quiet", "HEAD"])
    const branch = branchResult.code === 0 ? branchResult.stdout.trim() : null
    const unmerged = lines(await c.git(["ls-files", "--unmerged", "-z"])).map((entry) => {
        const match = /^(\d+) ([a-f0-9]+) ([123])\t([\s\S]+)$/.exec(entry)
        if (!match) throw new Error("Invalid unmerged index entry")
        return { mode: match[1], sha: match[2], stage: Number(match[3]), path: match[4] }
    })
    const indexTree = unmerged.length ? null : (await c.git(["write-tree"])).trim()
    const indexEntries = await c.git(["ls-files", "--stage", "-z"])
    const disk = await privateIndex(c, path.join(c.gitDir, "index"), async (index) => {
        await readTree(c, index, diskCommit)
        await c.result(["update-index", "-q", "--refresh"], { index })
        const specs = heldPathspecs()
        const magic = { index, pathspecs: "magic" as const }
        return {
            changed: lines(await c.git(["diff-files", "--name-only", "-z", "--ignore-submodules=all"], { index })),
            untracked: lines(await c.git(["ls-files", "--others", "--exclude-standard", "-z"], { index })),
            heldChanged: lines(
                await c.git(["diff-files", "--name-only", "-z", "--ignore-submodules=all", "--", ...specs], magic),
            ),
            heldUntracked: lines(
                await c.git(["ls-files", "--others", "--exclude-standard", "-z", "--", ...specs], magic),
            ),
        }
    })
    return {
        head,
        branch,
        detached: branch === null,
        headCommit: head ? await commitMetadata(c, head) : null,
        indexTree,
        indexFingerprint: createHash("sha256").update(indexEntries).digest("hex"),
        unmerged,
        changed: disk.changed,
        untracked: disk.untracked,
        heldChanged: disk.heldChanged,
        heldUntracked: disk.heldUntracked,
        native: nativeOperation(c.gitDir),
    }
}
export type Observed = Awaited<ReturnType<typeof observe>>

/** Whether HEAD, the index, and the files on disk are exactly `workspace`, with nothing untracked or in progress. */
export function matches(observed: Observed, workspace: Workspace): boolean {
    return (
        observed.head === workspace.headSha &&
        observed.detached &&
        observed.indexTree === workspace.indexTree &&
        !observed.changed.length &&
        !observed.untracked.length &&
        !observed.unmerged.length &&
        observed.native.kind === "none"
    )
}

export function requireExact(observed: Observed, workspace: Workspace, what: string): void {
    if (!matches(observed, workspace))
        throw new Error(
            `${what}: the checkout must be exactly as the tool left it, with no edits, staging, untracked files, or Git operation in progress. Changed: ${JSON.stringify(observed.changed)}; untracked: ${JSON.stringify(observed.untracked)}`,
        )
}

/** The start state: the checkout before review, clean apart from ignored files. */
export async function observeStart(c: Checkout): Promise<{ start: Start; status: string[] }> {
    const head = (await c.result(["rev-parse", "--verify", "--quiet", "HEAD"])).stdout.trim()
    if (!head) throw new Error("The checkout has no commit yet; nothing to review against")
    const branch = await c.result(["symbolic-ref", "--quiet", "--short", "HEAD"])
    const status = lines(
        await c.git(["status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignore-submodules=none"]),
    )
    const native = nativeOperation(c.gitDir)
    if (native.kind !== "none")
        status.push(`Git operation in progress: ${native.kind === "rebase" ? "rebase" : native.name}`)
    return {
        start: branch.code === 0 ? { kind: "branch", name: branch.stdout.trim(), head } : { kind: "detached", head },
        status,
    }
}

/** Where a move ends: HEAD, the index tree, the files on disk, and the branch to attach for a restored start. */
export interface Target {
    head: string
    indexTree: string
    diskCommit: string
    branch: string | null
}

export function targetOf(workspace: Workspace): Target {
    return { head: workspace.headSha, indexTree: workspace.indexTree, diskCommit: workspace.diskCommit, branch: null }
}

/**
 * Moves the checkout from files on disk equal to `fromDisk` to `to`. First HEAD and the index point at `fromDisk`, so
 * the switch moves from one clean state to another: it never carries staged changes, and `--no-overwrite-ignore`
 * refuses to overwrite ignored files. Then the index and HEAD are set to the target. Callers verify the checkout is
 * exactly at `fromDisk` before calling, and record the move first so an interruption can be finished.
 */
export async function move(c: Checkout, fromDisk: string, to: Target): Promise<string[]> {
    await c.git(["read-tree", fromDisk])
    await c.git(["update-ref", "--no-deref", "-m", "piton: incoming PR review", "HEAD", fromDisk])
    await c.result(["update-index", "-q", "--refresh"])
    const switched = await c.result([
        "switch",
        "--detach",
        "--no-overwrite-ignore",
        "--no-recurse-submodules",
        "--quiet",
        to.diskCommit,
    ])
    if (switched.code !== 0) {
        // Nothing on disk changed: put HEAD and the index back where the caller's recorded state expects them.
        throw Object.assign(
            new Error(`The checkout can't be switched: ${switched.stderr.trim() || switched.stdout.trim()}`),
            {
                refused: true,
            },
        )
    }
    return settle(c, to)
}

/** Points the index and HEAD at the target once the files on disk are there, and attaches a restored branch. */
export async function settle(c: Checkout, to: Target): Promise<string[]> {
    const messages: string[] = []
    await c.git(["read-tree", to.indexTree])
    await c.git(["update-ref", "--no-deref", "-m", "piton: incoming PR review", "HEAD", to.head])
    if (to.branch) {
        const ref = `refs/heads/${to.branch}`
        const at = await c.result(["rev-parse", "--verify", "--quiet", ref])
        if (at.code === 0 && at.stdout.trim() === to.head) await c.git(["symbolic-ref", "HEAD", ref])
        else
            messages.push(
                `Branch ${to.branch} moved or was deleted during the review; the checkout stays detached at the starting revision ${to.head}.`,
            )
    }
    await c.result(["update-index", "-q", "--refresh"])
    return messages
}

/** Where a recorded move ends, for any state that records one. */
export function recordedTarget(state: CheckoutState, startTree: string): Target | null {
    switch (state.phase) {
        case "prepared":
            return null
        case "restored":
            return {
                head: state.location.start.head,
                indexTree: startTree,
                diskCommit: state.location.start.head,
                branch: state.location.start.kind === "branch" ? state.location.start.name : null,
            }
        case "contributorReview":
        case "fixes":
        case "rebasing":
        case "rebased":
        case "committingFixes":
        case "committedFixes":
            return targetOf(state.workspace)
    }
}

/** A plain checkout-relative path the maintainer approved: no absolute paths, `..`, Git metadata, or symlinked folders. */
export function checkoutSourcePath(directory: string, raw: string): string {
    const resolved = path.resolve(directory, raw)
    const relative = path.relative(directory, resolved)
    if (
        !relative ||
        path.isAbsolute(raw) ||
        relative === ".." ||
        relative.startsWith(".." + path.sep) ||
        relative.split(path.sep).includes(".git")
    )
        throw new Error("Paths must be exact checkout-relative source paths, not Git metadata")
    let current = directory
    for (const part of relative.split(path.sep).slice(0, -1)) {
        current = path.join(current, part)
        try {
            if (fs.lstatSync(current).isSymbolicLink()) throw new Error("Source paths must not traverse symlinks")
        } catch (error) {
            if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error
        }
    }
    return relative
}

export function matchesFixCommit(
    observed: Observed,
    intent: { sourceSha: string; treeSha: string; message: string },
): boolean {
    const commit = observed.headCommit
    return (
        commit !== null &&
        commit.tree === intent.treeSha &&
        commit.parents.length === 1 &&
        commit.parents[0] === intent.sourceSha &&
        commit.message === intent.message
    )
}
