# Epic Tasks

An accepted Epic owns one tracker issue for every planned Task, so the tracker's Epic progress reflects the whole plan. This file owns the Task format, the accepted Task list, Task issue processing, Task designs, progress, and removing Tasks.

## Epic Task format

`epic.md` lists Tasks as `### ` sections under one `## Tasks` section. Section order is Task order.

```markdown
## Tasks

### [#12] Build the parser

**Kind:** `feature`
**Labels:** `area=backend`, `perf=perf`
**Summary:** Parse config files into typed settings.

### Wire the CLI

**Kind:** `chore`
**Summary:** Expose the parser through the CLI.

The Task's full design: its outcome and where it stops. Any Markdown: paragraphs, lists, code blocks.
```

- **Heading:** `<title>` before the Task's issue exists, then `[<marker>] <title>`. Titles are unique within the Epic.
- **Fields:** each is a bold name, then the value. Values other than the summary are code spans. Code rejects the plain `Kind: feature` form.
- **Kind:** the first body line, one of `feature`, `bugfix`, `refactor`, `research`, `audit`, or `chore`. A Task starts with this Kind.
- **Labels:** an optional ``**Labels:** `group=value`, …`` line right after Kind, one code span per entry, with values for the label groups that apply to Deliverables, never `kind`. A group that needs a value and has none blocks the Task's issue processing.
- **Status:** an optional ``**Status:** `canceled` `` line after Kind and Labels, only on a tagged Task. It removes the Task from the plan.
- **Summary:** a required `**Summary:**` line after the other fields: one line of plain text on what the Task is for.
- **Design:** the rest of the section, only before the Task's issue exists. It must not be empty, and becomes the issue's body. Once tagged, a section has no design: the issue holds it ([Task designs](#task-designs)).

Code rejects anything else, including a Task section outside `## Tasks`.

## Accepted Task list

The Task list is each Task's title, Kind, labels, and status, in order. Markers, summaries, and designs aren't part of it.

- `accept_epic_tasks` saves it in the Epic's metadata after the user accepts. It refuses unless the newest plan review cycle finished and reviewed this exact list.
- Task issue processing and Epic plan publication refuse while `epic.md`'s list differs from the accepted one, and say what differs.
- So adding, removing, canceling, retitling, or reordering a Task needs `review_epic` with `newReview`, the user's acceptance, then `accept_epic_tasks`.
- Any other `epic.md` edit, such as rewording a summary, a design, or the Overview, needs neither.
- [Restoring](./epic.md#restore-an-epic) an Epic records the published plan's list as accepted.

## Task issue processing

Run it after `accept_epic_tasks` and on every Epic resume, before any publication of the Epic:

1. Read the Epic's Priority fresh, and every child issue with complete pagination: GitHub sub-issues in their listed order, with their configured Project Status and Priority, or Linear issues in the Epic's Project.
2. Call `plan_epic_task_issues`. It reconciles the whole plan against those facts and keeps no saved progress.
3. Apply its steps in order:
   - Run each `create`, then call `record_epic_task_issue` with that create as `evidence`. The create sends the design as the issue body, and recording trims the section to its fields and summary.
   - For a `tag`, call `record_epic_task_issue` without evidence. A tag links an existing queued child with the same title, such as one created before an interruption.
   - Run each board repair, cancellation, close, and reorder as returned.
4. Repeat from step 1 until it returns `complete`. On `blocked`, stop and report the Task and reason.
5. On Linear, then call `order_linear_epic_issues`. On failure, report it and rerun it later; it reads fresh positions.

**Rules:**

- Accepting the Task list approves these creates, cancellations, and board updates, with no second confirmation. Rerunning after an interruption needs no new approval.
- Created issues are sub-issues on GitHub, or Project issues on Linear, with the Task's `kind` label and its Labels values.
- A tagged Task whose issue is In Progress, In Review, or Done has started, with or without a local Task directory; it may have started in another clone.
- A tagged Task that hasn't started must be a child of this Epic in Backlog, Todo, or Canceled. A local Task directory on a queued issue is an error.
- Processing ensures every queued Task's board state itself, whatever Project automations did first: on the Project, Status Todo (Backlog or empty becomes Todo), and the Epic's Priority when the Task has none. A Priority already set on the Task is kept, and a Canceled issue is left alone. To defer a Task, mark it canceled or replan; a manual Backlog move is reverted.
- A started Task (In Progress or In Review) marked canceled is set to Canceled and closed. Its branch, PR, and local directory stay as they are. A Done Task can't be canceled.
- Once every Task is settled, the Epic's children are ordered to the plan: its Tasks first, in section order, then canceled Tasks and other children in their current order. GitHub gets reorder steps; Linear uses step 5.
- An uncertain create needs no separate recovery: the next run's title lookup finds the issue or confirms it's absent.
- Creating issues never initializes a Task. Task issues stay unassigned until the Task starts.

## Task designs

A Task issue's body is the Task's current design until the Task is Done or Canceled, queued or started. The Epic section keeps only the summary, so the design has one home.

**Changing a design:** whoever changes it updates the issue. That's an Epic session that replans the Task, a sibling Task's [parent synchronization](../skills/task/SKILL.md#parent-synchronization), or the Task itself when a decision changes its own design.

1. Read the issue body fresh, then write the complete new body: `issue_write` `update` on GitHub, `save_issue` on Linear.
2. Post a one-line comment on that issue saying what changed and why.
3. When the change alters what the Task is for, update its `**Summary:**` line and republish the Epic plan.

The design change is the user's decision, so neither write needs approval of its text. A Task that rewrites its own body then calls `record_task_design` with a fresh issue read.

**A started Task follows its issue.** On every resume, read the issue fresh and call `inspect_task_design`:

| Result | Action |
| --- | --- |
| `unchanged` | Continue. |
| `unrecorded` | The Task started before designs were tracked. Call `record_task_design`; change nothing else. |
| `changed` | Replace the Request in `brief.md` with the returned design, update `plan.md` to match and republish it with a plan-update note, then call `record_task_design`. Name the change in one line. No approval is needed. |

Task initialization calls `record_task_design` with the adopted issue read, after creating the Task.

## Progress and replanning

Progress comes from the plan and each tagged Task's lifecycle, read fresh from the tracker.

| Plan Task | Tracker lifecycle | State |
| --- | --- | --- |
| untagged | — | unpublished; issue processing hasn't finished |
| tagged | Backlog or Todo | queued |
| tagged | In Progress or In Review | started |
| tagged | Done | complete |
| tagged | Canceled | canceled |

**Replanning:**

- New Tasks get issues through issue processing.
- To remove a queued or started Task, keep its section and marker and add ``**Status:** `canceled` ``. Never delete a tagged section.
- A complete or canceled Task can't be removed.
- To link an existing queued child issue of this Epic, add its section already tagged with that issue's marker, its fields, and a summary.
- A Task's design changes in its issue ([Task designs](#task-designs)). Editing its title, Kind, or summary in `epic.md` doesn't rewrite the issue.
- A tagged section that still holds a design (from before Task designs moved to issues) fails to parse. Move that text into the issue body, unless the body already matches, post the one-line note, and keep only the summary in the section.
