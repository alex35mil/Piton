import "./host-runtime.ts"

import assert from "node:assert/strict"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { describe, it } from "node:test"

import { parse } from "../extensions/__lib/schema.ts"
import { PhaseValidator, projectedPhase, type LifecycleState } from "../workflows/extensions/__lib/domain.ts"
import { assertReviewPreconditions } from "../workflows/extensions/review/index.ts"
import { GitHubTrackerValidator, LinearTrackerValidator } from "../workflows/extensions/integrations/config.ts"
import { githubPhaseFacts } from "../workflows/extensions/integrations/github-phase-evidence.ts"
import {
    NativePhaseFactsValidator,
    phaseProjectionContract,
    initialPhase,
    planPhaseMutation,
    resolveLinearPhaseGroup,
    trackerPhase,
    type NativePhaseFacts,
} from "../workflows/extensions/integrations/phase.ts"
import { itemFieldEntry, itemFieldValue } from "../workflows/extensions/integrations/github-project-reads.ts"
import { requireProviderCall } from "../workflows/extensions/integrations/provider-evidence.ts"
import { resourceIdsFromRead } from "../workflows/extensions/integrations/resource-id-operations.ts"
import type { EntityStatus } from "../workflows/extensions/__lib/entity.ts"
import { workflowMcpCatalog } from "./mcp-catalog-fixture.ts"
import { githubPhaseReads, kindLabels, linearPhases, phaseValues } from "./workflow-phase-fixtures.ts"

/** "accepted" or "rejected", so two checks can be compared on the same input. */
function attempt(check: () => unknown): "accepted" | "rejected" {
    try {
        check()
        return "accepted"
    } catch {
        return "rejected"
    }
}

const statuses = {
    backlog: "Backlog",
    todo: "Todo",
    inProgress: "In Progress",
    inReview: "In Review",
    done: "Done",
    canceled: "Canceled",
}
const github = parse(GitHubTrackerValidator, {
    provider: "github",
    mcpServer: "github",
    repository: { owner: "example", repo: "example" },
    project: { owner: "example", ownerType: "user", number: 1 },
    labels: kindLabels(),
    fields: {
        status: { field: "Status", values: statuses },
        phase: { field: "Phase", values: phaseValues },
        priority: { scope: "project", field: "Priority", values: ["High"] },
    },
})
const linear = parse(LinearTrackerValidator, {
    provider: "linear",
    mcpServer: "linear",
    team: "Engineering",
    phase: linearPhases(),
    statuses: { projects: statuses, issues: statuses },
})
const githubFacts: Extract<NativePhaseFacts, { provider: "github" }> = {
    provider: "github",
    field: { name: "Phase", dataType: "SINGLE_SELECT" },
    current: null,
}
function linearFacts(): Extract<NativePhaseFacts, { provider: "linear" }> {
    const label = {
        team: null,
        parentId: null,
        isGroup: false,
        groupType: null,
        retiredAt: null,
    }
    return {
        provider: "linear",
        catalog: [
            {
                ...label,
                id: "phase",
                name: "Phase",
                isGroup: true,
                groupType: "singleSelect",
            },
            ...Object.entries(phaseValues).map(([id, name]) => ({
                ...label,
                id,
                name,
                parentId: "phase",
            })),
            {
                ...label,
                id: "other-group",
                name: "Other",
                isGroup: true,
                groupType: "singleSelect",
            },
            {
                ...label,
                id: "other-execution",
                name: "Execution",
                parentId: "other-group",
            },
        ],
        applied: ["customer-label", "other-execution", "planning"],
    }
}
const lifecycles: LifecycleState[] = ["backlog", "todo", "inProgress", "inReview", "done", "canceled"]

describe("shared workflow Phase", () => {
    it("projects exactly the active Phase only while In Progress for every entity and provider", () => {
        for (const entity of ["epic", "task", "gig"] as const) {
            for (const phase of ["planning", "execution", "selfReview"] as const) {
                assert.equal(PhaseValidator.check(phase), true)
                for (const lifecycle of lifecycles) {
                    const expected = lifecycle === "inProgress" ? phase : null
                    assert.equal(projectedPhase(lifecycle, phase), expected)
                    const field = planPhaseMutation(github, entity, phase, lifecycle, githubFacts)
                    assert.equal(field.provider, "github")
                    if (field.provider !== "github") assert.fail("expected GitHub")
                    assert.equal(field.value, expected === null ? null : phaseValues[expected])
                    const labels = planPhaseMutation(linear, entity, phase, lifecycle, linearFacts())
                    assert.equal(labels.provider, "linear")
                    if (labels.provider !== "linear") assert.fail("expected Linear")
                    assert.deepEqual(labels.labels, [
                        "customer-label",
                        "other-execution",
                        ...(expected === null ? [] : [expected]),
                    ])
                }
            }
        }
        assert.equal(PhaseValidator.check("paused"), false)
    })

    it("returns a local lifecycle-dependent contract, not an inferred tracker status", () => {
        for (const tracker of [github, linear]) {
            const contract = phaseProjectionContract("selfReview", tracker, "gig")
            if (contract.state !== "configured") assert.fail("expected configured Phase")
            assert.equal(contract.inProgress, "selfReview")
            assert.equal(contract.otherwise, null)
            assert.match(contract.authority, /read fresh native lifecycle/)
        }
    })

    it("tracks no Phase when the tracker doesn't configure it", () => {
        const githubWithout = structuredClone(github)
        delete githubWithout.fields.phase
        const linearWithout = structuredClone(linear)
        delete linearWithout.phase
        for (const [tracker, facts] of [
            [githubWithout, githubFacts],
            [linearWithout, linearFacts()],
        ] as const) {
            assert.equal(trackerPhase(tracker).state, "notConfigured")
            assert.deepEqual(initialPhase(tracker, "planning"), {})
            assert.equal(phaseProjectionContract(undefined, tracker, "gig").state, "notConfigured")
            assert.throws(() => planPhaseMutation(tracker, "gig", "execution", "inProgress", facts), /isn't configured/)
        }
        assert.deepEqual(initialPhase(github, "planning"), { phase: "planning" })
        assert.throws(() => resolveLinearPhaseGroup(linearWithout, "gig", linearFacts().catalog), /isn't configured/)
    })

    it("clears native Phase for an entity whose metadata has no Phase yet, but won't project one", () => {
        const selected = { ...githubFacts, current: phaseValues.execution }
        assert.deepEqual(planPhaseMutation(github, "gig", undefined, "inReview", selected), {
            provider: "github",
            field: "Phase",
            value: null,
            changed: true,
        })
        assert.throws(() => planPhaseMutation(github, "gig", undefined, "inProgress", selected), /set it/)
        const contract = phaseProjectionContract(undefined, github, "gig")
        if (contract.state !== "configured") assert.fail("expected configured Phase")
        assert.equal(contract.phase, null)
        assert.equal(contract.inProgress, null)
    })

    it("recognizes no-op and clearing assignments without losing unrelated labels", () => {
        const facts = linearFacts()
        facts.applied = ["customer-label", "execution", "other-execution"]
        assert.equal(planPhaseMutation(linear, "gig", "execution", "inProgress", facts).changed, false)
        const cleared = planPhaseMutation(linear, "gig", "selfReview", "inReview", facts)
        assert.equal(cleared.provider, "linear")
        if (cleared.provider !== "linear") assert.fail("expected Linear")
        assert.deepEqual(cleared.labels, ["customer-label", "other-execution"])
        assert.equal(cleared.changed, true)
        assert.equal(
            planPhaseMutation(github, "gig", "selfReview", "inProgress", {
                ...githubFacts,
                current: "Self-Review",
            }).changed,
            false,
        )
        assert.equal(planPhaseMutation(github, "gig", "selfReview", "done", githubFacts).changed, false)
    })

    it("requires the exact GitHub field name and type", () => {
        for (const field of [
            { ...githubFacts.field, name: "Other" },
            { ...githubFacts.field, dataType: "TEXT" },
        ])
            assert.throws(
                () =>
                    planPhaseMutation(github, "gig", "planning", "inProgress", {
                        ...githubFacts,
                        field,
                    }),
                /Phase/,
            )
        assert.throws(() => planPhaseMutation(github, "gig", "planning", "inProgress", linearFacts()), /provider/)
        assert.equal(NativePhaseFactsValidator.check(githubFacts), true)
        assert.equal(NativePhaseFactsValidator.check(linearFacts()), true)
    })

    describe("GitHub Phase evidence", () => {
        const mcpTools = workflowMcpCatalog("github", "other")
        const target = { server: "github", project: github.project, repository: github.repository, issueNumber: 7 }
        const status = {
            tracker: {
                provider: "github",
                repository: github.repository,
                external: { issueNumber: 7 },
                planComment: { state: "unpublished" },
            },
        } as EntityStatus
        const entity = { name: "7-example", status }
        const facts = (input: ReturnType<typeof githubPhaseReads>) => githubPhaseFacts(github, entity, input, mcpTools)
        type Reads = ReturnType<typeof githubPhaseReads>
        const edit = (change: (reads: Reads) => void) => {
            const reads = githubPhaseReads(target)
            change(reads)
            return reads
        }

        const plan = (reads: Reads) => {
            const { lifecycle, native } = facts(reads)
            return planPhaseMutation(github, "gig", "planning", lifecycle, native)
        }

        it("extracts lifecycle and Phase from one item read as the MCP returns it", () => {
            const selected = facts(githubPhaseReads(target, "Execution"))
            assert.deepEqual(selected, {
                lifecycle: "inProgress",
                native: {
                    provider: "github",
                    field: { name: "Phase", dataType: "single_select" },
                    current: "Execution",
                },
            })
            assert.equal(
                planPhaseMutation(github, "gig", "execution", selected.lifecycle, selected.native).changed,
                false,
            )
            assert.deepEqual(plan(githubPhaseReads(target, null)), {
                provider: "github",
                field: "Phase",
                value: "Planning",
                changed: true,
            })
            // Complete native MCP results are accepted as well as decoded JSON.
            const reads = githubPhaseReads(target)
            const wrapped = {
                item: {
                    ...reads.item,
                    response: { content: [{ type: "text", text: JSON.stringify(reads.item.response) }] },
                },
            }
            assert.equal(facts(wrapped as unknown as Reads).native.provider, "github")
        })

        it("clears Phase when the item read, taken after a Status change, shows another lifecycle", () => {
            const reads = githubPhaseReads(target, "Self-Review", "In Review")
            assert.equal(facts(reads).lifecycle, "inReview")
            assert.deepEqual(plan(reads), { provider: "github", field: "Phase", value: null, changed: true })
            assert.throws(() => facts(githubPhaseReads(target, "Planning", "Blocked")), /configured: Backlog, Todo/)
            assert.throws(
                () => facts(edit((r) => delete (r.item.response.fields[0] as { value?: unknown }).value)),
                /no Status/,
            )
        })

        it("rejects reads outside the configured field, Project, or entity issue", () => {
            const rejected: Array<[string, Reads, RegExp]> = [
                // Wrong-tool errors name the expected registered tool (#156).
                [
                    "item server",
                    edit((r) => (r.item.tool = "mcp__other__projects_get")),
                    /must come from mcp__github__projects_get/,
                ],
                ["item owner_type", edit((r) => (r.item.arguments.owner_type = "org")), /required Project/],
                ["item method", edit((r) => (r.item.arguments.method = "get_project_field")), /get_project_item/],
                ["item Project", edit((r) => (r.item.arguments.project_number = 9)), /project_number/],
                ["Phase type", edit((r) => (r.item.response.fields[1].data_type = "text")), /single-select/],
                ["other issue", githubPhaseReads({ ...target, issueNumber: 8 }), /durable issue/],
                [
                    "other repository",
                    githubPhaseReads({ ...target, repository: { owner: "example", repo: "other" } }),
                    /durable issue/,
                ],
                ["pull request item", edit((r) => (r.item.response.content_type = "PullRequest")), /durable issue/],
                ["item_id", edit((r) => (r.item.arguments.item_id = 1)), /item_id/],
                ["field_names Phase", edit((r) => (r.item.arguments.field_names = ["Status"])), /missing Phase/],
                ["field_names Status", edit((r) => (r.item.arguments.field_names = ["Phase"])), /missing Status/],
                ["no Phase entry", edit((r) => r.item.response.fields.pop()), /no Phase entry/],
                ["no Status entry", edit((r) => r.item.response.fields.shift()), /no Status entry/],
                [
                    "duplicate Phase",
                    edit((r) => r.item.response.fields.push({ ...r.item.response.fields[1] })),
                    /Ambiguous/,
                ],
            ]
            // Both tools parse the read, then plan the mutation, which validates the field's name and type.
            for (const [name, reads, error] of rejected) assert.throws(() => plan(reads), error, name)
            assert.throws(
                () => githubPhaseFacts(linear, entity, githubPhaseReads(target), mcpTools),
                /configured GitHub tracker/,
            )
        })

        it("accepts Phase reads that omit the optional owner_type", () => {
            const reads = edit((r) => delete (r.item.arguments as { owner_type?: string }).owner_type)
            assert.equal(facts(reads).native.provider, "github")
        })

        it("accepts and rejects the same item reads as resource-ID recording", () => {
            const scope = {
                kind: "github-project-item" as const,
                entity: entity.name,
                mcpServer: "github",
                repository: github.repository,
                issueNumber: 7,
                project: github.project,
            }
            for (const reads of [
                githubPhaseReads(target),
                githubPhaseReads({ ...target, issueNumber: 8 }),
                githubPhaseReads({ ...target, repository: { owner: "example", repo: "other" } }),
                edit((r) => (r.item.response.content_type = "PullRequest")),
                edit((r) => (r.item.arguments.owner = "EXAMPLE")),
            ]) {
                const recorded = attempt(() => resourceIdsFromRead(scope, reads.item, mcpTools))
                assert.equal(
                    attempt(() => facts(reads)),
                    recorded,
                )
            }
        })

        it("shares one provider-call check and Project item field parsing", () => {
            const reads = githubPhaseReads(target)
            const call = (expected: Record<string, unknown>) => () =>
                requireProviderCall(
                    reads.item,
                    { server: "github", operation: "projects_get", expected },
                    mcpTools,
                    "Project",
                )
            assert.doesNotThrow(call({ owner: "EXAMPLE", method: "get_project_item" }))
            // A mismatch names the expected and received values.
            assert.throws(
                call({ method: "GET_PROJECT_ITEM" }),
                /method must be "GET_PROJECT_ITEM", got "get_project_item"/,
            )
            assert.throws(call({ project_number: 2 }), /project_number must be 2, got 1/)
            assert.throws(call({ field_id: 2 }), /field_id must be 2, got nothing/)
            const response = reads.item.response as unknown as Record<string, unknown>
            assert.deepEqual(itemFieldEntry(response, "Phase"), {
                state: "present",
                id: 427,
                dataType: "single_select",
                value: "Planning",
            })
            assert.deepEqual(itemFieldEntry(response, "Priority"), { state: "absent" })
            assert.deepEqual(itemFieldValue(response, { id: 427, name: "Phase" }), {
                state: "present",
                value: "Planning",
            })
            assert.deepEqual(itemFieldValue(response, { id: 1, name: "Priority" }), { state: "absent" })
            const cleared = githubPhaseReads(target, null).item.response as unknown as Record<string, unknown>
            assert.deepEqual(itemFieldValue(cleared, { id: 427, name: "Phase" }), { state: "present", value: null })
            assert.throws(() => itemFieldValue(response, { id: 1, name: "Phase" }), /Ambiguous/)
            const duplicate = { ...response, fields: [...reads.item.response.fields, reads.item.response.fields[1]] }
            assert.throws(() => itemFieldValue(duplicate, { id: 427, name: "Phase" }), /Ambiguous/)
        })
    })

    it("validates group scope, membership, active state, and issue single-select", () => {
        const invalid = [
            (f: ReturnType<typeof linearFacts>) => {
                f.catalog[0].team = "Other team"
            },
            (f: ReturnType<typeof linearFacts>) => {
                f.catalog[0].retiredAt = "retired"
            },
            (f: ReturnType<typeof linearFacts>) => {
                f.catalog[0].isGroup = false
            },
            (f: ReturnType<typeof linearFacts>) => {
                f.catalog[0].groupType = "multiSelect"
            },
            (f: ReturnType<typeof linearFacts>) => {
                f.catalog[1].parentId = "other-group"
            },
            (f: ReturnType<typeof linearFacts>) => {
                f.catalog[1].retiredAt = "retired"
            },
            (f: ReturnType<typeof linearFacts>) => {
                f.catalog.push({ ...f.catalog[0], id: "duplicate" })
            },
        ]
        for (const mutate of invalid) {
            const facts = linearFacts()
            mutate(facts)
            assert.throws(() => resolveLinearPhaseGroup(linear, "gig", facts.catalog), /Phase/)
        }
        const project = linearFacts()
        project.catalog[0].groupType = null
        assert.equal(resolveLinearPhaseGroup(linear, "epic", project.catalog).values.selfReview, "selfReview")
        project.catalog[0].groupType = "multiSelect"
        assert.throws(
            () => resolveLinearPhaseGroup(linear, "epic", project.catalog),
            /Project Phase requires a single-select group/,
        )

        const teamScoped = structuredClone(linear)
        teamScoped.phase!.issues.scope = "team"
        const facts = linearFacts()
        for (const label of facts.catalog) label.team = "Engineering"
        assert.equal(resolveLinearPhaseGroup(teamScoped, "task", facts.catalog).values.planning, "planning")
        assert.throws(() => resolveLinearPhaseGroup(linear, "task", facts.catalog), /scope/)
    })

    it("uses distinct Project and issue group configurations", () => {
        const tracker = structuredClone(linear)
        tracker.phase!.projects.group.name = "Project Phase"
        const facts = linearFacts()
        facts.catalog[0].name = "Project Phase"
        assert.doesNotThrow(() => resolveLinearPhaseGroup(tracker, "epic", facts.catalog))
        assert.throws(() => resolveLinearPhaseGroup(tracker, "gig", facts.catalog), /Phase/)
    })

    it("lets every review category run in any Phase for every entity on both trackers", () => {
        const root = fs.mkdtempSync(path.join(os.tmpdir(), "phase-review-"))
        const branch = {
            state: "ready",
            name: "example",
            start: "main",
            target: "main",
            source: "generated",
        }
        const common = {
            title: "Example",
            slug: "example",
            createdAt: "2026-01-01T00:00:00Z",
            branch,
        }
        try {
            for (const provider of ["github", "linear"] as const) {
                const epicName = provider === "github" ? "1-example" : "20261001-example"
                const epicDir = path.join(root, provider, ".piton/.local/epics", epicName)
                const tracker = (entity: "epic" | "task" | "gig") =>
                    provider === "github"
                        ? {
                              provider,
                              repository: { owner: "example", repo: "example" },
                              external: { issueNumber: entity === "epic" ? 1 : 2 },
                              planComment: { state: "unpublished" },
                          }
                        : entity === "epic"
                          ? {
                                provider,
                                resource: "project",
                                external: {
                                    projectId: "project-uuid",
                                    createdAt: "2026-10-01T09:00:00.000Z",
                                },
                                planDocument: { state: "unpublished" },
                            }
                          : {
                                provider,
                                resource: entity === "task" ? "task-issue" : "gig-issue",
                                external: {
                                    issueId: "issue-uuid",
                                    identifier: "ENG-2",
                                    ...(entity === "task" ? { projectId: "project-uuid" } : {}),
                                },
                                planDocument: { state: "unpublished" },
                            }
                const childName = provider === "github" ? "2-example" : "ENG-2-example"
                for (const entity of ["epic", "task", "gig"] as const) {
                    const directory =
                        entity === "epic"
                            ? epicDir
                            : entity === "task"
                              ? path.join(epicDir, "tasks", childName)
                              : path.join(root, provider, ".piton/.local/gigs", childName)
                    fs.mkdirSync(directory, { recursive: true })
                    const metadata = {
                        ...common,
                        entity,
                        tracker: tracker(entity),
                        ...(entity === "epic" ? {} : { kind: "feature" }),
                    }
                    if (entity === "epic") {
                        fs.writeFileSync(path.join(directory, "brief.md"), "# Brief\n")
                        fs.writeFileSync(
                            path.join(directory, "epic.md"),
                            `## Tasks\n\n### [${provider === "github" ? "#2" : "ENG-2"}] Example\n\nKind: feature\n\nExample.\n`,
                        )
                    } else {
                        fs.writeFileSync(path.join(directory, "plan.md"), "# Plan\n")
                        fs.writeFileSync(path.join(directory, "result.md"), "# Result\n\nDone.\n")
                    }
                    // Phase is visibility only: it never decides whether a review may run.
                    for (const phase of ["planning", "execution", "selfReview"]) {
                        fs.writeFileSync(path.join(directory, "metadata.json"), JSON.stringify({ ...metadata, phase }))
                        for (const category of entity === "epic"
                            ? (["plan"] as const)
                            : (["plan", "interim", "final"] as const))
                            assert.doesNotThrow(() => assertReviewPreconditions(directory, category))
                    }
                    if (entity === "epic")
                        fs.writeFileSync(
                            path.join(directory, "metadata.json"),
                            JSON.stringify({ ...metadata, phase: "planning" }),
                        )
                    assert.equal(fs.existsSync(path.join(directory, "status.md")), false)
                }
            }
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })
})
