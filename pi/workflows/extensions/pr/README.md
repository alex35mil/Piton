# Incoming PR command and checkout tool

Registers `/pr <GitHub PR URL or #NN> [context]` and routes the explicit selection to the `pr` skill as a follow-up message. The first word is the reference; any text after it (the Neovim picker pre-fills the title) is passed as maintainer context, never as part of the reference. The command itself makes no provider call, branch change or mutation grant. Empty input displays usage.

Also registers the separate native deferred checkout tools listed in [Workflow tools](../../references/tools.md#incoming-prs), each with `reference`, actual configured PR-get `evidence`, and only that operation's additional inputs. `operations.ts` owns the shared name map used by registration and generated guidance. The tools work in the **current checkout**, the repository the session runs in (normally the PR's worker).

## Modules

- `checkout.ts`: registration, record ownership across checkouts, and the operation state machine (prepare, review, fixes, configuration release and hold, restore, cleanup, inspection, and recovery of interrupted moves).
- `state.ts`: the validated record: original evidence, the checkout's location and start, the trusted revision, configuration mode, the state the tool left the checkout in (`workspace`: HEAD, index tree, disk commit), the in-flight move, and phase-specific rebase and fix facts.
- `workspace.ts`: Git state of a checkout. It builds the tool-owned *disk commit* (a target tree with held paths taken from the trusted revision), observes the checkout against it through a private index, and moves the checkout between disk states with `git switch --no-overwrite-ignore`, from a clean state, then sets the index and HEAD.
- `held.ts`: held pathspecs from `piton tools` (the CLI's `project/tools.rs` registry).
- `rebase.ts`: target observation, standard rebase in the private scratch worktree `rebase/`, approved-resolution continuation, moving the checkout to the result, and immediate pinned-lease publication with reconciliation.
- `fixes.ts`: approved exact-path additive commits on detached HEAD, and their separately approved normal publication.
- `cleanup.ts`: merge and Project Done proof for cleanup, which then restores the checkout.
- `git.ts`: safe Git execution: hooks, fsmonitor, filters, external diff and textconv, custom merge drivers, signing, editors, rerere, maintenance, and submodule recursion disabled; literal pathspecs for approved paths; SSH URLs for all GitHub transport.

No arbitrary command or path overrides, contributor-code execution, or unapproved commits, review, or merge mutations. See [checkout tooling](../../references/pr/checkout.md) for approvals and refusal handling. The agent never reconstructs these operations in Bash; there is no manual fallback.

`piton worker delete` reads `phase`, `location.root`, and `workspace` from `checkout.json` to treat an untouched review as clean (`cli/src/project/worker.rs`); keep those fields compatible.

The [Project procedure](../../references/pr/project.md) keeps maintainer-controlled Backlog/Todo -> In Review until confirmed Done/Canceled; fork-only [setup automation](../../references/pr/automation.md) adds membership without field resets. [Shipping](../../references/pr/shipping.md) uses the one shared repository merge operation and includes restoring the checkout in the exact shipping and merge set.

The [PR skill](../../skills/pr/SKILL.md) and [incoming PR procedure](../../references/pr/incoming.md) own private inspection, exact-revision approval, contributor access, and execution/merge boundaries. Configuration and identity belong to `integrations/forge/github/incoming-pr-context.ts`; independent review belongs to `review/pull-request.ts`. This is not Epic and Task initialization or submission.

## Private evidence layout

Everything lives under `.piton/.local/pull-requests/<number>/` and stays outside Git. A worker's `.piton/.local` links to its source's, so the source and its workers share it; the record names the checkout that owns the review.

- `checkout.json`: original evidence and revision, location and start, merge base, trusted revision, configuration mode, the tool's checkout state, separately observed target/source tips, native rebase progress, pinned lease, publication outcome, and current working baseline.
- `checkout-inspection.json`: observed state and phase validation.
- `checkout-contribution.diff`, `checkout-rebase.diff`, `checkout-conflicts.diff`, `checkout-fixes.diff`, `checkout-published-fixes.diff`, `checkout-untracked.json`: complete mutable inspection inputs for later immutable reviewer snapshots.
- `rebase/`: the scratch worktree, only while a rebase runs.
- Safety assessments and notes stay at the PR root.
- `reviews/<base-sha>-<head-sha>/review-NNN/`:
  - `inputs/`: that pass's write-once source, diff, and dependency files, including successive uncommitted drafts at the same head;
  - `revision.json`, provider `evidence.json`, dossier `evidence.md`, and `source-files.json` mapping original paths to immutable snapshots;
  - the shared request, report, review, response, transcript, agent, and outcome files.

Existing review records are never moved or rewritten automatically; current-round lookup uses the `reviews/` namespace only. Full repository identities stay in the revision metadata; the PR number and commit SHAs identify local evidence. A record from the earlier nested-checkout layout is refused and preserved, never migrated.
