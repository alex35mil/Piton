import "./host-runtime.ts"

import assert from "node:assert/strict"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { describe, it } from "node:test"

import {
    EntityStatusValidator,
    resolveEntityDirectory,
    resolveEntityPath,
} from "../workflows/extensions/__lib/entity.ts"
import { TrackerRecordValidator } from "../workflows/extensions/integrations/records.ts"
import { parseReviewReport } from "../workflows/extensions/review/findings.ts"
import { RecordReviewResponseValidator } from "../workflows/extensions/review/index.ts"

const commonMetadata = {
    slug: "example",
    title: "Example",
    branch: {
        state: "ready",
        name: "example",
        start: "main",
        target: "main",
        source: "generated",
    },
    tracker: {
        provider: "github",
        repository: { owner: "example", repo: "project" },
        external: { issueNumber: 2 },
        planComment: { state: "unpublished" },
    },
}

const finding = {
    id: "F1",
    title: "Problem",
    problem: "Problem",
    useCase: "Supported case",
    impact: "Impact",
    resolution: "Correction",
    userDecision: null,
}

describe("workflow consolidation contracts", () => {
    it("keeps a strict metadata boundary with no local lifecycle, ID, or authority", () => {
        for (const entity of ["epic", "task"] as const) {
            const metadata = {
                ...commonMetadata,
                entity,
                ...(entity === "epic" ? {} : { kind: "feature", parent: { kind: "none" } }),
            }
            assert.equal(EntityStatusValidator.check(metadata), true)
            for (const extra of [
                { state: "inProgress" },
                { id: "TASK-01KDVDNA00" },
                { authority: { kind: "tracker", provider: "github" } },
                { integrations: [] },
                { parentEpic: "example" },
                { extra: true },
            ])
                assert.equal(EntityStatusValidator.check({ ...metadata, ...extra }), false)
            assert.equal(
                EntityStatusValidator.check({
                    ...metadata,
                    ...(entity === "epic" ? { kind: "feature" } : {}),
                    taskTarget: "epic",
                }),
                false,
            )
            const { tracker: _tracker, ...untracked } = metadata
            assert.equal(EntityStatusValidator.check(untracked), false)
            // Phase and creation time live on the tracker, never in metadata.
            assert.equal(EntityStatusValidator.check({ ...metadata, phase: "planning" }), false)
            assert.equal(EntityStatusValidator.check({ ...metadata, createdAt: "2026-01-01T00:00:00Z" }), false)
            assert.equal(
                EntityStatusValidator.check({
                    ...metadata,
                    branch: { ...metadata.branch, state: "tracker-pending" },
                }),
                false,
            )
        }
    })

    it("records each tracker object by its identity only", () => {
        const cases = [
            {
                provider: "github",
                repository: { owner: "example", repo: "project" },
                planComment: { state: "unpublished" },
                external: { issueNumber: 2 },
            },
            {
                provider: "linear",
                planDocument: { state: "unpublished" },
                resource: "issue",
                external: { issueId: "issue", identifier: "ENG-2" },
            },
            {
                provider: "linear",
                planDocument: { state: "unpublished" },
                resource: "project",
                external: { projectId: "p", createdAt: "2026-10-01T09:00:00.000Z" },
            },
        ]
        for (const record of cases) {
            assert.equal(TrackerRecordValidator.check(record), true)
            assert.equal(TrackerRecordValidator.check({ ...record, state: "bound" }), false)
            assert.equal(
                TrackerRecordValidator.check({
                    ...record,
                    operations: ["confirm"],
                }),
                false,
            )
            const { external: _external, ...unidentified } = record
            assert.equal(TrackerRecordValidator.check(unidentified), false)
        }
        assert.equal(
            TrackerRecordValidator.check({
                ...cases[2],
                external: { issueId: "issue", identifier: "eng-2" },
            }),
            false,
        )
    })

    it("keeps report semantics separate from response input validation", () => {
        const report = (overrides = {}) =>
            JSON.stringify({
                findings: [{ ...finding, severity: "blocking", ...overrides }],
                signoff: "blocked",
            })
        assert.doesNotThrow(() => parseReviewReport(report()))
        for (const overrides of [{ id: "F2" }, { title: " " }, { extra: true }, { userDecision: " " }]) {
            assert.throws(() => parseReviewReport(report(overrides)))
        }
        assert.throws(() => parseReviewReport(JSON.stringify({ findings: [], signoff: "blocked" })))
        assert.throws(() =>
            parseReviewReport(
                JSON.stringify({
                    findings: [{ ...finding, severity: "blocking" }],
                    signoff: "approved",
                }),
            ),
        )
        // An entity response names only what the agent doesn't simply fix.
        const response = { entityDir: "example", phase: "plan", round: 1, responses: [] as unknown[] }
        assert.equal(RecordReviewResponseValidator.check(response), true)
        const rejected = { id: "F1", outcome: "rejected", explanation: "Not a defect." }
        for (const [entry, valid] of [
            [rejected, true],
            [{ ...rejected, outcome: "needs-decision" }, true],
            [{ ...rejected, outcome: "accepted" }, false],
            [{ ...rejected, explanation: "" }, false],
            [{ ...rejected, severity: "blocking" }, false],
            [finding, false],
        ] as const)
            assert.equal(RecordReviewResponseValidator.check({ ...response, responses: [entry] }), valid)
    })

    it("preserves caller-relative bases and rejects paths outside workflow roots", () => {
        const root = fs.mkdtempSync(path.join(os.tmpdir(), "workflow-paths-"))
        try {
            const entity = path.join(root, ".piton", ".local", "tasks", "example")
            fs.mkdirSync(entity, { recursive: true })
            const cwd = path.join(root, "src")
            assert.equal(resolveEntityPath(root, cwd, ".piton/.local/tasks/example"), entity)
            assert.equal(resolveEntityPath(root, cwd, "../.piton/.local/tasks/example"), entity)
            assert.equal(resolveEntityPath(root, root, ".piton/.local/tasks/example"), entity)
            assert.equal(resolveEntityDirectory(root, cwd, entity), entity)
            for (const raw of [
                ".piton/.local/tasks",
                ".piton/.local/tasks/../../outside",
                ".piton/.local/tasks-sibling/example",
            ]) {
                assert.throws(() => resolveEntityPath(root, root, raw), /must be inside/)
            }
            assert.throws(() => resolveEntityDirectory(root, root, ".piton/.local/tasks/missing"), /does not exist/)
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })
})
