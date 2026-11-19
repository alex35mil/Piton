import { readEntityStatus, writeEntityStatus } from "../workflows/extensions/__lib/entity.ts"
import { epicTaskList, parseEpicTasks } from "../workflows/extensions/__lib/epic-tasks.ts"

/** Records an Epic's Task list in `source` as accepted, as accept_epic_tasks leaves it. */
export function acceptTaskList(epicDirectory: string, source: string): void {
    const status = readEntityStatus(epicDirectory)
    if (status.entity !== "epic") throw new Error(`not an Epic: ${epicDirectory}`)
    writeEntityStatus(epicDirectory, { ...status, acceptedTasks: epicTaskList(parseEpicTasks(source)) })
}

export const phaseValues = {
    planning: "Planning",
    execution: "Execution",
    selfReview: "Self-Review",
}

const label = (name: string) => ({ name, color: "#8250DF", description: "" })

export function linearPhaseGroup(scope: "workspace" | "team" = "workspace", group = "Phase") {
    return {
        scope,
        group: label(group),
        values: {
            planning: label(phaseValues.planning),
            execution: label(phaseValues.execution),
            selfReview: label(phaseValues.selfReview),
        },
    }
}

export function linearPhases() {
    return { issues: linearPhaseGroup(), projects: linearPhaseGroup() }
}

export const kindLabelNames = {
    epic: "Kind: Epic",
    feature: "Kind: Feature",
    bugfix: "Kind: Bugfix",
    research: "Kind: Research",
    refactor: "Kind: Refactor",
    audit: "Kind: Audit",
    chore: "Kind: Chore",
}

/** The `kind` group with one label per workflow kind, named from `names`. */
export function kindGroup(names: Record<keyof typeof kindLabelNames, string> = kindLabelNames) {
    return {
        appliesTo: ["epic", "deliverable"] as ("epic" | "deliverable")[],
        cardinality: "exactlyOne" as const,
        values: Object.fromEntries(Object.entries(names).map(([key, name]) => [key, label(name)])),
    }
}

export function kindLabels(names?: Record<keyof typeof kindLabelNames, string>) {
    return { groups: { kind: kindGroup(names) } }
}

/** The Linear `kind` group: Deliverables only, since a Linear Epic is a Project. */
export function linearKindGroup(scope: "workspace" | "team" = "workspace") {
    const { epic: _epic, ...values } = kindGroup().values
    return {
        appliesTo: ["deliverable"] as ("epic" | "deliverable")[],
        cardinality: "exactlyOne" as const,
        values,
        scope,
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

export interface GitHubPhaseReadsTarget {
    server: string
    project: { owner: string; ownerType: "user" | "org"; number: number }
    repository: { owner: string; repo: string }
    issueNumber: number
}

/**
 * The `projects_get` item read that GitHub Phase tools take, in the shape the GitHub MCP returns it, with
 * Status and Phase; `current: null` omits the Phase `value`.
 */
export function githubPhaseReads(
    target: GitHubPhaseReadsTarget,
    current: string | null = "Planning",
    status = "In Progress",
) {
    const address = {
        owner: target.project.owner,
        owner_type: target.project.ownerType,
        project_number: target.project.number,
    }
    const tool = `mcp__${target.server}__projects_get`
    return {
        item: {
            tool,
            arguments: { method: "get_project_item", ...address, item_id: 291, field_names: ["Status", "Phase"] },
            response: {
                id: 291,
                node_id: "PVTI_item",
                content_type: "Issue",
                content: {
                    id: 900,
                    number: target.issueNumber,
                    repository: `${target.repository.owner}/${target.repository.repo}`,
                    html_url: `https://github.com/${target.repository.owner}/${target.repository.repo}/issues/${target.issueNumber}`,
                },
                fields: [
                    {
                        id: 418,
                        name: "Status",
                        data_type: "single_select",
                        value: { id: "status", name: status, color: "YELLOW" },
                    },
                    {
                        id: 427,
                        name: "Phase",
                        data_type: "single_select",
                        ...(current === null ? {} : { value: { id: "phase", name: current, color: "BLUE" } }),
                    },
                ],
            },
        },
    }
}

// A minimal valid tracker for fixtures that only exercise the forge. It's a Linear
// tracker, so incoming-PR work sees no GitHub Project board.
export function minimalTracker() {
    return {
        provider: "linear" as const,
        mcpServer: "linear",
        team: "Engineering",
        phase: linearPhases(),
        statuses: { issues: { ...statuses }, projects: { ...statuses } },
    }
}
