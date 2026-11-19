# Head-branch deletion

Read this in the GitHub forge step of setup. Shipping deletes only the local Deliverable branch, and the workflow never deletes remote branches. GitHub's **Automatically delete head branches** repository setting (`delete_branch_on_merge`) removes a PR's head branch when it merges, so merged branches don't pile up.

## Read the setting

`inspect_github_repository_setup` returns `headBranchDeletion` for the forge repository.

- When the tracker and forge share a repository, reuse the tracker step's result.
- Otherwise call it with a known GitHub credential variable: the tracker's, or one from a recognized GitHub registration. Ask only for the variable's name; the user may decline.
- With no variable, or when the read fails, treat the value as `unknown`, name the reason, and continue. A failure here never stops setup.

## Propose

| Value | Setup proposal |
| --- | --- |
| `on` | an explicit no-op; nothing to ask |
| `off` | the optional manual step below |
| `unknown` | the reason, then the optional manual step, to check and turn it on if needed |

**Manual step:** a repository admin opens the repository's **Settings** → **General** → **Pull Requests**, and selects **Automatically delete head branches**.

- Ask the user to confirm once it's done, or to decline. Either answer continues setup.
- When the user confirms, re-read the value when the read is available. Otherwise report it as confirmed by the user.
- For a PR opened from a fork, GitHub applies the fork's own setting, not this repository's.

## Why setup doesn't change it

- No registered GitHub MCP tool can read or change the setting.
- The REST API change needs repository admin rights: Administration: write on a fine-grained token, or the `repo` scope plus admin on a classic token. That grant also allows deleting the repository, making it public, and editing branch protection.
- Never ask for those rights for this setting. Never change it through the API, `gh`, or another client.
