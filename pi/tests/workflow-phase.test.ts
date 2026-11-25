import "./host-runtime.ts"

import assert from "node:assert/strict"
import * as cp from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { describe, it } from "node:test"

import { parse } from "../extensions/__lib/schema.ts"
import { PhaseValidator, projectedPhase, type LifecycleState } from "../workflows/extensions/__lib/domain.ts"
import { assertReviewPreconditions } from "../workflows/extensions/review/index.ts"
import type { GitHubTracker, LinearTracker } from "../workflows/extensions/integrations/config.ts"
import { githubPhaseFacts } from "../workflows/extensions/integrations/github-phase-evidence.ts"
import {
    NativePhaseFactsValidator,
    phaseProjectionContract,
    planPhaseMutation as planForItem,
    resolveLinearPhaseGroup,
    trackerPhase,
    type NativePhaseFacts,
    type PhaseItem,
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
const github: GitHubTracker = {
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
}
const linear: LinearTracker = {
    provider: "linear",
    mcpServer: "linear",
    team: "Engineering",
    phase: linearPhases(),
    statuses: { projects: statuses, issues: statuses },
}
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

function phaseItem(provider: "github" | "linear", entity: "epic" | "task" | "gig"): PhaseItem {
    if (provider === "github")
        return {
            entity,
            tracker: {
                provider,
                repository: { owner: "example", repo: "example" },
                external: { issueNumber: 7 },
                planComment: { state: "unpublished" },
            },
        }
    const planDocument = { state: "unpublished" } as const
    switch (entity) {
        case "epic":
            return {
                entity,
                tracker: {
                    provider,
                    resource: "project",
                    external: { projectId: "project-uuid", createdAt: "2026-01-01T00:00:00.000Z" },
                    planDocument,
                },
            }
        case "task":
            return {
                entity,
                tracker: {
                    provider,
                    resource: "task-issue",
                    external: { issueId: "issue-uuid", identifier: "ENG-7", projectId: "project-uuid" },
                    planDocument,
                },
            }
        case "gig":
            return {
                entity,
                tracker: {
                    provider,
                    resource: "gig-issue",
                    external: { issueId: "issue-uuid", identifier: "ENG-7" },
                    planDocument,
                },
            }
    }
}

const resolveTool = (server: string, operation: string) => `mcp__${server}__${operation}`

/** Plans for the entity's item on the tracker's provider, resolving tools by the fixture naming. */
function planPhaseMutation(
    tracker: typeof github | typeof linear,
    entity: "epic" | "task" | "gig",
    phase: Parameters<typeof planForItem>[2],
    lifecycle: LifecycleState,
    facts: NativePhaseFacts,
) {
    return planForItem(tracker, phaseItem(tracker.provider, entity), phase, lifecycle, facts, resolveTool)
}

describe("shared workflow Phase", () => {
    it("projects a Phase only while In Progress, and clears it otherwise, for every entity and provider", () => {
        for (const entity of ["epic", "task", "gig"] as const) {
            for (const phase of ["planning", "execution", "selfReview"] as const) {
                assert.equal(PhaseValidator.check(phase), true)
                for (const lifecycle of lifecycles) {
                    const active = lifecycle === "inProgress"
                    const target = active ? phase : null
                    const expected = target
                    assert.equal(projectedPhase(lifecycle, target), expected)
                    // The target must match lifecycle: a Phase only In Progress, none otherwise.
                    assert.throws(
                        () => projectedPhase(lifecycle, active ? null : phase),
                        active ? /In Progress/ : /none/,
                    )
                    assert.throws(
                        () => planPhaseMutation(github, entity, active ? null : phase, lifecycle, githubFacts),
                        active ? /In Progress/ : /none/,
                    )
                    const field = planPhaseMutation(github, entity, target, lifecycle, githubFacts).assignment
                    assert.equal(field.provider, "github")
                    if (field.provider !== "github") assert.fail("expected GitHub")
                    assert.equal(field.value, expected === null ? null : phaseValues[expected])
                    const labels = planPhaseMutation(linear, entity, target, lifecycle, linearFacts()).assignment
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

    it("returns the registered provider call for a changed assignment, and none otherwise", () => {
        const project = github.project
        const githubCall = (value: string | null) => ({
            tool: "mcp__github__projects_write",
            arguments: {
                method: "update_project_item",
                owner: project.owner,
                owner_type: project.ownerType,
                project_number: project.number,
                item_owner: "example",
                item_repo: "example",
                issue_number: 7,
                updated_field: { name: "Phase", value },
            },
        })
        const set = planPhaseMutation(github, "gig", "execution", "inProgress", githubFacts)
        assert.deepEqual(set, {
            assignment: { provider: "github", field: "Phase", value: "Execution" },
            nativeChanged: true,
            mutation: githubCall("Execution"),
        })
        const selected = { ...githubFacts, current: "Execution" }
        assert.deepEqual(planPhaseMutation(github, "gig", null, "inReview", selected), {
            assignment: { provider: "github", field: "Phase", value: null },
            nativeChanged: true,
            mutation: githubCall(null),
        })
        assert.equal("mutation" in planPhaseMutation(github, "gig", "execution", "inProgress", selected), false)

        for (const [entity, tool, id] of [
            ["epic", "mcp__linear__save_project", "project-uuid"],
            ["task", "mcp__linear__save_issue", "issue-uuid"],
            ["gig", "mcp__linear__save_issue", "issue-uuid"],
        ] as const) {
            const labels = ["customer-label", "other-execution", "execution"]
            assert.deepEqual(planPhaseMutation(linear, entity, "execution", "inProgress", linearFacts()), {
                assignment: { provider: "linear", labels },
                nativeChanged: true,
                mutation: { tool, arguments: { id, labels } },
            })
            const unchanged = planPhaseMutation(linear, entity, "planning", "inProgress", linearFacts())
            assert.deepEqual(unchanged, {
                assignment: { provider: "linear", labels: ["customer-label", "other-execution", "planning"] },
                nativeChanged: false,
            })
        }

        // An unregistered tool fails while preparing, not when the agent runs the call.
        const missing = () => {
            throw new Error("no registered tool")
        }
        assert.throws(
            () => planForItem(github, phaseItem("github", "gig"), "execution", "inProgress", githubFacts, missing),
            /no registered tool/,
        )
        assert.throws(
            () => planForItem(github, phaseItem("linear", "gig"), "execution", "inProgress", githubFacts, resolveTool),
            /provider/,
        )
    })

    it("returns only how the tracker shows Phase, with no local Phase", () => {
        for (const tracker of [github, linear]) {
            const contract = phaseProjectionContract(tracker, "gig")
            if (contract.state !== "configured") assert.fail("expected configured Phase")
            assert.deepEqual(Object.keys(contract).sort(), ["authority", "representation", "state"])
            assert.equal(contract.representation.provider, tracker.provider)
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
            assert.equal(phaseProjectionContract(tracker, "gig").state, "notConfigured")
            assert.throws(() => planPhaseMutation(tracker, "gig", "execution", "inProgress", facts), /isn't configured/)
        }
        assert.throws(() => resolveLinearPhaseGroup(linearWithout, "gig", linearFacts().catalog), /isn't configured/)
    })

    it("recognizes no-op and clearing assignments without losing unrelated labels", () => {
        const facts = linearFacts()
        facts.applied = ["customer-label", "execution", "other-execution"]
        assert.equal(planPhaseMutation(linear, "gig", "execution", "inProgress", facts).nativeChanged, false)
        const cleared = planPhaseMutation(linear, "gig", null, "inReview", facts)
        assert.equal(cleared.assignment.provider, "linear")
        if (cleared.assignment.provider !== "linear") assert.fail("expected Linear")
        assert.deepEqual(cleared.assignment.labels, ["customer-label", "other-execution"])
        assert.equal(cleared.nativeChanged, true)
        assert.equal(
            planPhaseMutation(github, "gig", "selfReview", "inProgress", {
                ...githubFacts,
                current: "Self-Review",
            }).nativeChanged,
            false,
        )
        assert.equal(planPhaseMutation(github, "gig", null, "done", githubFacts).nativeChanged, false)
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

        // Targets Planning while In Progress and clears Phase otherwise, as the agent would.
        const plan = (reads: Reads) => {
            const { lifecycle, native } = facts(reads)
            return planPhaseMutation(github, "gig", lifecycle === "inProgress" ? "planning" : null, lifecycle, native)
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
                planPhaseMutation(github, "gig", "execution", selected.lifecycle, selected.native).nativeChanged,
                false,
            )
            assert.partialDeepStrictEqual(plan(githubPhaseReads(target, null)), {
                assignment: { provider: "github", field: "Phase", value: "Planning" },
                nativeChanged: true,
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
            assert.partialDeepStrictEqual(plan(reads), {
                assignment: { provider: "github", field: "Phase", value: null },
                nativeChanged: true,
            })
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

    it("loads the shared Phase operation without a Pi runtime", () => {
        const output = cp.execFileSync(
            process.execPath,
            [
                "--import",
                "tsx",
                "--input-type=module",
                "-e",
                `
            import { registerHooks } from 'node:module';
            registerHooks({ resolve(specifier, context, next) {
                // pi-schema is the package's own dependency, not the Pi runtime.
                if (specifier.startsWith('@earendil-works/') && specifier !== '@earendil-works/pi-schema')
                    throw new Error('Pi runtime dependency: ' + specifier);
                return next(specifier, context);
            }});
            await import('./workflows/extensions/integrations/phase.ts');
            console.log('portable operations loaded');
        `,
            ],
            { encoding: "utf8" },
        )
        assert.match(output, /portable operations loaded/)
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
                            `## Tasks\n\n### [${provider === "github" ? "#2" : "ENG-2"}] Example\n\n**Kind:** \`feature\`\n**Summary:** Example.\n`,
                        )
                    } else {
                        fs.writeFileSync(path.join(directory, "plan.md"), "# Plan\n")
                        fs.writeFileSync(path.join(directory, "result.md"), "# Result\n\nDone.\n")
                    }
                    // Phase is visibility only: it never decides whether a review may run.
                    fs.writeFileSync(path.join(directory, "metadata.json"), JSON.stringify(metadata))
                    for (const category of entity === "epic"
                        ? (["plan"] as const)
                        : (["plan", "interim", "final"] as const))
                        assert.doesNotThrow(() => assertReviewPreconditions(directory, category))
                    assert.equal(fs.existsSync(path.join(directory, "status.md")), false)
                }
            }
        } finally {
            fs.rmSync(root, { recursive: true, force: true })
        }
    })
})
