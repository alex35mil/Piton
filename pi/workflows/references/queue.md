# Queue

Backlog and Todo save work in the configured tracker without starting an Epic or Task:

- `backlog`: work we may do later but haven't committed to;
- `todo`: work we've committed to and can pick up next.

Both hold work that hasn't started. Adding work creates no `.piton` entity, artifact, branch, or saved recovery state.

## Choose the project

- `/backlog`, `/todo`, and natural-language queue requests use `target: "current"`.
- An explicit request to report a workflow defect or recurring friction ("Report this workflow issue" is enough) follows [report a workflow problem](#report-a-workflow-problem).
- Missing or invalid configuration ends the attempt with the tool's short corrective error. Don't start setup or repair configuration.

## Add work

1. **Read configuration.** Call `inspect_integrations` with the `target`. Stop and report a missing or invalid tracker.
2. **Read the guides.** Read [communication](./communication.md) and the provider guide: [GitHub tracker](./integrations/tracker/github.md) or [Linear tracker](./integrations/tracker/linear.md). When it refers to shared rules, read [integration roles](./integrations/shared.md).
3. **Draft the intake:** the title, the description, the tracker destination, Backlog or Todo, and a configured Priority or `not set`.
4. **Entity type.** Require `epic` or `task` (standalone) before checking mutation capability.
   - Never queue an Epic's Task. Change the Epic's Task list; acceptance creates its issue ([Epic Tasks](./epic-tasks.md#progress-and-replanning)).
   - Default to a Task, even for multi-step work; use an Epic only when asked.
5. **Kind and labels** (when `inspect_integrations` shows `labels.groups`):
   - Reuse the Kind the request states (Epic, or a Deliverable kind); otherwise recommend the best fit, or `not set`.
   - Pick a value for each other group from Project Policies and the request, and ask only when one can't be derived.
6. **Create.** For `target: "current"`, create the item from the user's words without an approval step, then report it in one line with its URL, Kind, labels, and Priority. A workflow report keeps its approval of the sanitized text and destination ([below](#prepare-a-workflow-report)).
   - Call `inspect_queue_intake` with the same `target`, the `entity`, `priority`, `kind`, and the label selection. Create the item with the returned `labels`; an `invalid` result names the group to fix.
   - Use only its selected registered tools, under normal permissions. Let native mutations validate their own inputs; don't preflight labels, options, workspaces, or statuses.
   - Never load the source project's credentials or run its MCP settings.

**Attribution:** publication code signs AI-assisted text, cross-project reports included ([attribution](./integrations/shared.md#attribution)).

**Creating:**

- Intake is a direct create. Don't check for duplicates first; look up existing work only for an adoption or an uncertain create.
- **GitHub:** create an ordinary issue. Add it to the configured Project, then set Status and Priority with named writes addressed by issue. When an item ID is needed, follow the [Project item ID rule](./integrations/tracker/github.md#project-item-ids).
- **Linear:** create only the selected Project or projectless issue, with team, state, Priority, and labels together.
- Reuse returned identities and outcome proof; read only missing required facts. No cache or recovery state is kept.

Apart from those labels, adding work assigns no native issue Type, native Phase, assignee, branch, artifact, or pending record. Starting the work assigns it.

## Report a workflow problem

Call `inspect_integrations` with `target: "workflow"`, then prepare the report below.

- **An error with a new-issue address:** give the user it with `?title=…&body=…`, URL-encoded. The user submits it; never create the issue.
- **Success** ([development install](./setup/reporting-source.md)): add Todo work using only the returned tracker and server. `projectRoot` isn't report content.

## Prepare a workflow report

Before asking for approval, prepare a minimal, standalone description.

**Include:**

- the affected workflow, skill, or tool;
- expected and actual behavior;
- ordered reproduction steps, or a concrete example of the repeated friction;
- impact;
- relevant sanitized error or instruction excerpts.

Distinguish observed friction, or an unconfirmed cause, from a verified defect.

**Version:**

- Include the installed workflow version or source revision, package-relative file names, and known local changes, so maintainers can reproduce it. Say plainly when that evidence is unavailable or mismatched.
- The consuming project's commit is not the workflow version, and a separate source checkout's revision doesn't prove an installed copy matches it.
- Don't hash or inventory source, update installations, or change checkouts to prepare a report.

**Exclude:**

- Replace project-specific values with synthetic examples, and prefer a small independent reproduction.
- Never include consuming-project names, remotes, absolute paths, tracker links, workflow IDs, proprietary code, credentials, customer data, raw logs, or transcript or artifact dumps.
- Don't collect session history or attach files automatically. This applies to public and private destinations alike.
- Evidence that can't be made safe stays local; explain the safe reproduction that's still needed.

**Approve and submit:**

- Show the complete sanitized candidate and the destination, explain omissions and missing evidence, and get explicit approval before creating it. Suggesting a report, or approving its preparation, is not approval to send it.
- Keep remote mutation tools outside skill-derived allow rules; configured permissions still apply.
- Submit it as ordinary Todo work through the provider procedure, and return its URL and any incomplete update. Maintainers handle it through the usual Todo and Task process.
- Leave both projects' lifecycle, branches, installed workflows, and files unchanged. Don't initialize maintenance work or create report stores or recovery records.
- Continue the consuming project's task afterward, including after a declined or blocked attempt.

## If the provider response is uncertain

Follow [core outcome rules](./core.md#outcomes-and-recovery). Keep a known object's identity and retry only incomplete updates. Don't add candidate confirmation, temporary identifiers, or another recovery mechanism. Always report the object's URL and any incomplete update.

## Start the work

When the user starts a Backlog or Todo object through `/epic` or `/task`:

- Reuse that object and its Priority, create the workflow entity and branch contract, and move straight to In Progress, with Planning when Phase is configured. Todo is never required between Backlog and In Progress.
- Both providers keep the request's title or name and description.
- The first plan publishes as one managed GitHub comment or one Linear document attached to the issue or Project, and later updates reuse it. This is the same for workflow-created and externally submitted items; no ownership question is needed. Queue intake itself publishes no plan.
- **Phase** ([Phase](./phase.md)), when configured: GitHub uses the Project Phase field, Linear the configured label group. Other system metadata stays private.
