# User review of the branch diff

Read this at step 8 when the plan had commit steps, or whenever the user asks to see the diff ("show me the diff").

## Begin

Call `begin_user_review`. HEAD detaches at the merge base with the target, so the whole branch diff shows as staged. Only HEAD moves.

- `ready`: tell the user, in one line, that the diff is staged in the worktree.
- `uncommitted`: nothing is committed, so the diff is already in the worktree. Say so; there's nothing to end.

## End

Call `end_user_review` before anything that commits or moves HEAD: "submit", "ship it", a requested fix, or another commit step. Edits the user made during the review stay as uncommitted changes; name them and ask whether they belong in this work.

## During the review

- Don't commit, stash, reset, or switch. `git stash` here would stash the whole branch diff.
- A refusal changes nothing. Report it and ask the user how to proceed.
