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

What pi 1.5 writes in its agent directory (`PI_CODING_AGENT_DIR`, default `~/.pi/agent`) and in a project's `.pi/`,
read from every file write in its `dist/` (pi 1.5.2). The sandbox keeps exactly the runtime state and the locks
writable (`AGENT_DIR_WRITES` in `cli/src/launch/sandbox/policy.rs`; "Agent access" in design.md). Re-read this when
pi is upgraded: search its `dist/` (outside `bundle/`) for `writeFileSync`, `mkdirSync`, `appendFileSync`,
`renameSync`, `rmSync`, `createWriteStream`, and `lockfile.`.

`proper-lockfile` locks are folders (`<file>.lock/`) pi creates, touches while it holds them, and removes. pi takes
them to read a file as well as to write it.

- Runtime state, writable:
  - `sessions/` (the default session folder; `sessionDir`, `--session-dir`, and `PI_CODING_AGENT_SESSION_DIR` move
    it, see below).
  - `auth.json` and `auth.json.lock/` (provider logins and token refreshes); `models-store.json` and its lock (the
    providers' model catalogs, refreshed at startup); `mcp-auth.json` and its lock (MCP OAuth state).
  - `mcp-auth-refresh-<hash>.lock/`: one lock per MCP server while its OAuth token refreshes.
  - `crashes.json`; `mcp.log` and `mcp.log.1` (MCP servers' log messages, rotated at 5 MB; write errors ignored).
- Locks of read-only files, writable without the files:
  - `settings.json.lock/`: taken to read `settings.json` at startup. Without it pi prints `Warning: Invalid settings
    file ... EPERM ... mkdir ...settings.json.lock` and runs without the global settings: no packages, so no
    extension providers such as `claude-code`, and none of the extensions, default model, or other settings.
  - `trust.json.lock/`: taken to read `trust.json`, at startup when the folder has `.pi/` resources or
    `.agents/skills`. Without it pi exits (`EPERM ... mkdir ...trust.json.lock`).
  - A project's `.pi/settings.json.lock/`, in the workspace: taken to read `.pi/settings.json` when the project is
    trusted. Without it pi warns as above and ignores the project's settings.
- Configuration and code a later pi loads, read-only (a write fails in the sandbox):
  - `settings.json`: written when a setting changes in a session; a failed write is recorded as a settings error and
    pi keeps running.
  - `trust.json`: written when the user answers the trust prompt (interactive only) or an extension's
    `project_trust` handler asks to remember; RPC sessions with no handler only read it.
  - `mcp.json` (MCP servers; `/mcp` changes, `pi mcp add`), `keybindings.json` (a one-time format migration),
    `models.json`, `extensions/`, `skills/`, `prompts/`, `themes/`, `SYSTEM.md`, `APPEND_SYSTEM.md`.
  - `bin/` (managed `fd` and `rg`, downloaded when they aren't on `PATH`), `npm/` and `git/` (packages: installed
    when missing, updated with `pi update`), `tmp/extensions/` (packages from `-e`).
  - Startup migrations of old layouts (`oauth.json`, `commands/`, `tools/`, session files at the folder's root): they
    find nothing to do in a current agent directory, and their errors are ignored.
- Elsewhere: tool output pi doesn't show in full goes to the OS temp folder. `/export` and the interactive debug log
  (`pi-debug.log` in the agent directory) are interactive commands.

A session folder set with `sessionDir` (settings or a project's `.pi/settings.json`), `--session-dir`, or
`PI_CODING_AGENT_SESSION_DIR` outside the agent directory isn't granted: a sandboxed pi can't create its session file
there. Grant it with `[sandbox] write`.

## Sandboxes don't nest

A process in a Seatbelt sandbox can't apply another one (`sandbox_init` fails with "Operation not permitted"). The
Piton daemon runs outside any sandbox (launchd), so this only affects running Piton's sandbox mode from a sandboxed
agent session. Tests of sandbox mode run on the host.

## The kernel's record of sandbox refusals

Verified on the host, macOS 27.0.1, with `piton sandbox-exec` (#241's spike):

- Each refusal is a `log stream` event from the Sandbox kernel extension (`senderImagePath` ends in `/Sandbox`),
  within ~5 ms: `Sandbox: <process>(<pid>) deny(1) <operation> <path or service>`. Seen: `file-write-create`,
  `file-write-data`, `file-read-data`, `mach-lookup com.apple.SecurityServer` (the keychain),
  `user-preference-write <domain>` (`defaults write`). `log` refuses to run inside a sandbox.
- A deny with `(with message "<text>")` appends `\n<text>` to its event. nono's own `(deny default)` has none.
- A profile has one default action, and a later `(deny default (with message "<text>"))` replaces nono's untagged one:
  every refusal no rule decides then carries the message, and allows still win (verified with `/usr/bin/sandbox-exec`
  and with nono, the tagged default as a platform rule; #357, 2026-11-23). Newly tagged that way:
  `user-preference-write <domain>` (`defaults write`), `signal pgrp [<process>(<pid>)] signum:<n>` (a signal to another
  process), `lsopen` with an empty target (`open`, `open -a`, `open x.command`, JXA `NSWorkspace.openURL`),
  `file-issue-extension target:<app> extension-class:com.apple.app-sandbox.read` (`osascript` telling another app to
  act; still refused with launching apps open, the `apps` preset: `System Events got an error: A privilege violation
  occurred`), `sysctl-write <name>`, `network-outbound /private/var/run/syslog`. A signal 0 (`kill -0`) is refused
  with no record.
- Two refusals never carry the profile's message: `job-creation` (`launchctl submit`, `at`: launchd checks the caller's
  sandbox itself) and `forbidden-exec-sugid` (running a setuid program: `ps`, `top`, `sudo`). They can't be routed to a
  call.
- Incidental: `open` and `osascript` hit `sysctl-write kern.grade_cputype` and connect to
  `/private/var/run/syslog` while working, also when they succeed. Six hours of sandboxed sessions (pi `--mode rpc` with
  Piton's package, the GitHub MCP server, pi-claude-code's `claude`, dozens of bash calls) left no other refusal from a
  session's processes that the tagged default would surface; `claude`'s only one was `forbidden-exec-sugid`. Ordinary
  commands (`git`, `node`, `python3`, `cargo`, `npm`, `swift`, `xcrun`, `gh`, `mise env`) produced none.
- Repeats in a burst fold: 200 refused writes in one process, to one path or to 200, logged one event. Separate
  processes each get theirs, and the kernel folds repeats into `N duplicate reports for ...`. Refusals a second or
  more apart are each reported, to the same path or new ones (#342).
- A sandboxed `node` process's first refusals are sometimes not reported: in several runs, nothing for the first
  5-7 s after its first refusal, then every one; in others, all of them. `touch` and `python3` were always reported
  (#342; the rule behind it is unverified). A block in that window isn't asked about unless it happens again, so
  `cli/tests/e2e/blocks.mjs`'s faux tool retries its refused write once a second for 10 s.
- The pid is the refused process (`touch`, not the shell that ran it), usually gone when the event arrives. macOS
  returns another process's arguments but not its environment to a non-root reader (`KERN_PROCARGS2`), even for the
  same user's processes.
- A rerun with one more grant opens exactly that grant; `defaults write` works with `~/Library/Preferences` writable
  (cfprefsd checks the caller's write access to the plist).
- Other apps' refusals stream all the time (WhatsApp, sharingd, WebKit), so only tagged ones are Piton's.

## Launching apps and Mach services from the sandbox

Checked in a sandboxed session (nono 0.82, macOS 27.0, `standard`), from exit codes and `sandbox_check` (#249):

- nono's `(deny default)` never allows `lsopen` or `appleevent-send`. `open x.command`, `open -a Calculator`,
  `open -b com.apple.Terminal`, and `open https://…` fail with `_LSOpenURLsWithCompletionHandler() … error -54`;
  `osascript` telling Terminal to `do script` fails with `sandbox_extension_issue_file failed … -54`; JXA
  `NSWorkspace.openURL` returns false. Nothing ran on the host.
- `launchctl submit`, `bootstrap`, `load`, `kickstart`, and `at` fail; `launchctl asuser` runs its command still
  sandboxed. `job-creation`, `distributed-notification-post`, and `user-preference-write` are denied.
- `(allow mach-lookup)` reaches every other app's service: Hammerspoon's `hs` reached the lookup and failed only
  because its `ipc` module wasn't loaded; `shortcuts run` reached the Shortcuts service.
- `bootstrap_look_up` returns 1100 for a sandbox-denied lookup and 1102 for a service that isn't running, so a probe can
  tell them apart (`cli/tests/probe/protected.sh`).
- Launch Services' `LSCopyDefaultApplicationURLForURL` answers from inside the sandbox (lookups go through
  `com.apple.lsd.mapdb` and `launchservicesd`, not `lsopen`).
- On the host, with only the built-in list (`strict`), the kernel's record shows `open https://…` looking up
  `CoreServices.coreservicesd`, `DiskArbitration.diskarbitrationd`, `coreservices.quarantine-resolver`, `lsd.open`,
  `SharedWebCredentials`, `coreservices.sharedfilelistd.xpc`, `metadata.mds.legacy`, and `coreservices.appleevents`. It
  opens the link with all but `SharedWebCredentials`. git, curl, python, and node TLS, `id`, and `defaults read` need
  nothing beyond the list.

## pi behind fnox's credential proxy

Verified on the host with fnox 1.48.0, op 2.34.1, claude 2.1.309, pi 1.5.2 (#229):

- `fnox --non-interactive proxy run --no-sandbox -- <piton> sandbox-exec -- pi --mode rpc` keeps pi's RPC clean:
  stdout carries only pi's records, fnox logs on stderr.
- The GitHub MCP server's `Authorization: Bearer ${GITHUB_PERSONAL_ACCESS_TOKEN}` header gets the real token substituted
  for the placeholder (HTTP 200).
- The claude binary with `CLAUDE_CODE_OAUTH_TOKEN` (from `claude setup-token`) behind the proxy: every Anthropic
  request, `/v1/messages` included, gets the token injected; `claude -p` and pi's claude-code models (through the
  extension's relay) answer with only a placeholder in the environment and the keychain closed to the sandbox.
- 1Password's desktop app approves each new process, including launchd starts while it's unlocked. A service account
  whose token fnox reads from the keychain needs no prompt from a terminal or launchd; the proxy starts in 4-5 s.
- fnox prints its warnings on stderr and starts the command anyway when a secret can't be resolved (a missing item, an
  unknown provider, 1Password unreachable): `Error resolving secret 'X': <reason>` or `Warning: Secret 'X' not found
  ...`, then `fnox proxy: 'X' is not set; requests that need it are refused until it is`. A broken config, a missing
  `[proxy]`, or `[proxy]` without rules makes `proxy run` and `proxy rules` exit 1 before anything starts.
- `proxy run` sets `HTTP(S)_PROXY`/`ALL_PROXY` (with a per-session password), `NO_PROXY=localhost,127.0.0.1,::1`,
  `NODE_USE_ENV_PROXY=1`, the CA variables (`NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE`, `CURL_CA_BUNDLE`, ...), and
  `FNOX_DAEMON=off`; each rule's variable holds a `fnox_proxy_<hex>` placeholder unless the rule sets one.
- `fnox config-files` lists the files fnox loads from a folder, nearest first, each followed by its imports, then the
  global config.
- `fnox proxy rules` prints `egress:`, `audit:`, then one line per rule ending in `env=<VAR>`, the variable that holds
  its placeholder, without resolving secrets.
- `gh` 2.96.0 (Go 1.26.4) on macOS 27 refuses fnox's certificate ("x509: certificate is not trusted"), with or
  without `SSL_CERT_FILE`; `curl` with the same placeholder gets the token injected (HTTP 200).
- `gh` 2.105.0 (Go 1.27.4) behind fnox 1.50 in a sandboxed session: REST (`gh api user`) and GraphQL (`gh issue view`)
  get the token injected, and a host without a rule (`example.com`) still verifies. Go 1.27 reads `SSL_CERT_FILE` on
  macOS (`x509sslcertoverrideplatform`, on by default), and fnox's certificate file holds the system's root
  certificates too (158). `gh` needs a placeholder to send: its own login is in the keychain, closed to the sandbox.
- Git over HTTPS to GitHub signs in only with `Authorization: Basic`; `Bearer` and `token` get 401. fnox 1.48 can't
  substitute a placeholder inside Basic, so Git over HTTPS doesn't work behind it (jdx/fnox#1151 proposed it, closed
  unmerged).

## The network gate (#228)

Checked in a sandboxed session with nono 0.82, fnox 1.50, pi 1.5.2, Node 24 (2026-11-23). Applying the Seatbelt rules is
checked by the host probe (sandboxes don't nest).

- nono's `CapabilitySet` without a network mode is `AllowAll`: its profile allows `network-outbound`, `-inbound`,
  `-bind`, and `system-socket`, then appends the platform network rules after that section, so a later
  `(deny network-outbound (remote ip "*:*"))` filters TCP and UDP with Piton's allows after it.
- `sandbox-exec -p` parses `(remote ip "*:*")`, `(remote tcp "localhost:8080")`, `(remote tcp "*:22")`,
  `(remote udp "*:53")`, and `(remote ip "localhost:*")`, also with `(with message ...)`; a named host is refused:
  "host must be * or localhost in network address".
- fnox's `proxy run` (permissive egress) tunnels `CONNECT github.com:22` (the SSH banner comes back) and
  `CONNECT <host>:443` to hosts no rule names, so the gate can forward every tunnel to it. Its URL carries a
  per-session password; a CONNECT without it gets `407`. Inherited file descriptors reach fnox's child.
- pi's built-in providers and their hosts come from `@earendil-works/pi-ai`'s `getBuiltinProviders()` and
  `getAllBuiltinModels()` (`dist/providers/all.js`, reached with `findPackageJSON` from pi's package; the package's
  `exports` are import-only). 41 providers with a plain host in 1.5.2; Google Vertex's is a template
  (`{location}-aiplatform.googleapis.com`). Sign-in hosts are the literal `https://` hosts in `dist/auth/oauth/*.js`
  (`platform.claude.com`, `claude.ai`, `auth.openai.com`, `auth.x.ai`, ...); GitHub Copilot's device flow builds its
  hosts from the domain (`github.com` by default), which isn't literal.
- `models-store.json` (pi's catalog cache) holds each discovered model's `baseUrl` (`https://chatgpt.com/backend-api` for
  openai-codex, `process://claude-code` for the claude-code extension). pi rewrites it at startup.
- pi's own fetch installs undici's `EnvHttpProxyAgent`; Node child processes read the proxy variables only with
  `NODE_USE_ENV_PROXY=1`. curl reads only the lowercase `http_proxy` for `http://`.

## TCP listening in the sandbox (#361)

Verified on the host with `/usr/bin/sandbox-exec`, macOS 27, a nono-shaped profile (tagged `(deny default)`, then
unfiltered `network-bind`/`-inbound`/`-outbound` allows, candidates after), Python sockets, 2026-11-23:

- An address filter's host is `*` or `localhost` only: `127.0.0.1`, `::1`, `[::1]`, and `loopback` are refused ("host
  must be * or localhost in network address"). `ip4`, `ip6`, `tcp4`, `tcp6` parse.
- `localhost` in a `local` filter matches the any-address too: `(deny ... (local tcp "*:*"))` followed by
  `(allow ... (local tcp "localhost:*"))`, on `network-bind` or `network-inbound`, lets a listen on `0.0.0.0`, the
  LAN address, and `::` through. So listening can't be limited to loopback.
- `(remote ip "localhost:*")` on `network-inbound` changes nothing: a LAN client is accepted. `listen()` has no
  remote, and `accept()` isn't checked per peer.
- `(deny network-inbound (local tcp "*:*"))` refuses every TCP `listen()`, loopback included; `bind()` still passes,
  and UDP, TCP clients, and Unix socket listens are unaffected. The kernel's record is
  `deny(1) network-inbound local:*:<port>`, the port an ephemeral one rather than the bound one, so a port-scoped
  allow (`(local tcp "*:18361")`) never matches either.
- `(deny network-inbound (local ip "*:*"))` also refuses UDP replies to a client (a DNS query's answer), and a
  `network-bind` deny refuses UDP binds: only the `tcp` form leaves DNS working.
- A filtered network (`(deny network-outbound (remote ip "*:*"))` with `localhost:*` allowed) doesn't stop a
  sandboxed server on `0.0.0.0`: its send on an accepted TCP connection from a LAN client passes, and a UDP server on
  `0.0.0.0` answers a LAN peer. `(remote ip "localhost:*")` outbound also reaches this Mac's own LAN and tailnet
  addresses.
- Claude Code's sandbox-runtime (`allowLocalBinding`: `(local ip "*:*")`) and Codex (`allow_local_binding`:
  `(local ip "localhost:*")`, which the above shows also matches every address) open every address when they open
  listening; Microsoft's mxc documents inbound scoped to loopback as not expressible.

## pi's Claude Code in the sandbox

Verified with claude 2.1.309, pi 1.5.2, and pi-claude-code at `d5b5e04` (#311):

- The extension reads `PI_CLAUDE_CODE_CONFIG_DIR` through pi's provider auth context, which is pi's process
  environment, and runs `claude` with it as `CLAUDE_CONFIG_DIR`.
- In a fresh `CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_OAUTH_TOKEN` alone signs in: `claude auth status` reports
  `loggedIn: true`, `authMethod: "oauth_token"` (without it, `"none"`). No keychain access.
- `claude auth status` writes `.claude.json`, its lock, and `backups/` in the folder. A `claude -p` turn with the
  extension's flags writes `.claude.json`, `.last-cleanup`, `sessions/`, and `projects/<cwd>/` in the folder.
- `claude` locks the folder itself with `proper-lockfile`: it creates `<CLAUDE_CONFIG_DIR>.lock`, a folder beside it
  (seen as a sandbox block of `~/.config/claude.lock`).
- The log cache is `~/Library/Caches/claude-cli-nodejs/<cwd>/` (`mcp-logs-<server>/`, `errors/`), whatever
  `CLAUDE_CONFIG_DIR` is.
- The keychain login's entry is `Claude Code-credentials` plus `-<first 8 hex of sha256(config folder)>` when
  `CLAUDE_CONFIG_DIR` is set: each folder has its own login.
- The extension's relay listens on `127.0.0.1` at a random port and reaches Anthropic from pi's process.

Read from the `claude` 2.1.309 binary (#331):

- Every `claude` serves a messaging socket at `($XDG_RUNTIME_DIR || $CLAUDE_CODE_TMPDIR || /tmp)/cc-socks/<pid>.sock`,
  falling back to `/tmp/cc-socks-<uid>/<pid>.sock` above 103 bytes. Each folder on the way must be private or sticky,
  owned by the user or root. Before binding it tries `listen`; on `EADDRINUSE` it connects for 250 ms to see whether a
  live session owns the file, unlinks it if not, and binds again. It removes its socket when it exits normally.
- The extension ends every `claude` with SIGKILL (`killProcessTree`), so none removes its socket:
  `/private/tmp/cc-socks` held about 29,850 files on the user's Mac, none of a live process
  ([pi-claude-code#50](https://github.com/earendil-works/pi-claude-code/issues/50)).
- `CLAUDE_CODE_TMPDIR` also moves Claude Code's per-user temp root (`<dir>/claude-<uid>`: task output, prompts,
  plugin staging), plugin zips, ssh-config and git-bundle temp files. Its children's `$TMPDIR` is
  `<dir>/claude-<uid>` only when that's at most 44 bytes, else `/tmp/claude-<uid>`.
- At startup it reads a stored login from the keychain (`security find-generic-password -a <account> -w -s
  <service>`, run by `claude`) even with `CLAUDE_CODE_OAUTH_TOKEN` set; no setting turns that off. With the keychain
  closed, both `claude` and `security` are refused `mach-lookup com.apple.SecurityServer`, and `claude` signs in with
  the token.

## Not yet verified

- Automatic compaction during a run (between turns) and its interaction with replay.
- Compaction with a real provider (the faux model's summary is canned).
