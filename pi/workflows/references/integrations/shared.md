# Integration roles

A project has two optional integration roles:

- **tracker** (GitHub or Linear): external work items, Backlog and Todo queues, lifecycle, Priority, hierarchy, and plan publication. Its object is the entity's identity.
- **forge** (GitHub): pull requests.

The local entity artifacts stay authoritative for everything else.

## When a role is off

A role is off when its table is missing, or set to `provider = "none"` ([setup](../setup.md#integration-configuration)). `inspect_integrations` and every context report each role as `enabled` or `disabled` with the reason.

- **Steps that need an off role refuse** and name the role and why. Report it in one line and continue with the steps that don't need it. Never work around a refusal.
- **Tracker off:** skip the tracker item, board setup, lifecycle, Priority, Phase, labels, gates, and plan publication. Start standalone Tasks with `init_local_task` ([local Task](../../skills/task/SKILL.md#local-task)). Epics and Epic Tasks need the tracker: say so and stop.
- **Forge off:** stop after the local commits. Submission and shipping need the forge.
- **A local Task** has no tracker item, so it treats the tracker as off even when one is configured.
- Reviews, verification, commits, and the standalone `review` need neither role.

- A configured role isn't proof its provider tools are available. Follow each role's provider reference.
- Lifecycle and Priority live only on the tracker; nothing local replaces them.
- The [MCP boundary](../core.md#mcp-boundary) and [outcome rules](../core.md#outcomes-and-recovery) apply to every provider call. When changing Phase, read [Phase](../phase.md).

**Durable checkpoints** exist only for non-idempotent creation, duplicate-creation risk, or a user choice that can't be recovered elsewhere. Lifecycle, Priority, and board updates, and updates to an established publication, create none; recover them by reading provider state and retrying the absolute value. The first plan comment or document saves pending state before its create; a PR saves its intent before its create.

## Operations and Resource IDs

### Context tools

These tools read local configuration, authority, and entity state. They never call providers. Each returns only what its operation uses; only `inspect_integrations` returns the full configuration, label definitions included.

| Situation | Tool |
| --- | --- |
| Inspect configuration | `inspect_integrations`, with `target` |
| Queue intake (choose entity, Priority, and Kind first) | `inspect_queue_intake`, with `target` |
| Adopt explicitly selected work | `inspect_adoption` |
| Set up the board for a new entity, or resume one | `initialize_integration`, `resume_integration` |
| Publish a plan (tracker only) | `prepare_integration_projection` |
| Submit a PR (forge only) | `prepare_integration_pull_request` |
| Correct lifecycle, Priority, or Task hierarchy (tracker only) | `prepare_tracker_mutation` |
| Set an item's labels from a selection, or follow a Kind change (tracker only) | `prepare_label_mutation` |
| Set or clear native Phase | `prepare_github_phase_mutation`, `prepare_linear_phase_mutation`, with the fresh facts in [Phase](../phase.md#fresh-native-facts) |
| Ship a Task | `prepare_integration_shipping`, then `prepare_integration_merge` only when a merge is needed |

- `target: "current"` covers ordinary work. `target: "workflow"` is only for workflow-problem reports through Todo and for [session analysis](../session-analysis.md) searches; when reporting, read [queue](../queue.md#prepare-a-workflow-report). Use the returned tracker configuration.
- The returned local `projectRoot` is never report content. Missing or invalid source configuration stops with a corrective error, without starting setup or falling back.
- `prepare_integration_merge` takes the PR read as `evidence` and checks the recorded PR number, repository, head, base, and local branch SHA. It returns squash-merge arguments pinned to the head, complete merged proof, or a blocker.

**Evidence and validation:**

- Provider references own the call procedures. Use the artifact content, configured names, recorded IDs, and required live responses directly.
- Evidence is the actual decoded provider object or the complete native MCP result. The MCP layer extracts structured content or one JSON text block and rejects errors or ambiguous results.
- The harness validates its own inputs, configuration, and delivery evidence, not provider tool schemas.

### Resource IDs

`resource-ids.json` is strict and local. Its root is `{ entity, resourceIds }`, where `entity` is the entity's directory name, and each entry stores IDs and URLs under an exact scope: entity, server, repository, Project, team, object, or head and target. Lifecycle, Priority, labels, bodies, branch names, and merge state are never cached; they're live or authoritative facts.

| Object | Durable identity and progress | Local Resource IDs |
| --- | --- | --- |
| GitHub tracker issue | repository and issue number; plan-comment state | the configured Project-item ID |
| Linear Project | Project UUID; plan-document state | URL |
| Linear issue | issue UUID and identifier; a Task's parent Project UUID; plan-document state | identifier and URL |
| GitHub PR | immutable repository, head, and target intent | current PR number and URL |

- **`resolve_resource_ids`** reads the operation memo or scoped cache and reports missing IDs without calling providers.
- **`record_resource_ids`** validates a required read, or a clear-success mutation, before caching.
- **`invalidate_resource_ids`** removes only IDs the provider proved stale. A scope mismatch drops only that entry; access or network errors keep it.
- **Missing, stale, or malformed data:** investigate through the configured tracker or forge tools, then record the actual evidence. Reuse required live facts from that response; never read just to fill the cache.
- **Memo:** each context request starts a fresh addressing memo, and shutdown or reload discards it. Lifecycle and merge reads always use current facts.

### PR creation

`begin_resource_creation` saves only the immutable repository, head, and target intent in `metadata.json`, with the title from a fresh tracker issue read, and returns the PR create call. Run that call once with the rendered body, and pass clear-success evidence to `record_resource_creation`, which saves the number and URL in the local IDs file. When choosing an existing PR, `select_pull_request_resource` records the user's choice in the same file.

- Recording never changes `metadata.json`. Existing intent or an uncertain outcome needs investigation, never a blind replacement.
- Initialization and mutations seed IDs they already have. Queued work keeps IDs only during its own operation.

## Initialize from the tracker object

The tracker object comes first; the local entity is named from it. New work and adoption end in the same initialization call.

1. **Preflight new work.** Call `preflight_epic` or `preflight_task` with the approved values, the label selection included. It returns the labels to create the object with. New Task work is always standalone; an Epic Task never creates its object, it adopts the issue its Epic's [Task issue processing](../epic-tasks.md#task-issue-processing) created. It runs every local check that doesn't need the tracker object and changes nothing. A failure stops before anything is created on the tracker.
2. **Get the object.**
   - New work: create only the tracker object, with the approved title and Request, as the provider reference describes. No search first.
   - Adoption: use the explicitly selected object after its provider preflight.
3. **Initialize.** For an Epic, call `init_github_epic` or `init_linear_epic` with the object's identity as `object`. For a Task, call `init_task` with the issue's tracker reads, which decide its identity and parent. It names the directory and branch from the object, creates the local entity with a ready branch, and records the object's Resource IDs.
4. **Set up the board.** Call `initialize_integration`, then apply the provider reference's board steps: Project membership, In Progress, Priority, Planning Phase when configured, Task hierarchy, labels when configured, and the assignee. These are ordinary absolute updates; a failure is reconciled on resume, except assignment.

**Assignment:** whoever starts the work owns it. Initialization assigns the authenticated user to the tracker item, new or adopted, replacing existing assignees. Set it once and trust a clear success; never read it back or reconcile it later. Queue intake and Task issue processing assign nobody.

**When something fails:**

- **Uncertain create:** nothing local exists yet. Before any new create for the same intake, look up the exact approved title on the tracker ([recovery](./recovery.md#uncertain-tracker-creation)). One match continues at step 3; none allows a retry; several stop for the user.
- **Initialization fails after a clear create:** never create again. Report the created object and the failure, fix the cause (for example a name collision), and call initialization again with the same object.
- **Board setup incomplete on resume:** read fresh Project membership, Status, Priority, and Phase, and apply what's missing. Skip assignment. When native Priority is unset, ask the user for it; it isn't kept locally.

## Provider bodies

**Sources:**

- `prepare_integration_projection` needs only the tracker role. It publishes the current `epic.md` or `plan.md`.
- PR bodies use the synchronized `result.md` or `report.md`.
- The tracker item keeps its original request in its title or name and description. GitHub publishes the plan as one managed issue comment, the same for new and adopted issues. Linear publishes it as one document attached to the issue or Project.

**Ownership and discussion:**

- The artifact owns the content; the publication layer owns the attribution footer.
- Discussion, feedback, screenshots, logs, and supporting links go in separate comments. Requirements and agreed decisions go in the artifact.
- Before adopting queued work, and whenever reconciling requirements, read the object's full discussion through registered tools, with complete pagination. Resolve material questions with the maintainer before initialization or a plan change. Contributor comments don't authorize scope changes on their own.

### Attribution

- Code signs AI-assisted communication sent through the official GitHub and Linear MCP registrations: GitHub gets a final `<sub>` paragraph, Linear gets italics, with no added divider. This covers initialization, queues, publication, and comments.
- Never add signatures yourself, and keep them out of repository artifacts. The signature is the only change made to the content.
- Set `__pi_authorship: "user-verbatim"` automatically when you relay a complete user-written message unchanged; no opt-out wording is needed. Code keeps that text byte-for-byte and strips the field before sending.
- Use the default `ai-assisted` for anything you compose, reword, expand, or wrap around a quote. The mode applies per call. Metadata-only operations are unchanged.
- Edits need the complete text. Patch-only or template-only writes need explicit registered reads and a full-text write; signing never makes hidden provider calls.

### Publishing

Workflow publications need no approval of their text ([approvals](../core.md#approvals)).

1. **Start from the full source** in the [reviewed document shape](../artifacts.md#reviewed-document-shape). Never summarize or condense during publication.
2. **Local references:** published text never points readers at ignored files under `.piton/.local/` ([communication](../communication.md)).
3. **Tracker plans:** call `prepare_tracker_publication` with `entityDir` and the full source.
   - Run its returned mutation immediately, in the same step. The first call for an unpublished plan already saves pending state before it returns. Never call it again just to look at the output; a second call returns `recovery-required` and needs recovery reads.
   - Record a first creation with `record_tracker_publication`, passing the actual call as `evidence`.
   - `reads-required`: run the calls in its `lookups`, then call `prepare_tracker_publication_update` with the source, those reads as `reads`, and a [plan-update note](#plan-update-notes). Linear documents always need them; the parent read maps the display identifier to the bound UUID. Run a Linear update by document ID only, never with parent or title arguments.
   - A missing, moved, archived, or conflicting document stops for maintainer direction, never reattachment or replacement.
4. **PR bodies:** call `render_provider_body` with `destination: pullRequest` and publish its returned Markdown unchanged.
5. **Rendering:** `projection.ts` owns presentation and closing references.
   - GitHub plans render `# Plan`, the overview prose, `## Highlights`, and a folded Full plan.
   - Linear's native document title is Plan; the content is the overview prose, `## Highlights`, and a native `>>>` folded Full plan, with no repeated H1 and no marker.
   - Linear reads any line starting with `>>>` as a fold boundary, so a Linear plan or plan-update note with one fails locally.
   - Source headings and PR presentation are unchanged.
6. **Fix the source, not the output.** An incompatible source fails locally; fix the source and publish again. Never hand-edit rendered output.
7. **Publish** the complete body only to its destination: the managed GitHub comment, the attached Linear document, or the PR body.
   - Trust a clear provider success without an echo, comparison, or confirmation read. Linear's pre-update parent check is a separate scope check.
   - Provider Markdown normalization never justifies rewriting the source.
8. **Uncertain updates and pending creates:** read [recovery](./recovery.md#pending-plan-publication).

The same contract applies when an open PR's synchronized `result.md` or `report.md` changes.

### Plan-update notes

Editing a published plan notifies nobody, so every update to a published Epic or Task plan also posts one discussion comment on that entity's own tracker item: a GitHub issue comment, or a Linear Project or issue comment. The first publication has no note. Post it without asking.

1. **Write** a one-line summary of what changed and why; say when the change is only editorial. Pass it as `announcement.summary` to `prepare_tracker_publication_update`. Set `announcement.origin` only for an Epic update made from one of its Tasks.
2. **Rendering** is code-owned: `**Plan updated**`, `from Task <marker>` when an Epic update came from a Task, the summary, a link to the plan, and a folded diff of the published plan against the new one. An oversized diff is left out.
3. **Run** the returned plan update, then record its clear success with `record_tracker_publication`. That returns the note; run it and record it the same way.
4. **Until it's recorded**, every publication of that entity returns the pending note. When the outcome is uncertain, read [recovery](./recovery.md#pending-plan-update-note).

Identical source returns `unchanged`; nothing is published or announced.

**Publication state:**

- GitHub keeps plan-comment identity in `planComment`; Linear keeps plan-document identity in `planDocument`. They're not disposable Resource IDs.
- Each is `unpublished` before the first attempt, `pending` before creation, then `published` with the confirmed native ID and URL.
- GitHub comments keep a hidden token marker. Linear shows HTML comments, so its documents have none, and plan-update notes carry the token in the plan link.
- Markers and signatures stay out of source artifacts.
- New issues use the intake title and Request; later publication leaves those fields alone. Resume reuses saved state and never republishes on its own.

## Branch contract

Every initialized entity has a ready branch contract: exact `name`, `start`, `target`, and a generated or tracker source. With Linear's `tracker` format, a Task uses the issue's exact `gitBranchName` and an Epic uses its dated directory name. Never invent or rewrite a provider-generated name.

## Workflow-first updates

For initialized entities, update workflow kind and artifacts first.

- Lifecycle, Priority, and Phase are native tracker values.
- For the tracker object, read the required fresh values, then set only the intended absolute lifecycle, Priority, or changed labels. Verify required outcome facts that a clear-success response didn't include. Reconcile uncertain or partial results before retrying.
- With a configured `kind` label group, keep the item's `kind` label in step with the workflow kind, on both trackers.
- A provider failure never rolls back workflow artifacts.

Adding work to Backlog or Todo creates no workflow artifact or saved recovery state. Recover uncertain results by reading provider state.

## Labels

- Label groups live in `labels.groups`. Each value maps to one tracker label, and the group's cardinality limits how many an item carries. When adopting, use the `labelGroups` that `inspect_adoption` returns; otherwise read them with `inspect_integrations`.
- The `kind` value always comes from the item's Kind. `gate` values are the user's: keep them as the item has them, and set one at intake only when the user asks. Values for other groups come from Project Policies and the work; ask the user only when one can't be derived.
- For new work, report the chosen labels with the other intake values.
- New work gets its labels from preflight or queue intake. Existing work changes them only through `prepare_label_mutation`, which returns the complete label set; apply it only when `changed`.
- A selection names only the groups it sets, never `kind`. A group it leaves out has no value on new work and keeps its labels on existing work; `[]` clears a group.
- On a Kind change, call `prepare_label_mutation` without a selection, so only the `kind` label changes.
- An `invalid` result names the group and the problem. Report it and ask; never fix a value on your own.
- Mention the `unknown` labels it returns and suggest `/project-setup`. Runtime never creates, edits, removes, or keeps catalog labels.
