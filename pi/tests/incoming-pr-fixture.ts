/**
 * An incoming-PR fixture for the checkout tools, which work in the current checkout: a base repository (`upstream/app`)
 * and a contributor fork (`contributor/app`) as local bare remotes, and `root`, the maintainer's clone of the base where
 * the tools run (a source checkout or a worker; both are ordinary clones). Production SSH URLs are redirected to the
 * local remotes by a private global Git configuration; every other transport is blocked.
 */

import { writeProjectConfig } from "./config-fixture.ts"

import assert from "node:assert/strict"
import * as cp from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { createEventBus, type ExtensionAPI, type ExtensionContext } from "@earendil-works/pi-coding-agent"

import { compile } from "../extensions/__lib/schema.ts"
import registerPr from "../workflows/extensions/pr/index.ts"
import { INCOMING_CHECKOUT_TOOLS } from "../workflows/extensions/pr/operations.ts"
import type { IncomingCheckoutInput } from "../workflows/extensions/pr/checkout.ts"
import { prepareArtifactPersistence } from "../workflows/extensions/__lib/project-config.ts"
import { withToolExecution } from "./extension-context-fixture.ts"
import { MCP_TOOL_CATALOG_EVENT } from "../extensions/__lib/mcp.ts"
import { minimalTracker } from "./workflow-phase-fixtures.ts"
import { workflowMcpCatalog } from "./mcp-catalog-fixture.ts"

export type Kind = "clean" | "conflict" | "multiple" | "merged" | "alreadyApplied" | "customDriver" | "literalPath"

export interface FixtureOptions {
    kind?: Kind
    /** Files the contributor changes in its first commit, besides the fixture's own. */
    contribution?: Record<string, string>
    /** Files in the common base. */
    base?: Record<string, string>
}

export function fixture(options: FixtureOptions = {}) {
    const kind = options.kind ?? "clean"
    const temp = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "incoming-pr-")))
    const source = path.join(temp, "source")
    const baseRemote = path.join(temp, "base.git")
    const forkRemote = path.join(temp, "fork.git")
    const root = path.join(temp, "app")
    const global = path.join(temp, "global.gitconfig")
    fs.mkdirSync(source)
    fs.writeFileSync(global, "")
    const previous = {
        GIT_CONFIG_GLOBAL: process.env.GIT_CONFIG_GLOBAL,
        GIT_CONFIG_NOSYSTEM: process.env.GIT_CONFIG_NOSYSTEM,
    }
    process.env.GIT_CONFIG_GLOBAL = global
    process.env.GIT_CONFIG_NOSYSTEM = "1"
    const git = (cwd: string, ...args: string[]) =>
        cp
            .execFileSync(
                "git",
                ["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false", "-c", "commit.gpgSign=false", ...args],
                { cwd, encoding: "utf-8", stdio: ["ignore", "pipe", "pipe"] },
            )
            .trim()
    const write = (dir: string, files: Record<string, string>) => {
        for (const [name, content] of Object.entries(files)) {
            fs.mkdirSync(path.dirname(path.join(dir, name)), { recursive: true })
            fs.writeFileSync(path.join(dir, name), content)
        }
    }
    git(temp, "config", "--global", "user.name", "Fixture")
    git(temp, "config", "--global", "user.email", "fixture@example.invalid")
    git(temp, "config", "--global", "init.defaultBranch", "main")
    // The user's own excludes file (XDG) applies even with a private global config; it may ignore `.envrc`.
    git(temp, "config", "--global", "core.excludesFile", "/dev/null")

    git(source, "init", "-b", "main")
    write(source, {
        ".gitignore": "/.piton/.local\nignored/\n",
        "file.txt": "base\n",
        "other.txt": "other base\n",
        "mise.toml": '[env]\nFROM = "base"\n',
        ...(kind === "literalPath" ? { "*": "literal base\n" } : {}),
        ...(kind === "customDriver"
            ? { ".gitattributes": "file.txt merge=blocked filter=blocked diff=blocked\n" }
            : {}),
        ...options.base,
    })
    git(source, "add", "-A")
    git(source, "commit", "-m", "common")
    const common = git(source, "rev-parse", "HEAD")
    git(temp, "clone", "--bare", "--quiet", source, baseRemote)

    write(source, { "file.txt": "contributor first\n", ...options.contribution })
    if (kind === "literalPath") write(source, { "*": "contributor literal\n" })
    git(source, "add", "-A")
    git(source, "commit", "-m", "contributor first")
    const first = git(source, "rev-parse", "HEAD")
    write(source, { "other.txt": "contributor second\n", "ignored/added.txt": "tracked ignored addition\n" })
    git(source, "add", "other.txt")
    git(source, "add", "-f", "ignored/added.txt")
    git(source, "commit", "-m", "contributor second")
    if (kind === "merged") {
        git(source, "checkout", "-b", "side", common)
        write(source, { "side.txt": "side contribution\n" })
        git(source, "add", "side.txt")
        git(source, "commit", "-m", "side contribution")
        git(source, "checkout", "main")
        git(source, "merge", "--no-ff", "side", "-m", "merge side branch")
    }
    const head = git(source, "rev-parse", "HEAD")
    git(temp, "clone", "--bare", "--quiet", source, forkRemote)

    git(source, "checkout", "-b", "upstream", common)
    if (kind === "alreadyApplied") git(source, "cherry-pick", "-x", first)
    if (["conflict", "multiple", "customDriver"].includes(kind)) write(source, { "file.txt": "upstream first\n" })
    if (kind === "multiple") write(source, { "other.txt": "upstream second\n" })
    if (kind === "literalPath") write(source, { "*": "upstream literal\n" })
    write(source, { "upstream.txt": "upstream addition\n" })
    git(source, "add", "-A")
    git(source, "commit", "-m", "target advanced")
    const target = git(source, "rev-parse", "HEAD")
    git(source, "push", "--quiet", baseRemote, "HEAD:refs/heads/main")

    git(temp, "config", "--global", "protocol.allow", "never")
    git(temp, "config", "--global", "protocol.file.allow", "always")
    git(temp, "config", "--global", `url.file://${baseRemote}.insteadOf`, "git@github.com:upstream/app.git")
    git(temp, "config", "--global", `url.file://${forkRemote}.insteadOf`, "git@github.com:contributor/app.git")

    // The maintainer's checkout: a clone of the base at the target, configured as a Piton project.
    git(temp, "clone", "--quiet", "git@github.com:upstream/app.git", root)
    fs.mkdirSync(path.join(root, ".piton"), { recursive: true })
    const forge = { provider: "github", mcpServer: "fixture", repository: { owner: "upstream", repo: "app" } }
    writeProjectConfig(root, { branchFormat: "identifier-title", tracker: minimalTracker(), forge })
    // Project configuration is the source's own, outside Git here, so it never shows as checkout work.
    fs.appendFileSync(path.join(root, ".git/info/exclude"), "/.piton/\n")
    prepareArtifactPersistence(root)

    const evidence = {
        tool: "mcp__fixture__pull_request_read",
        arguments: { method: "get", owner: "upstream", repo: "app", pullNumber: 12 },
        response: {
            number: 12,
            state: "open",
            draft: false,
            merged: false,
            merged_at: null as string | null,
            html_url: "https://github.com/upstream/app/pull/12",
            base: { ref: "main", sha: common, repo: { full_name: "upstream/app" } },
            head: { ref: "main", sha: head, repo: { full_name: "contributor/app" } },
        },
    }
    const originalEvidence = structuredClone(evidence)
    const tools: Parameters<ExtensionAPI["registerTool"]>[0][] = []
    const events = createEventBus()
    registerPr({
        events,
        on() {},
        registerCommand: () => {},
        registerTool: (tool: (typeof tools)[number]) => tools.push(tool),
    } as never)
    events.emit(MCP_TOOL_CATALOG_EVENT, workflowMcpCatalog("fixture", "board"))
    const callIn = async (cwd: string, operation: IncomingCheckoutInput["operation"], extra: object = {}) => {
        const tool = tools.find((entry) => entry.name === INCOMING_CHECKOUT_TOOLS[operation])
        assert.ok(tool, `The PR extension must register ${operation}`)
        const input = { reference: "#12", evidence, ...extra }
        assert.ok(compile(tool.parameters).check(input), "Exercise the real registered schema")
        const result = await tool.execute(
            "fixture",
            input,
            undefined,
            undefined,
            withToolExecution({
                cwd,
            } as ExtensionContext),
        )
        return JSON.parse((result.content[0] as { text: string }).text)
    }
    const call = (operation: IncomingCheckoutInput["operation"], extra: object = {}) => callIn(root, operation, extra)
    const local = path.join(root, ".piton/.local/pull-requests/12")
    const record = () => JSON.parse(fs.readFileSync(path.join(local, "checkout.json"), "utf-8"))
    const ready = async () => {
        await call("prepare")
        await call("checkout")
        return call("beginFixes")
    }
    const rebase = async () => {
        const observation = await call("observeTarget")
        return call("rebase", {
            targetSha: observation.observation.targetSha,
            expectedHeadSha: observation.observation.headSha,
        })
    }
    /** Everything the checkout's Git state and files amount to, for unchanged-state assertions. */
    const snapshot = (dir = root) => ({
        head: git(dir, "rev-parse", "HEAD"),
        branch: cp
            .spawnSync("git", ["-C", dir, "symbolic-ref", "--quiet", "HEAD"], { encoding: "utf-8" })
            .stdout.trim(),
        index: git(dir, "ls-files", "--stage"),
        // The tool's private area (records, the scratch rebase worktree) isn't part of the checkout's state.
        status: git(dir, "status", "--porcelain=v1", "--untracked-files=all", "--ignored")
            .split("\n")
            .filter((line) => !line.slice(3).startsWith(".piton/"))
            .join("\n"),
        files: Object.fromEntries(
            git(dir, "ls-files", "--cached", "--others", "-z", "--", ":!.piton")
                .split("\0")
                .filter(Boolean)
                .sort()
                .map((name) => {
                    const file = path.join(dir, name)
                    return [name, fs.existsSync(file) ? fs.readFileSync(file, "utf-8") : null]
                }),
        ),
    })
    const forkHead = () => git(forkRemote, "rev-parse", "refs/heads/main")
    const contributorUpdate = () => {
        git(source, "checkout", "-b", `contributor-update-${Date.now()}`, head)
        write(source, { "contributor-new.txt": "new remote work\n" })
        git(source, "add", "contributor-new.txt")
        git(source, "commit", "-m", "new remote contribution")
        git(source, "push", "--quiet", forkRemote, "HEAD:refs/heads/main")
        return forkHead()
    }
    const merged = () => {
        evidence.response.state = "closed"
        evidence.response.merged = true
        evidence.response.merged_at = "2026-10-21T12:00:00Z"
    }
    /** A worker of `root`: a clone at its own path sharing `.piton/.local`, as `piton worker create` lays it out. */
    const worker = (name: string) => {
        const dir = `${root}--${name}`
        git(temp, "clone", "--quiet", "git@github.com:upstream/app.git", dir)
        fs.cpSync(path.join(root, ".piton"), path.join(dir, ".piton"), {
            recursive: true,
            filter: (from) => from !== path.join(root, ".piton/.local"),
        })
        fs.symlinkSync(path.join(root, ".piton/.local"), path.join(dir, ".piton/.local"))
        fs.appendFileSync(path.join(dir, ".git/info/exclude"), "/.piton/\n")
        git(dir, "config", "--local", "agent.source", root)
        return dir
    }
    return {
        temp,
        root,
        source,
        baseRemote,
        forkRemote,
        global,
        local,
        common,
        first,
        head,
        target,
        evidence,
        originalEvidence,
        git,
        write,
        call,
        callIn,
        record,
        ready,
        rebase,
        snapshot,
        forkHead,
        contributorUpdate,
        merged,
        worker,
        close() {
            for (const [key, value] of Object.entries(previous)) {
                if (value === undefined) delete process.env[key]
                else process.env[key] = value
            }
            fs.rmSync(temp, { recursive: true, force: true })
        },
    }
}

/** Runs `body` with `PATH` led by a fake `git` that wraps the real one with `script` (a Node program's body). */
export async function withGitWrapper(temp: string, script: (realGit: string) => string, body: () => Promise<void>) {
    const previousPath = process.env.PATH
    const realGit = cp.execFileSync("which", ["git"], { encoding: "utf-8" }).trim()
    const bin = fs.mkdtempSync(path.join(temp, "bin-"))
    fs.writeFileSync(path.join(bin, "git"), `#!${process.execPath}\n${script(realGit)}`, { mode: 0o700 })
    process.env.PATH = `${bin}${path.delimiter}${previousPath ?? ""}`
    try {
        await body()
    } finally {
        if (previousPath === undefined) delete process.env.PATH
        else process.env.PATH = previousPath
    }
}
