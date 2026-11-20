import * as path from "node:path"

import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent"
import { type Static, Type } from "typebox"
import { compile, parse } from "../../../extensions/__lib/schema.js"
import { workflowTools } from "../__lib/tools.js"

import * as project from "../../../extensions/__lib/project.js"
import { getMcpToolName, observeMcpTools, type McpToolCatalog } from "../../../extensions/__lib/mcp.js"
import { acceptEpicTasks } from "../__lib/epic-acceptance.js"
import { readEntityStatus, resolveEntityDirectory } from "../__lib/entity.js"
import { type EntityTracker, requireTrackerItem } from "./records.js"
import {
    loadIntegrationRoles,
    requireForge,
    requireTracker,
    type Role,
    roleContext,
    type TrackerConfig,
} from "./config.js"
import { GateSchema, labelItemType, type WorkflowKind } from "./labels.js"
import {
    LabelFactsSchema,
    LabelSelectionFailure,
    LabelSelectionSchema,
    labelGroupSummary,
    type LabelFacts,
    type LabelSelection,
    linearLabelFacts,
    planGitHubLabelUpdate,
    planLinearLabelUpdate,
    selectedLabelNames,
} from "./label-selection.js"
import { type LifecycleState, LifecycleStateSchema } from "../__lib/domain.js"
import {
    type NativePhaseFacts,
    NativePhaseFactsSchema,
    LinearPhaseInputSchema,
    LinearPhaseInputValidator,
    linearPhaseFacts,
    linearPhaseGroup,
    phaseItem,
    phaseProjectionContract,
    planPhaseMutation,
} from "./phase.js"
import { GitHubPhaseInputSchema, GitHubPhaseInputValidator, githubPhaseFacts } from "./github-phase-evidence.js"
import { gateOutcome } from "./gate.js"
import {
    assertArtifactPersistencePrepared,
    assertWorkerArtifactsLinked,
    developmentCheckout,
    loadSessionAnalysis,
} from "../__lib/project-config.js"
import * as Git from "../__lib/git.js"
import { assertEntityForge, assertEntityTracker, sameRepository } from "./policy.js"
import { renderProviderBody, verifyMarkdownProjection } from "./projection.js"
import { isResourceIdsKind, OperationResourceIdsMemo, readCachedResourceIds } from "./resource-ids.js"
import { shippingReadDecision, resolveResourceIdsContext } from "./resource-id-operations.js"
import type { GitHubMergeDecision } from "./forge/github/merge.js"
import { EvidenceSchema, registerResourceIds } from "./resource-id-tool.js"
import { registerWorkflowOperations } from "./operations.js"
import { registerWorkflowPublication } from "./publication-mcp.js"
import { registerTrackerPublication } from "./tracker-publication.js"
import { registerWorkflowContext } from "./workflow-context.js"
import { inspectTaskDesign, recordTaskDesign, TaskDesignSchema, TaskDesignValidator } from "./task-design.js"
import {
    planEpicTaskIssues,
    PlanEpicTaskIssuesSchema,
    PlanEpicTaskIssuesValidator,
    recordEpicTaskIssue,
    RecordEpicTaskIssueSchema,
    RecordEpicTaskIssueValidator,
} from "./epic-task-issues.js"
import { incomingPullRequestContext } from "./forge/github/incoming-pr-context.js"
import {
    IncomingMergeContextSchema,
    incomingMergeContext,
    type IncomingMergeContextInput,
} from "./forge/github/incoming-pr-merge.js"
import {
    IncomingProjectContextSchema,
    incomingProjectContext,
    type IncomingProjectContextInput,
} from "./forge/github/incoming-pr-project.js"

const ProjectionVerificationSchema = Type.Object(
    {
        expected: Type.String(),
        actual: Type.String(),
    },
    { additionalProperties: false },
)
const ProjectionVerificationValidator = compile(ProjectionVerificationSchema)

export const RenderProviderBodySchema = Type.Object(
    {
        entityDir: Type.Optional(
            Type.String({
                minLength: 1,
                description:
                    "The entity whose plan or result this is. Required for a tracker body; optional for a PR body, which then has no closing reference.",
            }),
        ),
        destination: Type.Enum(["tracker", "pullRequest"], { type: "string" }),
        source: Type.String(),
    },
    { additionalProperties: false },
)
export const RenderProviderBodyValidator = compile(RenderProviderBodySchema)

export const AcceptEpicTasksSchema = Type.Object(
    { entityDir: Type.String({ minLength: 1, description: "The Epic directory" }) },
    { additionalProperties: false },
)
const AcceptEpicTasksValidator = compile(AcceptEpicTasksSchema)

export const InspectGateSchema = Type.Object(
    {
        entityDir: Type.String({ minLength: 1, description: "The Task or Gig directory" }),
        gate: Type.Union([GateSchema], { description: "The stop point to check" }),
        current: LabelFactsSchema,
    },
    { additionalProperties: false },
)
const InspectGateValidator = compile(InspectGateSchema)

const ContextEntitySchema = Type.Enum(["epic", "task", "gig"], { type: "string" })
const ContextTargetSchema = Type.Enum(["current", "workflow"], { type: "string" })
export const IntegrationContextSchema = Type.Union([
    IncomingMergeContextSchema,
    IncomingProjectContextSchema,
    Type.Object(
        { operation: Type.Literal("incomingPullRequest"), reference: Type.String({ minLength: 1 }) },
        { additionalProperties: false },
    ),
    Type.Object({ operation: Type.Literal("inspect"), target: ContextTargetSchema }, { additionalProperties: false }),
    Type.Object(
        {
            operation: Type.Literal("queueIntake"),
            target: ContextTargetSchema,
            // Tasks are never queued directly; accepted Epic plans create their issues.
            entity: Type.Enum(["epic", "gig"], { type: "string" }),
            priority: Type.String({ minLength: 1 }),
            kind: Type.Enum(["not set", "feature", "bugfix", "research", "refactor", "audit", "chore", "epic"], {
                type: "string",
                description: "Approved Kind, or not set. Applied as a label only when a `kind` label group covers it.",
            }),
            labels: Type.Optional(LabelSelectionSchema),
        },
        { additionalProperties: false },
    ),
    Type.Object({ operation: Type.Literal("adopt"), entity: ContextEntitySchema }, { additionalProperties: false }),
    Type.Object(
        {
            operation: Type.Literal("shippingMerge"),
            entityDir: Type.String({ minLength: 1 }),
            evidence: EvidenceSchema,
        },
        { additionalProperties: false },
    ),
    Type.Object(
        {
            operation: Type.Literal("trackerMutation"),
            entityDir: Type.String({ minLength: 1 }),
            change: Type.Enum(["lifecycle", "priority", "hierarchy"], { type: "string" }),
        },
        { additionalProperties: false },
    ),
    Type.Object(
        {
            operation: Type.Literal("labelMutation"),
            entityDir: Type.String({ minLength: 1 }),
            labels: Type.Optional(
                Type.Union([LabelSelectionSchema], {
                    description:
                        "The groups to change. Groups left out keep the item's labels; omit it entirely to change only the `kind` label, on a Kind change.",
                }),
            ),
            current: LabelFactsSchema,
        },
        { additionalProperties: false },
    ),
    Type.Object(
        {
            operation: Type.Literal("trackerMutation"),
            entityDir: Type.String({ minLength: 1 }),
            change: Type.Literal("phase"),
            lifecycle: LifecycleStateSchema,
            native: NativePhaseFactsSchema,
        },
        { additionalProperties: false },
    ),
    Type.Object(
        {
            operation: Type.Enum(["initialize", "resume", "artifactProjection", "pullRequest", "shippingRead"], {
                type: "string",
            }),
            entityDir: Type.String({ minLength: 1 }),
        },
        { additionalProperties: false },
    ),
])
export const IntegrationContextValidator = compile(IntegrationContextSchema)

export type IntegrationContextInput = Static<typeof IntegrationContextSchema>

export default function (pi: ExtensionAPI): void {
    const toolsApi = workflowTools(pi, "integrations")
    registerWorkflowPublication(pi)
    const tools = observeMcpTools(pi)
    const resourceIdsMemo = new OperationResourceIdsMemo()
    registerResourceIds(pi, resourceIdsMemo, tools)
    registerTrackerPublication(pi, tools)
    registerWorkflowOperations(pi, tools)
    registerWorkflowContext(pi)
    pi.on("session_shutdown", async () => resourceIdsMemo.reset())

    for (const queue of [
        {
            command: "backlog",
            label: "Backlog",
            usage: "potential work",
            skill: "backlog",
            description: "Add potential work to the configured external Backlog",
        },
        {
            command: "todo",
            label: "Todo",
            usage: "queued work",
            skill: "todo",
            description: "Add queued work to the configured external Todo queue",
        },
    ] as const) {
        pi.registerCommand(queue.command, {
            description: `${queue.description} without initializing workflow work`,
            handler: async (args, ctx) => {
                const request = args.trim()
                if (!request) {
                    ctx.ui.notify(`Usage: /${queue.command} <${queue.usage}>`, "error")
                    return
                }
                pi.sendUserMessage(
                    [
                        `${queue.label} intake:`,
                        "",
                        request,
                        "",
                        `Use the ${queue.skill} skill. This is external ${queue.label} intake, not Epic/Task/Gig initialization.`,
                    ].join("\n"),
                )
            },
        })
    }

    toolsApi.registerTool({
        name: "accept_epic_tasks",
        label: "accept_epic_tasks",
        description:
            "Call after the user accepts the Epic plan or a change to its Task list. Saves epic.md's Task list (titles, Kind, labels, status, order) as accepted. Refuses unless the newest plan review cycle finished and reviewed this exact list. Task issue processing and Epic plan publication refuse while the list differs from the accepted one. Never calls providers.",
        parameters: AcceptEpicTasksSchema,
        async execute(_id, params, _signal, _onUpdate, ctx) {
            const input = parse(AcceptEpicTasksValidator, params)
            const root = project.resolveRootDir(ctx.cwd)
            assertArtifactPersistencePrepared(root)
            const result = acceptEpicTasks(resolveEntityDirectory(root, ctx.cwd, input.entityDir))
            return { content: [{ type: "text" as const, text: JSON.stringify(result, null, 2) }], details: result }
        },
    })

    toolsApi.registerTool({
        name: "plan_epic_task_issues",
        label: "plan_epic_task_issues",
        description:
            "Reconcile an Epic's accepted Tasks with fresh child-issue facts. Returns the next native creates, tags, board repairs, and cancellations in plan order, `complete`, or `blocked` with the failing Task. Keeps no saved progress; rerun after applying steps. Never calls providers.",
        parameters: PlanEpicTaskIssuesSchema,
        async execute(_id, params, _signal, _onUpdate, ctx) {
            const input = parse(PlanEpicTaskIssuesValidator, params)
            const result = planEpicTaskIssues(ctx.cwd, input, tools)
            return { content: [{ type: "text" as const, text: JSON.stringify(result, null, 2) }], details: result }
        },
    })

    toolsApi.registerTool({
        name: "record_epic_task_issue",
        label: "record_epic_task_issue",
        description:
            "Tag one untagged Epic plan Task with its issue's marker, from clear-success create evidence or a matched existing child, and drop the section's design, which now lives in the issue body. Refuses a marker the plan already carries; repeating the same tag is a no-op.",
        parameters: RecordEpicTaskIssueSchema,
        async execute(_id, params, _signal, _onUpdate, ctx) {
            const input = parse(RecordEpicTaskIssueValidator, params)
            const result = recordEpicTaskIssue(ctx.cwd, input, tools)
            return { content: [{ type: "text" as const, text: JSON.stringify(result, null, 2) }], details: result }
        },
    })

    toolsApi.registerTool({
        name: "inspect_task_design",
        label: "inspect_task_design",
        description:
            "Compare a started Task's issue body (its design), from a fresh issue read, with the design the Task last synced from. Returns `unchanged`, `unrecorded` (nothing synced yet), or `changed` with the new design. Ignores the publication signature and whitespace. Writes nothing.",
        parameters: TaskDesignSchema,
        async execute(_id, params, _signal, _onUpdate, ctx) {
            const result = inspectTaskDesign(ctx.cwd, parse(TaskDesignValidator, params), tools)
            return { content: [{ type: "text" as const, text: JSON.stringify(result, null, 2) }], details: result }
        },
    })

    toolsApi.registerTool({
        name: "record_task_design",
        label: "record_task_design",
        description:
            "Record a Task issue's current body, from a fresh issue read, as the design the Task follows: at start, after taking in a changed design, and after the Task rewrites its own issue body. Never calls providers.",
        parameters: TaskDesignSchema,
        async execute(_id, params, _signal, _onUpdate, ctx) {
            const result = recordTaskDesign(ctx.cwd, parse(TaskDesignValidator, params), tools)
            return { content: [{ type: "text" as const, text: JSON.stringify(result, null, 2) }], details: result }
        },
    })

    toolsApi.registerTool({
        name: "render_provider_body",
        label: "render_provider_body",
        description:
            "Render a reviewed ## Overview visibly and collapse the remaining accepted source using GitHub or Linear syntax for the configured destination. Preserves content and adds eligible GitHub PR closing references outside the fold. Reads local configuration and metadata without external mutation.",
        parameters: RenderProviderBodySchema,
        async execute(_toolCallId, params, _signal, _onUpdate, ctx) {
            const input = parse(RenderProviderBodyValidator, params)
            const result = { markdown: renderEntityProviderBody(ctx, input) }
            return {
                content: [{ type: "text" as const, text: JSON.stringify(result, null, 2) }],
                details: result,
            }
        },
    })

    toolsApi.registerTool({
        name: "verify_artifact_projection",
        label: "verify_artifact_projection",
        description:
            "Compare expected and provider-returned Markdown byte-for-byte and explain how to follow up on repeated provider-only formatting differences. Performs no external operation.",
        parameters: ProjectionVerificationSchema,
        async execute(_toolCallId, params) {
            const input = parse(ProjectionVerificationValidator, params)
            const result = verifyMarkdownProjection(input.expected, input.actual)
            return {
                content: [{ type: "text" as const, text: JSON.stringify(result, null, 2) }],
                details: result,
            }
        },
    })

    toolsApi.registerTool({
        name: "inspect_session_analysis",
        label: "inspect_session_analysis",
        description:
            "Read whether a finished Task, Gig, Epic, or incoming PR ends with the in-chat session analysis: on in a development install (the pi package running from a git checkout of piton) unless `[development] session_analysis = false` in the global config.host.toml, off otherwise. Reads local settings only; never calls providers or changes configuration.",
        parameters: Type.Object({}, { additionalProperties: false }),
        async execute() {
            const result = { sessionAnalysis: loadSessionAnalysis() }
            return { content: [{ type: "text" as const, text: JSON.stringify(result, null, 2) }], details: result }
        },
    })

    toolsApi.registerTool({
        name: "inspect_gate",
        label: "inspect_gate",
        description:
            "Decide whether a Task or Gig waits for the user's approval at one gate, from the item's fresh labels: `awaitApproval` when it carries that gate's configured `gate` label, otherwise `continue`. Reads local configuration only; never calls providers or changes labels.",
        parameters: InspectGateSchema,
        async execute(_id, params, _signal, _onUpdate, ctx) {
            const input = parse(InspectGateValidator, params)
            assertArtifactPersistencePrepared(ctx.cwd)
            const tracker = requireTracker(loadIntegrationRoles(ctx.cwd), "inspect_gate")
            const entity = readWorkflowEntity(ctx.cwd, input.entityDir)
            assertEntityTracker(entity.status, tracker)
            if (entity.status.entity === "epic")
                throw new Error("gates apply to a Task or Gig; an Epic plan always waits for acceptance")
            const result = gateOutcome(tracker, input.gate, input.current)
            return { content: [{ type: "text" as const, text: JSON.stringify(result, null, 2) }], details: result }
        },
    })

    const [merge, , incoming, inspect, queue, adopt, shippingMerge, mutation, labelMutation, phase, entity] =
        IntegrationContextSchema.anyOf
    const contexts = [
        ["inspect_integrations", "inspect", inspect],
        ["inspect_queue_intake", "queueIntake", queue],
        ["inspect_adoption", "adopt", adopt],
        ["initialize_integration", "initialize", entity],
        ["resume_integration", "resume", entity],
        ["prepare_integration_projection", "artifactProjection", entity],
        ["prepare_integration_pull_request", "pullRequest", entity],
        ["prepare_integration_shipping", "shippingRead", entity],
        ["prepare_integration_merge", "shippingMerge", shippingMerge],
        ["prepare_tracker_mutation", "trackerMutation", mutation],
        ["prepare_label_mutation", "labelMutation", labelMutation],
        ["inspect_incoming_pr_context", "incomingPullRequest", incoming],
        ["plan_incoming_pr_merge", "incomingPullRequestMerge", merge],
    ] as const
    for (const [name, operation, schema] of contexts) {
        const parameters = Type.Omit(schema, ["operation"])
        const validator = compile(parameters)
        toolsApi.registerTool({
            name,
            label: name,
            description: `${name.replaceAll("_", " ")}: read the exact configured tracker/forge context and required evidence. Select current/workflow explicitly when required. Return context or approved-operation arguments only; never execute provider operations or change configuration.`,
            parameters,
            async execute(_id, params, _signal, _onUpdate, ctx) {
                const input = parse(IntegrationContextValidator, { ...parse(validator, params), operation })
                resourceIdsMemo.reset()
                const result = buildIntegrationContext(ctx, input, tools)
                return { content: [{ type: "text" as const, text: JSON.stringify(result, null, 2) }], details: result }
            },
        })
    }
    const phaseMutation = (
        ctx: ExtensionContext,
        entityDir: string,
        lifecycle: LifecycleState,
        native: NativePhaseFacts,
    ) => {
        const input = parse(IntegrationContextValidator, {
            operation: "trackerMutation",
            change: "phase",
            entityDir,
            lifecycle,
            native,
        })
        resourceIdsMemo.reset()
        const result = buildIntegrationContext(ctx, input, tools)
        return { content: [{ type: "text" as const, text: JSON.stringify(result, null, 2) }], details: result }
    }
    const githubPhase = "prepare_github_phase_mutation"
    toolsApi.registerTool({
        name: githubPhase,
        label: githubPhase,
        description:
            "Calculate the native Phase assignment from the actual Project item read, which gives both lifecycle (its Status) and the current Phase. A changed assignment carries the registered provider call as `mutation`, to run unchanged. Never execute a provider mutation or change lifecycle.",
        parameters: Type.Object(
            { entityDir: phase.properties.entityDir, native: GitHubPhaseInputSchema },
            { additionalProperties: false },
        ),
        async execute(_id, params, _signal, _onUpdate, ctx) {
            const facts = githubPhaseFacts(
                requireTracker(loadIntegrationRoles(project.resolveRootDir(ctx.cwd)), githubPhase),
                entityName(readWorkflowEntity(ctx.cwd, params.entityDir)),
                parse(GitHubPhaseInputValidator, params.native),
                tools,
            )
            return phaseMutation(ctx, params.entityDir, facts.lifecycle, facts.native)
        },
    })
    const linearPhase = "prepare_linear_phase_mutation"
    toolsApi.registerTool({
        name: linearPhase,
        label: linearPhase,
        description:
            "Calculate the native Phase assignment from fresh lifecycle and provider-specific label catalog and selection facts; omit only observed empty native leaves. A changed assignment carries the registered provider call as `mutation`, to run unchanged. Never execute a provider mutation or change lifecycle.",
        parameters: Type.Object(
            {
                entityDir: phase.properties.entityDir,
                lifecycle: phase.properties.lifecycle,
                native: LinearPhaseInputSchema,
            },
            { additionalProperties: false },
        ),
        async execute(_id, params, _signal, _onUpdate, ctx) {
            const native = linearPhaseFacts(parse(LinearPhaseInputValidator, params.native))
            return phaseMutation(ctx, params.entityDir, params.lifecycle, native)
        },
    })
    const [intake, projectAction] = IncomingProjectContextSchema.anyOf
    const projects = [
        ["plan_incoming_pr_intake", "intake", intake],
        ["plan_incoming_pr_review", "review", projectAction],
        ["plan_incoming_pr_shipping", "shipping", projectAction],
        ["plan_incoming_pr_cancellation", "cancellation", projectAction],
    ] as const
    for (const [name, action, schema] of projects) {
        const parameters = Type.Omit(schema, ["operation", "action"])
        const validator = compile(parameters)
        toolsApi.registerTool({
            name,
            label: name,
            description: `Plan scoped incoming-PR Project ${action} from complete exact native membership, field, and PR evidence. Return native operation arguments without executing them; preserve maintainer-controlled Status and separate approval requirements.`,
            parameters,
            async execute(_id, params, _signal, _onUpdate, ctx) {
                const input = parse(IntegrationContextValidator, {
                    ...parse(validator, params),
                    operation: "incomingPullRequestProject",
                    action,
                })
                resourceIdsMemo.reset()
                const result = buildIntegrationContext(ctx, input, tools)
                return { content: [{ type: "text" as const, text: JSON.stringify(result, null, 2) }], details: result }
            },
        })
    }
}

type IncomingContextInput = Extract<IntegrationContextInput, { operation: "incomingPullRequest" }>
type WorkflowContextInput = Exclude<
    IntegrationContextInput,
    IncomingContextInput | IncomingMergeContextInput | IncomingProjectContextInput
>
/** The context one workflow operation returns. */
type WorkflowContext<I extends WorkflowContextInput = WorkflowContextInput> = ContextFor<
    ReturnType<typeof readProjectIntegrationContext>,
    I["operation"]
> & { target?: "current" | "workflow"; projectRoot?: string }
/** The response members whose operation covers `O`; a shared case such as `initialize | resume` covers both. */
type ContextFor<C, O> = O extends unknown
    ? C extends { operation: infer P }
        ? O extends P
            ? C
            : never
        : never
    : never
type IncomingContext = ReturnType<typeof incomingPullRequestContext>
type IncomingMergeContext = ReturnType<typeof incomingMergeContext>
type IncomingProjectContext = ReturnType<typeof incomingProjectContext>

export function buildIntegrationContext(
    ctx: ExtensionContext,
    input: IncomingContextInput,
    tools: McpToolCatalog,
): IncomingContext
export function buildIntegrationContext(
    ctx: ExtensionContext,
    input: IncomingMergeContextInput,
    tools: McpToolCatalog,
): IncomingMergeContext
export function buildIntegrationContext(
    ctx: ExtensionContext,
    input: IncomingProjectContextInput,
    tools: McpToolCatalog,
): IncomingProjectContext
export function buildIntegrationContext<I extends WorkflowContextInput>(
    ctx: ExtensionContext,
    input: I,
    tools: McpToolCatalog,
): WorkflowContext<I>
export function buildIntegrationContext(
    ctx: ExtensionContext,
    input: IntegrationContextInput,
    tools: McpToolCatalog,
): IncomingContext | IncomingMergeContext | IncomingProjectContext | WorkflowContext
export function buildIntegrationContext(ctx: ExtensionContext, input: IntegrationContextInput, tools: McpToolCatalog) {
    if (!IntegrationContextValidator.check(input))
        throw new Error("invalid integration context request: select the exact operation and its required inputs")
    if (input.operation === "incomingPullRequest") return incomingPullRequestContext(ctx.cwd, input.reference, tools)
    if (input.operation === "incomingPullRequestMerge") return incomingMergeContext(ctx.cwd, input, tools)
    if (input.operation === "incomingPullRequestProject") return incomingProjectContext(ctx.cwd, input, tools)
    return buildWorkflowIntegrationContext(ctx, input, tools)
}

function buildWorkflowIntegrationContext(ctx: ExtensionContext, input: WorkflowContextInput, tools: McpToolCatalog) {
    const selection =
        "target" in input
            ? {
                  target: input.target,
                  projectRoot: input.target === "workflow" ? developmentCheckout() : project.resolveRootDir(ctx.cwd),
              }
            : undefined
    const cwd = selection?.projectRoot ?? ctx.cwd
    try {
        const result = readProjectIntegrationContext(cwd, input, tools)
        return { ...result, ...selection }
    } catch (error) {
        if (selection?.target !== "workflow") throw error
        const message = error instanceof Error ? error.message : String(error)
        throw new Error(`Workflow project ${cwd}: ${message}`)
    }
}

/**
 * Whether an operation starts, resumes, or addresses an entity whose artifacts a worker must
 * keep in its source checkout. Project inspection and queue intake create no entity or
 * artifacts, so the worker link guard doesn't apply to them.
 */
function persistsEntityArtifacts(operation: WorkflowContextInput["operation"]): boolean {
    switch (operation) {
        case "inspect":
        case "queueIntake":
            return false
        case "adopt":
        case "initialize":
        case "resume":
        case "artifactProjection":
        case "pullRequest":
        case "shippingRead":
        case "shippingMerge":
        case "trackerMutation":
        case "labelMutation":
            return true
    }
}

/** The integration role an operation works through; `both` reports each role as it is and needs neither. */
function operationRole(operation: WorkflowContextInput["operation"]): "tracker" | "forge" | "both" {
    switch (operation) {
        case "queueIntake":
        case "adopt":
        case "artifactProjection":
        case "trackerMutation":
        case "labelMutation":
            return "tracker"
        case "pullRequest":
        case "shippingRead":
        case "shippingMerge":
            return "forge"
        case "inspect":
        case "initialize":
        case "resume":
            return "both"
    }
}

function readProjectIntegrationContext(cwd: string, input: WorkflowContextInput, tools: McpToolCatalog) {
    const operation = input.operation
    const entityDir = "entityDir" in input ? input.entityDir : undefined
    const role = operationRole(operation)
    const roles = loadIntegrationRoles(cwd)
    const name = `${operation} context`
    const configuredTracker =
        role === "tracker"
            ? requireTracker(roles, name)
            : roles.tracker.state === "on"
              ? roles.tracker.config
              : undefined
    const configuredForge =
        role === "forge" ? requireForge(roles, name) : roles.forge.state === "on" ? roles.forge.config : undefined
    assertArtifactPersistencePrepared(cwd)
    if (persistsEntityArtifacts(operation)) assertWorkerArtifactsLinked(cwd)
    const entity = entityDir ? readWorkflowEntity(cwd, entityDir) : undefined
    // A local Gig has no tracker item: tracker steps refuse for it, and others treat the tracker as off.
    const trackedEntity = entity?.status.tracker.provider !== "local"
    if (entity && role === "tracker") requireTrackerItem(entity.status.tracker, name)
    const entityTracker = trackedEntity ? configuredTracker : undefined
    if (entity && entityTracker && role !== "forge") assertEntityTracker(entity.status, entityTracker)
    if (entity && configuredForge && role !== "tracker") assertEntityForge(entity.status, configuredForge)
    if ((operation === "shippingRead" || operation === "shippingMerge") && entity?.status.entity === "epic")
        throw new Error("shipping operations require a Task or Gig")
    let shipping: GitHubMergeDecision | undefined
    if (input.operation === "shippingMerge") {
        const status = entity!.status
        const context = resolveResourceIdsContext(cwd, input.entityDir, "github-pull-request")
        if (context.scope.kind !== "github-pull-request") throw new Error("shipping requires PR addressing")
        const saved = readCachedResourceIds(context.directory, context.scope)
        shipping =
            saved && isResourceIdsKind(saved, "github-pull-request")
                ? shippingReadDecision(
                      context.scope,
                      input.evidence,
                      Git.run(cwd, ["rev-parse", `refs/heads/${status.branch.name}`]),
                      saved.value.number,
                      tools,
                  )
                : {
                      state: "blocked",
                      reason: "shipping requires locally recorded PR resource IDs; investigate and record the exact PR first",
                  }
    }
    // The tracker as the entity sees it: a local Gig has no tracker item, so its tracker reads as off.
    const tracker = <T extends { provider: TrackerConfig["provider"] }>(shape: (config: TrackerConfig) => T) =>
        trackedEntity
            ? roleContext("tracker", mapRole(roles.tracker, shape), (config) => config.provider)
            : { state: "disabled" as const, reason: "This Gig is local: it has no tracker item." }
    const forge = () => roleContext("forge", roles.forge, (config) => config.provider)
    const base = <O extends WorkflowContextInput["operation"]>(operation: O) => ({
        state: "enabled" as const,
        operation,
    })

    // Each operation returns only what it uses. The full configuration, label catalog included, is
    // `inspect`'s; every other tracker context carries the board without its labels.
    switch (input.operation) {
        case "inspect":
            return { ...base(input.operation), roles: { tracker: tracker((config) => config), forge: forge() } }
        case "queueIntake":
            return {
                ...base(input.operation),
                labels: labelsOrFailure(() =>
                    selectedLabelNames(
                        configuredTracker!.labels,
                        input.entity === "epic" ? "epic" : "deliverable",
                        input.kind === "not set" ? undefined : input.kind,
                        input.labels ?? {},
                    ),
                ),
                roles: { tracker: tracker(trackerBoard) },
            }
        case "adopt":
            return {
                ...base(input.operation),
                labelGroups: labelGroupSummary(configuredTracker!.labels, labelItemType(input.entity)),
                roles: { tracker: tracker(trackerBoard) },
            }
        case "initialize":
        case "resume":
            return {
                ...base(input.operation),
                ...(entityTracker
                    ? {
                          phaseProjection: phaseProjectionContract(
                              entity!.status.phase,
                              entityTracker,
                              entity!.status.entity,
                          ),
                      }
                    : {}),
                roles: { tracker: tracker(trackerBoard), forge: forge() },
                entity: entity!,
            }
        case "artifactProjection":
            return {
                ...base(input.operation),
                projection: planProjectionContract(configuredTracker!, entity!.status.tracker),
                roles: { tracker: tracker(trackerBoard) },
            }
        case "pullRequest":
        case "shippingRead":
            return { ...base(input.operation), roles: { forge: forge() }, entity: entity! }
        case "shippingMerge":
            // A forge operation already required the forge, so shipping always comes with an enabled forge.
            return {
                ...base(input.operation),
                roles: { forge: { ...forge(), shipping: shipping! } },
                entity: entity!,
            }
        case "trackerMutation":
            return {
                ...base(input.operation),
                ...(input.change === "phase"
                    ? planPhaseMutation(
                          configuredTracker!,
                          phaseItem(entity!.status, name),
                          entity!.status.phase,
                          input.lifecycle,
                          input.native,
                          (server, operation) => getMcpToolName(server, operation, tools),
                      )
                    : {}),
                roles: { tracker: tracker(trackerBoard) },
                entity: entity!,
            }
        case "labelMutation":
            return {
                ...base(input.operation),
                labelMutation: labelsOrFailure(() =>
                    planLabelMutation(configuredTracker!, entity!.status, input.labels, input.current),
                ),
                roles: { tracker: tracker(trackerBoard) },
                entity: entity!,
            }
    }
}

/** A role with its config reshaped, or the same off role. */
function mapRole<T, U>(role: Role<T>, shape: (config: T) => U): Role<U> {
    switch (role.state) {
        case "on":
            return { state: "on", config: shape(role.config) }
        case "off":
            return role
    }
}

/**
 * The tracker configuration without its label catalog: what board, lifecycle, Priority, Phase, and
 * publication steps use. Label choices read the catalog from `inspect` or adoption's `labelGroups`.
 */
function trackerBoard(config: TrackerConfig) {
    switch (config.provider) {
        case "github": {
            const { labels: _labels, ...board } = config
            return board
        }
        case "linear": {
            const { labels: _labels, ...board } = config
            return board
        }
    }
}

/** How plan publication works on the configured tracker, with the entity's publication state. */
function planProjectionContract(configured: TrackerConfig, bound: EntityTracker | undefined) {
    const github = configured.provider === "github"
    return {
        source: "the current epic.md or plan.md",
        contentPolicy:
            "lossless: preserve the complete source wording, sections, technical detail, and ordinary repository paths; keep its reviewed overview prose visible and collapse all remaining sections; never summarize or condense during publication",
        rendering: github
            ? "Call prepare_tracker_publication with the complete source; execute its exact registered comment mutation and save first-creation evidence with record_tracker_publication. A published plan returns reads-required: run the comment reads in its lookups and pass them as reads to prepare_tracker_publication_update, with a one-line summary of the change as announcement.summary; record the plan update, then post and record the returned plan-update note. None of these need user approval. Preserve issue title/description. Workflow publication code applies communication attribution; keep source artifacts signature-free."
            : "Call prepare_tracker_publication with the complete source; execute its exact registered document mutation and save first-creation evidence with record_tracker_publication. A published document returns reads-required: run the get_document and parent reads in its lookups and pass them as reads to prepare_tracker_publication_update, with a one-line summary of the change as announcement.summary; record the plan update, then post and record the returned Project or issue comment. None of these need user approval. Preserve issue/Project title/name and description. Workflow publication code applies communication attribution; keep source artifacts signature-free.",
        ...planPublicationState(bound),
        attribution:
            "AI-assisted text is signed by workflow publication code using current runtime identity. Automatically select local __pi_authorship: user-verbatim for complete unchanged user-written messages, without special user opt-out wording; generated or reworded content remains ai-assisted. Code strips the local mode before sending. Use complete text, not patches or template-only content.",
        authority: "local artifact" as const,
        outcomeProof:
            "publish the complete rendered body directly; trust established clear provider success without requiring a full echo or a routine confirmation read",
        recovery: github
            ? "for a pending creation, pass the source and complete registered comment pages to recover_tracker_publication. One marked comment is recorded; no marked comment returns the same create with the saved token, which you run; several stop for the maintainer. For uncertain updates, read the exact saved comment body and retry only confirmed non-application."
            : "for a pending creation, pass the source, an exact parent UUID/identifier read, complete list_documents cursor pages, and the full get_document reads requested for the parent's Plan documents to recover_tracker_publication. Listings truncate content. Documents carry no marker: one Plan document matching the pending fingerprint is recorded; no Plan document returns a create for the source, which you run; any other Plan document, several, or incomplete evidence stop. After uncertain or partial updates, read the exact saved document and reconcile before retrying only confirmed non-application.",
        discussion: "keep discussion in comments; requirements and agreed decisions belong in the source artifact",
    }
}

/** The entity's saved plan publication state, keyed by how its tracker publishes plans. */
function planPublicationState(bound: EntityTracker | undefined) {
    if (!bound) return {}
    switch (bound.provider) {
        case "github":
            return { planComment: bound.planComment }
        case "linear":
            return { planDocument: bound.planDocument }
        case "local":
            return {}
    }
}

/** A label result, or the selection's typed failure for the agent to report or correct. */
function labelsOrFailure<T>(plan: () => T): T | { state: "invalid"; reason: string; error: unknown } {
    try {
        return plan()
    } catch (error) {
        if (error instanceof LabelSelectionFailure)
            return { state: "invalid", reason: error.message, error: error.reason }
        throw error
    }
}

/**
 * The complete label set for an item, from its fresh labels. The `kind` label follows the item's
 * workflow Kind; a group the selection leaves out keeps the item's current labels.
 */
function planLabelMutation(
    tracker: TrackerConfig,
    status: ReturnType<typeof readWorkflowEntity>["status"],
    selection: LabelSelection | undefined,
    current: LabelFacts,
) {
    const itemType = status.entity === "epic" ? "epic" : "deliverable"
    const kind: WorkflowKind = status.entity === "epic" ? "epic" : status.kind
    if (tracker.provider === "github") {
        if (current.provider !== "github") throw new Error("label facts don't match the configured GitHub tracker")
        return {
            provider: "github" as const,
            ...planGitHubLabelUpdate(tracker.labels, itemType, kind, selection, current.names),
        }
    }
    if (current.provider !== "linear") throw new Error("label facts don't match the configured Linear tracker")
    const update = planLinearLabelUpdate(
        tracker.labels,
        linearPhaseGroup(tracker.phase, status.entity),
        tracker.team,
        itemType,
        kind,
        selection,
        linearLabelFacts(current),
    )
    return { provider: "linear" as const, ...update }
}

/**
 * Renders a provider body. A tracker body is an entity's plan, so it needs the entity and the tracker. A PR body needs
 * only the forge; with an entity, it's checked against the entity and closes the entity's same-repository issue.
 */
export function renderEntityProviderBody(
    ctx: ExtensionContext,
    input: { entityDir?: string; destination: "tracker" | "pullRequest"; source: string },
): string {
    const roles = loadIntegrationRoles(ctx.cwd)
    if (input.entityDir === undefined) {
        if (input.destination === "tracker") throw new Error("a tracker body is an entity's plan: pass its entityDir")
        requireForge(roles, "PR body rendering")
        return renderProviderBody(input.source, { kind: "github-pull-request" })
    }
    assertArtifactPersistencePrepared(ctx.cwd)
    const entity = readWorkflowEntity(ctx.cwd, input.entityDir)

    if (input.destination === "tracker") {
        const configured = requireTracker(roles, "tracker body rendering")
        assertEntityTracker(entity.status, configured)
        return renderProviderBody(input.source, {
            kind: configured.provider === "github" ? "github-plan-comment" : "linear-plan-document",
        })
    }
    const forge = requireForge(roles, "PR body rendering")
    assertEntityForge(entity.status, forge)
    const tracker = entity.status.tracker
    const configured = roles.tracker.state === "on" ? roles.tracker.config : undefined
    const closes =
        tracker.provider === "github" &&
        configured?.provider === "github" &&
        sameRepository(configured.repository, forge.repository)
            ? tracker.external.issueNumber
            : undefined
    return renderProviderBody(input.source, {
        kind: "github-pull-request",
        ...(closes ? { githubIssueNumber: closes } : {}),
    })
}

function readWorkflowEntity(cwd: string, raw: string) {
    const root = project.resolveRootDir(cwd)
    const resolved = resolveEntityDirectory(root, cwd, raw)
    return { directory: path.relative(root, resolved), status: readEntityStatus(resolved) }
}
function entityName(entity: ReturnType<typeof readWorkflowEntity>) {
    return { name: path.basename(entity.directory), status: entity.status }
}
