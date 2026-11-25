# Phase

Phase is the current activity: Planning, Execution, or Self-Review; Status is the lifecycle. Both live only on the tracker, like Priority. Nothing local stores Phase.

Phase is for visibility: it shows team members and observers where the work is. Nothing waits on it, and no tool requires a Phase.

## When Phase isn't configured

Phase is optional: a GitHub Project field, or Linear label groups, chosen at [setup](#setup).

- When `inspect_integrations` shows no Phase, or an entity context returns `phaseProjection` with `state: notConfigured`, skip every Phase step in every workflow: setting, clearing, reading, and labeling.
- The Phase tools then refuse.

## Transitions

| Activity or event | Status | Phase |
| --- | --- | --- |
| Backlog or Todo queue | Backlog, Todo | absent |
| Initialization, investigation, planning, plan review | In Progress | Planning |
| Implementation and its corrections, including commit steps | In Progress | Execution |
| Final review, and the stop with the result | In Progress | Self-Review |
| PR created, or revisions published, for external human review | In Review | absent |
| Corrections after Self-Review, submitted or not | In Progress | Execution |
| Final review of those corrections | In Progress | Self-Review |
| Confirmed completion or cancellation | Done, Canceled | absent |

- A direct-path Deliverable goes from Planning straight to Execution once it chose the path.
- An Epic stays in Planning until the user accepts it, then moves to Execution. A Task or Gig whose plan [waits for approval](./planning.md#plan-approval) does the same.
- **A failed Phase read or write** is reported in one line, and work continues. The next Phase change or resume reconciles it.
- **On resume,** take the activity from where the work stands (no plan yet, implementing, or stopped after final review), and set that Phase when the native one differs.

**In Review** starts only after real submission for external human review: a successfully created or recovered reviewable PR, or successful publication of approved revisions.

- A local commit is not submission. With no external submission route, never set In Review.
- Existing delivery evidence can still allow direct completion.
- Accepting an Epic returns it to Execution. Its explicit completion goes from In Progress straight to Done.

Provider automation can change native Status; for example, GitHub can move pushed work back to In Progress. Reuse fresh provider facts and skip assignments that are already applied. Don't configure automation or infer Phase from native Status.

## Native projection

During In Progress, Phase shows the current activity. Outside In Progress, clear it. Queued work has no Phase.

- **GitHub:** the configured Project single-select Phase field.
- **Linear:** the configured label group, a Project-label group for Epics and an issue-label group for Tasks and Gigs. The groups are mutually exclusive; apply only their children and keep every unrelated label.

## Reconciling

1. **Lifecycle first.** For a required lifecycle change, update native Status and confirm the outcome. Never cache lifecycle or Phase locally.
2. **Prepare.** Call `prepare_github_phase_mutation` or `prepare_linear_phase_mutation` with `entityDir`, the target `phase`, and fresh `native` facts (below); Linear also takes fresh `lifecycle`. Reuse reads from this operation, except that a GitHub item read must come after any Status change.
   - The target is the activity's Phase while In Progress, and `none` otherwise, which clears it. The tool refuses a target that doesn't match the fresh lifecycle.
3. **Run the returned `mutation`** unchanged. Only a changed assignment has one; otherwise there's nothing to write.
   - Exception: when Linear labels change in the same step, run only the combined label save ([Linear board setup](./integrations/tracker/linear.md#initialize)), not this `mutation`.
4. The tool calls no provider and grants no approval. A provider failure creates no local substitute; report it in one line and continue.

**New work:** the tracker object exists before the local entity. Board setup after initialization moves it to In Progress and applies Planning from fresh native facts. A Linear create may already carry the Planning label; GitHub Phase is assigned after Project membership exists.

### Fresh native facts

**GitHub:** pass one actual `projects_get` `get_project_item` read of the entity's Project item, unchanged, with the configured Status and Phase fields in `field_names`. Code takes lifecycle from its Status and the current Phase from its Phase entry, and checks the Project, issue, and field type. No field read is needed; setup validates the Phase options.

- Read the item after any Status change in the same operation. An earlier read still shows the old Status, so Phase wouldn't clear.

**Linear:** `{ catalog, applied }`, both required even when empty. Omit an optional field only when you actually read it and it was empty. Code turns that absence into null. Never omit a value you didn't read.

- `applied`: every currently applied label ID.
- `catalog`: the selected group, all its children, and any same-named groups needed to prove scope and identity. Finish pagination first. Each entry has `id`, `name`, and `isGroup`, plus `team`, `parentId`, `groupType`, and `retiredAt` when present.
- The configured group must resolve to one active group in its workspace or team scope, and each Phase to one active child of it. Issue groups must be `singleSelect`. Project groups must be `singleSelect` too; omit `groupType` only when Linear reports none, which it treats as single-select.
- Use Project-label reads for Projects and issue-label reads for issues.
- Same-named children outside the group are unrelated labels and must survive. Replacing Phase removes every applied child of the managed group, including extras, and changes no other ID.

## Setup

Setup asks whether to track Phase. When the user declines, it provisions nothing and saves no Phase. Otherwise, GitHub setup provisions or validates the Project field and options under its preview and hash approval. On Linear, the label setup tools create or check one Project-label group and one issue-label group, each single-select with the three active children, under the same approval ([label groups](./setup.md#label-groups)). Runtime never creates native metadata.
