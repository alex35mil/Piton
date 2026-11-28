import * as fs from "node:fs"
import * as path from "node:path"

import { type Static, Type } from "typebox"

import { compile, describeErrors, parse } from "../../../extensions/__lib/schema.js"
import * as project from "../../../extensions/__lib/project.js"
import type { McpToolCatalog } from "../../../extensions/__lib/mcp.js"
import {
    DeliverableKindSchema,
    EPIC_ROOT,
    entityName,
    formatBranchName,
    readyBranch,
    type ReadyBranchContract,
    slugify,
    TASK_ROOT,
    trackerReferenceLabel,
} from "../__lib/domain.js"
import {
    ENTITY_METADATA_FILE,
    entityLabel,
    LOCAL_DESIGNS_DIR,
    LOCAL_NOTES_FILE,
    LOCAL_PENDING_FILE,
    LOCAL_REVIEWS_DIR,
    LOCAL_SCRATCH_DIR,
    type EntityStatus,
    EntityStatusValidator,
    readEntityStatus,
    readEpicTaskDirectories,
    resolveEntityDirectory,
    writeEntityStatus,
} from "../__lib/entity.js"
import { type EpicTask, epicTaskMarker, readEpicRouting, readEpicTasks } from "../__lib/epic-tasks.js"
import type { ResourceIds } from "../integrations/resource-ids.js"
import { stripPublicationSignature } from "../integrations/publication.js"
import { resourceIdsFromTrackerObject } from "./resource-ids.js"
import { issueParent, type IssueReads, IssueReadsSchema } from "../integrations/issue-parent.js"
import * as Git from "../__lib/git.js"
import {
    assertArtifactPersistencePrepared,
    assertWorkerArtifactsLinked,
    type ProjectConfig,
} from "../__lib/project-config.js"
import { loadIntegrationRoles, requireTracker, type TrackerConfig } from "../integrations/config.js"
import { LabelSelectionFailure, LabelSelectionSchema, selectedLabelNames } from "../integrations/label-selection.js"
import {
    GitHubTrackerObjectSchema,
    GitHubTrackerObjectValidator,
    describeTaskParent,
    type EpicTrackerItem,
    isEpicTrackerItem,
    LinearProjectObjectSchema,
    LinearProjectObjectValidator,
    type TaskTrackerParent,
    type TrackerObject,
    trackerRecordFromObject,
    trackerReference,
} from "../integrations/records.js"

const OneLineSchema = Type.String({ minLength: 1, pattern: "^[^\\r\\n]+$" })

const baseProperties = {
    title: Type.String({
        minLength: 1,
        pattern: "^[^\\r\\n]+$",
        description:
            "User-approved cleaned title; correct accidental spelling, capitalization, punctuation, and formatting without changing meaning or exact technical literals",
    }),
    slug: OneLineSchema,
    request: Type.String({
        minLength: 1,
        description:
            "User-approved durable Request; preserve meaning while correcting grammar, spelling, capitalization, punctuation, and formatting; use the workflow command argument for new work or the tracker item's title/name and description for adopted work, plus agreed clarifications",
    }),
}
const epicProperties = { ...baseProperties, entity: Type.Literal("epic") }
const taskEntityProperties = { ...baseProperties, kind: DeliverableKindSchema, entity: Type.Literal("task") }
// Only new work picks labels before its tracker object exists; adopted work keeps its own.
const preflightLabels = { labels: Type.Optional(LabelSelectionSchema) }

// The read-only checks that run before a new Epic or standalone Task tracker object is created. An Epic
// Task never creates its object: its Epic's issue processing did.
export const EpicPreflightSchema = Type.Object(
    { ...epicProperties, ...preflightLabels },
    { additionalProperties: false },
)
export const TaskPreflightSchema = Type.Object(
    { ...taskEntityProperties, ...preflightLabels },
    { additionalProperties: false },
)
export const PreflightSchema = Type.Union([EpicPreflightSchema, TaskPreflightSchema])
export const PreflightValidator = compile(PreflightSchema)
export type PreflightInput = Static<typeof PreflightSchema>

/** The fields an Epic's init tool shares with its preflight, without the label selection. */
export const EpicInitializationPropertiesSchema = Type.Object(
    Type.Omit(Type.Object(epicProperties), ["entity"]).properties,
    { additionalProperties: false },
)
/**
 * A Task's init tool fields: its preflight's, without the labels, plus the issue's tracker reads. The reads,
 * never caller input, decide the tracker object and the parent: an Epic Task adopts the queued issue its Epic's
 * issue processing created, and the issue's marker selects the plan Task, whose Kind must match.
 */
export const TaskInitializationPropertiesSchema = Type.Object(
    {
        ...Type.Omit(Type.Object(taskEntityProperties), ["entity"]).properties,
        reads: IssueReadsSchema,
        projectItemId: Type.Optional(
            Type.Integer({
                minimum: 1,
                maximum: Number.MAX_SAFE_INTEGER,
                description:
                    "GitHub, adopted work only: the numeric Project item ID from the Project read, recorded with the issue",
            }),
        ),
    },
    { additionalProperties: false },
)

// Initialization records an existing tracker object, created or adopted, as the entity.
export const EntityInitializationSchema = Type.Union([
    Type.Object(
        {
            ...epicProperties,
            object: Type.Union([GitHubTrackerObjectSchema, LinearProjectObjectSchema]),
        },
        { additionalProperties: false },
    ),
    Type.Object(
        { ...TaskInitializationPropertiesSchema.properties, entity: Type.Literal("task") },
        { additionalProperties: false },
    ),
])
export const EntityInitializationValidator = compile(EntityInitializationSchema)
export type EntityInitialization = Static<typeof EntityInitializationSchema>

export interface InitializeEntityOptions {
    cwd: string
    now?: Date
    tools?: McpToolCatalog
    /** Runs the branch cleanup after a failed initialization (tests); piton's daemon in a sandboxed session. */
    runner?: Git.ProgramRunner
}

export interface InitializedEntity {
    directory: string
    status: EntityStatus
    resourceIds: ResourceIds[]
}

/** A Task's parent, checked: its local Epic's plan and routing, or none. An Epic has no parent. */
type PreparedParent = { kind: "epic"; epic: ParentEpic } | { kind: "none" }

interface Prepared {
    root: string
    projectConfig: ProjectConfig
    tracker: TrackerConfig
    slug: string
    start: string
    parent: PreparedParent
}

/** A Task's tracker object and parent, as its issue reads decide them. */
interface ResolvedTaskIssue {
    object: TrackerObject
    parent: TaskTrackerParent
    prepared: PreparedParent
}

/** The local checks every initialization runs before anything is created. */
function prepareRepository(cwd: string) {
    const root = project.resolveRootDir(cwd)
    assertGitRepository(root)
    const projectConfig = assertArtifactPersistencePrepared(root)
    assertWorkerArtifactsLinked(root)
    return { root, projectConfig }
}

function prepare(
    input: PreflightInput | EntityInitialization,
    options: InitializeEntityOptions,
    resolveParent: (root: string, tracker: TrackerConfig) => PreparedParent = () => ({ kind: "none" }),
): Prepared {
    const { root, projectConfig } = prepareRepository(options.cwd)
    const tracker = requireTracker(loadIntegrationRoles(root), `${input.entity} initialization from a tracker item`)
    const slug = slugify(input.slug)
    const parent = resolveParent(root, tracker)
    const start = (() => {
        switch (parent.kind) {
            case "epic":
                return parent.epic.branch
            case "none":
                return resolveDefaultBranch(root)
        }
    })()
    assertCloneBranch(root, start)
    return { root, projectConfig, tracker, slug, start, parent }
}

/**
 * A Task issue's tracker object and parent, from its reads. A parent Epic must be local: restore it first
 * when it isn't. The reads must be complete; a missing read refuses with the call to run.
 */
function resolveTaskIssue(
    root: string,
    tracker: TrackerConfig,
    reads: IssueReads,
    tools: McpToolCatalog,
): ResolvedTaskIssue {
    const result = issueParent(tracker, reads, tools, (epic) => findLocalEpic(root, epic) !== undefined)
    switch (result.state) {
        case "readRequired":
            throw new Error(
                `Task initialization needs the issue's ${result.as} read first: ${result.read.operation} ${JSON.stringify(result.read.arguments)}`,
            )
        case "unclassified":
            throw new Error(
                `${result.facts.label} is a sub-issue of #${result.parentNumber}, and no Epic Kind label is configured to tell whether #${result.parentNumber} is an Epic`,
            )
        case "resolved":
            break
    }
    const parent = result.parent
    switch (parent.kind) {
        case "none":
            return { object: result.facts.object, parent, prepared: { kind: "none" } }
        case "epic": {
            const epic = findLocalEpic(root, parent.epic)
            if (!epic)
                throw new Error(
                    `${result.facts.label} belongs to ${describeTaskParent(parent)}, which this clone doesn't have; restore the Epic first`,
                )
            return { object: result.facts.object, parent, prepared: { kind: "epic", epic: readParentEpic(root, epic) } }
        }
    }
}

/** The local Epic directory for a tracker item, when this clone has one. */
export function findLocalEpic(root: string, epic: EpicTrackerItem): string | undefined {
    const epicsRoot = path.join(root, EPIC_ROOT)
    if (!fs.existsSync(epicsRoot)) return undefined
    for (const entry of fs
        .readdirSync(epicsRoot, { withFileTypes: true })
        .sort((a, b) => a.name.localeCompare(b.name))) {
        if (!entry.isDirectory()) continue
        const directory = path.join(epicsRoot, entry.name)
        const status = readEntityStatus(directory)
        if (status.entity === "epic" && isEpicTrackerItem(status.tracker, epic)) return path.relative(root, directory)
    }
    return undefined
}

// Runs every local check that doesn't need the tracker object, so a failure stops
// before anything is created on the tracker. Changes nothing.
export function preflightEntity(input: PreflightInput, options: InitializeEntityOptions) {
    parse(PreflightValidator, input)
    const prepared = prepare(input, options)
    const { tracker, projectConfig } = prepared
    if (projectConfig.branches.format === "tracker" && tracker.provider !== "linear")
        throw new Error("the tracker branch format requires a Linear tracker")
    const itemType = input.entity === "epic" ? "epic" : "deliverable"
    const kind = input.entity === "epic" ? "epic" : input.kind
    let labels: string[]
    try {
        labels = selectedLabelNames(tracker.labels, itemType, kind, input.labels ?? {})
    } catch (error) {
        if (error instanceof LabelSelectionFailure) throw new Error(`labels: ${error.message}`)
        throw error
    }
    return {
        state: "ready" as const,
        entity: input.entity,
        tracker: tracker.provider,
        resource: tracker.provider === "linear" && input.entity === "epic" ? ("project" as const) : ("issue" as const),
        start: prepared.start,
        labels,
        next: "Create only the tracker object with the approved title and Request and these labels, then call the matching init tool with its evidence.",
    }
}

export async function initializeEntity(
    input: EntityInitialization,
    options: InitializeEntityOptions,
): Promise<InitializedEntity> {
    if (!EntityInitializationValidator.check(input)) {
        const details = describeErrors(EntityInitializationValidator.errors(input)).join("; ")
        throw new Error(`invalid entity initialization: ${details}`)
    }
    let resolved: ResolvedTaskIssue | undefined
    const prepared = prepare(input, options, (root, tracker) => {
        switch (input.entity) {
            case "epic":
                return { kind: "none" }
            case "task":
                resolved = resolveTaskIssue(root, tracker, input.reads, options.tools ?? new Map())
                if (input.projectItemId !== undefined) {
                    if (resolved.object.provider !== "github")
                        throw new Error("projectItemId is a GitHub Project item; a Linear issue has none")
                    resolved = { ...resolved, object: { ...resolved.object, projectItemId: input.projectItemId } }
                }
                return resolved.prepared
        }
    })
    const { root, tracker: configured, slug, start, parent } = prepared
    const object = (() => {
        switch (input.entity) {
            case "epic":
                return epicTrackerObject(input, configured)
            case "task":
                return resolved!.object
        }
    })()
    const reference = trackerReference(object)
    const name = entityName(reference, slug)
    const relativeDirectory = (() => {
        if (input.entity === "epic") return path.join(EPIC_ROOT, name)
        switch (parent.kind) {
            case "epic": {
                // The marker selects the plan Task. Issue processing verified that the issue is a
                // queued child of this Epic before it tagged the plan; adoption re-reads it.
                const planned = plannedTaskFor(parent.epic, trackerReferenceLabel(reference)!)
                if (planned.kind !== input.kind)
                    throw new Error(
                        `the Epic plan gives ${trackerReferenceLabel(reference)} the Kind ${planned.kind}, not ${input.kind}; start it with the plan's Kind or change the plan first`,
                    )
                return path.join(parent.epic.directory, "tasks", name)
            }
            case "none":
                return path.join(TASK_ROOT, name)
        }
    })()
    const absoluteDirectory = path.join(root, relativeDirectory)
    if (fs.existsSync(absoluteDirectory)) throw new Error(`entity directory already exists: ${relativeDirectory}`)
    const branch = branchFor(prepared, name, object)
    assertBranchAvailable(root, branch, start)
    const contract = readyBranch(
        branch,
        start,
        prepared.projectConfig.branches.format === "tracker" ? "tracker" : "generated",
    )
    const tracker = trackerRecordFromObject(
        object,
        configured.provider === "github" ? configured.repository : undefined,
    )

    let createdBranch: string | undefined
    try {
        createBranch(root, contract)
        createdBranch = contract.name
        const common = {
            slug,
            title: input.title.trim(),
            branch: contract,
            tracker,
        }
        const status = parse(
            EntityStatusValidator,
            input.entity === "epic"
                ? { ...common, entity: "epic" }
                : { ...common, entity: "task", kind: input.kind, parent: resolved!.parent },
        )
        createEntityArtifacts(absoluteDirectory, input, status)
        return {
            directory: relativeDirectory,
            status: readEntityStatus(absoluteDirectory),
            resourceIds: resourceIdsFromTrackerObject(object, configured, name),
        }
    } catch (error) {
        fs.rmSync(absoluteDirectory, { recursive: true, force: true })
        if (createdBranch) await deleteCreatedBranch(root, createdBranch, options.runner)
        throw error
    }
}

export const LocalTaskInitializationSchema = Type.Object(
    { ...baseProperties, kind: DeliverableKindSchema },
    { additionalProperties: false },
)
export const LocalTaskInitializationValidator = compile(LocalTaskInitializationSchema)
export type LocalTaskInitialization = Static<typeof LocalTaskInitializationSchema>

/**
 * Initializes a standalone Task with no tracker item, whether or not a tracker is configured. Its creation date
 * names it (`<YYYYMMDD>-<slug>`); it has no Phase and records no Resource IDs. Every tracker step refuses for it.
 */
export async function initializeLocalTask(
    input: LocalTaskInitialization,
    options: InitializeEntityOptions,
): Promise<InitializedEntity> {
    parse(LocalTaskInitializationValidator, input)
    const { root, projectConfig } = prepareRepository(options.cwd)
    const slug = slugify(input.slug)
    const start = resolveDefaultBranch(root)
    assertCloneBranch(root, start)
    const createdAt = (options.now ?? new Date()).toISOString()
    const tracker = { provider: "local" as const, createdAt }
    const name = entityName(trackerReference(tracker), slug)
    const relativeDirectory = path.join(TASK_ROOT, name)
    const absoluteDirectory = path.join(root, relativeDirectory)
    if (fs.existsSync(absoluteDirectory)) throw new Error(`entity directory already exists: ${relativeDirectory}`)
    // No tracker names its branch, so the `tracker` format falls back to the dated name, as a Linear Project Epic does.
    const format = projectConfig.branches.format
    const branch = format === "tracker" ? name : formatBranchName(name, format, projectConfig.branches.username)
    assertBranchAvailable(root, branch, start)
    const contract = readyBranch(branch, start, "generated")

    let createdBranch: string | undefined
    try {
        createBranch(root, contract)
        createdBranch = contract.name
        const status = parse(EntityStatusValidator, {
            slug,
            title: input.title.trim(),
            branch: contract,
            tracker,
            entity: "task",
            kind: input.kind,
            // No tracker item, so no tracker parent: a local Task is always standalone.
            parent: { kind: "none" },
        })
        createEntityArtifacts(absoluteDirectory, { entity: "task", request: input.request }, status)
        return { directory: relativeDirectory, status: readEntityStatus(absoluteDirectory), resourceIds: [] }
    } catch (error) {
        fs.rmSync(absoluteDirectory, { recursive: true, force: true })
        if (createdBranch) await deleteCreatedBranch(root, createdBranch, options.runner)
        throw error
    }
}

type EpicInitialization = Extract<EntityInitialization, { entity: "epic" }>

/** An Epic's tracker object, as the caller gives it. A Task's comes from its reads (`resolveTaskIssue`). */
function epicTrackerObject(input: EpicInitialization, tracker: TrackerConfig): TrackerObject {
    const provider = tracker.provider
    if (provider === "github") {
        if (!GitHubTrackerObjectValidator.check(input.object))
            throw new Error("initialization requires a GitHub issue for the configured GitHub tracker")
        return { provider, ...input.object }
    }
    if (!LinearProjectObjectValidator.check(input.object))
        throw new Error("initialization requires a Linear Project for the configured Linear tracker")
    return { provider, resource: "project", ...input.object }
}

function branchFor(prepared: Prepared, name: string, object: TrackerObject): string {
    const format = prepared.projectConfig.branches.format
    if (format !== "tracker") return formatBranchName(name, format, prepared.projectConfig.branches.username)
    // Linear names issue branches; a Project has none, so an Epic uses its dated name.
    if (object.provider === "linear" && object.resource === "project") return name
    if (object.provider !== "linear" || !object.gitBranchName)
        throw new Error("the tracker branch format requires the Linear issue's gitBranchName")
    return object.gitBranchName
}

function createEntityArtifacts(
    absoluteDirectory: string,
    input: Pick<EntityInitialization, "entity" | "request">,
    status: EntityStatus,
): void {
    const directories =
        input.entity === "epic"
            ? [LOCAL_REVIEWS_DIR, LOCAL_SCRATCH_DIR, "tasks"]
            : [LOCAL_REVIEWS_DIR, LOCAL_SCRATCH_DIR, LOCAL_DESIGNS_DIR]
    // An adopted item's description can end with the signature publication added; it stays out of artifacts.
    const request = stripPublicationSignature(input.request).trim()
    if (!request) throw new Error("request is empty without its publication signature")
    fs.mkdirSync(path.dirname(absoluteDirectory), { recursive: true })
    fs.mkdirSync(absoluteDirectory, { recursive: false })
    for (const directory of directories) fs.mkdirSync(path.join(absoluteDirectory, directory))
    fs.writeFileSync(path.join(absoluteDirectory, "brief.md"), `# Brief\n\n${request}\n`)
    fs.writeFileSync(path.join(absoluteDirectory, LOCAL_NOTES_FILE), "# Notes\n")
    if (input.entity !== "epic") fs.writeFileSync(path.join(absoluteDirectory, LOCAL_PENDING_FILE), "# Pending\n")
    writeEntityStatus(absoluteDirectory, status)
}

function plannedTaskFor(parent: ParentEpic, marker: string): EpicTask {
    const task = parent.tasks.find((candidate) => epicTaskMarker(candidate) === marker)
    if (!task || task.issue !== "tagged")
        throw new Error(
            `parent Epic has no Task tagged ${marker}; finish the Epic's issue processing before starting it`,
        )
    if (task.canceled) throw new Error(`parent Epic Task ${marker} is canceled and can't be started`)
    return task
}

interface ParentEpic {
    label: string
    directory: string
    branch: string
    tasks: EpicTask[]
}

function readParentEpic(root: string, rawDirectory: string): ParentEpic {
    const epicsRoot = path.join(root, EPIC_ROOT)
    const directory = path.resolve(root, rawDirectory)
    if (!directory.startsWith(`${epicsRoot}${path.sep}`)) {
        throw new Error(`parent Epic directory must be inside ${EPIC_ROOT}`)
    }

    const status = readEntityStatus(directory)
    if (status.entity !== "epic") throw new Error(`parent directory is not an Epic: ${rawDirectory}`)
    const epicPath = path.join(directory, "epic.md")
    if (!fs.existsSync(epicPath)) throw new Error(`parent Epic has no accepted epic.md: ${rawDirectory}`)
    const tasks = readEpicTasks(epicPath)
    const routing = readEpicRouting(epicPath)
    if (routing.epicBranch !== status.branch.name)
        throw new Error(
            `epic.md says \`\`**Epic branch:** \`${routing.epicBranch}\` \`\`, but the Epic's branch is ${status.branch.name}`,
        )
    // A default-target Task needs nothing from the Epic branch; an Epic-target Task starts from it.
    const taskBranch = routing.taskTarget === "epic" ? status.branch.name : status.branch.target
    if (routing.taskTarget === "epic" && !cloneBranchExists(root, taskBranch))
        throw new Error(`Epic-target Tasks start from the Epic branch ${taskBranch}; fetch or create it locally first`)
    if (status.tracker.provider === "linear" && status.tracker.resource !== "project")
        throw new Error("parent Epic must be bound to a Linear Project")
    const started = readEpicTaskDirectories(directory)

    return {
        label: entityLabel(status),
        directory: path.relative(root, directory),
        branch: taskBranch,
        // A Task that already has a directory can't be started again.
        tasks: tasks.filter((task) => {
            const marker = epicTaskMarker(task)
            return marker === undefined || !started.has(marker)
        }),
    }
}

export function resolveDefaultBranch(root: string): string {
    const remoteHead = Git.tryRun(root, ["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"])
    if (remoteHead) {
        const branch = remoteHead.replace(/^origin\//, "")
        assertCloneBranch(root, branch)
        return branch
    }

    const recorded = recordedDefaultBranches(root)
    if (recorded.length === 1) {
        assertCloneBranch(root, recorded[0])
        return recorded[0]
    }
    if (recorded.length > 1) {
        throw new Error(`initialized work records conflicting repository default branches: ${recorded.join(", ")}`)
    }

    const currentBranch = Git.tryRun(root, ["symbolic-ref", "--quiet", "--short", "HEAD"])
    if (currentBranch && cloneBranchExists(root, currentBranch)) return currentBranch

    const branches = Git.run(root, ["for-each-ref", "--format=%(refname:short)", "refs/heads"])
        .split(/\r?\n/)
        .filter(Boolean)
    if (branches.length === 1) return branches[0]
    if (branches.length === 0) {
        throw new Error("repository has no commits; create the initial commit before initializing workflow work")
    }
    throw new Error(
        "repository default branch is ambiguous; check out the intended local branch before initializing work",
    )
}

interface RecordedBranchFacts {
    name: string
    start: string
    target: string
}

// Reads only the branch facts default-branch discovery needs, without strict
// validation; readEntityStatus owns that for the entity being operated on.
function readRecordedBranchFacts(entityDirectory: string): RecordedBranchFacts | null {
    const metadataPath = path.join(entityDirectory, ENTITY_METADATA_FILE)
    if (!fs.existsSync(metadataPath)) return null
    let metadata: unknown
    try {
        metadata = JSON.parse(fs.readFileSync(metadataPath, "utf-8"))
    } catch (cause) {
        throw new Error(`cannot parse ${metadataPath}: ${String(cause)}`)
    }
    if (typeof metadata !== "object" || metadata === null) {
        throw new Error(`invalid entity metadata in ${metadataPath}`)
    }
    const record = metadata as Record<string, unknown>
    // An Epic Task's base can be its Epic's branch, so only Epics and standalone Tasks record the default.
    if (record.entity !== "epic" && record.entity !== "task") return null
    const branch = (record.branch ?? {}) as Record<string, unknown>
    if (typeof branch.start !== "string" || typeof branch.target !== "string") {
        throw new Error(`cannot read recorded branch in ${metadataPath}`)
    }
    return {
        name: path.basename(entityDirectory),
        start: branch.start,
        target: branch.target,
    }
}

function recordedDefaultBranches(root: string): string[] {
    const branches = new Set<string>()
    for (const relativeRoot of [EPIC_ROOT, TASK_ROOT]) {
        const entityRoot = path.join(root, relativeRoot)
        if (!fs.existsSync(entityRoot)) continue
        for (const entry of fs.readdirSync(entityRoot, { withFileTypes: true })) {
            if (!entry.isDirectory()) continue
            const facts = readRecordedBranchFacts(path.join(entityRoot, entry.name))
            if (!facts) continue
            if (facts.start !== facts.target) {
                throw new Error(`${facts.name} has inconsistent default-branch start and target`)
            }
            branches.add(facts.start)
        }
    }
    return [...branches]
}

function assertGitRepository(root: string): void {
    Git.run(root, ["rev-parse", "--show-toplevel"])
}

function assertCloneBranch(root: string, branch: string): void {
    Git.run(root, ["show-ref", "--verify", `refs/heads/${branch}`])
}

function cloneBranchExists(root: string, branch: string): boolean {
    return Git.succeeds(root, ["show-ref", "--verify", `refs/heads/${branch}`])
}

function assertBranchAvailable(root: string, branch: string, start: string): void {
    assertCloneBranch(root, start)
    if (cloneBranchExists(root, branch)) throw new Error(`workflow branch already exists: ${branch}`)
    Git.run(root, ["check-ref-format", "--branch", branch])
}

function createBranch(root: string, contract: ReadyBranchContract): void {
    Git.run(root, ["branch", contract.name, contract.start])
}

/** Best effort: removes the branch a failed initialization created. The initialization's own error is what's reported. */
async function deleteCreatedBranch(root: string, branch: string, runner?: Git.ProgramRunner): Promise<void> {
    if (!Git.succeeds(root, ["show-ref", "--verify", `refs/heads/${branch}`])) return
    try {
        await Git.deleteBranch(root, branch, `remove the branch ${branch} a failed initialization created`, runner)
    } catch {
        // Left in place; the next initialization reports it as an existing branch.
    }
}
