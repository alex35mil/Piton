# Session analysis

Read this when a finished workflow's `inspect_session_analysis` returns `enabled`: after a Task or Gig ships, an Epic completes, or an incoming PR ships. Look back over the session for workflow problems that Piton should fix, check whether they're already reported, and present them in chat.

## Scope

- Use only this session's conversation as it is in your context.
- Don't read session files, other sessions, or logs. Anything compaction removed is out of scope.
- Call `inspect_integrations` with `target: "workflow"` for the workflow tracker. On an error, report it and end the analysis: findings can't be reported without it.
- Call providers only to read that tracker, through its registered tools.

## Look for

- workflow tool errors and validation failures;
- rejected evidence and permission denials;
- retries and corrected re-calls;
- instructions that were unclear, contradictory, or missing;
- workarounds;
- avoidable round trips and redundant reads.

## Triage

1. Keep a candidate only when Piton's workflow instructions or tools contributed to it. Exclude user choices, project code, provider outages, and one-off agent mistakes that no instruction or tool invited.
   - A result with a `safeguard` field is designed behavior, not a defect. Keep it only when workflow instructions told the agent to make the call that triggered it.
2. When it helps, confirm the cause in the workflow files this session loaded, in the source checkout the pi package runs from.
3. Keep only findings that are addressable and worth a tracker item.
4. List the workflow tracker's open issues once, numbers and titles only, with complete pagination, and compare each finding against them. On GitHub, use `list_issues`, not `search_issues`: its free-text matching can return an empty result for issues that exist.
   - Read an issue only when its title looks related.
   - When the list can't be read completely, mark the findings as not checked for duplicates. Don't retry or change permissions or configuration.
5. When the finished work's tracker item is in the workflow tracker, also check that list for open issues the work resolves.

## Present

Present two groups, and omit an empty one:

- **Already reported:** one line per finding, with a short title and the issue link.
- **Needs attention:**
  - each new finding: a short title, what happened, the likely cause in workflow files or tools, and a suggested fix;
  - each partial overlap: the issue, what differs, and a suggestion to comment there instead of filing a new report;
  - each open issue the finished work resolves, with the reason.

When nothing qualifies, say so in one line.

Presenting findings authorizes nothing. When the user picks findings to report, handle each one as a workflow report: read [workflow report preparation](./queue.md#prepare-a-workflow-report) and follow its sanitization and approval rules.
