# Workflow reporting source (development install)

Read this when a `target: "workflow"` inspection succeeds or reports an error. It applies only to a development install: the pi package running from a git checkout of piton.

**The source:**

- The workflow source is the checkout the pi package runs from. Nothing configures it.
- `inspect_integrations` and `inspect_queue_intake` take `target: "current" | "workflow"`. `current` inspects the current repository; `workflow` reads the source checkout's `.piton/config.toml` with the same tracker schema, validation, and read-only readiness checks, and returns `target` and `projectRoot`.
- In an app install, `target: "workflow"` errors with the new-issue address: report through the user instead ([queue](../queue.md#report-a-workflow-problem)).

**Session analysis:**

- In a development install, every shipped Task or Gig, completed Epic, and shipped incoming PR ends with an in-chat analysis of the session for workflow problems ([session analysis](../session-analysis.md)). `inspect_session_analysis` says whether it's on.
- `[debugging] session_analysis = false` in the global `config.host.toml` turns it off.
