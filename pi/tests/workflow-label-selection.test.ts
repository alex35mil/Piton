import "./host-runtime.ts"

import assert from "node:assert/strict"
import { describe, it } from "node:test"

import type { GitHubLabelGroup, GitHubLabels, LinearLabels } from "../workflows/extensions/integrations/labels.ts"
import {
    LabelSelectionFailure,
    planGitHubLabelUpdate,
    planLinearLabelUpdate,
    selectedLabelNames,
} from "../workflows/extensions/integrations/label-selection.ts"
import { parseEpicTasks } from "../workflows/extensions/__lib/epic-tasks.ts"
import type { TrackerConfig } from "../workflows/extensions/integrations/config.ts"
import { gateOutcome } from "../workflows/extensions/integrations/gate.ts"
import { kindLabels, linearKindGroup, linearPhaseGroup } from "./workflow-phase-fixtures.ts"

const def = (name: string) => ({ name, color: "#1D76DB", description: "" })
const group = (cardinality: string, values: Record<string, string>, appliesTo = ["deliverable"]) => ({
    appliesTo,
    cardinality,
    values: Object.fromEntries(Object.entries(values).map(([key, name]) => [key, def(name)])),
})
const labels = {
    groups: {
        ...kindLabels().groups,
        area: group("zeroOrMore", { fe: "Area: FE", be: "Area: BE" }, ["epic", "deliverable"]),
        perf: group("zeroOrOne", { perf: "Perf" }),
        size: group("exactlyOne", { s: "Size: S", l: "Size: L" }),
        team: group("oneOrMore", { core: "Team: Core", web: "Team: Web" }),
    },
    unmanaged: { keep: [{ name: "good first issue" }] },
    retired: [{ name: "Area: Legacy" }],
} as GitHubLabels

const failure = (run: () => unknown) => {
    try {
        run()
    } catch (error) {
        if (error instanceof LabelSelectionFailure) return error.reason
        throw error
    }
    assert.fail("expected a label selection failure")
}

describe("label selection", () => {
    const valid = { area: ["fe"], perf: [], size: ["s"], team: ["core", "web"] }

    it("returns the selected labels plus the kind label from the item's Kind", () => {
        assert.deepEqual(selectedLabelNames(labels, "deliverable", "bugfix", valid), [
            "Area: FE",
            "Size: S",
            "Team: Core",
            "Team: Web",
            "Kind: Bugfix",
        ])
        assert.deepEqual(selectedLabelNames(labels, "epic", "epic", { area: [] }), ["Kind: Epic"])
        assert.deepEqual(selectedLabelNames(undefined, "deliverable", "bugfix", {}), [])
    })

    it("checks every cardinality", () => {
        assert.deepEqual(
            failure(() => selectedLabelNames(labels, "deliverable", "bugfix", { ...valid, size: [] })),
            {
                kind: "cardinality",
                group: "size",
                cardinality: "exactlyOne",
                selected: [],
            },
        )
        assert.equal(
            failure(() => selectedLabelNames(labels, "deliverable", "bugfix", { ...valid, perf: ["perf", "perf2"] }))
                .kind,
            "unknownValue",
        )
        assert.equal(
            failure(() => selectedLabelNames(labels, "deliverable", "bugfix", { ...valid, size: ["s", "l"] })).kind,
            "cardinality",
        )
        assert.equal(
            failure(() => selectedLabelNames(labels, "deliverable", "bugfix", { ...valid, team: [] })).kind,
            "cardinality",
        )
    })

    it("rejects unknown groups and a selection naming kind", () => {
        assert.equal(
            failure(() => selectedLabelNames(labels, "deliverable", "bugfix", { ...valid, nope: [] })).kind,
            "unknownGroup",
        )
        assert.equal(failure(() => selectedLabelNames(labels, "epic", "epic", { perf: [] })).kind, "unknownGroup")
        assert.equal(
            failure(() => selectedLabelNames(labels, "deliverable", "bugfix", { ...valid, kind: ["feature"] })).kind,
            "kindInSelection",
        )
    })

    it("leaves an omitted group empty for new work, so only a required one fails", () => {
        assert.deepEqual(selectedLabelNames(labels, "deliverable", "bugfix", { size: ["s"], team: ["core"] }), [
            "Size: S",
            "Team: Core",
            "Kind: Bugfix",
        ])
        assert.deepEqual(
            failure(() => selectedLabelNames(labels, "deliverable", "bugfix", { team: ["core"] })),
            {
                kind: "cardinality",
                group: "size",
                cardinality: "exactlyOne",
                selected: [],
            },
        )
    })

    it("takes an omitted group from the base selection, and an empty list clears it", () => {
        const base = { area: ["fe"], perf: ["perf"], size: ["s"], team: ["core"] }
        assert.deepEqual(selectedLabelNames(labels, "deliverable", "bugfix", { area: ["be"], perf: [] }, base), [
            "Area: BE",
            "Size: S",
            "Team: Core",
            "Kind: Bugfix",
        ])
    })
})

describe("GitHub label update", () => {
    it("replaces managed labels, keeps everything else, and reports unknown labels", () => {
        const update = planGitHubLabelUpdate(
            labels,
            "deliverable",
            "feature",
            { area: ["be"], perf: [], size: ["l"], team: ["core"] },
            ["Kind: Bugfix", "Area: FE", "Size: S", "Team: Web", "wontfix", "good first issue", "Area: Legacy"],
        )
        assert.deepEqual(update.labels, [
            "wontfix",
            "good first issue",
            "Area: Legacy",
            "Area: BE",
            "Size: L",
            "Team: Core",
            "Kind: Feature",
        ])
        assert.equal(update.changed, true)
        assert.deepEqual(update.unknown, ["wontfix"], "kept and retired labels aren't unknown")
    })

    it("keeps the labels of groups a selection leaves out", () => {
        const current = ["Kind: Feature", "Area: FE", "Perf", "Size: S", "Team: Core", "wontfix"]
        const update = planGitHubLabelUpdate(labels, "deliverable", "feature", { area: ["be"] }, current)
        assert.deepEqual(update.labels, ["wontfix", "Area: BE", "Perf", "Size: S", "Team: Core", "Kind: Feature"])
        assert.equal(update.changed, true)
        assert.equal(
            planGitHubLabelUpdate(labels, "deliverable", "feature", { area: ["fe"] }, current).changed,
            false,
            "restating the current value changes nothing",
        )
    })

    it("keeps the current selection on a Kind change, changing only the kind label", () => {
        const current = ["Kind: Feature", "Area: FE", "Size: S", "Team: Core"]
        const update = planGitHubLabelUpdate(labels, "deliverable", "bugfix", undefined, current)
        assert.deepEqual(update.labels, ["Area: FE", "Size: S", "Team: Core", "Kind: Bugfix"])
        assert.equal(planGitHubLabelUpdate(labels, "deliverable", "feature", undefined, current).changed, false)
    })

    it("reports current labels that break cardinality instead of fixing them", () => {
        const current = ["Kind: Feature", "Size: S", "Size: L", "Team: Core"]
        assert.deepEqual(
            failure(() => planGitHubLabelUpdate(labels, "deliverable", "bugfix", undefined, current)),
            {
                kind: "cardinality",
                group: "size",
                cardinality: "exactlyOne",
                selected: ["s", "l"],
            },
        )
    })

    it("leaves labels alone without any configured group", () => {
        const update = planGitHubLabelUpdate(undefined, "deliverable", "feature", undefined, ["bug"])
        assert.deepEqual(update, { labels: ["bug"], changed: false, unknown: ["bug"] })
    })
})

describe("Linear label update", () => {
    const linearLabels = {
        groups: {
            kind: linearKindGroup(),
            area: { ...group("zeroOrMore", { fe: "Area: FE" }), scope: "team" },
        },
    } as LinearLabels
    const label = (id: string, name: string, extra: Record<string, unknown> = {}) => ({
        id,
        name,
        team: null,
        parentId: null,
        isGroup: false,
        retiredAt: null,
        ...extra,
    })
    const catalog = [
        label("k-f", "Kind: Feature"),
        label("k-b", "Kind: Bugfix"),
        label("fe", "Area: FE", { team: "Engineering" }),
        label("fe-ws", "Area: FE"),
        label("phase", "Phase", { isGroup: true }),
        label("planning", "Planning", { parentId: "phase" }),
        label("other", "Customer"),
    ]

    it("returns the full ID set, keeping Phase and unrelated labels", () => {
        const update = planLinearLabelUpdate(
            linearLabels,
            linearPhaseGroup(),
            "Engineering",
            "deliverable",
            "bugfix",
            { area: ["fe"] },
            {
                catalog,
                applied: ["k-f", "planning", "other"],
            },
        )
        assert.deepEqual(
            [...update.labels].sort(),
            ["fe", "k-b", "other", "planning"],
            "the team label, not the workspace one",
        )
        assert.deepEqual(update.unknown, ["Customer"])
        assert.equal(update.changed, true)
    })

    it("keeps the labels of groups a selection leaves out", () => {
        const facts = { catalog, applied: ["k-b", "fe", "planning"] }
        const run = (selection: Record<string, string[]>) =>
            planLinearLabelUpdate(
                linearLabels,
                linearPhaseGroup(),
                "Engineering",
                "deliverable",
                "bugfix",
                selection,
                facts,
            )
        assert.equal(run({}).changed, false)
        assert.deepEqual([...run({ area: [] }).labels].sort(), ["k-b", "planning"])
    })

    it("treats Phase-named labels as ordinary ones when Phase isn't configured", () => {
        const update = planLinearLabelUpdate(
            linearLabels,
            undefined,
            "Engineering",
            "deliverable",
            "bugfix",
            { area: ["fe"] },
            { catalog, applied: ["k-f", "planning", "other"] },
        )
        assert.deepEqual([...update.labels].sort(), ["fe", "k-b", "other", "planning"])
        assert.deepEqual(update.unknown, ["Planning", "Customer"])
    })

    it("doesn't report a child of a kept native group, the same rule setup uses", () => {
        const kept = {
            ...linearLabels,
            unmanaged: { keep: [{ name: "Clients", catalog: "issue" as const, scope: "workspace" as const }] },
        } as LinearLabels
        const withGroup = [
            ...catalog,
            label("clients", "Clients", { isGroup: true }),
            label("acme", "Acme", { parentId: "clients" }),
        ]
        const facts = { catalog: withGroup, applied: ["k-b", "acme", "other"] }
        const update = planLinearLabelUpdate(
            kept,
            linearPhaseGroup(),
            "Engineering",
            "deliverable",
            "bugfix",
            { area: [] },
            facts,
        )
        // Acme belongs to the kept Clients group; the unrelated "Customer" label is still unknown.
        assert.deepEqual(update.unknown, ["Customer"])

        const notKept = planLinearLabelUpdate(
            linearLabels,
            linearPhaseGroup(),
            "Engineering",
            "deliverable",
            "bugfix",
            { area: [] },
            facts,
        )
        assert.deepEqual(notKept.unknown, ["Acme", "Customer"], "without the keep entry, the child is unknown")
    })

    it("fails when a selected label is missing or retired", () => {
        const retired = catalog.map((entry) => (entry.id === "k-b" ? { ...entry, retiredAt: "2026-11-04" } : entry))
        assert.deepEqual(
            failure(() =>
                planLinearLabelUpdate(
                    linearLabels,
                    linearPhaseGroup(),
                    "Engineering",
                    "deliverable",
                    "bugfix",
                    { area: [] },
                    {
                        catalog: retired,
                        applied: [],
                    },
                ),
            ),
            { kind: "missingLabel", name: "Kind: Bugfix" },
        )
    })
})

describe("gates", () => {
    const gates = group("zeroOrMore", { plan: "Gate: Plan" }) as GitHubLabelGroup
    const github = (groups: GitHubLabels["groups"]) =>
        ({ provider: "github", labels: { groups } }) as unknown as TrackerConfig
    const linear = (scope: "workspace" | "team") =>
        ({
            provider: "linear",
            team: "Engineering",
            labels: { groups: { gate: { ...gates, scope } } },
        }) as unknown as TrackerConfig
    const names = (...names: string[]) => ({ provider: "github" as const, names })

    it("waits at a gate only when a GitHub item carries its configured label", () => {
        const tracker = github({ ...labels.groups, gate: gates })
        assert.deepEqual(gateOutcome(tracker, "plan", names("Kind: Feature", "gate: plan")), {
            gate: "awaitApproval",
            label: "Gate: Plan",
        })
        assert.deepEqual(gateOutcome(tracker, "plan", names("Kind: Feature")), {
            gate: "continue",
            reason: "unmarked",
            label: "Gate: Plan",
        })
        assert.deepEqual(gateOutcome(github(labels.groups), "plan", names("Gate: Plan")), {
            gate: "continue",
            reason: "unconfigured",
        })
    })

    it("counts only the Linear label in the group's scope", () => {
        const catalog = [
            { id: "team", name: "Gate: Plan", team: "Engineering", isGroup: false },
            { id: "ws", name: "Gate: Plan", isGroup: false },
        ]
        const facts = (...applied: string[]) => ({ provider: "linear" as const, catalog, applied })
        assert.equal(gateOutcome(linear("team"), "plan", facts("team")).gate, "awaitApproval")
        assert.deepEqual(gateOutcome(linear("team"), "plan", facts("ws")), {
            gate: "continue",
            reason: "unmarked",
            label: "Gate: Plan",
        })
        assert.equal(gateOutcome(linear("workspace"), "plan", facts("ws")).gate, "awaitApproval")
    })

    it("rejects label facts from the other provider", () => {
        assert.throws(() => gateOutcome(linear("team"), "plan", names("Gate: Plan")), /configured Linear tracker/)
    })

    it("keeps the marker through a Kind change and a selection without gate, and an empty list removes it", () => {
        const configured = { groups: { ...labels.groups, gate: gates } } as GitHubLabels
        const current = ["Kind: Feature", "Size: S", "Team: Core", "Gate: Plan"]
        const kindChange = planGitHubLabelUpdate(configured, "deliverable", "bugfix", undefined, current)
        assert.ok(kindChange.labels.includes("Gate: Plan"))
        assert.ok(kindChange.labels.includes("Kind: Bugfix"))
        const areaOnly = planGitHubLabelUpdate(configured, "deliverable", "feature", { area: ["fe"] }, current)
        assert.ok(areaOnly.labels.includes("Gate: Plan"))
        const cleared = planGitHubLabelUpdate(
            configured,
            "deliverable",
            "feature",
            { area: [], perf: [], size: ["s"], team: ["core"], gate: [] },
            current,
        )
        assert.ok(!cleared.labels.includes("Gate: Plan"))
    })
})

describe("Task Labels lines", () => {
    const task = (lines: string) =>
        `## Tasks\n\n### Work\n\n**Kind:** \`feature\`\n${lines}**Summary:** Work.\n\nDo the work.\n`

    it("parses a Labels line into a selection", () => {
        const [parsed] = parseEpicTasks(task("**Labels:** `area=fe`, `area=be`, `perf=perf`\n"))
        assert.deepEqual(parsed.labels, { area: ["fe", "be"], perf: ["perf"] })
        assert.equal(parsed.issue === "pending" && parsed.design, "Do the work.")
        assert.deepEqual(parseEpicTasks(task(""))[0].labels, {})
    })

    it("rejects malformed and repeated entries", () => {
        assert.throws(() => parseEpicTasks(task("**Labels:** `area`\n")), /invalid Labels entry/)
        assert.throws(() => parseEpicTasks(task("**Labels:** `area=fe, area=be`\n")), /invalid Labels entry/)
        assert.throws(() => parseEpicTasks(task("**Labels:** area=fe\n")), /invalid Labels entry/)
        assert.throws(() => parseEpicTasks(task("**Labels:** `area=fe`, `area=fe`\n")), /repeats/)
    })

    it("names kind as a selection error when the Task's labels are checked", () => {
        const [parsed] = parseEpicTasks(task("**Labels:** `kind=bugfix`\n"))
        assert.equal(
            failure(() => selectedLabelNames(labels, "deliverable", parsed.kind, parsed.labels)).kind,
            "kindInSelection",
        )
    })
})
