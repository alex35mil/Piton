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
    // ``**Labels:** `group=value`, …``: the Task's label selection, grouped by label group. Without
    // the line, every group is empty; Task issue processing checks it against the configuration.
    labels: Record<string, string[]>
    // `**Summary:** …`: one line on what the Task is for. The Epic keeps only this once the issue exists.
    summary: string
}

// One planned Task in accepted epic.md: a `### ` section under `## Tasks`. The Task issue is the
// home of its design: before the issue exists the section carries the design that becomes the
// issue body; once tagged, the section is only a reference and the issue body holds the design.
export type EpicTask = EpicTaskFields &
    (
        | {
              issue: "pending"
              // The section body after `**Summary:**`: the Task's full design, sent as the new issue's body.
              design: string
          }
        | {
              issue: "tagged"
              // The Task's tracker reference: `#12` or `ENG-12`.
              marker: string
              // ``**Status:** `canceled` ``: removed from the plan; its issue is canceled, never started.
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

// Epic field lines read `**Name:** value`: a bold name, then the value. Values other than the
// summary are code spans: ``**Kind:** `feature` ``, ``**Labels:** `area=fe`, `perf=perf` ``.
type FieldName = "Kind" | "Labels" | "Status" | "Summary" | "Task target" | "Epic branch"
const TASK_FIELDS = ["Kind", "Labels", "Status", "Summary"] as const satisfies readonly FieldName[]

/** The trimmed value of a `**<name>:** value` line, or undefined when the line is another line. */
function fieldValue(line: Line | undefined, name: FieldName): string | undefined {
    if (!line || line.fenced) return undefined
    const prefix = `**${name}:**`
    const text = line.text.trim()
    return text.startsWith(prefix) ? text.slice(prefix.length).trim() : undefined
}

/** The field a line writes in the plain `Name: value` form, which Epics no longer accept. */
function plainField(line: Line | undefined, names: readonly FieldName[]): FieldName | undefined {
    if (!line || line.fenced) return undefined
    return names.find((name) => line.text.trim().startsWith(`${name}:`))
}

const plainFieldMessage = (name: FieldName) =>
    `\`${name}:\` lines are written \`**${name}:** <value>\`, with code-formatted values except the summary`

/** The text inside a value that is exactly one code span, or undefined. */
function codeSpan(value: string): string | undefined {
    return /^`([^`\s]+)`$/.exec(value)?.[1]
}

type ParsedTask = EpicTask & {
    headingIndex: number
    // The `**Summary:**` line and the end of the section, so tagging can drop the design.
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
        if (fieldValue(next, "Kind") !== undefined)
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
    // A plain-form line among the leading field lines gets the new form as its error. The design
    // after them is free text, so a paragraph starting with `Status:` there is fine.
    for (const line of fields) {
        if (fieldValue(line, "Summary") !== undefined) break
        if (TASK_FIELDS.some((name) => fieldValue(line, name) !== undefined)) continue
        const plain = plainField(line, TASK_FIELDS)
        if (plain) fail(plainFieldMessage(plain))
        break
    }
    const kindValue = fieldValue(fields[0], "Kind")
    if (kindValue === undefined) fail("the first body line must be ``**Kind:** `<kind>` ``")
    const kind = codeSpan(kindValue!)
    if (!kind) fail(`Kind ${JSON.stringify(kindValue)} must be one code span, such as \`\`**Kind:** \`feature\` \`\``)
    let validatedKind: DeliverableKind
    try {
        validatedKind = validateDeliverableKind(kind!)
    } catch {
        return fail(`invalid Kind ${JSON.stringify(kind)}`)
    }
    let nextField = 1
    const labelsValue = fieldValue(fields[nextField], "Labels")
    let labels: Record<string, string[]> = {}
    if (labelsValue !== undefined) {
        labels = parseLabels(labelsValue, fail)
        nextField++
    }
    const statusValue = fieldValue(fields[nextField], "Status")
    let canceled = false
    if (statusValue !== undefined) {
        if (codeSpan(statusValue) !== "canceled") fail("the only allowed Status is ``**Status:** `canceled` ``")
        if (!tagged) fail("``**Status:** `canceled` `` is only valid on a Task that already has an issue")
        canceled = true
        nextField++
    }
    const summaryLine = fields[nextField]
    const summary = fieldValue(summaryLine, "Summary")
    if (!summary)
        fail(
            "needs a one-line `**Summary:** <what the Task is for>` after `**Kind:**`, `**Labels:**`, and `**Status:**`",
        )
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
                "a Task with an issue keeps only its fields and `**Summary:**` in the Epic; its design belongs in the Task issue's body",
            )
        return { ...common, issue: "tagged", marker: tagged[1], canceled }
    }
    if (!design) fail("the design after `**Summary:**` is empty; it becomes the Task issue's body")
    return { ...common, issue: "pending", design }
}

/** `` `area=frontend`, `area=backend`, `perf=perf` `` → `{ area: ["frontend", "backend"], perf: ["perf"] }`. */
function parseLabels(text: string, fail: (message: string) => never): Record<string, string[]> {
    const labels: Record<string, string[]> = {}
    const entries = text.split(",").map((entry) => entry.trim())
    if (entries.length === 1 && entries[0] === "") return labels
    for (const entry of entries) {
        const pair = codeSpan(entry) ?? ""
        const match = /^([a-z][a-z0-9-]*)=([a-z][a-z0-9-]*)$/.exec(pair)
        if (!match) fail(`invalid Labels entry ${JSON.stringify(entry)}; write each entry as \`\` \`group=value\` \`\``)
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

// Reads ``**Task target:** `…` `` and ``**Epic branch:** `…` `` from the `## Task delivery strategy` section.
export function parseEpicRouting(content: string): EpicRouting {
    const lines = scanLines(content)
    const start = lines.find((line) => !line.fenced && line.text.trim() === ROUTING_HEADING)
    if (!start)
        throw new Error(
            "Epic has no `## Task delivery strategy` section with `**Task target:**` and `**Epic branch:**`",
        )
    const end = lines.find((line) => line.index > start.index && isLevelOneOrTwo(line))?.index ?? lines.length
    const names = ["Task target", "Epic branch"] as const
    const fields = new Map<FieldName, string>()
    for (const line of lines.slice(start.index + 1, end)) {
        const plain = plainField(line, names)
        if (plain) throw new Error(`Epic ${plainFieldMessage(plain)}`)
        for (const name of names) {
            const value = fieldValue(line, name)
            if (value === undefined) continue
            if (fields.has(name)) throw new Error(`Epic repeats \`**${name}:**\``)
            fields.set(name, value)
        }
    }
    const targetValue = fields.get("Task target")
    const target = targetValue === undefined ? undefined : codeSpan(targetValue)
    if (target !== "default" && target !== "epic")
        throw new Error(
            "Epic `## Task delivery strategy` needs ``**Task target:** `default` `` or ``**Task target:** `epic` ``",
        )
    const branchValue = fields.get("Epic branch")
    const branch = branchValue === undefined ? undefined : codeSpan(branchValue)
    if (!branch) throw new Error("Epic `## Task delivery strategy` needs ``**Epic branch:** `<name>` ``")
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
