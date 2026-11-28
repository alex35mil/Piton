# GitHub tracker

This procedure extends [integration roles](../shared.md). Read configuration through the context tools, then call the configured registered tools. Setup validates configuration fully; ordinary operations inspect only the fields and labels they need. When reconciling Phase, read [Phase](../../phase.md). Never enumerate unrelated provider metadata.

## Adopt selected queued work

Adoption applies only to an explicitly selected existing issue, before initialization.

1. Call `inspect_adoption` with the selected `entity`.
2. Read the configured repository and issue number to get:
   - title, description, URL, and labels;
   - issue-scoped Priority, when Priority is issue-scoped;
   - for a Task, its parent: it must be a sub-issue of the parent Epic's issue.

   Reuse facts already returned together.
3. Read every discussion page. Treat contributor messages as proposals and evidence, and resolve material requirements with the maintainer before changing scope.
4. Find the issue's item in the configured Project with one `list_project_items` read, requesting only the Status and Priority fields:
   - Filter with `repo:<owner>/<repo> is:issue #<N>`. The `#` matches the number only; a bare number also matches text, and `in:number` matches nothing.
   - When that returns no item, page through the unfiltered list before concluding the issue isn't in the Project.

   Then verify:
   - repository and issue number;
   - the numeric Project-item ID;
   - Status is Backlog or Todo;
   - Priority is a configured value, or `not set` when empty.
5. Under the explicit-selection approval in [artifacts](../../artifacts.md#initialization-inputs), reuse the title, description, copied Priority, and any configured Kind without another confirmation. A Task's Kind comes from its plan section. Ask only for missing or conflicting information.
6. For an Epic, call `init_github_epic` with the issue number, URL, and Project-item ID as `object`. A GitHub issue is its repository and number; no numeric issue ID is needed. For a Task, call `init_task` with the issue's `get` and `get_parent` reads (plus the parent's `get` when `route_task` asked for it) and the Project-item ID.

**Stops adoption:** the wrong repository, Project, or parent, a non-queue Status, ambiguity, denied access, or an unconfigured Priority.

**Kind:** a single `kind` group label on the issue becomes the initialized Kind unless the user changes it. Missing or conflicting Kind needs a user choice. After initialization, workflow metadata owns Kind.

## Initialize

Follow [initialize from the tracker object](../shared.md#initialize-from-the-tracker-object).

**Create new work** (after preflight):

- Call registered `get_me` once and use its `login` as the authenticated user for this operation.
- One registered `issue_write` create with the intake title and Request, the labels preflight returned, issue-scoped Priority, and that login as the only assignee. Task issues come only from [Task issue processing](../../epic-tasks.md#task-issue-processing), with no assignee.
- No duplicate, label, or option preflight. The create returns the issue's URL, which ends in its number. For an Epic, pass that number and URL to `init_github_epic` as `object`. For a Task, read the new issue with `get` and `get_parent`, and pass them to `init_task`. A minimal URL-only success isn't automatically uncertain.

**Board setup** (after initialization, for new and adopted work):

1. Add a new issue to the configured Project, and record the add evidence with `record_resource_ids`. Follow the [Project item ID rule](#project-item-ids). Skip this for adopted work: its item is already in the Project, and initialization recorded its ID.
2. Set In Progress, Priority, and, when Phase is configured, Planning in the Phase field, with Project writes addressed by the issue, not by item ID. Combine supported issue-field writes.
3. For adopted work and a started Task, call registered `get_me` once, then run one `issue_write` update that sets its login as the only assignee. A new issue already has it.
4. Reuse labels and parent assignment proven at creation. For adopted work and a started Task, read the issue's labels and call `prepare_label_mutation` with only the groups whose values you derived ([labels](../shared.md#labels)). For a Task's parent, call `prepare_tracker_mutation` with `hierarchy` only when a correction is needed.

**When `get_me` is unavailable or fails:** start the work without an assignee. Report that assignment was skipped and that the GitHub registration needs the `context` toolset.

A clear success proves only what that tool's contract returns; read required missing facts and reconcile partial outcomes. Assignment is the exception: `issue_write` doesn't confirm assignees, and nothing reads them back. Adopted Backlog or Todo work moves straight to In Progress, keeping the same issue and its Priority.

When a create result is uncertain, read [recovery](../recovery.md#uncertain-tracker-creation).

### Project item IDs

- The `add_project_item` response's `id` is the GraphQL node ID (`PVTI_…`). Its numeric Project item ID is `item_id`. Never pass `id` as `item_id`.
- Every other Project item response returns the numeric ID in `id`.
- Right after an add, address Project writes by issue (`item_owner`, `item_repo`, `issue_number`); they need no item ID.
- `owner_type` is optional in Project calls and their evidence. When passed, it must match the configured owner type.

## Resume and mutations

1. Call `resume_integration` with `entityDir`, then read fresh Project membership, Status, Priority, and Phase. Finish any board setup that's missing; ask the user for Priority when it's unset.
2. Reconcile Phase from the item read with Status and Phase. A cached Project-item ID reaches the current fields directly; that response must confirm the issue and Project.
3. Investigate missing or confirmed-stale Resource IDs through the configured tools, and record the actual response afterward. Network or permission errors don't prove absence. Ordinary resume never discovers PRs.

**What's projected:**

- Native Phase exists only during In Progress and shows the current activity ([Phase](../../phase.md)).
- With a configured `kind` group, exactly one `kind` label matches the workflow Kind; without it, no Kind is projected.
- Keep unrelated labels. Reuse this operation's reads and write only when the intended set differs. Confirm labels when a mutation's success doesn't show the full applied set; no full label-catalog preflight is needed.

**Changing values:**

- Use `prepare_tracker_mutation` with exactly one change: `lifecycle`, `priority`, or Task `hierarchy`. Labels change through `prepare_label_mutation`, with the issue's label names as its current facts, and apply as one `issue_write` update.
- For Phase, use `prepare_github_phase_mutation` with the target Phase and an item read taken after any Status change ([fresh native facts](../../phase.md#fresh-native-facts)), and run its returned call.
- Priority follows its configured Project or issue scope.
- Lifecycle and Priority are provider authority: never cached and never replaced by a local fallback.
- A user's request to assign an issue is an ordinary `issue_write` update; resume never reads or repairs assignees.

## Queue and plan publication

**Queue intake** follows [queue](../../queue.md). Queued work has no workflow artifact, cache, or checkpoint. Create the issue with no Phase, the `labels` that `inspect_queue_intake` returns, and set the selected Status and Priority.

**Plan publication:**

- The issue keeps its original title and description. New work creates them from the Request; adopted work keeps the existing text. A Task issue's body is its design instead, and changes as [Task designs](../../epic-tasks.md#task-designs) describes.
- For `epic.md` or `plan.md`, call `prepare_integration_projection`, then `prepare_tracker_publication` with `entityDir` and the full source ([provider bodies](../shared.md#provider-bodies)).
- Run the returned comment call, never an issue body or title update.
- The first publication saves `planComment: pending`, then returns one `add_issue_comment`. Run it immediately, then record it with `record_tracker_publication`.
- Recording or recovering the first publication also pins the comment through GitHub's API. When its `pin` is `failed`, report the reason in one line and continue. Never retry it; the maintainer can pin the comment by hand.
- Once published, prepare returns `reads-required`: run the comment reads in its `lookups`, then call `prepare_tracker_publication_update`. The update uses `update_issue_comment` on that same comment. Every update also posts a [plan-update note](../shared.md#plan-update-notes) as a new issue comment.
- A pending attempt returns `recovery-required`; read [recovery](../recovery.md#github-comment).
- Don't read just to confirm a successful update. Plan-comment state lives in authoritative metadata, and Resource IDs and Phase are unaffected.
- Resume reuses saved state and never republishes on its own.
