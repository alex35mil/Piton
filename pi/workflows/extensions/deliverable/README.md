# Deliverable extension

Registers native deferred `cleanup_delivery_branch`, which validates and executes one explicitly approved post-merge Task branch cleanup in the current clone without remote deletion, and `prepare_merge_resolution_comment`.

`merge-resolution.ts` renders the PR comment that records a pushed merge-conflict resolution. It requires the recorded PR, a stored branch head that is a two-parent merge whose second parent is on `origin/<target>`, `origin/<branch>` at that merge, and a non-empty `git show --remerge-diff`. The body has the heading, target and merge SHAs, the push-approved summary, the remerge-diff folded by the shared `integrations/comment-diff.ts` helper, and a `merge-resolution:<sha>` marker; oversized diffs are replaced by the local command. It returns the native `add_issue_comment` call as top-level `mutation`, as every prepare tool names its provider call, and never calls providers.

It takes the merged PR's exact `pull_request_read` get as `evidence`; `githubMergeProof` in `index.ts` extracts merge facts through the same `shippingReadDecision` validation used by `prepare_integration_merge`. Every Deliverable ships through a merged PR, so there is no other delivery route. The strict internal merge-proof schema and the repository operation live in `cleanup.ts`. See [Deliverable guidance](../../references/deliverable.md) for the lifecycle and approval contract.

## User review

`user-review.ts` owns `begin_user_review` and `end_user_review`, which put a Task branch's diff in the current checkout for the user and return to the branch ([guidance](../../references/deliverable/user-review.md)).

Stateless: begin detaches HEAD at the merge base with the target (`branchBase` from `review/diff.ts`, so the user sees the reviewer's diff); end points HEAD back at the branch. Only HEAD moves, so uncommitted work and edits carry through. Both refuse during a merge, rebase, cherry-pick, or revert. End also refuses unless HEAD is still at the base, since reattaching would drop a commit made during the review.
