# pi RPC behavior: verified facts

**Verified on pi 1.5.2 (every scenario, 2026-11-11).** Earlier: pi 1.3.0 (2026-11-07 to 2026-11-10). After a pi
upgrade, re-run the probes and update this line.

The probes are `cli/tests/probe/probe.mjs`: real `pi --mode rpc` with the faux extension in
`cli/tests/fixtures/faux-extension.ts`, a scratch workspace and `PI_CODING_AGENT_DIR`. Run a scenario with
`node tests/probe/probe.mjs <scenario>` in `cli/`.

## Test model without credentials

`fauxProvider()` from `@earendil-works/pi-ai`, registered by an extension through `pi.registerProvider(faux.provider)`,
works in RPC mode: `pi --mode rpc -e cli/tests/fixtures/faux-extension.ts --model faux/faux-1`. Replies are scripted per
prompt (`slow <n>`, `tool`, `ask`, otherwise an echo). `PITON_FAUX_TPS` sets the streaming speed.

## Session files (`session-file`, `resume`)

- `get_state` reports `sessionId` and `sessionFile` immediately, but the file doesn't exist until the first run
  persists messages. `messageCount` is 0 before that.
- `pi --session <file>` resumes the same file and the same `sessionId`; `get_entries` returns the saved entries.
- `pi --session-id <id>` starts an empty session with that id; the file name ends in `_<id>.jsonl`.
- The session folder is per workspace under `$PI_CODING_AGENT_DIR/sessions/--<escaped cwd>--/`. On macOS the cwd is
  the resolved path (`/private/var/...` for `/var/...`), so Piton must resolve symlinks the same way when it compares
  workspaces.

## Event stream (`events`)

For one prompt, in order: the `prompt` response (`disposition: "started"`), `agent_start`, `turn_start`,
`message_start`/`message_end` for the **system** message, then for the **user** message, then the assistant
`message_start`, `message_update` (`text_start`, `text_delta`..., `text_end`), `message_end`, `turn_end`, `agent_end`,
`agent_settled` (`aborted: false`).

- User messages are emitted as events, so a prompt sent by one client reaches every client.
- A system message is emitted at the start of each run (role `system`). Session views must render or skip it
  deliberately.
- The response to a command arrives before the events it causes.

## Streaming (`streaming`)

- Mid-stream, `get_state.isStreaming` is true and `get_entries` has the user message but not the streaming assistant
  message (`messageCount` counts system + user). The in-progress message exists only in the event stream, which is
  why Piton's replay is needed.
- `prompt` while streaming needs `streamingBehavior` (`"steer"` or `"followUp"`); the response has
  `disposition: "queued"`, followed by `queue_update` with `{steering: [], followUp: [...]}`.

## Queued prompts (`queued`)

- A run is active from the moment pi answers a prompt, before any event reaches a client. A second plain `prompt`
  sent after that answer is refused ("Agent is already processing. Specify streamingBehavior..."); with
  `streamingBehavior` it's queued. When idle, a `prompt` with `streamingBehavior` starts a run (`disposition:
  "started"`): the behavior applies only during one. So piton always sends `prompt` with a `streamingBehavior` and
  lets pi decide, instead of choosing from its own (lagging) status.
- pi marks the run active only after preparing the prompt (input hooks, auth check), and answers only then. On 1.3.0
  a second prompt written right behind the first still finds the run active. From 1.5 (checked on 1.5.2,
  2026-11-11) it can arrive during that preparation: pi answers `"started"` to both and then drops the second, with
  no message, no `queue_update`, and no error (pi #10606 was the same bug on 1.0.4). With a gap of 1 ms or more it's
  queued as before. So Piton writes a `prompt` only after pi answered the previous one (`cli/src/session/prompts.rs`).
  The probe writes its prompts straight to pi, without that queue, so on 1.5 it times out waiting for the dropped
  ones: every prompt is answered `"started"` and only the first runs. `cli/tests/e2e/attach.mjs` ("prompts sent
  together all run") checks the queue and passes on 1.5.2.
- Follow-ups queued together become user messages with the same `timestamp` as the run's first prompt (one
  millisecond). A message's timestamp doesn't identify it.

## Tools (`tool`)

- `tool_execution_start` has `toolCallId`, `toolName`, `args`; `tool_execution_update` adds `partialResult`;
  `tool_execution_end` has `result`, `isError`, `durationMs`.
- While a tool runs, `isStreaming` is true and `get_entries` already contains the assistant message with the tool
  call; the tool result appears only after the tool ends.

## Dialogs (`dialog`)

- A `select` from a tool arrives as `extension_ui_request` with `id`, `method`, `title`, `options` (no `timeout` unless
  the extension sets one). `isStreaming` is true while pi waits.
- The first `extension_ui_response` resolves it. A second response with the same `id` is ignored silently: no error
  response, no event.

## Extension status and widgets (`status`)

- `ctx.ui.setStatus(key, text)` arrives as `extension_ui_request` with `method: "setStatus"`, `statusKey`,
  `statusText`; `setWidget(key, lines)` as `method: "setWidget"`, `widgetKey`, `widgetLines`. Each has its own `id`
  and needs no answer.
- Clearing (`undefined`) sends the same record without `statusText` / `widgetLines`. Piton's replay keeps the latest
  per key and drops a key on a record without its value.

## Compaction (`compaction`)

- `compact` (and automatic compaction) emits `compaction_start {reason}` and then `compaction_end {reason, aborted,
  willRetry}` with either `result {summary, firstKeptEntryId, tokensBefore, estimatedTokensAfter, usage, details}`
  or `errorMessage`. The error message is complete ("Compaction failed: Nothing to compact (session too small)").
- After a successful compaction `get_entries` still lists every entry, with a `compaction` entry (`summary`,
  `firstKeptEntryId`) appended on the active branch; the view rebuilds from it.
- Short sessions don't compact (keepRecentTokens, default 20k); the probe sets `compaction.keepRecentTokens: 1` in
  the scratch agent dir's `settings.json`.

## Context control from an extension (#207)

Verified on pi 1.5.2 (2026-11-17) with throwaway probes written for the #207 research (faux provider, scratch
workspace and agent dir). They aren't part of `probe.mjs`; the research report on #207 has the numbers.

- **Compaction threshold:** pi compacts when the context exceeds `contextWindow - reserveTokens`; there is no
  absolute budget. The settings live in pi's `settings.json`, which Piton never writes.
- **`ctx.compact()` has a floor:** it keeps `keepRecentTokens` (default 20k) word for word and fails with "Nothing to
  compact (session too small)" below that, before `session_before_compact` runs. An extension can't hold a smaller
  budget through it.
- **Compaction blocks prompts:** during a manual compaction `get_state.isCompacting` is true and a `prompt` is
  refused ("Cannot submit a prompt while compaction is in progress"), with or without `streamingBehavior`. It isn't
  queued.
- **Boundary drafts:** a `compaction` entry returned from `turn_end` or `agent_before_settle` is appended without a
  model call, without the floor, and without `compaction_start`/`compaction_end`: only `entry_appended` is emitted.
  Piton doesn't handle `entry_appended` yet, so its view shows such a cut only after a later rebuild. A cut at
  `turn_end` also bounds a single long tool run, which a cut at settle doesn't.
- **Slow `agent_before_settle`:** while a handler runs (for example, waiting for a summary), `isStreaming` is true and a
  `followUp` prompt is accepted and queued.
- **`context` handlers:** a returned message list is sent as is, with pi's prompt and tools restored at the head, and
  isn't persisted. A restarted pi without the handler sends the whole history.
- **Prompt sections:** sections set in `before_agent_start` are diffed, and only changed sections go out as a patch
  system message. Models with `supportsMidConvoSystemMessages` keep their cached prefix; others rebuild the prompt
  head (a cache miss per change). So per-turn data such as a clock belongs on the user message, through an `input`
  transform, not in a section.
- **Message order:** a message returned from `before_agent_start` follows the user's prompt in the same run and is
  stored as a `custom_message` entry.
- **`new_session`:** the new session is ready in about 30 ms. `session_start` reports `reason: "new"` and
  `previousSessionFile`; it fired twice per `new_session` in the probe, so handlers must be idempotent.
- **Model calls from an extension:** `ctx.modelRegistry.complete()` works inside a hook and from a background
  promise.
- **Session size and memory:** pi holds the whole session file in memory, and a full `get_entries` multiplies it.
  Start time and turn time stay flat. `get_entries` with `since` stays cheap.

  | Session file | pi memory before / after a full `get_entries` | Full `get_entries` |
  | --- | --- | --- |
  | 10 MB | 145 / 211 MB | 48 ms |
  | 50 MB | 247 / 469 MB | 123 ms |
  | 200 MB | 491 / 1,352 MB | 384 ms |

## switch_session while streaming (`switch-streaming`)

- pi aborts the run first: the streaming message ends with `stopReason: "aborted"` (`message_end`, `turn_end`,
  `agent_end`, `agent_settled`), and only then comes the `switch_session` response (`cancelled: false`). The
  aborted run's events precede the response, so Piton's rebuild after the response starts from a settled pi.

## Stopping (`stop`, `killed`)

- Closing stdin exits pi with code 0: idle within ~5 ms; mid-stream within ~10 ms on 1.3.0 and ~200–270 ms on 1.5.2
  (faux provider; a real provider with pending work may take longer). Either is far inside Piton's stop grace
  (`grace_ms`, 15 s). Mid-stream, the stream simply stops: no `message_end`, `agent_end`, or `agent_settled`.
- SIGKILL mid-stream: the session file has the earlier turns only; the interrupted turn is lost entirely, its user
  message included, although that message's `message_end` had already been emitted. Resuming works, and a new prompt
  continues the branch from the last persisted entry. So a client that saw the user message live won't find it after
  a crash: the conversation it rebuilds from the file is shorter.

## pi's agent directory

What pi 1.5 writes in its agent directory (`PI_CODING_AGENT_DIR`, default `~/.pi/agent`), read from its source: the
sandbox keeps exactly these writable ("Agent access" in design.md).

- Runtime state: `sessions/`, `auth.json` (provider logins and token refreshes, locked with `proper-lockfile`:
  `auth.json.lock/`), `models-store.json`, `mcp-auth.json`, `crashes.json`, `mcp.log`.
- Configuration and code a later pi loads: `settings.json` (written when a setting changes in a session; a failed
  write is recorded as a settings error and pi keeps running), `trust.json` (whether a folder's `.pi/` extensions,
  settings, MCP servers, skills, and prompts load), `keybindings.json`, `models.json`, `extensions/`,
  `skills/`, `prompts/`, `themes/`, `tools/`, `bin/` (managed `fd`, `rg`), `npm/` and `git/` (installed packages),
  `tmp/` (package installs).

## Sandboxes don't nest

A process in a Seatbelt sandbox can't apply another one (`sandbox_init` fails with "Operation not permitted"). The
Piton daemon runs outside any sandbox (launchd), so this only affects running Piton's sandbox mode from a sandboxed
agent session. Tests of sandbox mode run on the host.

## Not yet verified

- Automatic compaction during a run (between turns) and its interaction with replay.
- Compaction with a real provider (the faux model's summary is canned).
