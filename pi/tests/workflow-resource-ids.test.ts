import "./host-runtime.ts"

import assert from "node:assert/strict"
import * as cp from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { describe, it } from "node:test"

import {
    gitHubResourceNumber,
    invalidateResourceIds,
    OperationResourceIdsMemo,
    parseResourceIds,
    RESOURCE_IDS_CACHE_FILE,
    ResourceIdsCacheValidator,
    readResourceIdsCache,
    recordResourceIds,
    resolveCachedResourceIds,
} from "../workflows/extensions/integrations/resource-ids.ts"

const entity = "7-example"
const common = { entity, mcpServer: "github" }
const repository = { owner: "alex", repo: "example" }
// The removed `github-issue` kind, as caches written before its removal hold it: the reader rejects it.
const staleIssue = {
    scope: { ...common, kind: "github-issue", repository, issueNumber: 23 },
    value: { issueId: 5644393424, url: "https://github.com/alex/example/issues/23" },
}
const projectItem = {
    scope: {
        ...common,
        kind: "github-project-item" as const,
        repository,
        issueNumber: 23,
        project: { owner: "alex", ownerType: "user" as const, number: 2 },
    },
    value: { itemId: 258514508 },
}
const pullRequest = {
    scope: { ...common, kind: "github-pull-request" as const, repository, head: "task-branch", target: "main" },
    value: { number: 3, url: "https://github.com/alex/example/pull/3" },
}
const linearProject = {
    scope: {
        entity,
        mcpServer: "linear",
        kind: "linear-project" as const,
        team: "Engineering",
        projectId: "project-id",
    },
    value: { url: "https://linear.app/example/project/project-slug" },
}
const linearIssue = {
    scope: {
        entity,
        mcpServer: "linear",
        kind: "linear-issue" as const,
        team: "Engineering",
        issueId: "issue-id",
        relationship: { kind: "project" as const, projectId: "project-id" },
    },
    value: { identifier: "ENG-42", url: "https://linear.app/example/issue/ENG-42/work" },
}

function fixture() {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), "workflow-resourceIds-"))
    return { directory, cleanup: () => fs.rmSync(directory, { recursive: true, force: true }) }
}

function writeRaw(directory: string, raw: unknown) {
    const file = path.join(directory, RESOURCE_IDS_CACHE_FILE)
    fs.mkdirSync(path.dirname(file), { recursive: true })
    fs.writeFileSync(file, JSON.stringify(raw))
}

describe("local resource IDs storage", () => {
    it("reads an absent cache without creating files or fetching resources", async () => {
        const { directory, cleanup } = fixture()
        try {
            assert.equal(readResourceIdsCache(directory), undefined)
            assert.equal(await resolveCachedResourceIds(directory, projectItem.scope), undefined)
            assert.deepEqual(fs.readdirSync(directory), [])
        } finally {
            cleanup()
        }
    })

    it("records strict provider variants and reuses exact scopes", async () => {
        const { directory, cleanup } = fixture()
        try {
            const resourceIds = [projectItem, pullRequest, linearProject, linearIssue]
            await recordResourceIds(directory, resourceIds)
            assert.equal(ResourceIdsCacheValidator.check(readResourceIdsCache(directory)), true)
            for (const ids of resourceIds) {
                assert.deepEqual(await resolveCachedResourceIds(directory, ids.scope), ids)
            }
            const returned = await resolveCachedResourceIds(directory, projectItem.scope)
            returned!.scope.entity = "changed"
            assert.equal(readResourceIdsCache(directory)!.entity, entity)
        } finally {
            cleanup()
        }
    })

    it("queues full parallel updates and preserves other valid entries", async () => {
        const { directory, cleanup } = fixture()
        try {
            await Promise.all(
                [projectItem, pullRequest, linearProject, linearIssue].map((ids) =>
                    recordResourceIds(directory, [ids]),
                ),
            )
            assert.equal(readResourceIdsCache(directory)!.resourceIds.length, 4)
            await recordResourceIds(directory, [{ ...projectItem, value: { itemId: 42 } }])
            assert.deepEqual(await resolveCachedResourceIds(directory, linearProject.scope), linearProject)
            assert.deepEqual(await resolveCachedResourceIds(directory, pullRequest.scope), pullRequest)
            assert.deepEqual((await resolveCachedResourceIds(directory, projectItem.scope))!.value, { itemId: 42 })
        } finally {
            cleanup()
        }
    })

    it("discards only the mismatched kind when configuration or semantic scope changes", async () => {
        const { directory, cleanup } = fixture()
        try {
            for (const scope of [
                { ...projectItem.scope, mcpServer: "other" },
                { ...projectItem.scope, repository: { owner: "other", repo: "example" } },
                { ...projectItem.scope, issueNumber: 24 },
                { ...projectItem.scope, project: { ...projectItem.scope.project, number: 4 } },
                { ...projectItem.scope, project: { ...projectItem.scope.project, ownerType: "org" } },
            ]) {
                await recordResourceIds(directory, [pullRequest, projectItem])
                assert.equal(await resolveCachedResourceIds(directory, scope), undefined)
                assert.deepEqual(readResourceIdsCache(directory)!.resourceIds, [pullRequest])
            }
            await recordResourceIds(directory, [linearIssue])
            assert.equal(
                await resolveCachedResourceIds(directory, {
                    ...linearIssue.scope,
                    relationship: { kind: "projectless" },
                }),
                undefined,
            )
            await recordResourceIds(directory, [pullRequest])
            assert.equal(await resolveCachedResourceIds(directory, { ...pullRequest.scope, target: "epic" }), undefined)
        } finally {
            cleanup()
        }
    })

    it("does not use another entity's structurally valid cache", async () => {
        const { directory, cleanup } = fixture()
        try {
            await recordResourceIds(directory, [projectItem])
            const scope = { ...projectItem.scope, entity: "8-other" }
            assert.equal(await resolveCachedResourceIds(directory, scope), undefined)
            await recordResourceIds(directory, [{ ...projectItem, scope }])
            assert.equal(readResourceIdsCache(directory)!.entity, "8-other")
            assert.equal(await resolveCachedResourceIds(directory, projectItem.scope), undefined)
        } finally {
            cleanup()
        }
    })

    it("invalidates only the exact confirmed stale scope", async () => {
        const { directory, cleanup } = fixture()
        try {
            await recordResourceIds(directory, [pullRequest, projectItem])
            await invalidateResourceIds(directory, { ...projectItem.scope, issueNumber: 24 })
            assert.equal(readResourceIdsCache(directory)!.resourceIds.length, 2)
            await invalidateResourceIds(directory, projectItem.scope)
            assert.deepEqual(readResourceIdsCache(directory)!.resourceIds, [pullRequest])
        } finally {
            cleanup()
        }
    })

    it("rejects malformed JSON, extra fields, duplicate kinds, and wrong entry ownership without rewriting", async () => {
        const { directory, cleanup } = fixture()
        try {
            const file = path.join(directory, RESOURCE_IDS_CACHE_FILE)
            for (const raw of [
                { entity, resourceIds: [pullRequest], status: "Done" },
                { entity, resourceIds: [{ ...pullRequest, value: { ...pullRequest.value, body: "snapshot" } }] },
                { entity, resourceIds: [pullRequest, pullRequest] },
                { entity: "wrong", resourceIds: [pullRequest] },
                { entity, resourceIds: [{ ...pullRequest, scope: { ...pullRequest.scope, priority: "High" } }] },
                // A cache written before the `github-issue` kind was removed: the message says to delete it.
                { entity, resourceIds: [staleIssue, projectItem] },
            ]) {
                writeRaw(directory, raw)
                const before = fs.readFileSync(file, "utf-8")
                assert.throws(() => readResourceIdsCache(directory), /invalid resource IDs cache/)
                await assert.rejects(recordResourceIds(directory, [projectItem]), /invalid resource IDs cache/)
                assert.equal(fs.readFileSync(file, "utf-8"), before)
            }
            fs.writeFileSync(file, "{")
            assert.throws(() => readResourceIdsCache(directory), /inspect or remove this disposable cache/)
        } finally {
            cleanup()
        }
    })

    it("keeps immutable metadata and Git status unchanged", async () => {
        const { directory: root, cleanup } = fixture()
        try {
            const git = (args: string[]) => cp.execFileSync("git", args, { cwd: root, encoding: "utf-8" }).trim()
            git(["init", "-q"])
            const directory = path.join(root, ".piton/.local/tasks/work")
            fs.mkdirSync(directory, { recursive: true })
            const metadata = path.join(directory, "metadata.json")
            fs.writeFileSync(metadata, "immutable work identity\n")
            fs.writeFileSync(path.join(root, ".gitignore"), "/.piton/.local\n")
            git(["add", ".gitignore"])
            const status = git(["status", "--short"])
            const bytes = fs.readFileSync(metadata)
            await recordResourceIds(directory, [projectItem, pullRequest])
            assert.equal(git(["status", "--short"]), status)
            assert.deepEqual(fs.readFileSync(metadata), bytes)
            assert.equal(
                git(["check-ignore", "--", path.relative(root, path.join(directory, RESOURCE_IDS_CACHE_FILE))]),
                ".piton/.local/tasks/work/resource-ids.json",
            )
        } finally {
            cleanup()
        }
    })

    it("rejects resourceIds addressed to another repository or resource and parses PR numbers from URLs", () => {
        assert.equal(gitHubResourceNumber(pullRequest.value.url, repository, "pull"), 3)
        assert.throws(
            () => parseResourceIds({ ...pullRequest, value: { ...pullRequest.value, number: 4700971917 } }),
            /exact GitHub resource/,
        )
        assert.throws(
            () =>
                parseResourceIds({
                    ...pullRequest,
                    value: { ...pullRequest.value, url: "https://github.com/other/example/pull/3" },
                }),
            /exact GitHub resource/,
        )
        // The removed `github-issue` kind is no longer a valid entry.
        assert.throws(() => parseResourceIds(staleIssue), /invalid resource IDs/)
        assert.throws(
            () => parseResourceIds({ ...linearIssue, value: { ...linearIssue.value, url: linearProject.value.url } }),
            /wrong Linear resource/,
        )
        assert.throws(
            () => gitHubResourceNumber("https://github.com/alex/example/pull/3?other=1", repository, "pull"),
            /exact GitHub resource/,
        )
        assert.throws(
            () => parseResourceIds({ ...projectItem, value: { itemId: "PVTI_node" } }),
            /invalid resource IDs/,
        )
    })

    it("memoizes only scoped resourceIds and resets at operation boundaries", () => {
        const memo = new OperationResourceIdsMemo()
        memo.record(pullRequest)
        assert.deepEqual(memo.resolve(pullRequest.scope), pullRequest)
        assert.equal(memo.resolve({ ...pullRequest.scope, head: "other" }), undefined)
        memo.invalidate({ ...pullRequest.scope, target: "other" })
        assert.deepEqual(memo.resolve(pullRequest.scope), pullRequest)
        memo.invalidate(pullRequest.scope)
        assert.equal(memo.resolve(pullRequest.scope), undefined)
        memo.record(projectItem)
        assert.deepEqual(
            memo.resolve({ ...projectItem.scope, repository: { owner: "ALEX", repo: "EXAMPLE" } }),
            projectItem,
        )
        memo.reset()
        assert.equal(memo.resolve(projectItem.scope), undefined)
        assert.throws(
            () => memo.record({ ...pullRequest, value: { ...pullRequest.value, merged: true } }),
            /invalid resource IDs/,
        )
    })
})
