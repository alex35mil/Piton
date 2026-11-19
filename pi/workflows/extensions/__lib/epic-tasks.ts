import * as fs from "node:fs"

import { type Static, Type } from "typebox"

import {
    type DeliverableKind,
    DeliverableKindSchema,
    LINEAR_IDENTIFIER_PATTERN,
    validateDeliverableKind,
} from "./domain.js"
import { writeTextAtomically } from "./files.js"

interface EpicTaskFields {
    title: string
    kind: DeliverableKind
    // `Labels: group=value, …`: the Task's label selection, grouped by label group. Without the
    // line, every group is empty; Task issue processing checks it against the configuration.
    labels: Record<string, string[]>
    // `Summary: …`: one line on what the Task is for. The Epic keeps only this once the issue exists.
    summary: string
}

// One planned Task in accepted epic.md: a `### ` section under `## Tasks`. The Task issue is the
// home of its design: before the issue exists the section carries the design that becomes the
// issue body; once tagged, the section is only a reference and the issue body holds the design.
export type EpicTask = EpicTaskFields &
    (
        | {
              issue: "pending"
              // The section body after `Summary:`: the Task's full design, sent as the new issue's body.
              design: string
          }
        | {
              issue: "tagged"
              // The Task's tracker reference: `#12` or `ENG-12`.
              marker: string
              // `Status: canceled`: removed from the plan; its issue is canceled, never started.
              canceled: boolean
          }
    )

/** The marker of a tagged Task, or undefined before its issue exists. */
export function epicTaskMarker(task: EpicTask): string | undefined {
    switch (task.issue) {
        case "pending":
            return undefined
        case "tagged":
            return task.marker
    }
}

const TASKS_HEADING = "## Tasks"
const MARKER = `#[1-9][0-9]*|${LINEAR_IDENTIFIER_PATTERN.slice(1, -1)}`
const TASK_HEADING = /^###\s+(.*\S)\s*$/
const TAGGED_TITLE = new RegExp(`^\\[(${MARKER})\\]\\s+(\\S.*)$`)

interface Line {
    index: number
    text: string
    // Inside a fenced code block, where headings and fields are plain text.
    fenced: boolean
}

function scanLines(content: string): Line[] {
    let fence: string | undefined
    return content.split(/\r?\n/).map((text, index) => {
        const opener = /^\s*(`{3,}|~{3,})/.exec(text)?.[1]
        if (fence) {
            if (opener && opener[0] === fence[0] && opener.length >= fence.length) {
                fence = undefined
            }
            return { index, text, fenced: true }
        }
        if (opener) {
            fence = opener
            return { index, text, fenced: true }
        }
        return { index, text, fenced: false }
    })
}

const isLevelOneOrTwo = (line: Line) => !line.fenced && /^#{1,2}\s/.test(line.text)
const isSectionEnd = (line: Line) => !line.fenced && /^#{1,3}\s/.test(line.text)

type ParsedTask = EpicTask & {
    headingIndex: number
    // The `Summary:` line and the end of the section, so tagging can drop the design.
    summaryIndex: number
    endIndex: number
}

function parse(content: string): ParsedTask[] {
    const lines = scanLines(content)
    const starts = lines.filter((line) => !line.fenced && line.text.trim() === TASKS_HEADING)
    if (starts.length > 1) throw new Error("Epic contains more than one `## Tasks` section")

    const tasks: ParsedTask[] = []
    const inTasks = new Set<number>()
    if (starts.length === 1) {
        const start = starts[0].index
        const end = lines.find((line) => line.index > start && isLevelOneOrTwo(line))?.index ?? lines.length
        for (let index = start + 1; index < end; index++) inTasks.add(index)
        const headings = lines.filter(
            (line) => line.index > start && line.index < end && !line.fenced && /^###\s/.test(line.text),
        )
        for (const heading of headings) {
            const sectionEnd =
                lines.find((line) => line.index > heading.index && isSectionEnd(line))?.index ?? lines.length
            const sectionLines = lines.slice(heading.index, Math.min(sectionEnd, end))
            tasks.push(parseTask(sectionLines, Math.min(sectionEnd, end)))
        }
    }
    // A Kind-bearing level-three section outside `## Tasks` is a misplaced Task.
    for (const line of lines) {
        if (line.fenced || inTasks.has(line.index) || !/^###\s/.test(line.text)) continue
        const next = lines.slice(line.index + 1).find((candidate) => candidate.text.trim())
        if (next && !next.fenced && /^Kind:/.test(next.text.trim()))
            throw new Error(`Epic Task section is outside \`## Tasks\`: ${JSON.stringify(line.text.trim())}`)
    }

    const titles = tasks.map((task) => task.title)
    const duplicateTitle = titles.find((title, index) => titles.indexOf(title) !== index)
    if (duplicateTitle) throw new Error(`Epic contains duplicate Task title ${JSON.stringify(duplicateTitle)}`)
    const markers = tasks.flatMap((task) => epicTaskMarker(task) ?? [])
    const duplicateMarker = markers.find((marker, index) => markers.indexOf(marker) !== index)
    if (duplicateMarker) throw new Error(`Epic contains duplicate Task marker ${duplicateMarker}`)
    return tasks
}

function parseTask(section: Line[], endIndex: number): ParsedTask {
    const [heading, ...body] = section
    const headingText = TASK_HEADING.exec(heading.text)?.[1]
    if (!headingText) throw new Error(`Epic Task heading has no title: ${JSON.stringify(heading.text)}`)
    const tagged = TAGGED_TITLE.exec(headingText)
    if (!tagged && headingText.startsWith("["))
        throw new Error(`Epic contains an invalid Task marker: ${JSON.stringify(heading.text)}`)
    const title = tagged ? tagged[2] : headingText
    const fail = (message: string): never => {
        throw new Error(`Epic Task ${JSON.stringify(title)}: ${message}`)
    }

    const fields = body.filter((line) => line.text.trim())
    const kindLine = fields[0]
    const kind = kindLine && !kindLine.fenced ? /^Kind:\s*(\S+)\s*$/.exec(kindLine.text.trim())?.[1] : undefined
    if (!kind) fail("the first body line must be `Kind: <kind>`")
    let validatedKind: DeliverableKind
    try {
        validatedKind = validateDeliverableKind(kind!)
    } catch {
        return fail(`invalid Kind ${JSON.stringify(kind)}`)
    }
    const field = (index: number, name: string) => {
        const line = fields[index]
        return line && !line.fenced && line.text.trim().startsWith(`${name}:`) ? line : undefined
    }
    let nextField = 1
    const labelsLine = field(nextField, "Labels")
    let labels: Record<string, string[]> = {}
    if (labelsLine) {
        labels = parseLabels(labelsLine.text.trim().slice("Labels:".length), fail)
        nextField++
    }
    const statusLine = field(nextField, "Status")
    let canceled = false
    if (statusLine) {
        if (!/^Status:\s*canceled\s*$/.test(statusLine.text.trim()))
            fail("the only allowed Status is `Status: canceled`")
        if (!tagged) fail("`Status: canceled` is only valid on a Task that already has an issue")
        canceled = true
        nextField++
    }
    const summaryLine = field(nextField, "Summary")
    const summary = summaryLine?.text.trim().slice("Summary:".length).trim()
    if (!summaryLine || !summary)
        fail("needs a one-line `Summary: <what the Task is for>` after `Kind:`, `Labels:`, and `Status:`")
    const design = body
        .filter((line) => line.index > summaryLine!.index)
        .map((line) => line.text)
        .join("\n")
        .trim()
    const common = {
        title,
        kind: validatedKind!,
        labels,
        summary: summary!,
        headingIndex: heading.index,
        summaryIndex: summaryLine!.index,
        endIndex,
    }
    if (tagged) {
        if (design)
            fail(
                "a Task with an issue keeps only its fields and `Summary:` in the Epic; its design belongs in the Task issue's body",
            )
        return { ...common, issue: "tagged", marker: tagged[1], canceled }
    }
    if (!design) fail("the design after `Summary:` is empty; it becomes the Task issue's body")
    return { ...common, issue: "pending", design }
}

/** `area=frontend, area=backend, perf=perf` → `{ area: ["frontend", "backend"], perf: ["perf"] }`. */
function parseLabels(text: string, fail: (message: string) => never): Record<string, string[]> {
    const labels: Record<string, string[]> = {}
    const pairs = text.split(",").map((pair) => pair.trim())
    if (pairs.length === 1 && pairs[0] === "") return labels
    for (const pair of pairs) {
        const match = /^([a-z][a-z0-9-]*)=([a-z][a-z0-9-]*)$/.exec(pair)
        if (!match) fail(`invalid Labels entry ${JSON.stringify(pair)}; use \`group=value\``)
        const [, group, value] = match!
        const values = (labels[group] ??= [])
        if (values.includes(value)) fail(`Labels repeats ${JSON.stringify(pair)}`)
        values.push(value)
    }
    return labels
}

export function hasEpicTasksSection(content: string): boolean {
    return scanLines(content).some((line) => !line.fenced && line.text.trim() === TASKS_HEADING)
}

export function parseEpicTasks(content: string): EpicTask[] {
    return parse(content).map(({ headingIndex: _, summaryIndex: __, endIndex: ___, ...task }) => task)
}

export function readEpicTasks(epicPath: string): EpicTask[] {
    return parseEpicTasks(fs.readFileSync(epicPath, "utf-8"))
}

// The Task list the user accepted: each Task's title, Kind, labels, and status, in order. Issue
// markers, summaries, and designs aren't part of it, so tagging a Task or rewording its summary
// or design keeps the list accepted.
export const AcceptedTaskSchema = Type.Object(
    {
        title: Type.String({ minLength: 1 }),
        kind: DeliverableKindSchema,
        labels: Type.Record(Type.String(), Type.Array(Type.String())),
        status: Type.Enum(["planned", "canceled"], { type: "string" }),
    },
    { additionalProperties: false },
)
export type AcceptedTask = Static<typeof AcceptedTaskSchema>

export function epicTaskList(tasks: readonly EpicTask[]): AcceptedTask[] {
    return tasks.map((task) => ({
        title: task.title,
        kind: task.kind,
        // Groups and values sorted, so the list compares by content only.
        labels: Object.fromEntries(
            Object.keys(task.labels)
                .sort()
                .map((group) => [group, [...task.labels[group]].sort()]),
        ),
        status: task.issue === "tagged" && task.canceled ? "canceled" : "planned",
    }))
}

function describeTask(task: AcceptedTask): string {
    const labels = Object.entries(task.labels).flatMap(([group, values]) => values.map((value) => `${group}=${value}`))
    return [`"${task.title}"`, task.kind, ...(labels.length ? [labels.join(", ")] : []), task.status].join(" · ")
}

/** How `current` differs from `accepted`, one line per difference, or undefined when they are equal. */
export function taskListDifference(
    accepted: readonly AcceptedTask[],
    current: readonly AcceptedTask[],
): string | undefined {
    const lines: string[] = []
    const length = Math.max(accepted.length, current.length)
    for (let index = 0; index < length; index++) {
        const before = accepted[index]
        const after = current[index]
        if (before && after && JSON.stringify(before) === JSON.stringify(after)) continue
        const position = `Task ${index + 1}`
        if (!after) lines.push(`${position}: removed ${describeTask(before!)}`)
        else if (!before) lines.push(`${position}: added ${describeTask(after)}`)
        else lines.push(`${position}: ${describeTask(before)} → ${describeTask(after)}`)
    }
    return lines.length ? lines.join("\n") : undefined
}

// The accepted routing for Tasks started from now on, published with the plan.
export interface EpicRouting {
    taskTarget: "default" | "epic"
    epicBranch: string
}

const ROUTING_HEADING = "## Task delivery strategy"

// Reads `Task target:` and `Epic branch:` from the `## Task delivery strategy` section.
export function parseEpicRouting(content: string): EpicRouting {
    const lines = scanLines(content)
    const start = lines.find((line) => !line.fenced && line.text.trim() === ROUTING_HEADING)
    if (!start)
        throw new Error("Epic has no `## Task delivery strategy` section with `Task target:` and `Epic branch:`")
    const end = lines.find((line) => line.index > start.index && isLevelOneOrTwo(line))?.index ?? lines.length
    const fields = new Map<string, string>()
    for (const line of lines.slice(start.index + 1, end)) {
        if (line.fenced) continue
        const field = /^(Task target|Epic branch):\s*(\S+)\s*$/.exec(line.text.trim())
        if (!field) continue
        if (fields.has(field[1])) throw new Error(`Epic repeats \`${field[1]}:\``)
        fields.set(field[1], field[2])
    }
    const target = fields.get("Task target")
    if (target !== "default" && target !== "epic")
        throw new Error("Epic `## Task delivery strategy` needs `Task target: default` or `Task target: epic`")
    const branch = fields.get("Epic branch")
    if (!branch) throw new Error("Epic `## Task delivery strategy` needs `Epic branch: <name>`")
    return { taskTarget: target, epicBranch: branch }
}

export function readEpicRouting(epicPath: string): EpicRouting {
    return parseEpicRouting(fs.readFileSync(epicPath, "utf-8"))
}

// Tags an untagged Task's heading with its new issue's marker and drops its design, which now
// lives in the issue body, leaving the heading, fields, and summary. Re-tagging the same Task
// with the same marker is a no-op, so an interrupted record can be repeated safely.
export function tagEpicTask(epicPath: string, title: string, marker: string): "tagged" | "unchanged" {
    if (!new RegExp(`^(?:${MARKER})$`).test(marker)) throw new Error(`invalid Task marker ${JSON.stringify(marker)}`)
    const content = fs.readFileSync(epicPath, "utf-8")
    const tasks = parse(content)
    const task = tasks.find((candidate) => candidate.title === title)
    if (!task) throw new Error(`Epic has no Task titled ${JSON.stringify(title)}`)
    switch (task.issue) {
        case "tagged":
            if (task.marker === marker) return "unchanged"
            throw new Error(`Epic Task ${JSON.stringify(title)} is already tagged ${task.marker}`)
        case "pending":
            break
    }
    if (tasks.some((candidate) => epicTaskMarker(candidate) === marker))
        throw new Error(`Epic already contains Task marker ${marker}`)
    const newline = content.includes("\r\n") ? "\r\n" : "\n"
    const lines = content.split(/\r?\n/)
    lines[task.headingIndex] = lines[task.headingIndex].replace(/^(###\s+)/, `$1[${marker}] `)
    // One blank line separates the trimmed section from what follows, or ends the file.
    lines.splice(task.summaryIndex + 1, task.endIndex - task.summaryIndex - 1, "")
    writeTextAtomically(epicPath, lines.join(newline))
    return "tagged"
}
