import type { ExtensionAPI } from "@earendil-works/pi-coding-agent"
import { Type } from "typebox"

import { compile, parse } from "../../../extensions/__lib/schema.js"
import {
    EntityInitializationValidator,
    EpicInitializationPropertiesSchema,
    EpicPreflightSchema,
    initializeEntity,
    initializeLocalTask,
    type InitializedEntity,
    LocalTaskInitializationSchema,
    LocalTaskInitializationValidator,
    preflightEntity,
    PreflightValidator,
    TaskInitializationPropertiesSchema,
    TaskPreflightSchema,
} from "./entity.js"
import {
    restoreEpic,
    restoreEpicPlanSource,
    RestoreEpicPlanSourceSchema,
    RestoreEpicPlanSourceValidator,
    RestoreEpicSchema,
    RestoreEpicValidator,
} from "./restore.js"
import { workflowTools } from "../__lib/tools.js"
import { observeMcpTools } from "../../../extensions/__lib/mcp.js"
import {
    GitHubTrackerObjectSchema,
    LinearIssueObjectSchema,
    LinearProjectObjectSchema,
} from "../integrations/records.js"
import { seedInitializedEntityResourceIds } from "./resource-ids.js"
import { routeTask, RouteTaskSchema, RouteTaskValidator } from "./route.js"

export default function (pi: ExtensionAPI): void {
    const tools = workflowTools(pi, "initialization")
    const mcpTools = observeMcpTools(pi)
    const entities = [
        // Rebuild as strict objects; Omit alone doesn't keep additionalProperties: false.
        {
            entity: "epic",
            preflight: Type.Object(Type.Omit(EpicPreflightSchema, ["entity"]).properties, {
                additionalProperties: false,
            }),
            common: EpicInitializationPropertiesSchema,
            linear: LinearProjectObjectSchema,
        },
        {
            entity: "task",
            // New work is always a standalone Task: an Epic Task adopts the issue its Epic created.
            preflight: Type.Object(Type.Omit(TaskPreflightSchema, ["entity"]).properties, {
                additionalProperties: false,
            }),
            common: TaskInitializationPropertiesSchema,
            linear: LinearIssueObjectSchema,
        },
    ] as const
    for (const variant of entities) {
        const common = variant.common
        const preflight = variant.preflight
        const preflightValidator = compile(preflight)
        tools.registerTool({
            name: `preflight_${variant.entity}`,
            label: `preflight_${variant.entity}`,
            description:
                variant.entity === "task"
                    ? "Run every local check for an approved new standalone Task before its tracker issue is created, and return the labels to create it with. Changes nothing; a failure means nothing should be created on the tracker."
                    : "Run every local check for an approved new Epic before its tracker object is created, and return the labels to create it with. Changes nothing; a failure means nothing should be created on the tracker.",
            parameters: preflight,
            async execute(_toolCallId, params, _signal, _onUpdate, ctx) {
                const input = parse(PreflightValidator, {
                    ...parse(preflightValidator, params),
                    entity: variant.entity,
                })
                const result = preflightEntity(input, { cwd: ctx.cwd, tools: mcpTools })
                return {
                    content: [{ type: "text" as const, text: JSON.stringify(result, null, 2) }],
                    details: result,
                }
            },
        })
        for (const [provider, object] of [
            ["github", GitHubTrackerObjectSchema],
            ["linear", variant.linear],
        ] as const) {
            const name = `init_${provider}_${variant.entity}`
            const parameters = Type.Object({ ...common.properties, object }, { additionalProperties: false })
            const validator = compile(parameters)
            tools.registerTool({
                name,
                label: name,
                description:
                    variant.entity === "task"
                        ? `Start a Task from its ${provider === "github" ? "GitHub" : "Linear"} issue, with the parent route_task named. An Epic Task adopts its Epic's queued issue: the issue's marker selects the plan Task, whose Kind must match, and the Epic's routing sets the branch base. A standalone Task starts from and targets the repository default. Names the directory and branch from the issue and records its Resource IDs.`
                        : `Initialize an approved Epic from its existing ${provider === "github" ? "GitHub issue" : "Linear Project"}, newly created or explicitly selected. Names the directory and branch from the tracker object and records its Resource IDs.`,
                parameters,
                async execute(_toolCallId, params, _signal, _onUpdate, ctx) {
                    const input = parse(EntityInitializationValidator, {
                        ...parse(validator, params),
                        entity: variant.entity,
                    })
                    const initialized = await initializeEntity(input, { cwd: ctx.cwd, tools: mcpTools })
                    await seedInitializedEntityResourceIds(initialized, ctx.cwd)
                    return {
                        content: [{ type: "text" as const, text: formatInitializedEntity(initialized) }],
                        details: initialized,
                    }
                },
            })
        }
    }

    tools.registerTool({
        name: "route_task",
        label: "route_task",
        description:
            "Decide how a /task request starts: resume an existing Task, start an Epic's Task (with its parent, marker, plan Kind, and routing), restore a missing Epic first, start a standalone Task, start new standalone work, ask for one more read, or stop with a reason. Takes a Task path, a tracker issue's reads, or new work. Reads local artifacts and the given evidence only; never calls providers or writes.",
        parameters: RouteTaskSchema,
        async execute(_toolCallId, params, _signal, _onUpdate, ctx) {
            const result = routeTask(ctx.cwd, parse(RouteTaskValidator, params), mcpTools)
            return { content: [{ type: "text" as const, text: JSON.stringify(result, null, 2) }], details: result }
        },
    })

    tools.registerTool({
        name: "init_local_task",
        label: "init_local_task",
        description:
            "Initialize an approved standalone Task with no tracker item, for a project without a tracker or work the user keeps off it. Names the directory and branch from today's date and the slug (<YYYYMMDD>-<slug>), starting from and targeting the repository default branch. A local Task has no Phase, lifecycle, Priority, labels, or plan publication; every tracker step refuses for it.",
        parameters: LocalTaskInitializationSchema,
        async execute(_toolCallId, params, _signal, _onUpdate, ctx) {
            const initialized = await initializeLocalTask(parse(LocalTaskInitializationValidator, params), {
                cwd: ctx.cwd,
            })
            return {
                content: [{ type: "text" as const, text: formatInitializedEntity(initialized) }],
                details: initialized,
            }
        },
    })

    tools.registerTool({
        name: "restore_epic",
        label: "restore_epic",
        description:
            "Recreate a missing local Epic from its tracker read and its single published plan: directory, metadata, brief, and epic.md. Never runs over an existing local Epic for the same tracker item, and creates no branch.",
        parameters: RestoreEpicSchema,
        async execute(_toolCallId, params, _signal, _onUpdate, ctx) {
            const result = restoreEpic(ctx.cwd, parse(RestoreEpicValidator, params), mcpTools)
            return {
                content: [{ type: "text" as const, text: `Restored ${result.directory}.` }],
                details: result,
            }
        },
    })

    tools.registerTool({
        name: "restore_epic_plan_source",
        label: "restore_epic_plan_source",
        description:
            "Return an existing local Epic's published plan as epic.md source, for comparing with the local copy. Writes nothing.",
        parameters: RestoreEpicPlanSourceSchema,
        async execute(_toolCallId, params, _signal, _onUpdate, ctx) {
            const result = restoreEpicPlanSource(ctx.cwd, parse(RestoreEpicPlanSourceValidator, params), mcpTools)
            return { content: [{ type: "text" as const, text: result.source }], details: result }
        },
    })
}

export function formatInitializedEntity(initialized: Pick<InitializedEntity, "directory" | "status">): string {
    const branch = initialized.status.branch
    const board =
        initialized.status.tracker.provider === "local"
            ? "This Task is local: skip board setup and every tracker step."
            : "Next: set up the board (Project membership, Status, Priority, Assignee, and Phase and labels when configured)."
    return [
        `Initialized ${initialized.directory}.`,
        `Branch: ${branch.name}`,
        `Start/target: ${branch.start}`,
        `${board} Switch to the stored branch before implementation. Do not push it automatically.`,
    ].join("\n")
}
