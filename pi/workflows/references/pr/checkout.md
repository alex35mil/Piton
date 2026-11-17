# Incoming PR checkout tooling

Selection, safety assessment, contributor access, and approvals are in [incoming PR handling](./incoming.md). The rebase and fix workflow is in [rebase and fixes](./fixes.md), and merge, Done, cleanup, and cancellation are in [shipping](./shipping.md).

The named checkout tools, registered by the PR extension, work in the **current checkout**: the repository the session runs in, normally the PR's worker. They own:

- preparation and fetch;
- review setup and validated transitions;
- held configuration, its release, and its hold;
- target observation;
- standard rebase and its immediate SHA-pinned, lease-protected PR update;
- additive fix commits and their publication;
- restoring the checkout to where it started.

**No manual route:**

- Never reconstruct that Git work in Bash. No manual fallback is allowed after refusal, failure, or unavailability; preserve work and resolve the blocker.
- When a registered tool is unavailable, reload the trusted extensions.

## Inputs and approval scope

**Inputs:**

- Every operation takes `reference` and the actual configured `pull_request_read` `get` `evidence` (tool, arguments, and unchanged response).
- The tool derives repository URLs, full branch refs, and private paths from that identity. There are no arbitrary URL, path, ref, or command overrides, and no approval booleans.
- Get fresh provider evidence before mutations and on resume. Validation checks consistency, not independent provenance or freshness.

**Transport:** GitHub Git transport is SSH (`git@github.com:<owner>/<repo>.git`) for fetch, exact remote-tip observation, and push, with existing trusted SSH authentication. Never configure HTTPS credentials or change global Git configuration. HTTPS PR URLs and provider evidence are identity and API inputs, not transport overrides.

**The normal update flow:** standard rebase → approved conflict resolution → immediate `--force-with-lease` PR update → approved fixes and maintainer testing.

- Before `rebase_incoming_pr`, establish contributor consent and authenticated access, and get approval of the exact source head, target SHA, destination, and lease.
- That one approved set covers the rebase's replacement commits and its immediate push. `continue_incoming_pr_rebase` also pushes immediately once the last approved conflict is resolved.
- It doesn't authorize later fix commits or pushes, tests, GitHub review publication, or merge.

**Permissions:** keep every checkout operation outside skill-derived allows. Native-tool permissions and conversational gates both apply; change no permission settings automatically. Local state and observations are facts, not approval tokens.

| Operation | Required state and scope | Result |
| --- | --- | --- |
| `prepare_incoming_pr_checkout` | Explicit fetch approval, and a checkout with no staged, unstaged, or untracked work and no Git operation in progress. | Records the starting branch or revision, fetches and verifies the provider base and head commits, and computes their merge base. Returns `prepared`; nothing on disk changes. |
| `checkout_incoming_pr` | An evidence-backed **Checkout safety: N/5 (label)**, exact checkout approval at every score, fresh matching provider evidence, and an unchanged start. | Detaches HEAD at the merge base with the PR head in the index, so the contribution is a staged diff. Files on disk are the PR head, except held configuration. Refuses to overwrite ignored files. Returns `contributorReview`. |
| `begin_incoming_pr_fixes` | Finished contributor review, approved corrections, and no unsaved buffers (Git can't see unsaved editor buffers). | Validates that the checkout is exactly as the review left it, then moves HEAD to the PR head. Returns `fixes`: begin approved static edits, or request the separate rebase-and-update set first. |
| `release_incoming_pr_configuration` | Explicit approval to activate the PR's configuration in this checkout, during review or fixes, with no edits to held files. | Puts the PR's versions of held files on disk. Fix-draft edits elsewhere stay. |
| `hold_incoming_pr_configuration` | Released configuration, with no edits to held files. | Puts the trusted versions back on disk. |
| `observe_incoming_pr_target` | Approved Git reads of the verified target and contributor branch. | Observes both current remote tips, separately from provider evidence. Never changes HEAD, the index, or files, or replaces an active pinned rebase or lease. |
| `rebase_incoming_pr` | A `fixes` checkout exactly as the tool left it, the exact approved observed `targetSha` and `expectedHeadSha`, consent and access, and immediate-update approval. | Rechecks both tips, fetches the pinned target, and runs a standard linear rebase in a private scratch worktree. Returns `rebasing` at conflicts. On success, moves the checkout to the result, with held configuration unchanged, and immediately attempts the pinned-lease PR update. |
| `continue_incoming_pr_rebase` | A known native rebase in the scratch worktree, unchanged progress and index, and exact approved `resolutionPaths` relative to it. | Stages only those approved resolutions and deletions and continues without an editor. Returns the next conflict, or completes, moves the checkout, and publishes. Edits outside those paths, untracked additions, and unexplained staging are preserved and refused. |
| `publish_incoming_pr_rebase` | A completed but unpublished `rebased` state; explicit approval to recover only the recorded result and lease. | Reads the remote first. Already at the result: confirmed, no push. Still at the original lease: the approved retry. Any other or missing head: rejected. Never refreshes the lease or rebases again. |
| `commit_incoming_pr_fixes` | An accepted final draft, the exact approved `paths` and `message`, fresh open-PR evidence, and consent and access. | Stages only the complete literal approved fix set and creates one additive commit on detached HEAD, without pushing. Records `committingFixes` intent and the `committedFixes` result; exact-intent recovery never duplicates a completed commit. A fix to held configuration needs its release first. |
| `publish_incoming_pr_fixes` | Explicit approval of the recorded additive result and exact contributor destination. | Observes first, then pushes that result normally, without force, only from the recorded source head. Confirms an already-present result without another push; unexpected or missing heads stop. Advances the confirmed baseline. |
| `restore_incoming_pr_checkout` | A prepared, review, or fixes checkout exactly as the tool left it, with no unpublished commit or active rebase. | Returns the checkout to its starting branch and revision, clean, and records `restored`. Edits, staging, and untracked files are refused and preserved. |
| `cleanup_incoming_pr_checkout` | The exact shipping and merge scope, a matching merged PR, head, and timestamp, applicable Project Done proof, and no unpublished work. | Restores the checkout as `restore_incoming_pr_checkout` does. Never deletes a checkout, a worker, or the main repository. Keeps contributor branches and private evidence. No review is a no-op. |
| `inspect_incoming_pr_checkout` | A review in this checkout, and actual provider evidence. | Returns recorded and observed revisions, start, trusted revision, held configuration the PR changes, baseline, target, lease, phase, and Git, native-progress, and conflict state, even for inspectable mismatches. Finishes an interrupted recorded move; never changes anything else. |

**Repeated calls and mismatches:**

- Repeated valid prepare, review, or fixes calls don't reset drafts or re-fetch over work.
- Mismatches, unexpected changes, and incomplete operations stop. They never reset, stash, abort, skip, force a checkout, adopt work automatically, or delete branches. Inspect and agree on recovery instead of bypassing the tool.
- Each checkout move is recorded before it starts. An interrupted move is finished or retried by the next call, never guessed.

**Ownership:** a worker shares `.piton/.local` with its source, so one PR's record is shared too. The record belongs to the checkout it names.

- Another live checkout's review refuses every operation here, naming that checkout.
- A review whose checkout no longer exists (a deleted worker) is taken over by preparing again; its evidence stays.
- A restored review lets any checkout prepare afresh.

**During contributor review**, detached HEAD is the merge base while the index holds the PR head. **Don't edit, stage, unstage, commit, or push in this phase.** Move to the clean contributor HEAD with `begin_incoming_pr_fixes` before a rebase or static edits.

## Held configuration

**What's held:** every repository file the `piton tools` registry lists. That covers Git attributes, environment managers (mise, direnv, Nix, devbox), agent instructions and configuration (`AGENTS.md`, `CLAUDE.md`, `.agents/`, `.pi/`, `.piton/`), and editor project configuration (Neovim, VS Code, Zed, JetBrains), at any depth.

**How it's held:**

- On disk, held files keep the checkout's starting version: added ones are absent, changed ones are the start's, and removed ones stay. Shells, editors, and sessions in the checkout keep the configuration they already had.
- The PR's versions stay in the index and in `checkout-contribution.diff`, so they're reviewed like any other change. `heldConfiguration.changedByPr` lists them.
- Because the files on disk differ from the index there, an editor's Git view also shows an unstaged change that undoes the PR's configuration. Never stage it by hand; the tools stage only approved paths.

**Releasing:** `release_incoming_pr_configuration` makes the PR's configuration active in this checkout, for example to use a dependency a PR adds. It needs explicit approval of that activation. A shell, editor, or new session may then run that configuration. `hold_incoming_pr_configuration` puts the trusted versions back.

## Rebase and publication semantics

**How it rebases:**

- Use standard rebase, never merge-into-PR or preserve-merges. PR commits are replayed onto the approved target as a straight line, and merge commits aren't recreated.
- The rebase runs in a private scratch worktree at `.piton/.local/pull-requests/<number>/rebase/`. Nothing roots a shell, editor, or session there, and the checkout under review is never mid-rebase.
- Ordinary Git handling of already-upstream and empty commits applies, and returned messages keep Git's skipped-commit information.
- No interactive todo editing, exec commands, autosquash, autostash, or updates of unrelated refs.

**Target and lease:**

- Provider-reported base and head evidence stays unchanged. `observe_incoming_pr_target` reads the actual `refs/heads/<base-ref>` tip and the contributor branch separately.
- A changed tip before starting needs reassessment and renewed exact approval.
- During conflict resolution, the active target and lease stay pinned; later observations never silently replace them.

**Conflicts:**

- Conflicts are expected, inspectable states. No push while conflicts remain.
- In Git's rebase terms, **ours** is the target plus commits already replayed, and **theirs** is the contributor commit being replayed.
- The tool returns `conflictPaths` (absolute paths in the scratch worktree), unmerged stages, native progress, and diffs. The agent proposes and edits approved resolution contents there; the tool stages and continues.
- Mechanical validation doesn't prove a resolution is semantically correct or tested.

**The immediate push:**

- On a successful rebase, the tool verifies native completion, target ancestry, and a clean result, moves the checkout to it, then immediately pushes the recorded result SHA to the existing contributor repository and ref.
- The push uses `--force-with-lease=refs/heads/<head-ref>:<observed-pre-rebase-head-sha>` and an explicit result-SHA-to-ref refspec. Never plain `--force`, an implicit tracking-ref lease, or the base repository as a fork destination.
- A contributor update rejects the lease and preserves remote work and the local rebased result.

**Publication states:** publication is recorded as pending, attempted or uncertain, rejected with the observed head, or confirmed.

- The attempted result and lease are saved before sending.
- A lost response can follow a successful update: read the exact remote head before retrying. An already-published result is confirmed without another push. Changed or deleted branches are never overwritten or recreated.
- Cancellation and incomplete operations keep their actual state. They never grant a fresh lease or trigger an automatic rollback.
- Report confirmed remote completion even when a later local check fails.

**Recovery from authentication failures:**

- After an HTTPS authentication failure with a saved completed rebase, use approved `publish_incoming_pr_rebase` over SSH with the same recorded result and original lease. No refetch, rebase rerun, lease refresh, or state migration is needed.
- An SSH authentication failure keeps that result and lease. Resolve access separately, with no HTTPS or manual Git fallback.

**After publication:**

- Get fresh PR evidence for the new head; never edit an old response to manufacture it.
- Later fixes use the confirmed published head as their baseline. Before any separately approved fix commit, verify the actual HEAD equals the freshly verified published contributor head.
- Keep later fixes **uncommitted throughout maintainer review, discussion, and revision**. Ordinary fix commits and pushes, review publication, testing, and GitHub merge keep their own approvals.

## Additive fixes, restore, and cleanup

**Fix commits:**

- Use `commit_incoming_pr_fixes` and separately approved `publish_incoming_pr_fixes`, never a shell commit or push that bypasses managed state.
- An approved fix commit must have the recorded parent, tree, and message and a clean exact result. Unexpected staged, changed, or untracked paths are preserved.
- Fix commits stay on detached HEAD; they never move a local branch.
- `committingFixes` can recognize an already-applied exact commit after an interruption. `committedFixes` stays unpublished until remote confirmation.
- A rejected or uncertain normal push keeps the commit and never forces, rebases, or silently refreshes its source.

**Baselines:**

- The saved rebase result is historical integration evidence, not permanently the published baseline.
- Confirmed additive publication advances `baselineSha`, keeping original evidence, historical rebase facts, and valid inspection and resume.
- The original-to-rebase comparison stops at that recorded integration, separate from published and pending fixes.

**Restore:**

- `restore_incoming_pr_checkout` ends the review in this checkout and returns it to the recorded start: the same branch at the same revision, or the same detached revision. When that branch moved or was deleted meanwhile, the checkout stays detached at the starting revision and says so.
- It refuses while there are edits, untracked files, an unpublished fix commit, or an active rebase; resolve those through the tools first.
- Confirm unsaved buffers are safe before restoring.
- Deleting a worker whose review is untouched needs no restore: `piton worker delete` treats it as clean, and the next preparation of the PR takes the record over.

**Cleanup:**

- Cleanup is part of the exact shipping and merge set, after confirmed merge and applicable native Project Done.
- Supply `completion` with scoped field and item evidence for a configured Project, or explicit `forgeOnly` only when no GitHub Project is configured.
- It restores the checkout. Notes, state, evidence, diffs, and immutable reviews are kept. A later failure never reverses a confirmed merge or Done.

## Returned state and private evidence

**Output includes:**

- paths: the checkout root and private evidence, relative and absolute;
- revisions:
  - the unchanged original `revision` and the actual input `providerRevision`;
  - `location.start`, `reviewBaseSha`, `trustedSha`, and the current published `baselineSha`;
  - the separate remote `observation`;
- held configuration: its mode and the held files the PR changes;
- rebase:
  - active or completed `rebase_incoming_pr` facts and `conflictPaths`;
  - publication status and Git messages;
- phase: the phase and `phaseValid`;
- `fixCommit` intent and result, when present;
- working state: HEAD, the index tree, and files changed or untracked relative to what the tool left on disk.

Keep these files at `.piton/.local/pull-requests/<number>/`, not inside review rounds:

| File | Contents |
| --- | --- |
| `checkout.json` | original evidence and identity, the checkout's location and start, the state the tool left it in, observed tips, validated progress, working baseline, and completed publication facts; never approval or workflow lifecycle |
| `checkout-inspection.json` | the latest complete observations and phase validation |
| `checkout-contribution.diff` | the original merge-base-to-PR contribution |
| `checkout-rebase.diff` | the original source head to the current rebased or intermediate result; upstream integration isn't labeled as a later fix |
| `checkout-conflicts.diff` | the current combined conflict comparison in the scratch worktree |
| `checkout-fixes.diff` | the tool's disk state to the local draft; empty outside fixes |
| `checkout-published-fixes.diff` | additive fixes already published after the latest integration point, separate from the current draft |
| `checkout-untracked.json` | new files relative to the tool's disk state. Read relevant contents explicitly; don't stage later fixes just to see them |
| `rebase/` | the scratch worktree, only while a rebase runs |

**Output size:**

- Output is limited to 2000 lines or 50 KB and points to the full private evidence when truncated.
- These inspection files are mutable working evidence. Freeze relevant original, rebase, resolution, fix, and source evidence through `reviews/<provider-base-sha>-<provider-head-sha>/review-NNN/inputs/` before independent review.
- Earlier review records and source snapshots are never moved or rewritten.

## Safety boundary

**Execution:**

- The tool uses direct argument arrays and private-path validation, including dangling symlinks. It never loads contributor resources.
- Inherited Git relocation or configuration overrides are refused before context or artifact readers run. Unset those host overrides explicitly and retry only the approved operation.
- Operations on the same PR share the existing file-mutation queue.

**Git controls disable:**

- hooks (`core.hooksPath=/dev/null` on the supported Unix host);
- fsmonitor;
- configured clean, smudge, and process filters;
- external diff and textconv helpers;
- custom merge-driver commands. A blocked driver reports a conflict instead of running, or silently declaring a successful merge;
- signing and editor helpers;
- rerere reuse;
- maintenance;
- submodule recursion, including at push time.

It never fetches LFS or submodule assets automatically and changes no global configuration. Held `.gitattributes` keeps the contribution from selecting the user's own Git helpers in later manual Git use.

**What the tool can't enforce:**

- Editors and language servers run project code when files open: build scripts and proc macros (rust-analyzer), linter and formatter configuration (ESLint, Prettier), package scripts. Held configuration doesn't cover the PR's own code. The safety assessment calls such changes out; turning that tooling off for a review is the user's editor setting, such as VS Code's Restricted Mode.
- Git commands the user runs by hand in the checkout use the user's own hooks and helpers.
- Inspect symlinks as links or Git objects, never by following them outside the checkout.

**Execution boundary:**

- A checkout isn't a sandbox.
- A rebase and its immediate publication don't authorize contributor tests, builds, installs, scripts, hooks, plugins, or formatters. Running them needs separate exact-command and isolated-environment approval; report unavailable runtime checks as **not run**.
