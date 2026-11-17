import "./host-runtime.ts"
import { writeProjectConfig } from "./config-fixture.ts"

import assert from "node:assert/strict"
import * as fs from "node:fs"
import * as path from "node:path"
import { describe, it } from "node:test"

import { kindLabels } from "./workflow-phase-fixtures.ts"
import { fixture, withGitWrapper } from "./incoming-pr-fixture.ts"

const passThrough = (real: string, before = "", after = "") => `const cp = require('node:child_process');
const fs = require('node:fs');
const args = process.argv.slice(2);
${before}
const result = cp.spawnSync(${JSON.stringify(real)}, args, { stdio: 'inherit', env: process.env });
${after}
process.exit(result.status ?? 1);`

describe("incoming PR standard rebase and immediate leased update", () => {
    it("uses existing SSH authentication for fetch, observation and publication", async () => {
        const f = fixture()
        const previousSsh = process.env.GIT_SSH_COMMAND
        const previousVariant = process.env.GIT_SSH_VARIANT
        try {
            for (const remote of [f.baseRemote, f.forkRemote])
                f.git(f.root, "config", "--global", "--unset-all", `url.file://${remote}.insteadOf`)
            f.git(f.root, "config", "--global", "protocol.ssh.allow", "always")
            const realGit = (await import("node:child_process"))
                .execFileSync("which", ["git"], { encoding: "utf-8" })
                .trim()
            const ssh = path.join(f.temp, "ssh.cjs")
            const log = path.join(f.temp, "ssh-arguments.jsonl")
            fs.writeFileSync(
                ssh,
                `
const cp = require('node:child_process');
const fs = require('node:fs');
const args = process.argv.slice(2);
fs.appendFileSync(${JSON.stringify(log)}, JSON.stringify(args) + '\\n');
const command = /^(git-upload-pack|git-receive-pack) '(upstream|contributor)\\/app\\.git'$/.exec(args.at(-1));
if (args.at(-2) !== 'git@github.com' || !command) process.exit(1);
const repository = command[2] === 'upstream' ? ${JSON.stringify(f.baseRemote)} : ${JSON.stringify(f.forkRemote)};
const result = cp.spawnSync(${JSON.stringify(realGit)}, [command[1].slice(4), repository], { stdio: 'inherit', env: process.env });
process.exit(result.status ?? 1);
`,
            )
            process.env.GIT_SSH_COMMAND = `${JSON.stringify(process.execPath)} ${JSON.stringify(ssh)}`
            process.env.GIT_SSH_VARIANT = "ssh"
            const global = fs.readFileSync(f.global)
            await f.ready()
            const result = await f.rebase()
            assert.equal(result.rebase.publication.kind, "confirmed")
            assert.equal(f.forkHead(), result.rebase.resultSha)
            const calls: string[][] = fs
                .readFileSync(log, "utf-8")
                .trim()
                .split("\n")
                .map((line) => JSON.parse(line))
            assert.ok(calls.every((args) => args.at(-2) === "git@github.com"))
            for (const command of [
                "git-upload-pack 'upstream/app.git'",
                "git-upload-pack 'contributor/app.git'",
                "git-receive-pack 'contributor/app.git'",
            ])
                assert.ok(
                    calls.some((args) => args.at(-1) === command),
                    command,
                )
            assert.deepEqual(fs.readFileSync(f.global), global)
        } finally {
            if (previousSsh === undefined) delete process.env.GIT_SSH_COMMAND
            else process.env.GIT_SSH_COMMAND = previousSsh
            if (previousVariant === undefined) delete process.env.GIT_SSH_VARIANT
            else process.env.GIT_SSH_VARIANT = previousVariant
            f.close()
        }
    })

    it("rebases in a scratch worktree, updates the fork, and keeps held configuration", async () => {
        const f = fixture({ contribution: { "mise.toml": '[env]\nFROM = "contributor"\n' } })
        try {
            await f.ready()
            const observed = await f.call("observeTarget")
            assert.equal(observed.observation.targetSha, f.target)
            assert.equal(observed.observation.headSha, f.head)
            const result = await f.call("rebase", { targetSha: f.target, expectedHeadSha: f.head })
            assert.equal(result.phase, "fixes")
            assert.equal(result.phaseValid, true)
            assert.equal(result.rebase.publication.kind, "confirmed")
            assert.equal(result.rebase.leaseSha, f.head)
            assert.equal(f.forkHead(), result.baselineSha)
            assert.equal(f.git(f.root, "rev-parse", "HEAD"), result.baselineSha)
            assert.equal(f.git(f.root, "merge-base", "--is-ancestor", f.target, result.baselineSha), "")
            assert.equal(f.git(f.root, "rev-list", "--count", `${f.target}..HEAD`), "2")
            assert.equal(fs.readFileSync(path.join(f.root, "upstream.txt"), "utf-8"), "upstream addition\n")
            assert.equal(fs.readFileSync(path.join(f.root, "mise.toml"), "utf-8"), '[env]\nFROM = "base"\n')
            assert.equal(fs.existsSync(path.join(f.local, "rebase")), false, "The scratch worktree is removed")
            assert.equal(f.git(f.root, "worktree", "list", "--porcelain").match(/^worktree /gm)?.length, 1)
            assert.deepEqual(f.record().evidence, f.originalEvidence)
            assert.equal(result.providerRevision.head.sha, f.head, "No fabricated provider response after push")
            f.evidence.response.head.sha = f.forkHead()
            f.evidence.response.base.sha = f.target
            assert.equal((await f.call("inspect")).providerHeadMatchesBaseline, true)
            fs.appendFileSync(path.join(f.root, "file.txt"), "our later fix\n")
            const fixed = await f.call("inspect")
            assert.match(fs.readFileSync(fixed.paths.fixesDiff.absolute, "utf-8"), /\+our later fix/)
            assert.equal(f.forkHead(), result.baselineSha, "Later fixes are not automatically published")
        } finally {
            f.close()
        }
    })

    it("exposes conflicts in the scratch worktree and publishes after approved resolution", async () => {
        const f = fixture({ kind: "conflict" })
        try {
            await f.ready()
            const before = f.snapshot()
            const conflict = await f.rebase()
            assert.equal(conflict.phase, "rebasing")
            assert.equal(conflict.phaseValid, true)
            const scratch = path.join(f.local, "rebase")
            assert.deepEqual(conflict.conflictPaths, [path.join(scratch, "file.txt")])
            assert.match(fs.readFileSync(conflict.paths.conflictsDiff.absolute, "utf-8"), /file\.txt/)
            assert.deepEqual(f.snapshot(), before, "The checkout under review is never mid-rebase")
            assert.equal(f.forkHead(), f.head)
            await assert.rejects(f.call("restore"), /rebase is in progress/)
            fs.writeFileSync(path.join(scratch, "file.txt"), "approved combined resolution\n")
            const published = await f.call("continueRebase", { resolutionPaths: ["file.txt"] })
            assert.equal(published.phase, "fixes")
            assert.equal(published.rebase.publication.kind, "confirmed")
            assert.equal(f.forkHead(), published.baselineSha)
            assert.equal(fs.readFileSync(path.join(f.root, "file.txt"), "utf-8"), "approved combined resolution\n")
            assert.equal(published.phaseValid, true)
        } finally {
            f.close()
        }
    })

    it("continues multiple conflicts without publishing an intermediate commit", async () => {
        const f = fixture({ kind: "multiple" })
        try {
            await f.ready()
            await f.rebase()
            const scratch = path.join(f.local, "rebase")
            fs.writeFileSync(path.join(scratch, "file.txt"), "resolved first\n")
            const second = await f.call("continueRebase", { resolutionPaths: ["file.txt"] })
            assert.equal(second.phase, "rebasing")
            assert.ok(second.conflictPaths.includes(path.join(scratch, "other.txt")))
            assert.equal(f.forkHead(), f.head)
            fs.writeFileSync(path.join(scratch, "other.txt"), "resolved second\n")
            const completed = await f.call("continueRebase", { resolutionPaths: ["other.txt"] })
            assert.equal(completed.rebase.publication.kind, "confirmed")
            assert.equal(f.forkHead(), completed.baselineSha)
        } finally {
            f.close()
        }
    })

    for (const kind of ["merged", "alreadyApplied"] as const)
        it(`uses standard rebase semantics for ${kind} history`, async () => {
            const f = fixture({ kind })
            try {
                await f.ready()
                const result = await f.rebase()
                assert.equal(result.rebase.publication.kind, "confirmed")
                assert.equal(f.git(f.root, "rev-list", "--merges", `${f.target}..HEAD`), "")
                if (kind === "merged") assert.equal(f.git(f.root, "rev-list", "--count", `${f.target}..HEAD`), "3")
                else {
                    assert.equal(f.git(f.root, "rev-list", "--count", `${f.target}..HEAD`), "1")
                    assert.match(result.messages.join("\n"), /skipped previously applied commit|dropping/)
                }
            } finally {
                f.close()
            }
        })

    for (const dirty of ["tracked", "staged", "untracked"] as const)
        it(`refuses ${dirty} work before starting rebase`, async () => {
            const f = fixture()
            try {
                await f.ready()
                await f.call("observeTarget")
                if (dirty === "untracked") fs.writeFileSync(path.join(f.root, "new.txt"), "preserve this\n")
                else {
                    fs.appendFileSync(path.join(f.root, "file.txt"), "preserve this\n")
                    if (dirty === "staged") f.git(f.root, "add", "file.txt")
                }
                const before = f.snapshot()
                await assert.rejects(
                    f.call("rebase", { targetSha: f.target, expectedHeadSha: f.head }),
                    /exactly as the tool left it/,
                )
                assert.deepEqual(f.snapshot(), before)
                assert.equal(f.forkHead(), f.head)
            } finally {
                f.close()
            }
        })

    it("reports changed PR addressing through inspection while refusing mutations", async () => {
        for (const change of ["retarget", "missing-head"] as const) {
            const f = fixture()
            try {
                await f.ready()
                await f.call("observeTarget")
                const before = f.snapshot()
                if (change === "retarget") f.evidence.response.base.ref = "other-target"
                else f.evidence.response.head.repo = null as never
                const inspected = await f.call("inspect")
                assert.equal(inspected.providerAddressMatches, false)
                await assert.rejects(f.call("observeTarget"), /addressing changed/)
                await assert.rejects(
                    f.call("rebase", { targetSha: f.target, expectedHeadSha: f.head }),
                    /addressing changed/,
                )
                assert.deepEqual(f.snapshot(), before)
            } finally {
                f.close()
            }
        }
    })

    it("refuses a contributor update or a moved target after observation", async () => {
        for (const change of ["contributor", "target"] as const) {
            const f = fixture()
            try {
                await f.ready()
                await f.call("observeTarget")
                if (change === "contributor") f.contributorUpdate()
                else {
                    f.git(f.source, "checkout", "upstream")
                    f.write(f.source, { "upstream.txt": "moved again\n" })
                    f.git(f.source, "commit", "-am", "target moved again")
                    f.git(f.source, "push", "--quiet", f.baseRemote, "HEAD:refs/heads/main")
                }
                const before = f.snapshot()
                await assert.rejects(
                    f.call("rebase", { targetSha: f.target, expectedHeadSha: f.head }),
                    /changed after observation/,
                )
                assert.deepEqual(f.snapshot(), before)
            } finally {
                f.close()
            }
        }
    })

    for (const missing of [false, true])
        it(`preserves remote work when the ${missing ? "deleted" : "advanced"} contributor branch rejects the lease`, async () => {
            const f = fixture({ kind: "conflict" })
            try {
                await f.ready()
                await f.rebase()
                const changed = missing ? null : f.contributorUpdate()
                if (missing) f.git(f.forkRemote, "update-ref", "-d", "refs/heads/main")
                fs.writeFileSync(path.join(f.local, "rebase/file.txt"), "approved resolution\n")
                const rejected = await f.call("continueRebase", { resolutionPaths: ["file.txt"] })
                assert.equal(rejected.phase, "rebased")
                assert.equal(rejected.rebase.publication.kind, "rejected")
                assert.equal(rejected.rebase.publication.observedHead, changed)
                assert.equal(f.git(f.root, "rev-parse", "HEAD"), rejected.rebase.resultSha)
                const repeated = await f.call("publishRebase")
                assert.equal(repeated.rebase.leaseSha, f.head)
                assert.equal(repeated.rebase.publication.observedHead, changed)
                if (!missing) assert.equal(f.forkHead(), changed)
                await assert.rejects(f.call("restore"), /isn't published/)
            } finally {
                f.close()
            }
        })

    it("rejects escaping resolution paths and edits outside approved resolutions", async () => {
        const f = fixture({ kind: "conflict" })
        try {
            await f.ready()
            await f.rebase()
            const scratch = path.join(f.local, "rebase")
            for (const file of ["../notes.md", ".git/HEAD", path.join(scratch, "file.txt")])
                await assert.rejects(f.call("continueRebase", { resolutionPaths: [file] }), /checkout-relative/)
            fs.writeFileSync(path.join(scratch, "file.txt"), "approved resolution\n")
            fs.appendFileSync(path.join(scratch, "upstream.txt"), "unexpected\n")
            await assert.rejects(f.call("continueRebase", { resolutionPaths: ["file.txt"] }), /outside approved/)
            assert.match(fs.readFileSync(path.join(scratch, "upstream.txt"), "utf-8"), /unexpected/)
            assert.equal(f.forkHead(), f.head)
        } finally {
            f.close()
        }
    })

    it("pins the lease even when a contributor pushes after the final preflight read", async () => {
        const f = fixture()
        try {
            await f.ready()
            f.git(f.source, "checkout", "-b", "racing", f.head)
            f.write(f.source, { "new-work.txt": "newer contribution\n" })
            f.git(f.source, "add", "new-work.txt")
            f.git(f.source, "commit", "-m", "contributor update during push")
            const newer = f.git(f.source, "rev-parse", "HEAD")
            f.git(f.source, "push", "--quiet", f.forkRemote, "HEAD:refs/heads/racing")
            const argsFile = path.join(f.temp, "pushed-arguments")
            await withGitWrapper(
                f.temp,
                (real) =>
                    passThrough(
                        real,
                        `if (args.includes('push') && args.some(a => a.startsWith('--force-with-lease='))) { fs.writeFileSync(${JSON.stringify(argsFile)}, JSON.stringify(args)); cp.execFileSync(${JSON.stringify(real)}, ['--git-dir=${f.forkRemote}', 'update-ref', 'refs/heads/main', '${newer}', '${f.head}']); }`,
                    ),
                async () => {
                    const rejected = await f.rebase()
                    assert.equal(rejected.phase, "rebased")
                    assert.equal(rejected.rebase.publication.kind, "rejected")
                    assert.equal(rejected.rebase.publication.observedHead, newer)
                },
            )
            assert.equal(f.forkHead(), newer)
            const args: string[] = JSON.parse(fs.readFileSync(argsFile, "utf-8"))
            assert.ok(args.includes(`--force-with-lease=refs/heads/main:${f.head}`))
            assert.equal(args.includes("--force"), false)
        } finally {
            f.close()
        }
    })

    it("reconciles a lost push response against the actual remote without repeating the push", async () => {
        const f = fixture()
        try {
            await f.ready()
            const pushed = path.join(f.temp, "push-count")
            const failRead = path.join(f.temp, "fail-read-once")
            await withGitWrapper(
                f.temp,
                (real) =>
                    passThrough(
                        real,
                        `if (args.includes('ls-remote') && fs.existsSync(${JSON.stringify(failRead)})) { fs.unlinkSync(${JSON.stringify(failRead)}); process.exit(1); }`,
                        `if (result.status === 0 && args.includes('push')) { fs.appendFileSync(${JSON.stringify(pushed)}, 'push\\n'); fs.writeFileSync(${JSON.stringify(failRead)}, '1'); process.exit(1); }`,
                    ),
                async () => {
                    const uncertain = await f.rebase()
                    assert.equal(uncertain.phase, "rebased")
                    assert.equal(uncertain.rebase.publication.kind, "attempted")
                    assert.equal(f.forkHead(), uncertain.rebase.resultSha)
                    f.evidence.response.head.sha = uncertain.rebase.resultSha
                    f.evidence.response.base.sha = f.target
                    const recovered = await f.call("publishRebase")
                    assert.equal(recovered.phase, "fixes")
                    assert.equal(recovered.rebase.publication.kind, "confirmed")
                },
            )
            assert.equal(fs.readFileSync(pushed, "utf-8"), "push\n")
        } finally {
            f.close()
        }
    })

    it("recovers a rebase result whose checkout move was interrupted", async () => {
        const f = fixture()
        try {
            await f.ready()
            let switches = 0
            await withGitWrapper(
                f.temp,
                (real) =>
                    passThrough(
                        real,
                        "",
                        `if (result.status === 0 && args.includes('switch')) process.kill(process.pid, 'SIGKILL');`,
                    ),
                async () => {
                    await assert.rejects(f.rebase())
                    switches++
                },
            )
            assert.equal(switches, 1)
            assert.equal(f.record().phase, "rebased")
            assert.notEqual(f.record().moving, null)
            assert.equal(f.forkHead(), f.head, "Nothing is pushed before the checkout holds the result")
            const recovered = await f.call("inspect")
            assert.equal(recovered.phaseValid, true)
            assert.equal(f.record().moving, null)
            const published = await f.call("publishRebase")
            assert.equal(published.phase, "fixes")
            assert.equal(f.forkHead(), published.baselineSha)
        } finally {
            f.close()
        }
    })

    it("suppresses custom merge drivers, filters, hooks, signing and editor helpers", async () => {
        const f = fixture({ kind: "customDriver" })
        try {
            await f.ready()
            const marker = path.join(f.temp, "marker.sh")
            const executed = path.join(f.temp, "executed")
            fs.writeFileSync(marker, `#!/bin/sh\nprintf ran >> '${executed}'\nexit 1\n`, { mode: 0o700 })
            for (const [key, value] of [
                ["merge.blocked.driver", marker],
                ["filter.blocked.clean", marker],
                ["filter.blocked.smudge", marker],
                ["filter.blocked.process", marker],
                ["filter.blocked.required", "true"],
                ["core.editor", marker],
                ["sequence.editor", marker],
                ["commit.gpgSign", "true"],
                ["gpg.program", marker],
                ["rerere.enabled", "true"],
                ["diff.blocked.command", marker],
                ["diff.blocked.textconv", marker],
            ])
                f.git(f.root, "config", "--global", key, value)
            const hooks = path.join(f.root, ".git/hooks")
            fs.mkdirSync(hooks, { recursive: true })
            for (const name of ["pre-rebase", "post-rewrite", "pre-push", "post-checkout", "commit-msg"]) {
                fs.copyFileSync(marker, path.join(hooks, name))
                fs.chmodSync(path.join(hooks, name), 0o700)
            }
            const conflict = await f.rebase()
            assert.equal(conflict.phase, "rebasing")
            fs.writeFileSync(path.join(f.local, "rebase/file.txt"), "approved safe manual resolution\n")
            const result = await f.call("continueRebase", { resolutionPaths: ["file.txt"] })
            assert.equal(result.rebase.publication.kind, "confirmed")
            assert.equal(fs.existsSync(executed), false)
        } finally {
            f.close()
        }
    })
})

describe("incoming PR additive fix delivery", () => {
    it("commits then separately publishes fixes and advances the managed baseline", async () => {
        const f = fixture()
        try {
            await f.ready()
            fs.appendFileSync(path.join(f.root, "file.txt"), "approved fix\n")
            const committed = await f.call("commitFixes", { paths: ["file.txt"], message: "fix: reviewed correction" })
            assert.equal(committed.phase, "committedFixes")
            assert.equal(committed.phaseValid, true)
            assert.equal(f.forkHead(), f.head, "Commit approval alone must not push")
            const resultSha = committed.fixCommit.resultSha
            assert.equal(f.git(f.root, "rev-parse", "HEAD^"), f.head)
            assert.equal(committed.git.detached, true)
            assert.equal(f.git(f.root, "rev-parse", "refs/heads/main"), f.target, "Fix commits never move a branch")
            const published = await f.call("publishFixes")
            assert.equal(published.phase, "fixes")
            assert.equal(published.baselineSha, resultSha)
            assert.equal(f.forkHead(), resultSha)
            f.evidence.response.head.sha = resultSha
            fs.appendFileSync(path.join(f.root, "file.txt"), "later draft\n")
            const inspected = await f.call("inspect")
            assert.match(fs.readFileSync(inspected.paths.fixesDiff.absolute, "utf8"), /\+later draft/)
            assert.doesNotMatch(fs.readFileSync(inspected.paths.fixesDiff.absolute, "utf8"), /\+approved fix/)
            assert.match(fs.readFileSync(inspected.paths.publishedFixesDiff.absolute, "utf8"), /\+approved fix/)
            assert.deepEqual(f.record().evidence, f.originalEvidence)
        } finally {
            f.close()
        }
    })

    it("preserves changes outside the exact approved fix paths", async () => {
        const f = fixture()
        try {
            await f.ready()
            fs.appendFileSync(path.join(f.root, "file.txt"), "approved change\n")
            fs.writeFileSync(path.join(f.root, "extra.txt"), "preserve\n")
            const before = f.snapshot()
            await assert.rejects(
                f.call("commitFixes", { paths: ["file.txt"], message: "fix: correction" }),
                /complete approved fix set/,
            )
            assert.deepEqual(f.snapshot(), before)
        } finally {
            f.close()
        }
    })

    it("recognizes an interrupted completed fix commit without creating another", async () => {
        const f = fixture()
        try {
            await f.ready()
            fs.appendFileSync(path.join(f.root, "file.txt"), "approved correction\n")
            const count = path.join(f.temp, "commit-count")
            const request = { paths: ["file.txt"], message: "fix: interrupted correction" }
            await withGitWrapper(
                f.temp,
                (real) =>
                    passThrough(
                        real,
                        "",
                        `if (result.status === 0 && args.includes('commit') && args.includes('--no-verify')) { fs.appendFileSync(${JSON.stringify(count)}, 'commit\\n'); process.kill(process.pid, 'SIGKILL'); }`,
                    ),
                async () => {
                    await assert.rejects(f.call("commitFixes", request), /Checkout preserved/)
                },
            )
            assert.equal(f.record().phase, "committingFixes")
            const resultSha = f.git(f.root, "rev-parse", "HEAD")
            const recovered = await f.call("commitFixes", request)
            assert.equal(recovered.phase, "committedFixes")
            assert.equal(recovered.fixCommit.resultSha, resultSha)
            assert.equal(fs.readFileSync(count, "utf8"), "commit\n")
            assert.equal(f.forkHead(), f.head)
        } finally {
            f.close()
        }
    })

    it("uses a normal non-forced fix push and preserves a concurrent contributor update", async () => {
        const f = fixture()
        try {
            await f.ready()
            fs.appendFileSync(path.join(f.root, "file.txt"), "approved local fix\n")
            const committed = await f.call("commitFixes", { paths: ["file.txt"], message: "fix: normal publication" })
            f.git(f.source, "checkout", "-b", "racing-fix", f.head)
            f.write(f.source, { "new-work.txt": "preserve contributor work\n" })
            f.git(f.source, "add", "new-work.txt")
            f.git(f.source, "commit", "-m", "contributor update")
            const newer = f.git(f.source, "rev-parse", "HEAD")
            f.git(f.source, "push", "--quiet", f.forkRemote, "HEAD:refs/heads/racing-fix")
            const argsFile = path.join(f.temp, "fix-push-args")
            await withGitWrapper(
                f.temp,
                (real) =>
                    passThrough(
                        real,
                        `if (args.includes('push')) { fs.writeFileSync(${JSON.stringify(argsFile)}, JSON.stringify(args)); cp.execFileSync(${JSON.stringify(real)}, ['--git-dir=${f.forkRemote}', 'update-ref', 'refs/heads/main', '${newer}', '${f.head}']); }`,
                    ),
                async () => {
                    const rejected = await f.call("publishFixes")
                    assert.equal(rejected.phase, "committedFixes")
                    assert.equal(rejected.fixCommit.publication.kind, "rejected")
                },
            )
            assert.equal(f.forkHead(), newer)
            const args: string[] = JSON.parse(fs.readFileSync(argsFile, "utf8"))
            assert.ok(args.includes(`${committed.fixCommit.resultSha}:refs/heads/main`))
            assert.equal(
                args.some((arg) => arg === "--force" || arg.startsWith("--force-with-lease")),
                false,
            )
        } finally {
            f.close()
        }
    })

    it("requires exact configured Project Done proof before cleanup", async () => {
        const f = fixture()
        try {
            const values = {
                backlog: "Backlog",
                todo: "Todo",
                inProgress: "In Progress",
                inReview: "In Review",
                done: "Done",
                canceled: "Canceled",
            }
            writeProjectConfig(f.root, {
                tracker: {
                    provider: "github",
                    mcpServer: "board",
                    repository: { owner: "upstream", repo: "app" },
                    project: { owner: "board-owner", ownerType: "user", number: 2 },
                    labels: kindLabels(),
                    fields: {
                        status: { field: "Status", values },
                        phase: {
                            field: "Phase",
                            values: { planning: "Planning", execution: "Execution", selfReview: "Self-Review" },
                        },
                        priority: { scope: "project", field: "Priority", values: ["High"] },
                    },
                },
            })
            await f.ready()
            f.merged()
            await assert.rejects(f.call("cleanup", { completion: { kind: "forgeOnly" } }), /Project Done/)
            const scope = { owner: "board-owner", owner_type: "user", project_number: 2 }
            const fieldEvidence = {
                tool: "mcp__board__projects_get",
                arguments: { method: "get_project_field", ...scope, field_id: 70 },
                response: {
                    id: 70,
                    name: "Status",
                    data_type: "single_select",
                    options: Object.values(values).map((name, index) => ({ id: String(index), name })),
                },
            }
            const itemEvidence = {
                tool: "mcp__board__projects_get",
                arguments: { method: "get_project_item", ...scope, item_id: 99, field_names: ["Status"] },
                response: {
                    id: 99,
                    content_type: "PullRequest",
                    content: { repository: "upstream/app", number: 12 },
                    fields: [{ id: 70, name: "Status", value: { name: "In Review" } }],
                },
            }
            const completion = { kind: "project", fieldEvidence, itemEvidence }
            await assert.rejects(f.call("cleanup", { completion }), /Done is unconfirmed/)
            assert.equal(f.record().phase, "fixes")
            itemEvidence.response.fields[0].value.name = "Done"
            assert.equal((await f.call("cleanup", { completion })).phase, "restored")
        } finally {
            f.close()
        }
    })
})
