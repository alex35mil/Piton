---
name: task
description: "Use for Deliverable work: a standalone Task under <repo>/.piton/.local/tasks/, or one Epic's Task under <repo>/.piton/.local/epics/<epic>/tasks/. Initializes or resumes it, then follows the shared Deliverable lifecycle."
allowed-tools:
    - "inspect_session_analysis"
    - "preflight_task"
    - "init_local_task"
    - "init_github_task"
    - "restore_epic"
    - "restore_epic_plan_source"
    - "init_linear_task"
    - "review_task"
    - "review_epic"
    - "record_review_response"
    - "accept_epic_tasks"
    - "plan_epic_task_issues"
    - "record_epic_task_issue"
    - "order_linear_epic_issues"
    - "inspect_task_design"
    - "record_task_design"
    - "prepare_artifacts"
    - "render_provider_body"
    - "prepare_tracker_publication"
    - "prepare_tracker_publication_update"
    - "record_tracker_publication"
    - "recover_tracker_publication"
    - "verify_artifact_projection"
    - "inspect_integrations"
    - "inspect_adoption"
    - "initialize_integration"
    - "resume_integration"
    - "prepare_integration_projection"
    - "prepare_integration_pull_request"
    - "prepare_integration_shipping"
    - "prepare_integration_merge"
    - "prepare_tracker_mutation"
    - "prepare_label_mutation"
    - "inspect_gate"
    - "prepare_github_phase_mutation"
    - "prepare_linear_phase_mutation"
    - "begin_resource_creation"
    - "record_resource_creation"
    - "resolve_resource_ids"
    - "record_resource_ids"
    - "invalidate_resource_ids"
    - "select_pull_request_resource"
    - "find_pull_request"
    - "cleanup_delivery_branch"
    - "prepare_merge_resolution_comment"
    - "begin_user_review"
    - "end_user_review"
    - "read(.piton/*)"
    - "write(.piton/*)"
    - "edit(.piton/*)"
---

# Task

A Task is a Deliverable whose parent is an Epic or nothing (standalone).

## Read first

- [Core rules](../../references/core.md)
- [Deliverable lifecycle](../../references/deliverable.md)
- [Artifacts](../../references/artifacts.md)

The Deliverable lifecycle owns the shared flow. This file owns choosing the parent, initialization, and parent synchronization.

## Resume

Follow the Deliverable resume steps. The directory decides the parent. For an Epic Task, also:

1. **Read the parent:** its `brief.md`, `metadata.json`, `notes.md` when present, and `epic.md`. The Task's stored start and target never change.
2. **Design:** read the Task's issue fresh and follow [a started Task follows its issue](../../references/epic-tasks.md#task-designs).

Then continue through the Deliverable lifecycle.

## Initialize

| Request | Start |
| --- | --- |
| A Task directory, or an issue with one | [Resume](#resume) |
| An Epic's issue: on GitHub, a sub-issue of an issue labelled Epic; on Linear, an issue in a Project | [Epic Task](#epic-task) |
| Any other issue, or a description | [Standalone Task](#standalone-task) |

Ask when the parent can't be told from an ordinary issue.

### Standalone Task

Follow the intake in [artifacts](../../references/artifacts.md#initialization-inputs), then [initialize from the tracker object](../../references/integrations/shared.md#initialize-from-the-tracker-object):

1. **New work:** call `preflight_task`, then create the issue as the tracker reference describes.
2. **Adoption:** read the tracker reference ([GitHub](../../references/integrations/tracker/github.md#adopt-selected-queued-work) or [Linear](../../references/integrations/tracker/linear.md#queue-intake-and-explicit-adoption)) and the [discussion rules](../../references/integrations/shared.md#provider-bodies), and complete its preflight.
3. **Initialize:** call `init_github_task` or `init_linear_task` with the intake values, the parent `none`, and the issue as `object`.
4. **Before planning,** finish board setup.

#### Local Task

With the tracker off, or to keep a Task off it, call `init_local_task` instead. Every tracker step is skipped ([when a role is off](../../references/integrations/shared.md#when-a-role-is-off)); submission and shipping still use the forge.

### Epic Task

It starts from the queued issue its Epic's [Task issue processing](../../references/epic-tasks.md#task-issue-processing) created or linked, never a new one.

1. **Find the parent and Task:** the issue's Epic, and the plan Task its marker tags.
   - When this clone has no local directory for the Epic, [restore it](../../references/epic.md#restore-an-epic).
   - An untagged Task means issue processing hasn't finished: resume the Epic first.
   - A canceled Task can't start.
2. **Adopt the issue:** read the tracker reference ([GitHub](../../references/integrations/tracker/github.md#adopt-selected-queued-work) or [Linear](../../references/integrations/tracker/linear.md#queue-intake-and-explicit-adoption)) and the [discussion rules](../../references/integrations/shared.md#provider-bodies). Complete its Task adoption preflight, including the parent check, then follow the adopted intake in [artifacts](../../references/artifacts.md#initialization-inputs). The Kind comes from the plan's `**Kind:**` line; a conflicting `kind` label stops for a user choice.
3. **Routing:** the parent's `**Task target:**` line decides the target ([Epic routing](../../references/epic.md#routing)). When it's missing or conflicts with the intended target, ask the user before starting. An `epic` target needs the Epic branch locally.
4. **Initialize:** call `init_github_task` or `init_linear_task` with the Epic as `parent`, the plan Task's Kind, the title, Request, and slug, and the issue as `object`.
   - The issue's marker selects the plan Task, whose Kind must match. The plan is never rewritten.
   - The tool names the directory and branch from the issue. The `tracker` format uses the Linear issue's `gitBranchName`.
   - Then call `record_task_design` with the adopted issue read: the issue body is the design the Task follows.
5. **Targets:** the Task copies the routing into its immutable `start` and `target`, either the Epic branch or the repository default. No delivery check runs.

Finish board setup before planning.

## Parent synchronization

Only an Epic Task has this step. The parent's `epic.md` is the current state of the initiative, so its Task keeps it true. Stay on the Task's stored branch; Epic files live outside Git, so no branch switch is needed.

| Situation | Action |
| --- | --- |
| The Epic is still accurate | Keep implementation detail in Task artifacts. Progress comes from the Task's lifecycle. |
| A Task decision makes part of `epic.md` wrong, and the user decided it or it follows from their decision | Rewrite that text in place, then republish the Epic plan with a [plan-update note](../../references/integrations/shared.md#plan-update-notes) that names this Task as its `origin`. Don't ask. |
| A Task decision changes a Task's design (this Task's or a sibling's), and the user decided it or it follows from their decision | Rewrite that Task's issue body as [Task designs](../../references/epic-tasks.md#task-designs) describes, never the Epic section. Don't ask. |
| A mismatch the user hasn't decided on | Explain it and ask before touching the Epic. |
| The change adds, removes, cancels, retitles, or reorders a Task | Ask. It needs the Epic's [Task list acceptance](../../references/epic-tasks.md#accepted-task-list). |

- Name each Epic edit and each Task issue body rewrite in the Task's result message, in one line, so the user can object.
- A change to the Task's own published `plan.md` posts its note on the Task's issue, with no origin.
- Keep every Task marker (`[#N]` or `[ENG-N]`) through Epic edits.
- Follow the shared delivery and cleanup. The Task's `done` lifecycle completes its plan Task without another commit.

A Task never changes its parent: a standalone Task never joins an Epic, and an Epic Task never leaves it.
