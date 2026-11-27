# Upgrading

## Gigs become standalone Tasks

`/gig`, the `gig` skill, and the Gig entity are gone. `/task` starts any Deliverable: describe new work, name a tracker issue, or give an existing Task's path. The tracker decides whether it's standalone or one of an Epic's Tasks: a GitHub sub-issue of an Epic-labelled issue, or a Linear issue in a Project, is that Epic's.

- **Tools:**
  - Removed: `preflight_gig`, `init_github_gig`, `init_linear_gig`, `init_local_gig`, `review_gig`, `init_github_task`, and `init_linear_task`.
  - Use `preflight_task`, `init_task`, `init_local_task`, and `review_task`. `init_task` takes the issue's tracker reads, which decide its identity and parent, instead of an `object` and a parent Epic directory.
  - New: `route_task` decides how a `/task` request starts, `check_task_parent` stops a resumed Task whose tracker parent changed, and `restore_task` rebuilds a Task started in another clone from the tracker and its pushed branch.
  - `inspect_adoption` and `inspect_queue_intake` take `task` instead of `gig`.
- **Artifacts:** standalone Tasks live under `.piton/.local/tasks/`. Task metadata records its `parent`. Linear Task records are now `resource: "issue"`, replacing `task-issue` and `gig-issue`.
- **Permissions:** rules that name a removed tool no longer match anything; rename them to the new one.

**Before upgrading,** finish every Gig and every Task in flight. Nothing migrates: old `.piton/.local/gigs/` directories stay on disk and the new code doesn't read them, and Task metadata written before this change, without `parent`, fails validation.

## Tool permissions move to `[tools.permissions]`

The tool-call rules move from `[permissions]` to `[tools.permissions]` in every `config.toml` and `config.local.toml`: `[permissions.host]` becomes `[tools.permissions.host]` and `[permissions.sandbox]` becomes `[tools.permissions.sandbox]`. Dotted keys move the same way (`permissions.allow = [...]` becomes `tools.permissions.allow = [...]`). Keys, values, and how rules combine don't change. A leftover `[permissions]` is an unknown key and makes the file invalid.

`piton config resolve` now reports the rules under `tools`, so update `piton` and the pi package together. `piton config get permissions` is now `piton config get tools`.

## Epic Task issue processing orders child issues

`plan_epic_task_issues` now takes `children` as `{ provider, issues }`. GitHub issues are listed in `get_sub_issues` order, each with its numeric `issueId`; Linear issues drop `onBoard` and `closed`. Once every Task is settled, GitHub Epics also get `reorder` steps (`sub_issue_write` `reprioritize`) that put the sub-issues in the plan's Task order. If your permission settings prompt for `sub_issue_write`, expect one prompt per reorder.

Linear Epics get a new `order_linear_epic_issues` tool, run after processing is `complete`. It sets each Project issue's Manual order (`sortOrder`) through Linear's GraphQL API, using the credential environment variable the registered Linear MCP server's `Authorization` header names. That variable must hold a Linear API key or OAuth token valid for the GraphQL API.

No `.piton/.local` migration is needed.

## Kind label coverage follows the tracker

The `kind` label group's `applies_to` is now fixed by the tracker, and `/project-setup` no longer asks about it. A `kind` group with any other `applies_to` fails validation, which stops every workflow command.

- **GitHub:** `applies_to = ["epic", "deliverable"]`, with the `epic` value and the six Deliverable kinds. Epics and Tasks are both issues, so `Kind: Epic` tells them apart.
- **Linear:** `applies_to = ["deliverable"]`, with only the six Deliverable kinds. An Epic is a Project, so a Kind label on it tells nothing apart. Remove `"epic"` from `applies_to` and delete `[tracker.labels.groups.kind.values.epic]`, then run `/project-setup` and choose to remove the old `Kind: Epic` Project label.

Projects without a `kind` group are unaffected. No `.piton/.local` artifact migration is needed.

## Configuration moves to one `config.toml` per folder

The package no longer reads configuration itself: it asks `piton config resolve` (the daemon sets `PITON_BIN`; outside Piton, `piton` must be on `PATH`). Earlier files (`.project/`, `.agents/*.json`, `.nono.json`, `.piton.toml`, `.piton/`'s separate JSON files, and Piton's files in pi's agent folder) are no longer read; move their settings into these tables by hand.

- **Each folder** (a repository's `.piton/`, a parent folder's, and the global folder `$XDG_CONFIG_HOME/piton/`, default `~/.config/piton/`) has one `config.toml` and a `config.local.toml` that isn't committed. `mcp.json` and `policies.md` stay separate.
- **The global folder also has** `config.host.toml` (with `config.host.local.toml`) for how the daemon runs, this host's preferences, and `[debugging]`, and `config.client.toml` for the apps' preferences.
- **Keys are snake_case:** `default_mode`, `mcp_server`, `owner_type`, `in_progress`, `self_review`, `api_key`. Label keys you chose, like `pi-workflows`, stay as written.
- **Where keys may go:** `web_search.api_key` only in a `config.local.toml`; `branches.username` only in a project's `config.local.toml` (either file in the global folder); `[tracker]` and `[forge]` only in a project or parent folder.
- **Removed:** `[permissions.keybindings]`. Use `/permissions-toggle-auto-accept`.
- **Skill grants are off by default:** a skill's `allowed-tools` grants nothing unless `skill_grants = true` in `[permissions]` (or `[permissions.host]` / `[permissions.sandbox]`). Without it, allow the tools you want by name. Skills are no longer protected files.
- **Removed:** `workflowProject` and `sessionAnalysis`. Workflow reports open a pre-filled GitHub issue for you to submit; a development install (the package running from a git checkout of Piton) files them in that checkout's tracker and runs session analysis.
- **Parent folders** strictly below home can hold their own `.piton/` and `.agents/`, shared by every repository below them.
- **Ignore rules:** `.gitignore` needs `/.piton/.local`, and `.git/info/exclude` gets one `/.piton/*.local.*` line. Project setup writes them.

## Simpler workflows: fewer gates, bounded reviews

Workflows now stop for you only for clarifications, decisions, and once at the end. Task and Gig plans are reviewed once, published, and summarized instead of waiting for approval; small changes can skip the plan; reviews block only on real defects, with one verification pass; the result stops once before "submit" or "ship it"; intake and plan-update notes no longer ask; and Phase is visibility only. Epic plans keep their acceptance, now recorded by the new `accept_epic_tasks` tool. See the [workflows guide](workflows/README.md#working-lifecycle).

**Before upgrading,** finish every Epic, Task, Gig, and incoming-PR review that's in flight. Nothing migrates: drafts (`draft.md`, `result-draft.md`, `report-draft.md`), saved plan-update summaries, review rounds without `pass.json`, and Epics accepted without a saved Task list aren't read by the new code.

**Then, in each project:**

- Remove `reviews` from `.project/config.json`. The file is strict, so a leftover `reviews.userConfirmationAfter` fails validation and stops every workflow command.
- Remove the artifact `ask` rules for `epic.md`, `plan.md`, `result.md`, and `report.md` from your permission settings (for example `write(.project/*/plan.md)` and `edit(.project/*/plan.md)` in `.agents/permissions.json`). Those files are now written without a gate, so the rules would only add prompts.

**Tool changes:**

- Removed: `prepare_epic_plan_presentation`, `prepare_task_plan_presentation`, `prepare_gig_plan_presentation`, `prepare_task_completion_presentation`, and `prepare_gig_completion_presentation`.
- `record_review_response` takes only the findings you reject or that need the user's decision.
- Review severities are `blocking` and `note`, for entity and incoming-PR reviews alike.
- `prepare_tracker_publication_update` always needs the plan-update summary as `announcement`.
- `begin_resource_creation` refuses a new PR until the newest final review cycle finished.
- `plan_epic_task_issues` and Epic plan publication refuse while the Task list differs from the accepted one, and can now cancel a started Task.

Update custom scripts and permission selectors that name removed tools.

## Pi 1.5.2

The package now requires Pi 1.5.2. Upgrade Pi before reloading the package; older Pi isn't supported.

- Validation runs on `@earendil-works/pi-schema`, the validator Pi uses for tool arguments, instead of `typebox/value`. The package installs it as its own dependency, pinned to Pi's version. Tool schemas are still built with TypeBox, which Pi supplies.
- Validation stays strict: unknown properties are rejected and values aren't converted, as before.
- Error details for invalid configuration and workflow files use pi-schema's wording, for example `invalid .project/config.json: /extra: unknown field` instead of `must not have additional properties`. Update anything that matches the old wording.

No `.project` artifact or settings migration is needed.

## GitHub MCP registrations get every workflow toolset

`configure_mcp` now gives every GitHub registration `context,issues,projects,pull_requests`, whichever roles it serves. A registration used only for the forge previously got just `pull_requests`, so workflow reports to a GitHub-tracked workflow source couldn't create issues or set Project fields through it.

- In each GitHub registration in `.agents/mcp.json`, `.agents/mcp.local.json`, or the global `mcp.json`, set `X-MCP-Toolsets` to `context,issues,projects,pull_requests`, then reload Pi.
- `configure_mcp` no longer accepts `roles` for GitHub registrations; drop it from scripts that call the tool.

No `.project` artifact migration is needed.

## Label groups

Labels in `.project/integrations.json` are now label groups under `tracker.labels.groups`. Each label stores its name, color, and description. Kind is the group with the reserved key `kind`. A configuration in the old shape fails validation, which stops every workflow command until it's migrated.

**GitHub Kind labels.** Replace `tracker.labels.kind`:

```json
"labels": {
    "kind": {
        "epic": "Kind: Epic",
        "deliverableKinds": { "feature": "Kind: Feature", "bugfix": "Kind: Bugfix" }
    }
}
```

with the `kind` group:

```json
"labels": {
    "groups": {
        "kind": {
            "appliesTo": ["epic", "deliverable"],
            "cardinality": "exactlyOne",
            "values": {
                "epic": { "name": "Kind: Epic", "color": "#8250DF", "description": "A multi-deliverable initiative." },
                "feature": { "name": "Kind: Feature", "color": "#0969DA", "description": "New user-visible or system capability." },
                "bugfix": { "name": "Kind: Bugfix", "color": "#CF222E", "description": "Correction of defective behavior." }
            }
        }
    }
}
```

- Keep each label name exactly as it was. `epic` becomes a value key next to the six Deliverable kinds (`feature`, `bugfix`, `research`, `refactor`, `audit`, `chore`), and all seven are required when `appliesTo` lists both item types. The examples above are shortened.
- Copy each label's current color and description from the repository's label settings, so the next setup run proposes no changes.
- Without Kind labels, leave `labels` out.

**Linear Phase groups.** In `tracker.phase.issues` and `tracker.phase.projects`, `group` and each Phase value change from a name to a label definition. `"group": "Phase"` becomes `"group": { "name": "Phase", "color": "#…", "description": "…" }`, and the same applies to `planning`, `execution`, and `selfReview`. Use the existing names and the current colors and descriptions from Linear's label settings.

After editing, run `/project-setup` once. It checks the configured labels against the tracker and lists every label outside the configuration, and you decide whether to keep or remove each one. Your answers are saved, so later runs ask only about labels added since.

No `.project` artifact migration is needed.

## Assigning work to the user who starts it

Starting an Epic, Task, or Gig now assigns its tracker item to the authenticated user. On GitHub this uses `get_me` from the `context` toolset. Add `context` to `X-MCP-Toolsets` in each GitHub tracker registration (for example `context,issues,projects,pull_requests`) by rerunning `/project-setup` or editing the MCP registration, then reload Pi. Until then, work starts without an assignee. Linear needs no change.

No `.project` artifact migration is needed.

## Shipping terminology and evidence inputs

Deliverable merge, Done, and cleanup is now called **shipping**, matching incoming-PR shipping. Names for an activity use nouns ("shipping", "cancellation"). There are no aliases; update custom permission selectors and scripts that use the old names.

| Old name | New name |
| --- | --- |
| `prepare_integration_finish` | `prepare_integration_shipping` |
| result field `roles.forge.finish` | `roles.forge.shipping` |
| `plan_incoming_pr_ship` | `plan_incoming_pr_shipping` |
| `plan_incoming_pr_cancel` | `plan_incoming_pr_cancellation` |
| incoming Project `action` values `"ship"` and `"cancel"` | `"shipping"` and `"cancellation"` |

- `cleanup_github_delivery_branch` takes the merged PR's exact `pull_request_read` get as `evidence` instead of pre-extracted `merge` facts.
- A Deliverable may have several commits within its accepted scope. Forge PR intent is saved before staging the last commit and committed with the work.

No `.project` artifact migration is needed.

## Workflow tool discovery and names

Workflow operations now use concrete, operation-specific tools discovered through the existing permission-aware Codemode. Keep Codemode enabled for workflows; explicit tool allowlists and exclusions remain authoritative. See the [current operation catalog](workflows/references/tools.md). Ordinary coding tools remain direct.

Update custom scripts, instructions, and explicit workflow-tool permission selectors to the current names. Replaced dispatch interfaces have no aliases. Incoming-PR checkout actions each have their own selector and remain outside skill-derived allows; review the [complete ask example](workflows/references/pr/incoming.md#existing-permission-prompts) before approving settings changes. Package upgrades do not rewrite private permission settings.

This interface change requires no `.project` artifact migration. Domain schemas, persisted state, approval gates, and provider-side records retain their existing contracts. Reviewer model overrides remain explicit; otherwise the configured startup default is used, not the parent conversation model. Claude Code reviewers require the installed/enabled provider, loaded alone alongside rules and permissions; no provider installation or credential/default-model change occurs.

## Pi 1.3.0

pi-af now uses native MCP and tool execution APIs. Configuration names change without compatibility readers:

| Family           | Previous filenames                                           | Current filenames                            |
| ---------------- | ------------------------------------------------------------ | -------------------------------------------- |
| MCP              | `mcp.settings.json`, `mcp.settings.local.json`               | `mcp.json`, `mcp.local.json`                 |
| Permissions      | `permission.settings.json`, `permission.settings.local.json` | `permissions.json`, `permissions.local.json` |
| Web search       | `web-search.settings.json`, `web-search.settings.local.json` | `web-search.json`, `web-search.local.json`   |
| Global workflows | `workflows.settings.json`                                    | `workflows.json`                             |

Rename files in place, preserving their values and global/project/local scopes. Project settings stay under `.agents`; every MCP tier uses `mcpServers` instead of `servers`. Pi's own `settings.json` and `.project/config.json` / `config.local.json` are unchanged.

Verify that private local files remain outside Git before renaming them. A global `**/.agents/*.local.json` ignore rule covers both the old and new local filenames; existing repository-local exclusions can instead be updated to the new names. Private project-local MCP registrations stay project-local.

The permissions extension is now `extensions/permissions/`. Update explicit extension paths and custom command references from `/permission-*` to `/permissions-*`, including `/permissions-toggle-auto-accept`, `/permissions-mode`, and `/permissions-settings`.

Coordinate live private/global settings renames with activation of Pi 1.3.0. Do not reload the changed extensions into an older running harness. Repeat the configuration migration explicitly in each consuming checkout; upgrading this package does not rename other projects' files automatically.
