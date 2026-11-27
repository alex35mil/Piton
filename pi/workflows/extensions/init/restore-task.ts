import * as fs from "node:fs"
import * as path from "node:path"

import { type Static, Type } from "typebox"

import { compile, parse } from "../../../extensions/__lib/schema.js"
import type { McpToolCatalog } from "../../../extensions/__lib/mcp.js"
import * as project from "../../../extensions/__lib/project.js"
import {
    type DeliverableKind,
    DeliverableKindValidator,
    formatBranchName,
    readyBranch,
    TASK_ROOT,
} from "../__lib/domain.js"
import {
    EntityStatusValidator,
    LOCAL_DESIGNS_DIR,
    LOCAL_NOTES_FILE,
    LOCAL_PENDING_FILE,
    LOCAL_REVIEWS_DIR,
    LOCAL_SCRATCH_DIR,
    readEntityStatus,
    writeEntityStatus,
} from "../__lib/entity.js"
import { epicTaskMarker, readEpicRouting, readEpicTasks } from "../__lib/epic-tasks.js"
import { writeTextAtomically } from "../__lib/files.js"
import * as Git from "../__lib/git.js"
import { assertArtifactPersistencePrepared, assertWorkerArtifactsLinked } from "../__lib/project-config.js"
import { loadIntegrationRoles, requireTracker } from "../integrations/config.js"
import { issueParent, IssueReadsSchema } from "../integrations/issue-parent.js"
import { stripPublicationSignature } from "../integrations/publication.js"
import { describeTaskParent, type TaskTrackerParent, trackerRecordFromObject } from "../integrations/records.js"
import { EvidenceSchema } from "../integrations/resource-id-tool.js"
import { findLocalEpic, resolveDefaultBranch } from "./entity.js"
import { restorePublishedPlan } from "./restore.js"
import { resourceIdsFromTrackerObject } from "./resource-ids.js"
import { startedTask } from "./route.js"

// Rebuilds a Task started in another clone from what's shared: the tracker issue decides identity, parent,
// and Kind; the published plan becomes plan.md; the pushed branch is the work. Review rounds and notes stay
// in the clone that wrote them.

export const RestoreTaskSchema = Type.Object(
    {
        reads: IssueReadsSchema,
        fetched: Type.Boolean({
            description:
                "True once you ran the `git fetch origin` restore asked for: a branch still missing then means the other clone never pushed it",
        }),
        pages: Type.Array(EvidenceSchema, {
            description:
                "The plan reads publication recovery uses: every GitHub get_comments page of the issue; or the Linear issue read, every list_documents page, and full get_document reads of matching candidates. A Task with no published plan passes the complete reads that show none.",
        }),
    },
    { additionalProperties: false },
)
export const RestoreTaskValidator = compile(RestoreTaskSchema)
export type RestoreTaskInput = Static<typeof RestoreTaskSchema>

export type RestoreTaskResult =
    | { state: "restored"; directory: string; branch: string; plan: "restored" | "none" }
    /** The remote branch isn't known locally yet: run the fetch, then restore again. */
    | { state: "fetchRequired"; command: string[] }

export async function restoreTask(
    cwd: string,
    input: RestoreTaskInput,
    tools: McpToolCatalog,
): Promise<RestoreTaskResult & { resourceIds?: ReturnType<typeof resourceIdsFromTrackerObject> }> {
    parse(RestoreTaskValidator, input)
    const root = project.resolveRootDir(cwd)
    const projectConfig = assertArtifactPersistencePrepared(root)
    assertWorkerArtifactsLinked(root)
    const tracker = requireTracker(loadIntegrationRoles(root), "Task restore")

    const resolved = issueParent(tracker, input.reads, tools, (epic) => findLocalEpic(root, epic) !== undefined)
    switch (resolved.state) {
        case "readRequired":
            throw new Error(
                `Task restore needs the issue's ${resolved.as} read first: ${resolved.read.operation} ${JSON.stringify(resolved.read.arguments)}`,
            )
        case "unclassified":
            throw new Error(
                `${resolved.facts.label} is a sub-issue of #${resolved.parentNumber}, and no Epic Kind label is configured to tell whether #${resolved.parentNumber} is an Epic`,
            )
        case "resolved":
            break
    }
    const { facts, parent } = resolved
    const existing = startedTask(root, facts.label)
    if (existing) throw new Error(`${facts.label} already has a local Task: ${existing}`)

    // The parent decides where the Task lives, its Kind's source, and where its branch starts.
    const owner = ownerFor(root, parent, facts.label)
    const kind = owner.kind ?? kindFromLabels(tracker.labels?.groups?.kind?.values, facts.labels, facts.label)

    // The branch is a shared fact: find it on the remote, never by a guessed slug.
    if (!Git.succeeds(root, ["remote", "get-url", "origin"]))
        throw new Error("Task restore needs the `origin` remote, where the other clone pushed the branch")
    const branch = remoteBranch(root, projectConfig.branches, facts)
    if (branch.state === "fetchRequired") {
        if (input.fetched)
            throw new Error(
                `origin has no branch for ${facts.label}: the clone that started it never pushed its work, so there's nothing to restore`,
            )
        return { state: "fetchRequired", command: ["git", "fetch", "origin"] }
    }
    const name = branch.name
    const slug = slugFromBranch(name, projectConfig.branches, facts.label)

    const record = trackerRecordFromObject(facts.object, tracker.provider === "github" ? tracker.repository : undefined)
    const plan = restorePublishedPlan(tracker, record, facts.title, input.pages, tools)
    const directoryName = `${facts.label.replace(/^#/, "")}-${slug}`
    const relativeDirectory = path.join(owner.directory, directoryName)
    const directory = path.join(root, relativeDirectory)
    if (fs.existsSync(directory)) throw new Error(`target directory already exists: ${relativeDirectory}`)
    if (Git.succeeds(root, ["show-ref", "--verify", `refs/heads/${name}`]))
        throw new Error(`a local branch ${name} already exists; restore never reuses or overwrites one`)

    const status = parse(EntityStatusValidator, {
        entity: "task",
        slug,
        title: facts.title.trim(),
        kind,
        parent,
        branch: readyBranch(name, owner.start, projectConfig.branches.format === "tracker" ? "tracker" : "generated"),
        tracker: plan.state === "found" ? plan.tracker : record,
    })
    fs.mkdirSync(path.dirname(directory), { recursive: true })
    fs.mkdirSync(directory)
    let createdBranch = false
    try {
        for (const entry of [LOCAL_REVIEWS_DIR, LOCAL_SCRATCH_DIR, LOCAL_DESIGNS_DIR])
            fs.mkdirSync(path.join(directory, entry))
        // An Epic Task's issue body is its design; the request lived in the Epic. A standalone Task's is its Request.
        const request = stripPublicationSignature(facts.body).trim() || facts.title
        fs.writeFileSync(path.join(directory, "brief.md"), `# Brief\n\n${request}\n`)
        fs.writeFileSync(
            path.join(directory, LOCAL_NOTES_FILE),
            "# Notes\n\nRestored from the tracker and the remote branch. Review rounds and earlier notes stay in the clone that wrote them.\n",
        )
        fs.writeFileSync(path.join(directory, LOCAL_PENDING_FILE), "# Pending\n")
        if (plan.state === "found") writeTextAtomically(path.join(directory, "plan.md"), plan.source)
        writeEntityStatus(directory, status)
        // The local branch tracks the pushed one; nothing is checked out or changed in the working tree.
        Git.run(root, ["branch", "--track", name, `origin/${name}`])
        createdBranch = true
        readEntityStatus(directory)
    } catch (error) {
        fs.rmSync(directory, { recursive: true, force: true })
        if (createdBranch) Git.tryRun(root, ["branch", "-D", name])
        throw error
    }
    return {
        state: "restored",
        directory: relativeDirectory,
        branch: name,
        plan: plan.state === "found" ? "restored" : "none",
        resourceIds: resourceIdsFromTrackerObject(facts.object, tracker, directoryName),
    }
}

interface Owner {
    directory: string
    start: string
    /** An Epic Task's Kind comes from its plan section. */
    kind?: DeliverableKind
}

function ownerFor(root: string, parent: TaskTrackerParent, label: string): Owner {
    switch (parent.kind) {
        case "none":
            return { directory: TASK_ROOT, start: resolveDefaultBranch(root) }
        case "epic": {
            const epicDir = findLocalEpic(root, parent.epic)
            if (!epicDir)
                throw new Error(
                    `${label} belongs to ${describeTaskParent(parent)}, which this clone doesn't have; restore the Epic first`,
                )
            const epic = readEntityStatus(path.join(root, epicDir))
            if (epic.entity !== "epic") throw new Error(`${epicDir} is not an Epic`)
            const epicPath = path.join(root, epicDir, "epic.md")
            const task = readEpicTasks(epicPath).find((candidate) => epicTaskMarker(candidate) === label)
            if (!task) throw new Error(`the Epic's plan has no Task tagged ${label}`)
            // The Task's base was copied from the routing when it started; the Epic branch or the default.
            const routing = readEpicRouting(epicPath)
            return {
                directory: path.join(epicDir, "tasks"),
                start: routing.taskTarget === "epic" ? epic.branch.name : epic.branch.target,
                kind: task.kind,
            }
        }
    }
}

function kindFromLabels(
    values: Record<string, { name: string }> | undefined,
    labels: string[],
    label: string,
): DeliverableKind {
    const kinds = Object.entries(values ?? {})
        .filter(([key, value]) => key !== "epic" && labels.includes(value.name))
        .map(([key]) => key)
    const [kind] = kinds
    if (kinds.length !== 1 || !DeliverableKindValidator.check(kind))
        throw new Error(
            `${label} needs exactly one Kind label to be restored; it has ${kinds.length}. Ask the user for its Kind and set the label first`,
        )
    return kind
}

type Branches = ReturnType<typeof assertArtifactPersistencePrepared>["branches"]

function remoteBranch(
    root: string,
    branches: Branches,
    facts: { label: string; object: { provider: string; gitBranchName?: string } },
): { state: "found"; name: string } | { state: "fetchRequired" } {
    const remote = Git.tryRun(root, ["for-each-ref", "--format=%(refname:strip=3)", "refs/remotes/origin"])
        .split(/\r?\n/)
        .filter((name) => name && name !== "HEAD")
    if (branches.format === "tracker") {
        const name = facts.object.gitBranchName
        if (!name) throw new Error("the tracker branch format needs the Linear issue's gitBranchName in the issue read")
        return remote.includes(name) ? { state: "found", name } : { state: "fetchRequired" }
    }
    const prefix = branchPrefix(branches, facts.label)
    const matches = remote.filter((name) => name.startsWith(prefix))
    if (matches.length > 1)
        throw new Error(
            `several remote branches match ${prefix}: ${matches.join(", ")}; ask the user which one is this Task's`,
        )
    return matches.length === 1 ? { state: "found", name: matches[0] } : { state: "fetchRequired" }
}

/** The branch name the configured format renders for this reference, up to the slug. */
function branchPrefix(branches: Branches, label: string): string {
    const format = branches.format
    if (format === "tracker") throw new Error("unreachable: the tracker format names branches itself")
    // `formatBranchName` lowercases and adds the username; a placeholder slug keeps the prefix exact.
    const sample = formatBranchName(`${label.replace(/^#/, "")}-x`, format, branches.username)
    return sample.slice(0, -1)
}

function slugFromBranch(name: string, branches: Branches, label: string): string {
    if (branches.format === "tracker") {
        // Linear branch names are `<user>/<identifier>-<title>`; the slug is the part after the identifier.
        const base = name.split("/").pop() ?? name
        const prefix = `${label.toLowerCase()}-`
        return base.toLowerCase().startsWith(prefix) ? base.slice(prefix.length) : base
    }
    return name.slice(branchPrefix(branches, label).length)
}
