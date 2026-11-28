import "./host-runtime.ts"
import { writeProjectConfig } from "./config-fixture.ts"

import assert from "node:assert/strict"
import * as cp from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { describe, it } from "node:test"
import { phaseValues } from "./workflow-phase-fixtures.ts"

import { prepareArtifactPersistence } from "../workflows/extensions/__lib/project-config.ts"
import { beginPullRequestCreation, recordPullRequestCreation } from "../workflows/extensions/integrations/creation.ts"
import { buildIntegrationContext } from "../workflows/extensions/integrations/index.ts"
import {
    OperationResourceIdsMemo,
    readResourceIdsCache,
    recordResourceIds,
} from "../workflows/extensions/integrations/resource-ids.ts"
import { ResourceIdsToolValidator } from "../workflows/extensions/integrations/resource-id-tool.ts"
import {
    readTrackerIssue,
    trackerIssueBody,
    trackerIssueTitle,
} from "../workflows/extensions/integrations/issue-read.ts"
import { linkPullRequestIssue } from "../workflows/extensions/integrations/forge/github/issue-link.ts"
import { findPullRequest } from "../workflows/extensions/integrations/forge/github/find-pull-request.ts"
import { readEntityStatus } from "../workflows/extensions/__lib/entity.ts"
import {
    evidenceResourceKind,
    shippingReadDecision,
    invalidateStaleResourceIds,
    recordResourceIdsEvidence,
    resourceIdsFromRead,
    resolveResourceIdsContext,
    resolveResourceIds,
    selectPullRequestResourceIds,
} from "../workflows/extensions/integrations/resource-id-operations.ts"

import { workflowMcpCatalog } from "./mcp-catalog-fixture.ts"

const mcpTools = workflowMcpCatalog("github", "linear")

const repo = { owner: "alex", repo: "example" }
const common = { entity: "23-example", mcpServer: "github" }
const prScope = {
    ...common,
    kind: "github-pull-request" as const,
    repository: repo,
    head: "task-branch",
    target: "main",
}
const itemScope = {
    ...common,
    kind: "github-project-item" as const,
    repository: repo,
    issueNumber: 23,
    project: { owner: "alex", ownerType: "user" as const, number: 2 },
}
const pr = {
    number: 7,
    state: "open",
    draft: false,
    html_url: "https://github.com/alex/example/pull/7",
    head: { ref: "task-branch", repo: { full_name: "alex/example" } },
    base: { ref: "main", repo: { full_name: "alex/example" } },
}
const evidence = {
    tool: "mcp__github__pull_request_read",
    arguments: { method: "get", ...repo, pullNumber: 7 },
    response: pr,
}
// A fresh read of the fixture Task's issue #23, which PR creation takes its title from.
const issueRead = (title = "Example", issueNumber = 23) => ({
    tool: "mcp__github__issue_read",
    arguments: { method: "get", ...repo, issue_number: issueNumber },
    response: { number: issueNumber, title, body: "Request." },
})

function fixture() {
    const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "workflow-resource-ids-")))
    const git = (args: string[]) => cp.execFileSync("git", args, { cwd: root, encoding: "utf8", stdio: "pipe" }).trim()
    git(["init", "-q"])
    fs.writeFileSync(path.join(root, ".gitignore"), "/.piton/.local\n")
    fs.mkdirSync(path.join(root, ".piton"))
    const statuses = {
        backlog: "Backlog",
        todo: "Todo",
        inProgress: "In Progress",
        inReview: "In Review",
        done: "Done",
        canceled: "Canceled",
    }
    writeProjectConfig(root, {
        branchFormat: "identifier-title",
        tracker: {
            provider: "github",
            mcpServer: "github",
            repository: repo,
            project: itemScope.project,
            fields: {
                status: { field: "Status", values: statuses },
                phase: { field: "Phase", values: { ...phaseValues } },
                priority: { scope: "project", field: "Priority", values: ["High"] },
            },
        },
        forge: { provider: "github", mcpServer: "github", repository: repo },
    })
    prepareArtifactPersistence(root)
    const directory = path.join(root, ".piton/.local/tasks", common.entity)
    fs.mkdirSync(directory, { recursive: true })
    const metadata: Record<string, unknown> = {
        slug: "example",
        title: "Example",
        entity: "task",
        kind: "refactor",
        parent: { kind: "none" },
        branch: { state: "ready", name: prScope.head, start: "main", target: "main", source: "generated" },
        tracker: {
            provider: "github",
            repository: repo,
            external: { issueNumber: 23 },
            planComment: { state: "unpublished" },
        },
        forge: { provider: "github", repository: repo, head: prScope.head, target: prScope.target },
    }
    const file = path.join(directory, "metadata.json")
    fs.writeFileSync(file, JSON.stringify(metadata))
    // A saved final review round as the review tool leaves it.
    const finalRound = (
        round: number,
        signoff: "approved" | "blocked" | null,
        pass: Record<string, unknown> = { kind: "review" },
    ) => {
        const roundDir = path.join(directory, "reviews", `final-${round.toString().padStart(3, "0")}`)
        fs.mkdirSync(roundDir, { recursive: true })
        fs.writeFileSync(path.join(roundDir, "pass.json"), JSON.stringify(pass))
        fs.writeFileSync(
            path.join(roundDir, "outcome.json"),
            JSON.stringify({ exitCode: signoff === null ? 1 : 0, signoff }),
        )
    }
    return {
        root,
        directory,
        file,
        git,
        metadata,
        finalRound,
        context: () => resolveResourceIdsContext(root, directory, "github-pull-request"),
        cleanup: () => fs.rmSync(root, { recursive: true, force: true }),
    }
}

describe("resource ID operations", () => {
    it("uses native MCP results for exact cached-ID reads", () => {
        const response = { content: [{ type: "text", text: JSON.stringify(pr) }] }
        assert.deepEqual(resourceIdsFromRead(prScope, { ...evidence, response }, mcpTools).value, {
            number: 7,
            url: pr.html_url,
        })
        assert.throws(
            () => resourceIdsFromRead(prScope, { ...evidence, response: { ...response, isError: true } }, mcpTools),
            /MCP error/,
        )
    })

    it("reports missing IDs without lookup instructions or recovery actions", async () => {
        const f = fixture()
        try {
            const result = await resolveResourceIds(f.context(), new OperationResourceIdsMemo())
            assert.equal(result.state, "missing")
            assert.equal("tool" in result, false)
            assert.equal(readResourceIdsCache(f.directory), undefined)
            assert.equal(
                ResourceIdsToolValidator.check({
                    entityDir: f.directory,
                    resource: "github-pull-request",
                    action: "recover",
                    pages: [evidence],
                }),
                false,
            )
        } finally {
            f.cleanup()
        }
    })

    it("uses cached IDs and operation memo without provider capabilities", async () => {
        const f = fixture()
        try {
            await recordResourceIds(f.directory, [{ scope: prScope, value: { number: 7, url: pr.html_url } }])
            const memo = new OperationResourceIdsMemo()
            assert.equal((await resolveResourceIds(f.context(), memo)).source, "cache")
            assert.equal((await resolveResourceIds(f.context(), memo)).source, "operation")
            memo.reset()
            assert.equal((await resolveResourceIds(f.context(), memo)).source, "cache")
        } finally {
            f.cleanup()
        }
    })

    it("records native Project-addition database IDs in the cache and operation memo", async () => {
        const f = fixture()
        try {
            const context = resolveResourceIdsContext(f.root, f.directory, "github-project-item")
            const payload = { id: "PVTI_example", item_id: 42, full_database_id: "42" }
            for (const response of [
                payload,
                { content: [{ type: "text", text: JSON.stringify(payload) }] },
                { content: [], structuredContent: payload },
            ]) {
                const memo = new OperationResourceIdsMemo()
                const recorded = await recordResourceIdsEvidence(
                    context,
                    {
                        tool: "mcp__github__projects_write",
                        arguments: {
                            method: "add_project_item",
                            owner: "alex",
                            owner_type: "user",
                            project_number: 2,
                            item_owner: "alex",
                            item_repo: "example",
                            item_type: "issue",
                            issue_number: 23,
                        },
                        response,
                    },
                    memo,
                    mcpTools,
                )
                assert.deepEqual(recorded.value, { itemId: 42 })
                assert.deepEqual(memo.resolve(context.scope), recorded)
                assert.deepEqual(readResourceIdsCache(f.directory)!.resourceIds, [recorded])
            }
        } finally {
            f.cleanup()
        }
    })

    it("preserves Project IDs when addition evidence has invalid identity or scope", async () => {
        const f = fixture()
        try {
            const context = resolveResourceIdsContext(f.root, f.directory, "github-project-item")
            const memo = new OperationResourceIdsMemo()
            await recordResourceIds(f.directory, [{ scope: itemScope, value: { itemId: 77 } }])
            await resolveResourceIds(context, memo)
            const before = readResourceIdsCache(f.directory)
            const previous = memo.resolve(context.scope)
            const addition = {
                tool: "mcp__github__projects_write",
                arguments: {
                    method: "add_project_item",
                    owner: "alex",
                    owner_type: "user",
                    project_number: 2,
                    item_owner: "alex",
                    item_repo: "example",
                    item_type: "issue",
                    issue_number: 23,
                },
                response: { id: "PVTI_example", item_id: 42, full_database_id: "42" },
            }
            for (const item_id of [undefined, null, "PVTI_example", "42.5", 0, -1, 1.5, Number.MAX_SAFE_INTEGER + 1]) {
                await assert.rejects(
                    recordResourceIdsEvidence(
                        context,
                        { ...addition, response: { ...addition.response, item_id } },
                        memo,
                        mcpTools,
                    ),
                    /invalid Project item ID/,
                )
                assert.deepEqual(readResourceIdsCache(f.directory), before)
                assert.deepEqual(memo.resolve(context.scope), previous)
            }
            for (const invalid of [
                { ...addition, tool: "mcp__other__projects_write" },
                { ...addition, response: { isError: true, content: [], structuredContent: addition.response } },
                ...Object.entries({
                    method: "update_project_item",
                    owner: "other",
                    owner_type: "org",
                    project_number: 3,
                    item_owner: "other",
                    item_repo: "other",
                    item_type: "pull_request",
                    issue_number: 24,
                }).map(([key, value]) => ({ ...addition, arguments: { ...addition.arguments, [key]: value } })),
            ]) {
                await assert.rejects(recordResourceIdsEvidence(context, invalid, memo, mcpTools))
                assert.deepEqual(readResourceIdsCache(f.directory), before)
                assert.deepEqual(memo.resolve(context.scope), previous)
            }
        } finally {
            f.cleanup()
        }
    })

    it("records the exact required Project read, and only its Project item ID", async () => {
        const f = fixture()
        try {
            const context = resolveResourceIdsContext(f.root, f.directory, "github-project-item")
            await recordResourceIdsEvidence(
                context,
                {
                    tool: "mcp__github__projects_get",
                    arguments: {
                        method: "get_project_item",
                        owner: "alex",
                        owner_type: "user",
                        project_number: 2,
                        item_id: 42,
                    },
                    response: {
                        id: 42,
                        content_type: "Issue",
                        content: {
                            id: 101,
                            number: 23,
                            repository: "alex/example",
                            html_url: "https://github.com/alex/example/issues/23",
                        },
                    },
                },
                new OperationResourceIdsMemo(),
                mcpTools,
            )
            assert.deepEqual(
                readResourceIdsCache(f.directory)!.resourceIds.map((x) => x.scope.kind),
                ["github-project-item"],
            )
            assert.throws(
                () => resourceIdsFromRead(prScope, { ...evidence, tool: "mcp__other__pull_request_read" }, mcpTools),
                /mcp__github__pull_request_read/,
            )
            assert.throws(
                () =>
                    resourceIdsFromRead(
                        prScope,
                        {
                            ...evidence,
                            response: { ...pr, head: { ...pr.head, ref: "other" } },
                        },
                        mcpTools,
                    ),
                /exact durable/,
            )
        } finally {
            f.cleanup()
        }
    })

    it("accepts Project evidence without owner_type and rejects a mismatched Project address", async () => {
        const f = fixture()
        try {
            const context = resolveResourceIdsContext(f.root, f.directory, "github-project-item")
            const address = { owner: "alex", project_number: 2 }
            const add = {
                tool: "mcp__github__projects_write",
                arguments: {
                    method: "add_project_item",
                    ...address,
                    item_owner: "alex",
                    item_repo: "example",
                    item_type: "issue",
                    issue_number: 23,
                },
                response: { id: "PVTI_example", item_id: 42 },
            }
            const read = (number: number) => ({
                tool: "mcp__github__projects_get",
                arguments: { method: "get_project_item", ...address, item_id: 42 },
                response: { id: 42, content_type: "Issue", content: { number, repository: "alex/example" } },
            })
            const mismatches = [{ owner_type: "org" }, { owner: "other" }, { project_number: 3 }]

            const added = await recordResourceIdsEvidence(context, add, new OperationResourceIdsMemo(), mcpTools)
            assert.deepEqual(added.value, { itemId: 42 })
            assert.deepEqual(resourceIdsFromRead(itemScope, read(23), mcpTools).value, { itemId: 42 })
            for (const changed of mismatches)
                assert.throws(() => {
                    const evidence = read(23)
                    resourceIdsFromRead(
                        itemScope,
                        { ...evidence, arguments: { ...evidence.arguments, ...changed } },
                        mcpTools,
                    )
                }, /do not match the required Project/)

            const before = readResourceIdsCache(f.directory)
            for (const changed of mismatches) {
                const stale = read(24)
                await assert.rejects(
                    invalidateStaleResourceIds(
                        context,
                        { ...stale, arguments: { ...stale.arguments, ...changed } },
                        new OperationResourceIdsMemo(),
                        mcpTools,
                    ),
                    /do not match the required Project/,
                )
                assert.deepEqual(readResourceIdsCache(f.directory), before)
            }
            const result = await invalidateStaleResourceIds(context, read(24), new OperationResourceIdsMemo(), mcpTools)
            assert.equal(result.state, "missing")
            assert.deepEqual(readResourceIdsCache(f.directory)!.resourceIds, [])
        } finally {
            f.cleanup()
        }
    })

    it("derives the resource kind from the evidence tool and method, naming every accepted one on rejection", () => {
        const f = fixture()
        try {
            const kind = (tool: string, method: string | undefined, purpose: "record" | "invalidate" = "record") =>
                evidenceResourceKind(
                    f.root,
                    f.directory,
                    { tool, arguments: method === undefined ? {} : { method } },
                    purpose,
                    mcpTools,
                )
            // An issue read records nothing: GitHub issues are addressed by number.
            assert.throws(() => kind("mcp__github__issue_read", "get"))
            assert.equal(kind("mcp__github__projects_get", "get_project_item"), "github-project-item")
            assert.equal(kind("mcp__github__projects_write", "add_project_item"), "github-project-item")
            assert.equal(kind("mcp__github__pull_request_read", "get"), "github-pull-request")
            assert.equal(kind("mcp__github__projects_get", "get_project_item", "invalidate"), "github-project-item")
            assert.throws(
                () => kind("mcp__github__projects_list", "list_project_items"),
                (error: Error) =>
                    /doesn't accept evidence from mcp__github__projects_list \(method list_project_items\)/.test(
                        error.message,
                    ) &&
                    /mcp__github__projects_get \(method get_project_item\)/.test(error.message) &&
                    /mcp__github__projects_write \(method add_project_item\)/.test(error.message) &&
                    !/issue_read/.test(error.message.split("accepted evidence:")[1]) &&
                    /mcp__github__pull_request_read \(method get\)/.test(error.message),
            )
            assert.throws(() => kind("mcp__github__projects_get", "get_project_field"), /accepted evidence/)
            // An issue read can't prove stale IDs, so invalidation names only the reads that can.
            assert.throws(
                () => kind("mcp__github__issue_read", "get", "invalidate"),
                (error: Error) =>
                    !/issue_read/.test(error.message.split("accepted evidence:")[1]) &&
                    /projects_get \(method get_project_item\)/.test(error.message),
            )
            // Linear tools aren't accepted for a GitHub-tracked entity.
            assert.throws(() => kind("mcp__linear__get_issue", undefined), /accepted evidence/)
        } finally {
            f.cleanup()
        }
    })

    it("removes only proven-stale IDs without finding replacements", async () => {
        const f = fixture()
        try {
            const memo = new OperationResourceIdsMemo()
            await selectPullRequestResourceIds(f.context(), evidence, memo, mcpTools)
            await recordResourceIds(f.directory, [{ scope: itemScope, value: { itemId: 42 } }])
            await assert.rejects(
                invalidateStaleResourceIds(f.context(), { ...evidence, response: "403 denied" }, memo, mcpTools),
                /Provider evidence requires an object response/,
            )
            await assert.rejects(
                invalidateStaleResourceIds(f.context(), evidence, memo, mcpTools),
                /confirms the cached relationship/,
            )
            assert.equal(readResourceIdsCache(f.directory)!.resourceIds.length, 2)
            const result = await invalidateStaleResourceIds(
                f.context(),
                { ...evidence, response: { ...pr, head: { ...pr.head, ref: "changed" } } },
                memo,
                mcpTools,
            )
            assert.equal(result.state, "missing")
            assert.equal("tool" in result, false)
            assert.deepEqual(
                readResourceIdsCache(f.directory)!.resourceIds.map((x) => x.scope.kind),
                ["github-project-item"],
            )
            assert.deepEqual(fs.readdirSync(f.directory).sort(), ["metadata.json", "resource-ids.json"])
        } finally {
            f.cleanup()
        }
    })

    it("checks the recorded PR and local head directly for merge and completion", async () => {
        const f = fixture()
        try {
            f.git(["add", "."])
            f.git(["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "-qm", "Fixture"])
            f.git(["branch", prScope.head])
            const sha = f.git(["rev-parse", prScope.head])
            const actual = { ...pr, head: { ...pr.head, sha }, merged: false }
            const read = { ...evidence, response: actual }
            await selectPullRequestResourceIds(f.context(), read, new OperationResourceIdsMemo(), mcpTools)
            const decision = shippingReadDecision(prScope, read, sha, 7, mcpTools)
            assert.deepEqual(decision, {
                state: "merge-needed",
                arguments: {
                    ...repo,
                    pullNumber: 7,
                    merge_method: "squash",
                    expectedHeadSha: sha,
                },
            })
            const context = buildIntegrationContext(
                { cwd: f.root } as never,
                {
                    operation: "shippingMerge",
                    entityDir: f.directory,
                    evidence: read,
                },
                mcpTools,
            )
            if (context.state !== "enabled") assert.fail("enabled shipping expected")
            assert.deepEqual(context.roles.forge?.shipping, decision)
            const merged = { ...actual, merged: true, merged_at: "2026-10-01T08:10:44Z" }
            assert.equal(shippingReadDecision(prScope, { ...read, response: merged }, sha, 7, mcpTools).state, "merged")
            for (const response of [
                { ...actual, number: 8, html_url: "https://github.com/alex/example/pull/8" },
                { ...actual, head: { ...actual.head, sha: "0".repeat(40) } },
                { ...actual, head: { ...actual.head, ref: "other" } },
                { ...actual, merged: true },
            ])
                assert.equal(shippingReadDecision(prScope, { ...read, response }, sha, 7, mcpTools).state, "blocked")
            const selected = { ...actual, number: 8, html_url: "https://github.com/alex/example/pull/8" }
            await selectPullRequestResourceIds(
                f.context(),
                {
                    ...read,
                    arguments: { ...read.arguments, pullNumber: 8 },
                    response: selected,
                },
                new OperationResourceIdsMemo(),
                mcpTools,
            )
            const mismatch = buildIntegrationContext(
                { cwd: f.root } as never,
                {
                    operation: "shippingMerge",
                    entityDir: f.directory,
                    evidence: read,
                },
                mcpTools,
            )
            if (mismatch.state !== "enabled") assert.fail("enabled shipping expected")
            assert.equal(mismatch.roles.forge?.shipping?.state, "blocked")
        } finally {
            f.cleanup()
        }
    })

    it("refuses a second PR intent and never creates one for an Epic", async () => {
        const f = fixture()
        try {
            await assert.rejects(
                beginPullRequestCreation(f.root, f.directory, issueRead(), mcpTools),
                /PR intent already exists/,
            )
            const epicDir = path.join(f.root, ".piton/.local/epics/9-rollout")
            fs.mkdirSync(epicDir, { recursive: true })
            fs.writeFileSync(
                path.join(epicDir, "metadata.json"),
                JSON.stringify({
                    slug: "rollout",
                    title: "Rollout",
                    entity: "epic",
                    branch: { state: "ready", name: "9-rollout", start: "main", target: "main", source: "generated" },
                    tracker: {
                        provider: "github",
                        repository: repo,
                        external: { issueNumber: 9 },
                        planComment: { state: "unpublished" },
                    },
                }),
            )
            await assert.rejects(
                beginPullRequestCreation(f.root, epicDir, undefined, mcpTools),
                /Epics never create pull requests/,
            )
        } finally {
            f.cleanup()
        }
    })

    it("creates a new PR only after the newest final review cycle finished", async () => {
        const f = fixture()
        try {
            delete f.metadata.forge
            const reset = () => fs.writeFileSync(f.file, JSON.stringify(f.metadata))
            const begin = () => beginPullRequestCreation(f.root, f.directory, issueRead(), mcpTools)
            reset()
            await assert.rejects(begin(), /no final review has run/)
            f.finalRound(1, "blocked")
            await assert.rejects(begin(), /final-001 blocked/)
            // A failed round isn't a pass.
            f.finalRound(2, null)
            await assert.rejects(begin(), /final-001 blocked/)
            // A blocked verification pass finishes the cycle; its open concerns went to the user.
            f.finalRound(3, "blocked", { kind: "verification", of: 1 })
            assert.equal((await begin()).state, "intent")
            // A newer blocked review without its verification refuses, though an older cycle finished.
            reset()
            f.finalRound(4, "blocked", { kind: "review", newReview: "Behavior changed." })
            await assert.rejects(begin(), /final-004 blocked/)
            assert.equal("forge" in JSON.parse(fs.readFileSync(f.file, "utf8")), false)
            f.finalRound(5, "approved", { kind: "verification", of: 4 })
            assert.equal((await begin()).state, "intent")
        } finally {
            f.cleanup()
        }
    })

    it("saves no PR intent when the configured create tool is unavailable", async () => {
        const f = fixture()
        try {
            delete f.metadata.forge
            fs.writeFileSync(f.file, JSON.stringify(f.metadata))
            f.finalRound(1, "approved")
            const before = fs.readFileSync(f.file, "utf8")
            await assert.rejects(
                beginPullRequestCreation(f.root, f.directory, issueRead(), new Map()),
                /create_pull_request/,
            )
            assert.equal(fs.readFileSync(f.file, "utf8"), before)
        } finally {
            f.cleanup()
        }
    })

    it("opens the PR with the tracker issue's current title and saves it with the intent", async () => {
        const f = fixture()
        try {
            delete f.metadata.forge
            const reset = () => fs.writeFileSync(f.file, JSON.stringify(f.metadata))
            reset()
            f.finalRound(1, "approved")
            const before = fs.readFileSync(f.file, "utf8")
            // Without a read, or with a read of another issue, nothing is written.
            await assert.rejects(beginPullRequestCreation(f.root, f.directory, undefined, mcpTools), /fresh read/)
            await assert.rejects(
                beginPullRequestCreation(f.root, f.directory, issueRead("Retitled", 24), mcpTools),
                /issue_number must be 23/,
            )
            const untitled = { ...issueRead(), response: { number: 23, title: "" } }
            await assert.rejects(beginPullRequestCreation(f.root, f.directory, untitled, mcpTools), /one-line title/)
            assert.equal(fs.readFileSync(f.file, "utf8"), before)

            // The issue was retitled after initialization: the PR and metadata follow it.
            const begun = await beginPullRequestCreation(f.root, f.directory, issueRead("Retitled"), mcpTools)
            assert.equal(begun.mutation.arguments.title, "Retitled")
            assert.deepEqual(begun.title, { state: "updated", from: "Example", to: "Retitled" })
            const saved = JSON.parse(fs.readFileSync(f.file, "utf8"))
            assert.equal(saved.title, "Retitled")
            assert.equal(saved.forge.head, prScope.head)
            // Recording checks the create against the synced title.
            const created = {
                tool: begun.mutation.tool,
                arguments: { ...begun.mutation.arguments, body: "Approved" },
                response: { url: pr.html_url },
            }
            assert.equal(
                (await recordPullRequestCreation(f.root, f.directory, created, mcpTools)).resourceIds.length,
                1,
            )

            // An unchanged title reports unchanged.
            reset()
            const same = await beginPullRequestCreation(f.root, f.directory, issueRead(), mcpTools)
            assert.deepEqual(same.title, { state: "unchanged" })
        } finally {
            f.cleanup()
        }
    })

    it("reads a standalone Task Linear issue's title by its stored UUID", () => {
        const record = {
            provider: "linear" as const,
            resource: "issue" as const,
            external: { issueId: "issue-uuid", identifier: "ENG-5" },
            parent: { kind: "none" as const },
            planDocument: { state: "unpublished" as const },
        }
        const tracker = { provider: "linear", mcpServer: "linear" } as never
        const read = (id: string) => ({
            tool: "mcp__linear__get_issue",
            arguments: { id },
            response: { uuid: "issue-uuid", id: "ENG-5", title: "Retitled", description: "Request." },
        })
        const issue = readTrackerIssue(record, tracker, read("issue-uuid"), mcpTools, "tracker issue")
        assert.equal(trackerIssueTitle(issue), "Retitled")
        assert.equal(trackerIssueBody(issue), "Request.")
        assert.throws(
            () => readTrackerIssue(record, tracker, read("ENG-5"), mcpTools, "tracker issue"),
            /argument id must be/,
        )
    })

    it("keeps the stored title when there is no tracker issue to read", async () => {
        const f = fixture()
        try {
            f.finalRound(1, "approved")
            const { forge: _forge, ...withoutForge } = f.metadata
            const begin = async (
                metadata: Record<string, unknown>,
                read?: ReturnType<typeof issueRead>,
                directory = f.directory,
            ) => {
                fs.writeFileSync(path.join(directory, "metadata.json"), JSON.stringify(metadata))
                return beginPullRequestCreation(f.root, directory, read, mcpTools)
            }
            // A local Task has no issue: a read is refused, and the stored title stands.
            const localDir = path.join(f.root, ".piton/.local/tasks/20261001-example")
            fs.cpSync(path.join(f.directory, "reviews"), path.join(localDir, "reviews"), { recursive: true })
            const local = { ...withoutForge, tracker: { provider: "local", createdAt: "2026-10-01T00:00:00Z" } }
            await assert.rejects(begin(local, issueRead("Retitled"), localDir), /no issue to read/)
            const localBegun = await begin(local, undefined, localDir)
            assert.equal(localBegun.mutation.arguments.title, "Example")
            assert.deepEqual(localBegun.title, { state: "unchanged" })

            // With the tracker role off, a tracked Task still submits with its stored title.
            writeProjectConfig(f.root, {
                tracker: { provider: "none" },
                forge: { provider: "github", mcpServer: "github", repository: repo },
            })
            await assert.rejects(begin(withoutForge, issueRead("Retitled")), /no issue to read/)
            assert.equal((await begin(withoutForge)).mutation.arguments.title, "Example")
        } finally {
            f.cleanup()
        }
    })

    for (const format of ["payload", "mcpText", "mcpStructured"] as const) {
        it(`records creation and selection locally after one submission commit (${format})`, async () => {
            const f = fixture()
            try {
                delete f.metadata.forge
                fs.writeFileSync(f.file, JSON.stringify(f.metadata))
                f.finalRound(1, "approved")
                f.git(["add", "."])
                f.git(["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "-qm", "Submission"])
                const begun = await beginPullRequestCreation(f.root, f.directory, issueRead(), mcpTools)
                assert.deepEqual(begun.mutation, {
                    tool: "mcp__github__create_pull_request",
                    arguments: { ...repo, head: prScope.head, base: prScope.target, title: "Example" },
                })
                const intent = fs.readFileSync(f.file, "utf8")
                assert.deepEqual(JSON.parse(intent).forge, {
                    provider: "github",
                    repository: repo,
                    head: prScope.head,
                    target: prScope.target,
                })
                const payload = { id: "4700971917", url: pr.html_url }
                const responses = {
                    payload,
                    mcpText: { content: [{ type: "text", text: JSON.stringify(payload) }] },
                    mcpStructured: { content: [], structuredContent: payload },
                }
                const created = {
                    tool: begun.mutation.tool,
                    arguments: { ...begun.mutation.arguments, body: "Approved" },
                    response: responses[format],
                }
                // A short provider name is rejected with the expected name and leaves nothing recorded.
                await assert.rejects(
                    recordPullRequestCreation(
                        f.root,
                        f.directory,
                        { ...created, tool: "create_pull_request" },
                        mcpTools,
                    ),
                    (error: Error) =>
                        error.message.includes("mcp__github__create_pull_request") &&
                        error.message.includes("got create_pull_request"),
                )
                assert.equal(readResourceIdsCache(f.directory), undefined)
                const result = await recordPullRequestCreation(f.root, f.directory, created, mcpTools)
                assert.deepEqual(Object.keys(result).sort(), ["entity", "resourceIds"])
                assert.deepEqual(result.resourceIds[0].value, { number: 7, url: pr.html_url })
                const recorded = readResourceIdsCache(f.directory)
                await assert.rejects(
                    recordPullRequestCreation(
                        f.root,
                        f.directory,
                        {
                            tool: "mcp__github__create_pull_request",
                            arguments: { ...repo, head: prScope.head, base: prScope.target, title: "Example" },
                            response: { content: [], structuredContent: payload, isError: true },
                        },
                        mcpTools,
                    ),
                    /MCP error/,
                )
                assert.deepEqual(readResourceIdsCache(f.directory), recorded)
                await selectPullRequestResourceIds(f.context(), evidence, new OperationResourceIdsMemo(), mcpTools)
                assert.equal(fs.readFileSync(f.file, "utf8"), intent)
                assert.deepEqual(fs.readdirSync(f.directory).sort(), ["metadata.json", "resource-ids.json", "reviews"])
                assert.equal(f.git(["rev-list", "--count", "HEAD"]), "1")
                assert.equal(f.git(["status", "--porcelain"]), "")
                assert.equal(f.git(["ls-files", "--", ".piton/.local"]), "")
            } finally {
                f.cleanup()
            }
        })
    }

    it("links the recorded PR to its issue and confirms it from GitHub's closing references", async () => {
        const f = fixture()
        try {
            fs.writeFileSync(
                path.join(f.root, ".piton/mcp.json"),
                JSON.stringify({
                    mcpServers: {
                        github: {
                            url: "https://api.githubcopilot.com/mcp/",
                            headers: { Authorization: "Bearer ${LINK_TOKEN}" },
                        },
                    },
                }),
            )
            const status = readEntityStatus(f.directory)
            // A fake GitHub: whether the PR references issue #23, and whether a link request applies or fails.
            const github = (linked: boolean, link: "applies" | "ignored" | "fails") => {
                const calls: string[] = []
                const fetchImpl = (async (_url: string, init: RequestInit) => {
                    assert.equal((init.headers as Record<string, string>).Authorization, "Bearer secret")
                    const { query, variables } = JSON.parse(String(init.body))
                    if (query.includes("addCloseIssueReferences")) {
                        calls.push("link")
                        assert.deepEqual(variables, { issue: "I_23", pr: "PR_7" })
                        if (link === "applies" || link === "fails") linked = true
                        if (link === "fails") throw new TypeError("fetch failed")
                        return Response.json({ data: { addCloseIssueReferences: {} } })
                    }
                    calls.push("read")
                    assert.deepEqual(variables, { ...repo, issue: 23, pr: 7 })
                    const nodes = [{ number: 5, repository: { nameWithOwner: "alex/example" } }]
                    if (linked) nodes.push({ number: 23, repository: { nameWithOwner: "alex/example" } })
                    return Response.json({
                        data: {
                            repository: {
                                issue: { id: "I_23" },
                                pullRequest: { id: "PR_7", closingIssuesReferences: { nodes } },
                            },
                        },
                    })
                }) as unknown as typeof fetch
                return { calls, fetchImpl }
            }
            const link = (fetchImpl: typeof fetch, env: NodeJS.ProcessEnv = { LINK_TOKEN: "secret" }) =>
                linkPullRequestIssue(f.root, status, 7, { projectTrusted: true, fetch: fetchImpl, env })

            const existing = github(true, "applies")
            assert.deepEqual(await link(existing.fetchImpl), { state: "linked", issueNumber: 23 })
            assert.deepEqual(existing.calls, ["read"])
            const added = github(false, "applies")
            assert.deepEqual(await link(added.fetchImpl), { state: "linked", issueNumber: 23 })
            assert.deepEqual(added.calls, ["read", "link", "read"])
            // A failed request that still applied is decided by the read-back.
            assert.deepEqual(await link(github(false, "fails").fetchImpl), { state: "linked", issueNumber: 23 })
            assert.deepEqual(await link(github(false, "ignored").fetchImpl), { state: "unconfirmed", issueNumber: 23 })
            assert.deepEqual(await link(github(false, "applies").fetchImpl, {}), {
                state: "failed",
                reason: "environment variable LINK_TOKEN is not set",
            })
            const local = { ...status, tracker: { provider: "local" as const, createdAt: "2026-10-01T00:00:00Z" } }
            assert.deepEqual(await linkPullRequestIssue(f.root, local, 7, { projectTrusted: true }), {
                state: "not-applicable",
            })
        } finally {
            f.cleanup()
        }
    })

    it("finds the PR among the issue's linked PRs first, then by head and base", async () => {
        const f = fixture()
        try {
            fs.writeFileSync(
                path.join(f.root, ".piton/mcp.json"),
                JSON.stringify({
                    mcpServers: {
                        github: {
                            url: "https://api.githubcopilot.com/mcp/",
                            headers: { Authorization: "Bearer ${LINK_TOKEN}" },
                        },
                    },
                }),
            )
            f.git(["add", "."])
            f.git(["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "-qm", "Fixture"])
            f.git(["branch", prScope.head])
            const sha = f.git(["rev-parse", prScope.head])
            const status = readEntityStatus(f.directory)
            const node = (number: number, overrides: Record<string, unknown> = {}) => ({
                number,
                url: `https://github.com/alex/example/pull/${number}`,
                state: "OPEN",
                headRefName: prScope.head,
                baseRefName: "main",
                headRefOid: sha,
                headRepository: { nameWithOwner: "alex/example" },
                baseRepository: { nameWithOwner: "alex/example" },
                ...overrides,
            })
            // A fake GitHub answering the linked-PR and head/base queries, each split into one-node pages.
            const github = (linked: object[], listed: object[]) => {
                const calls: string[] = []
                const page = (nodes: object[], after: string | null) => {
                    const index = after === null ? 0 : Number(after)
                    const more = index + 1 < nodes.length
                    return {
                        nodes: nodes.slice(index, index + 1),
                        pageInfo: { hasNextPage: more, endCursor: more ? String(index + 1) : null },
                    }
                }
                const fetchImpl = (async (_url: string, init: RequestInit) => {
                    const { query, variables } = JSON.parse(String(init.body))
                    if (query.includes("closedByPullRequestsReferences")) {
                        calls.push("linked")
                        assert.equal(variables.issue, 23)
                        return Response.json({
                            data: {
                                repository: {
                                    issue: { closedByPullRequestsReferences: page(linked, variables.after) },
                                },
                            },
                        })
                    }
                    calls.push("head-base")
                    assert.deepEqual([variables.head, variables.base], [prScope.head, "main"])
                    return Response.json({ data: { repository: { pullRequests: page(listed, variables.after) } } })
                }) as unknown as typeof fetch
                return { calls, fetchImpl }
            }
            const find = (fetchImpl: typeof fetch, entity = status) =>
                findPullRequest(f.root, entity, {
                    projectTrusted: true,
                    fetch: fetchImpl,
                    env: { LINK_TOKEN: "secret" },
                })

            const linked = github([node(5, { headRefName: "other" }), node(7, { state: "MERGED" })], [])
            assert.deepEqual(await find(linked.fetchImpl), {
                state: "found",
                source: "linked",
                pullRequest: { number: 7, url: "https://github.com/alex/example/pull/7", state: "merged" },
                rejected: [
                    {
                        pullRequest: { number: 5, url: "https://github.com/alex/example/pull/5", state: "open" },
                        mismatches: ["head"],
                    },
                ],
            })
            assert.deepEqual(linked.calls, ["linked", "linked"])

            // No linked match: the head and base listing decides, and a stale head or fork never matches.
            const fallback = github(
                [],
                [
                    node(3, { headRefOid: "0".repeat(40), state: "CLOSED" }),
                    node(8, { headRepository: { nameWithOwner: "someone/example" } }),
                    node(9),
                ],
            )
            const found = await find(fallback.fetchImpl)
            assert.equal(found.state === "found" && found.source, "head-base")
            assert.equal(found.state === "found" && found.pullRequest.number, 9)
            assert.deepEqual(
                found.rejected.map((entry) => [entry.pullRequest.number, entry.mismatches]),
                [
                    [3, ["head commit"]],
                    [8, ["head repository"]],
                ],
            )
            assert.equal((await find(github([node(7), node(9)], []).fetchImpl)).state, "ambiguous")
            assert.equal((await find(github([], []).fetchImpl)).state, "none")
            // Without a GitHub issue, only the head and base listing is read.
            const local = { ...status, tracker: { provider: "local" as const, createdAt: "2026-10-01T00:00:00Z" } }
            const listing = github([], [node(9)])
            assert.equal((await find(listing.fetchImpl, local)).state, "found")
            assert.deepEqual(listing.calls, ["head-base"])
        } finally {
            f.cleanup()
        }
    })
})
