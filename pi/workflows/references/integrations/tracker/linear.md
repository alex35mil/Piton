# Linear tracker

This procedure extends [integration roles](../shared.md).

| Workflow entity | Linear object | Relationship | Branch |
| --- | --- | --- | --- |
| Epic | Project | configured team | static format; with `tracker`, the Epic's dated name |
| Epic Task | issue | `project` equals the parent Epic's Project; never `parentId` | static format, or the issue's exact `gitBranchName` for `tracker` |
| standalone Task | issue | no Project | static format, or the issue's exact `gitBranchName` for `tracker` |

- With a configured `kind` group, Tasks carry a `kind` issue label ([labels](../shared.md#labels)). Epics carry none: a Project is always an Epic.
- Project and issue labels carry native Phase through the configured single-select groups. When reconciling Phase, read [Phase](../../phase.md).
- Native Priority is fixed: `Urgent=1`, `High=2`, `Medium=3`, `Low=4`, `not set=0`.

## Setup discovery

**Workspace:**

- During `/project-setup`, `inspect_linear_workspace` reads the workspace identity, the viewer's name and username, teams, and issue and Project statuses through Linear's public GraphQL API. It's read-only and never replaces MCP tracker operations.
- Select exact team and status names; don't infer meaning from them. Setup creates a missing status only through the approved status setup. For status setup and branch naming, read [Linear tracker configuration](../../setup/linear-tracker.md).

**Labels:** setup creates and checks Phase groups and every configured label group through the label setup tools ([setup](../../setup.md#label-groups)).

## Provider calls

- Read configuration through the context tools, then call registered Linear tools directly: Project writes for Epics and issue writes for Tasks. Named team and state inputs resolve inside the provider.
- Don't rediscover the workspace, teams, or all statuses during ordinary work.
- A denied or unavailable call stops that operation without affecting the other role.

## Queue intake and explicit adoption

New queue work needs the entity type and destination (`backlog` or `todo`). Create it directly, without searching first.

**Adoption.** Read the selected object and require:

| Entity | Read | Requirements |
| --- | --- | --- |
| Epic | the Project | configured team, a configured Project queue state, native Priority |
| Epic Task | the issue | configured team, a configured issue queue status, Priority, the parent Epic's exact Project ID |
| standalone Task | the issue | configured team, a configured issue queue status, Priority, no Project |

1. **Read the discussion.** Call registered `list_comments` with the Project UUID as `projectId` for an Epic, or the issue UUID as `issueId` for a Task. Follow cursors until `hasNextPage: false`, including threads and inline comments. Reconcile material maintainer clarifications before adopting; contributor comments don't authorize scope changes.
2. **Adopt.** Call `inspect_adoption`, then `init_linear_epic` or `init_task`.
   - Under the explicit-selection approval in [artifacts](../../artifacts.md#initialization-inputs), reuse the title or name, description, Priority, and relationship without another confirmation.
   - Ask only for missing or conflicting information. An Epic Task's Kind comes from its plan section. For a standalone Task, a single `kind` label on the issue becomes the initialized Kind unless the user changes it; a missing or conflicting one needs a user choice.
3. **Identity.** Responses expose the immutable identity as `uuid`; an issue's `id` is its display identifier.
   - For an Epic, pass the Project UUID, URL, and creation time, which dates the Epic's name, as `object`.
   - For a Task, pass the issue's `get_issue` read to `init_task`. Its Project, or none, decides the parent, and its `gitBranchName` names the branch with tracker naming.

## Initialize

Follow [initialize from the tracker object](../shared.md#initialize-from-the-tracker-object). A Linear Epic's name depends on the Project's creation date, so initialization, not preflight, catches its rare name collision.

**Create new work** (after preflight): one registered `save_project` (Epic) or `save_issue` (Task). Don't search or discover first. Include:

- the intake title and Request;
- native In Progress and Priority, and the configured team;
- the Planning label from the configured Phase group, when there is one, and the labels preflight returned;
- the authenticated user (`me`) as the issue assignee, or as the Project lead for an Epic.

For an Epic, pass the Project UUID, URL, and `createdAt` to `init_linear_epic` as `object`; when the create response lacks `createdAt`, read the Project. For a Task, read the new issue with `get_issue` and pass the read to `init_task`.

**Board setup** (after initialization): for adopted objects and a started Task, move to In Progress, set `me` as the issue assignee or Project lead, and, when Phase is configured, apply Planning, reading and keeping the full applied label set and replacing only managed Phase labels. Call `prepare_label_mutation` with only the groups whose values you derived, passing the label IDs from that Phase result as its applied IDs ([labels](../shared.md#labels)), and save its returned set in one write, so Planning survives. Reuse state, Priority, and relationship proof from the create response; read only missing required facts.

Tracker records hold the object's identity, never branch copies or cached lifecycle or Priority.

When a create result is uncertain, read [recovery](../recovery.md#uncertain-tracker-creation).

## Add to Backlog or Todo

Queue intake follows [queue](../../queue.md). Require the entity type and the selected destination first. Epic Tasks are never queued.

| Entity | Call |
| --- | --- |
| Epic | `save_project` in the selected Project Backlog or Todo state, with team, Priority, and the returned `labels` |
| standalone Task | `save_issue` in the selected issue Backlog or Todo status, with team, Priority, the returned `labels`, and no Project |

## Lifecycle, Phase, and Priority

**Lifecycle and Priority:**

- On resume, read fresh lifecycle, Priority, and the complete applied label set by UUID.
- Use `prepare_tracker_mutation` for one lifecycle or Priority change, applied with `save_project` (Epic) or `save_issue` (Task).
- Planning, Execution, and Self-Review all stay In Progress. In Review starts only after external submission; confirmed delivery is Done, and canceled work uses the configured Canceled status.
- Labels change through `prepare_label_mutation`, with the item's label catalog and applied IDs as its current facts. Apply its full label-ID set with `save_project` or `save_issue` only when it changed.
- When Phase and labels change together, compute Phase first and pass its label set as the applied IDs. The label result keeps Phase, so save that one set.
- A user's request to assign an issue or set a Project lead is an ordinary `save_issue` or `save_project` update; resume never reads or repairs it.

**Phase:**

- Use Project-label metadata for Epics and issue-label metadata for Tasks, with the full fresh catalog and applied set ([Phase](../../phase.md#fresh-native-facts)).
- Run the returned call, which saves the full label-ID set. When labels change in the same step, save the combined label result instead.
- No lifecycle or label snapshot becomes durable cache state.

## Artifact projection

1. **Prepare.** After an `epic.md` or `plan.md` change, call `prepare_integration_projection`, then `prepare_tracker_publication` with the full source. The Project or issue keeps its original title or name and description. A Task issue's description is its design instead, and changes as [Task designs](../../epic-tasks.md#task-designs) describes.
2. **First publication** creates one document titled Plan, attached by Project UUID for an Epic or issue UUID for a Task.
   - Run only the returned `save_document`, immediately, and record clear success with `record_tracker_publication`.
   - A pending creation needs recovery, never a create of your own.
3. **Updates to a published document:**
   - Run the `get_document` and parent reads in the returned `lookups`, then call `prepare_tracker_publication_update` with the full source and those reads. The parent read must prove that its display identifier maps to the stored UUID.
   - Every update also posts a [plan-update note](../shared.md#plan-update-notes) with `save_comment`: a Project comment for an Epic, an issue comment for a Task.
   - Run the resulting full-content update by document ID only, without title or parent arguments, so publication never renames or reparents it.
   - A missing, moved, archived, or conflicting document stops for maintainer direction.
4. **Recovery.** When creation is pending or an update is uncertain, read [recovery](../recovery.md#linear-document). An issue has no listing filter, so recovery scans every accessible document. Trust clear success after updates, without routine confirmation reads.

**Rendering:** the [API documentation](https://linear.app/developers/graphql#adding-collapsible-sections-in-markdown) supports native folded sections in documents. Verify live rendering separately; local Markdown tests don't prove it. The native title supplies Plan; the content holds the overview, Highlights, and a folded Full plan. Local source artifacts stay unchanged and authoritative.
