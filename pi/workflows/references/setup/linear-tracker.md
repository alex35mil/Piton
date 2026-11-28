# Linear tracker configuration

Read this when configuring a Linear tracker: the tracker object `configure_project` saves as `[tracker]`, status setup, and branch names.

```json
{
    "tracker": {
        "provider": "linear",
        "mcpServer": "linear",
        "team": "Engineering",
        "phase": {
            "issues": {
                "scope": "workspace",
                "group": { "name": "Phase", "color": "#8250DF", "description": "" },
                "values": {
                    "planning": { "name": "Planning", "color": "#8250DF", "description": "" },
                    "execution": { "name": "Execution", "color": "#8250DF", "description": "" },
                    "selfReview": { "name": "Self-Review", "color": "#8250DF", "description": "" }
                }
            },
            "projects": {
                "scope": "workspace",
                "group": { "name": "Phase", "color": "#8250DF", "description": "" },
                "values": {
                    "planning": { "name": "Planning", "color": "#8250DF", "description": "" },
                    "execution": { "name": "Execution", "color": "#8250DF", "description": "" },
                    "selfReview": { "name": "Self-Review", "color": "#8250DF", "description": "" }
                }
            }
        },
        "statuses": {
            "issues": {
                "backlog": "Backlog",
                "todo": "Todo",
                "inProgress": "In Progress",
                "inReview": "In Review",
                "done": "Done",
                "canceled": "Canceled"
            },
            "projects": {
                "backlog": "Backlog",
                "todo": "Todo",
                "inProgress": "In Progress",
                "inReview": "In Review",
                "done": "Completed",
                "canceled": "Canceled"
            }
        }
    }
}
```

**Statuses and team:**

- The issue and Project Status mappings each need all six keys, with six distinct, non-empty provider option names.
- `team` is non-empty.
- Linear Priority is native and isn't configured here.

**Phase groups:**

- Phase is optional ([Phase](../phase.md#when-phase-isnt-configured)). When configured, both mappings are required. Each has `scope: workspace | team`, the group's label definition, and distinct child definitions for `planning`, `execution`, and `selfReview`. Team scope means the configured team; workspace scope means no team.
- Phase groups are single-select, for issues and Projects. The label setup tools create and check them with the other label groups ([label groups](../setup.md#label-groups)). Runtime reconciliation is in [Phase](../phase.md).

**Workspace inspection:**

- During guided setup, `inspect_linear_workspace` reads the workspace, the authenticated viewer, accessible teams, team issue statuses, and Project statuses, through Linear's public GraphQL API, without mutating anything. It also returns `statusDefaults`, the definition for each lifecycle key, and, for each team, a `statusProposal` mapping issue and Project statuses.
- `viewer.name` is the Full Name; `viewer.displayName` is the workspace-unique Username and is returned as `viewer.username`.
- Setup saves only the team, status names, Phase groups, and label groups the user selects. Provider IDs, viewer identity, and credentials stay out of configuration.
- A recognized official MCP registration supplies its credential environment-variable name, never its value.

## Status setup

- Setup proposes the team's `statusProposal`, and the user confirms it or changes single entries. Nothing is saved or created before that confirmation.
- For each key, the proposal takes the existing status with the default's name and native type, ignoring case; else the only unclaimed status of that native type; else the default to create.
- A conflict means the default's name is taken by a status that doesn't fit; the user picks another existing status or name.
- `preview_linear_status_setup` plans the creates: issue statuses in the team, Project statuses at workspace level, each placed after the status of the previous lifecycle key. It blocks on an ambiguous or unknown existing name, a new name that already exists, or repeated names. Nothing is written.
- `apply_linear_status_setup` takes the approved `planHash`, creates Project statuses first, verifies every mapping, and returns `statuses`. Save that as `tracker.statuses`.
- Existing statuses are never renamed, reordered, archived, or deleted. A created status can't be deleted through the API, only archived.
- Creating statuses needs an API key with write access (full access or the Admin permission) that isn't limited to other teams. Project statuses also need a workspace admin or a role the workspace allows; the preview warns when the viewer isn't an admin.

## Branch names with `tracker`

- A Task uses the exact provider-generated issue branch (`gitBranchName`).
- An Epic, a Linear Project with no branch of its own, uses its dated name, `<YYYYMMDD>-<slug>`.
- Linear `tracker` never uses local or global workflow username settings.
