# Piton: design

Status: draft. Decisions below were settled in discussion; open questions are listed at the end.

## Problem

A pi RPC process lives exactly as long as the client that started it: Pi 1.3.0 RPC has one client per process, the
client owns pi's stdin, and closing stdin shuts pi down (still true on 1.5.2). Today that client is pi.nvim inside Neovide, so every session
dies with the editor and is unreachable away from the laptop.

Piton separates where pi runs from who is looking at it:

- pi processes run under a daemon on the laptop and outlive any client;
- pi.nvim becomes a client that connects and disconnects without interrupting a session;
- an iOS/iPadOS app (and a macOS menu bar app) connects over Tailscale to read and steer sessions;
- the user is notified by push when an agent needs them.

Piton is personal tooling for one user on one MacBook Pro. pi.nvim is a public, provider-neutral library; Piton is one
implementation of its generic connector contract and gets no special treatment there.

## Use cases

From the phone, in priority order:

1. Get told when an agent needs attention: a run finished (it stopped its loop), it asks a question (extension
   dialog), it failed, or pi exited.
2. Read a session live, including the reply that is still streaming.
3. Steer a drifting run, answer a finished one with a follow-up, answer a dialog, abort.
4. Start a new session in a workspace, or continue a saved one.
5. Later: review what the agent changed (diffs).

From the laptop: keep working in pi.nvim exactly as today, and pick up a session that was driven from the phone.

## Principles

- **Transparent to pi.** Piton runs `pi --mode rpc` in the workspace with the environment the user's shell would give
  it. It never changes pi's settings, agent directory, session files, or extensions, and never computes pi's session
  folder itself. Per-folder agent directories (set through mise) keep working because Piton makes no assumption about
  them.
- **Build on pi's protocol, don't replace it.** pi.nvim gets raw pi RPC. Piton types only the fields it acts on
  (command ids and types, event types, extension UI requests); everything else passes through as JSON.
- **One live pi per session file.** pi doesn't lock session files; two processes appending to one file corrupt its
  tree. Piton enforces the rule for every pi it owns. A pi started outside Piton (a terminal) is out of scope.
- **Single writer, many readers.** The last client to send a prompt, steer, follow-up, abort, or dialog answer becomes
  the session's writer. Everyone sees everything. No locks, no take-control step.
- **Exhaustive domain types.** Session lifecycle, connection, attention, and writer state are Rust enums matched
  exhaustively.

## Architecture

```
Neovide/pi.nvim ── stdio, raw pi RPC ── piton attach ──┐
                                                     ├─ Unix socket ──────────┐
Piton.app (macOS menu bar) ────────────────────────────┘                        ├─ piton daemon ── pi --mode rpc (× N)
iPhone / iPad ─────────── WebSocket over the tailnet ─────────────────────────┘        │
                                                                                       ├─ APNs (direct)
                                                                                       ├─ Telegram (optional)
                                                                                       └─ piton-sleepd (XPC)
```

### Components

- **`piton` (Rust, one binary):** the daemon (`piton daemon`) and the CLI (`piton attach`, `piton stop`, `piton ls`,
  `piton service install|uninstall`).
- **`Piton.app` (SwiftUI, one codebase for iOS, iPadOS, macOS):** session list, session view, steering, dialogs,
  notifications. On macOS it is a menu bar app that also bundles and registers the daemon and the sleep helper.
- **`piton-sleepd`:** privileged helper with exactly two operations (prevent sleep, restore sleep). Language decided
  during implementation (Swift is simplest for XPC).

The binary's crate (`cli/`) is one module per concept. Each module's `mod.rs` is its API, and its files are private:

- `commands/`: the command line, one file per command.
- `config/`: configuration files (see "Configuration and state"). `state/`: the state database.
- `daemon/`: the running daemon: startup, the session registry, the Unix socket (`rpc`, `control`, and the apps'
  `view` connections), keep-awake, reloading the global files, and the local socket's messages (`protocol`).
- `session/`: one pi process, and the conversation model the apps render (`view`).
- `launch/`: pi's environment and command line, and the sandbox (`sandbox`: the built-in policy, one launch's
  resolved rules, and applying them with the `nono` library; the only code that knows the backend).
- `apps/`: serving the native apps: the `view` protocol, the tailnet WebSocket, browsing roots, the Settings list.
- `agent/`: the bridge between a sandboxed agent and the daemon: which files are protected and their Seatbelt rules
  (`protected`), `agent.sock` (`socket`), and a protected write asked of the user (`write`).
- `project/`: a workspace's source or worker relation, and worker clones.
- `setup/`: `piton setup` and the launchd agent. `logging/`: Piton's own logs, the logging policy, the collector.

### Packaging

One signed `Piton.app`, distributed outside the App Store (a sandboxed app can't spawn processes or install a helper):

```
Piton.app/Contents/
  MacOS/Piton                          SwiftUI app
  MacOS/piton                          Rust daemon + CLI
  MacOS/piton-sleepd                   privileged helper
  Library/LaunchAgents/…piton.plist    daemon, user login agent
  Library/LaunchDaemons/…sleepd.plist
```

The app registers both through `SMAppService` and offers a "Start at login" toggle. launchd restarts a crashed
daemon. Quitting the app leaves the daemon and sessions running. `piton service install|uninstall` writes the same
LaunchAgent for development without the app. The CLI is symlinked onto `PATH`.

**Onboarding.** On first launch the Mac app asks what this Mac does: host (runs the daemon; the app connects to its
socket) or client (connects to another host over the tailnet, like the phone). The choice is kept on the Mac and can
be changed in Settings > Connection. A host's onboarding lists the steps from `piton setup check --json`, run with
the bundled binary: tools (pi, mise, tailscale), the global configuration, the CLI link
(`~/.local/bin/piton` to the bundled `piton`), start at login, the state database, and remote access (this host's
tailnet name and port). Each is ok, missing with its fix, or failed: `piton setup apply <step>` does the steps
Piton can (create the global folder, link the CLI, create the database), the app registers the daemon, and the rest
are instructions. A development build has no bundled binary and uses the linked CLI; its daemon comes from `piton
service install`. DMG packaging, signing, and notarization are tracked separately.

### Launching pi

For every pi it starts, Piton:

1. builds the environment: the user's login-shell environment captured at daemon start (launchd's environment is
   minimal), plus `mise env -C <workspace> --json`, computed once per launch;
2. reports an untrusted or failing mise config instead of launching;
3. runs pi in the workspace, in the launch's mode (below).

Sessions started from nvim and from the phone get identical environments, and each worker clone gets its own mise
environment (today a `:tcd` worker inherits the Neovide launch folder's environment).

### Launch mode: sandbox or host

The mode is chosen per launch, not per workspace. Seatbelt, through the `nono` library, is the only sandbox.

- **Sandbox (default):** `<piton> sandbox-exec -- pi --mode rpc [...]`, with Piton's environment passed through.
  `<piton>` is the daemon's own executable (in Piton.app, the bundled one). The daemon resolves the launch's policy
  ("Agent access") from Piton's built-in policy and the configuration chain's `[sandbox]` tables, and passes it in
  the environment; the helper applies it to itself, removes it from the environment, and becomes pi. The daemon is
  never sandboxed. See "Workers" for what "source" means.
- **Host:** `pi --mode rpc [...]` directly. Only chosen explicitly: `piton start --host`, or the host toggle in the app.
- `piton attach` (pi.nvim) always launches in sandbox mode. Choosing host from pi.nvim is out of scope for now.
- A session's mode is recorded; resuming a saved session from the app or CLI defaults to its last mode; an attach to a
  live session keeps whatever mode it runs in.
- No workspace file decides the mode, so an agent can't change how its own pi is launched.

### Workers

A worker is an APFS copy-on-write clone of a source checkout, `<parent>/<repo>--<name>` next to `<parent>/<repo>`,
with `.piton/.local` linked to the source's.

- **Relation:** a workspace is a worker when its local git config has `agent.source` and the folder layout matches;
  otherwise it's a source. A claim without the layout is refused, so a stray config value can't point a sandbox
  elsewhere.
- **Source-only settings:** `.piton/` (sandbox grants, permissions, workflows, the shared artifacts folder) is read
  from the source, never from the worker, so an agent can't widen its own sandbox or change what new workers copy by
  editing its clone. An agent running in the source checkout itself is held back by the protected-file rules (see
  "Configuration and state"). A workspace inside a checkout (pi started in a subfolder) belongs to that checkout: its
  settings come from the checkout's source, and deleting a worker stops the sessions in its subfolders too.
- **`[workers] skip`** in `.piton/config.toml` lists paths not copied into new workers, such as editor indexes that
  break tooling in a clone (`apps/.build/index-build` here). It only removes things from the copy.
- **`piton worker create <name> [--from <checkout>] [--label] [--issue]`:** the source must be clean on its default
  branch; Piton fast-forwards it, clones everything except the skip list and `.piton/.local` (one `clonefile(2)` per
  entry), links `.piton/.local`, and sets `agent.source`, `agent.label`, `agent.issue`. When mise trusts the
  source's own config, Piton trusts the worker's too (`mise trust <worker>`): its config files are the source's,
  cloned, so this grants nothing the user hasn't. An untrusted source gives an untrusted worker, and a launch there
  reports it (see "Launching pi"). It writes no pi.nvim
  descriptor: pi.nvim's global spec attaches through Piton, and a workspace descriptor would override that and bypass
  Piton (no phone access, no keep-awake). The worker's sandbox comes from Piton at launch (see "Launch mode").
- **`piton worker ls`:** the source's workers with their live sessions.
- **`piton worker delete <path> [--force]`:** refuses with the uncommitted changes and unpushed commits unless forced;
  stops the worker's sessions, drops mise's trust in the folder, unlinks `.piton/.local`, deletes the folder, and
  fast-forwards the source.
  - An incoming-PR review that the pi package's `/pr` tools left untouched isn't an uncommitted change. Piton reads
    `.piton/.local/pull-requests/<number>/checkout.json`, which pi writes, and checks four fields: `phase`,
    `location.root`, and `workspace.{headSha, indexTree, diskCommit}`.
  - The review is untouched when all of these hold: the phase is `contributorReview` or `fixes`, the record's root is
    this worker, HEAD is detached at `headSha`, the index tree is `indexTree`, and the files on disk equal
    `diskCommit` with nothing untracked.
  - Any edit, untracked file, or staging change is still a leftover. Unpushed commits are reported as usual.
  - A review in `rebasing`, `rebased`, `committingFixes`, or `committedFixes` is always a leftover. The tools keep
    HEAD detached, so an unpublished fix commit, rebase result, or rebase in progress is on no branch, and Git
    status can be clean.
- **`piton tools`:** prints the registry of tools a repository can configure for itself, as JSON. The registry
  (`project/tools.rs`) covers environment managers (mise, direnv, Nix, devbox), agents (`AGENTS.md`, `.agents/`,
  `.pi/`, `.piton/`), and editors (Neovim, VS Code, Zed, JetBrains). Each entry lists the repository files the tool
  reads, as case-insensitive Git pathspecs that match at any depth. Incoming-PR review keeps those files at a
  trusted revision on disk.
- **`--json`:** every `piton worker` command prints the daemon's response; refusals are typed (`kind`: `exists`,
  `source_dirty`, `source_off_default`, `unknown_default`, `invalid_name`, `not_a_repository`, `not_a_worker`,
  `leftovers`, `failed`) with what a caller needs to explain or force them, and exit 4.

The editor keeps its UI (tabs, picker, tracker queue, dialogs) and calls `piton worker` for the rest.

### pi version

Piton targets pi 1.5.2. pi is pinned in the user's global mise config and bumped centrally across tools, so this
repository doesn't pin pi itself (Piton launches whatever `pi` the workspace's environment resolves); the pi package's
minimum is in `pi/README.md`. The RPC protocol docs (`rpc-commands.md`, `json.md`, `rpc-extension-ui.md`) are
identical on 1.3.0, 1.5.0, 1.5.2, and `main` (checked 2026-11-07, 1.5.2 on 2026-11-11). Differences between
versions that shaped Piton:

- **Stopping pi.** On 1.3.0, a signal that arrives while pi is shutting down after stdin EOF cuts its cleanup short
  (for example a pending OAuth token save). So Piton stops a pi by closing its stdin only, waiting the grace period, and
  only then sending SIGTERM, and SIGKILL after a further short wait. The default grace is longer than pi.nvim's 5 s
  (proposed 15 s) because pi waits for pending work. This sequence is also correct on newer pi, where SIGTERM starts an
  orderly shutdown too.
- **Unrequested exits.** Newer pi exits at once when an extension calls `ctx.shutdown()` while idle (1.3.0 waits for
  the next command). Either way Piton must treat an exit it didn't request with code 0 as "pi ended" (connections exit
  3), not as a crash, in status and notifications.
- **Session writes.** 1.5.0 fixed failed session writes leaving entries in memory that never reached disk. On 1.3.0 a
  failed write can still make the live pi and its file disagree; Piton doesn't depend on the file matching the live pi
  except when resuming after a daemon restart.
- **Prompts sent together.** From 1.5, pi drops a `prompt` that arrives while the previous one is still being
  prepared, after answering it `"started"` (docs/pi-findings.md, "Queued prompts"). So the session task writes one
  `prompt` at a time, from every source (rpc clients, the apps, `start --prompt`), and holds the next until pi
  answered the previous one. Held prompts reach pi even if their client left.

### Workspace roots

Piton's config lists workspace roots (`roots`; `cli/src/apps/workspaces.rs`). There are none by default: until the owner
lists some, `workspaces.browse` and `session.start` from a `view` client refuse with "no workspace roots configured",
and the apps say so. A root is a tree of grouping folders with repositories at varying depth, e.g. `~/Projects/personal/apps/app` and its worker clone `~/Projects/personal/apps/app--fix-login`
next to it. A **workspace** is a folder under a root that
contains `.git`; Piton doesn't descend into a workspace, so subfolders of a repository aren't shown. Hidden folders are
skipped.

- `workspaces.browse {path?}` returns one level as `{entries}`: the direct children of `path` (the roots when
  omitted), each either `{kind: "group", name, path}` (a folder without `.git`, browsable further) or
  `{kind: "workspace", name, path, source?}` (`source` for a worker). Groups first, by name. The phone loads a level
  when you open it, so there is no depth limit and no full scan. Paths outside the roots are refused (canonicalized
  first, so `..` can't climb out).
- Not yet: `workspaces.recent` (workspaces Piton launched pi in, newest first), `workspaces.search {query}`, and live
  session counts per workspace.
- Worker clones are ordinary workspaces in that tree. Grouping them under their main repository is a later refinement.
- `session.start` from a `view` client rejects a path that isn't a git checkout under a root.
- `piton attach` and `piton start` are not limited by roots: they run locally, in a workspace you chose yourself.

### Session registry

Piton keeps one record per live pi: workspace, pi process, session id and file (from `get_state`, refreshed after
`switch_session`, `new_session`, `fork`, `clone`), status, connected clients, writer, pending dialogs, and the session
view (below).

Saved sessions that aren't running are listed from the session folder pi reports. Finding that folder for a workspace
with no live pi is part of the session creation redesign (#208).

### Attaching from pi.nvim

pi.nvim knows nothing about Piton: it supports any attached connector (its README, "Attached pi"), and Piton is one.
Piton relies on that generic contract:

- **Restore:** after connecting, after an automatic reconnect, and after every successful `switch_session` and
  `new_session`, pi.nvim rebuilds the chat from `get_state` and `get_entries`, holding back records that arrive before
  the `get_entries` response and dropping the ones the snapshot already has. Piton's replay (below) fills in what
  those two don't contain.
- **Reconnect:** a connector exit other than 3, when pi.nvim didn't ask for it, is a lost connection, retried with the
  same command. When the reached conversation is empty and idle and the previous one had messages, pi.nvim sends
  `switch_session` back to it. Every new `piton attach` reaches a fresh, empty pi, so this is what returns a tab to its
  live session.
- **Exit code 3:** the pi behind the connector ended; no reconnect.

The global launch spec:

```lua
process = {
    kind = "attached",
    cmd = { "piton", "attach" },
    terminate = { "piton", "stop", "--session", "${{SESSION_ID}}" },
}
```

`piton attach` runs in the tab's workspace (its working directory, symlinks resolved), connects to the daemon's Unix
socket, sends a hello, and then relays pi RPC records in both directions. Its stdout carries protocol records only;
diagnostics go to stderr. The connector's environment is not passed to pi; Piton builds pi's environment itself.

| pi.nvim action | What pi.nvim sends | What Piton does |
| --- | --- | --- |
| `:Pi` | runs the connector | launches a new pi in the workspace |
| `:PiContinue`, `:PiResume` | `switch_session <path>` | see "Commands Piton intercepts" |
| `:PiNewSession` | `new_session` (no `abort`) | see "Commands Piton intercepts" |
| `:PiAbort` | `abort` | passes through, even on a shared pi |
| `:PiDetach`, tab close, Neovim exit | closes the connector's stdin | drops the client, exits 0; pi keeps running |
| `:PiStop` | confirms, runs `terminate` | `piton stop --session <id>`: see below |
| automatic reconnect | runs the connector, then `switch_session <previous file>` | new pi, then the move rule brings it back |

#### Commands Piton intercepts

A pi is **shared** when more than one client is connected to it.

- **`switch_session <path>`, another live pi has that file:** don't forward. Move the connection to that pi (the move
  sequence below). This is what makes `:PiContinue`/`:PiResume` attach to a running session and what makes reconnect
  land back on the live pi.
- **`switch_session <path>`, no live pi has it, this pi shared:** launch a new pi on that file for this client and move
  the connection there, so the other clients keep their conversation.
- **`switch_session <path>`, no live pi has it, this pi not shared:** forward; the pi loads it (today's behavior).
- **`new_session`, this pi shared:** launch a fresh pi in the workspace for this client and move the connection there.
  Not shared: forward.
- **`fork`, `clone`, this pi shared:** reject with `success: false` and a clear error. Not shared: forward.

Move sequence, on the client's connection, in order:

1. a `success: false` response (`"connection moved to another session"`) for each of this client's commands still in
   flight on the old pi;
2. the success response to the intercepted command;
3. the target pi's replay (below);
4. the target pi's live records.

Nothing from the old pi is delivered after step 2. pi.nvim runs its restore after every successful `switch_session`
and `new_session`, so it rebuilds the chat from whatever pi it now talks to. Limitation: the old pi's
`session_before_switch` hook doesn't run on a move, because that pi isn't switching.

Not interceptable: an extension's `switch_session` or pi's own `/new` and `/resume` handled inside pi. Piton doesn't
detect a resulting collision today; whether it can is open question 1 (#210).

#### Ending connections

| `piton attach` exit | When |
| --- | --- |
| 0 | its stdin closed (detach); within a second or two |
| 3 | this session's pi ended (terminated or crashed); pi's last stderr lines are written to stderr first |
| other non-zero | connection lost: the daemon stopped or restarted, the socket dropped, or the daemon isn't running (fails fast) |

pi.nvim reconnects on anything but 3 when it didn't ask to detach or terminate. A daemon shutdown or restart must
therefore never use 3, so pi.nvim reconnects and resumes the session from its file.

`piton stop --session <id>` stops that pi as described in "pi version" (stdin close, grace period, then signals; a
forced stop is reported on stderr). It exits 0 only once pi has stopped; every connection on that pi exits 3. Daemon
shutdown stops every pi the same way, in parallel.

#### Empty pis

When the last client leaves a pi that has no messages, no run, and no pending dialog, Piton stops it after about 10 s
(so a quick reconnect lands on it). This covers `:Pi` then closing the tab, a cancelled `:PiResume` in a fresh tab,
the pi left behind by a move, and the same cases from Swift clients.

### Multiple clients on one session

Pi has one stdin, so the daemon multiplexes:

- **Command ids:** each client's ids are rewritten to unique ids on the way to pi and restored on the way back, so
  responses reach only the client that asked. Events go to everyone.
- **Writer:** updated on every steering command from any client.
- **Dialogs:** Piton tracks every pending `extension_ui_request` (dialog methods) until a client answers, its timeout
  passes, or pi exits. Answers after the first are dropped by Piton (pi ignores them anyway). A dialog answered on
  another client stays open in pi.nvim until dismissed, since pi sends nothing when a dialog resolves.
- **Order:** each pi's stdout has one reader; every connection gets records in pi's order. Responses to other clients'
  commands are filtered out, never reordered.

#### Replay

Piton keeps the temporary state that neither `get_state` nor `get_entries` contains, and sends it to an `rpc` client as
pi's original records, unchanged, as the first records on the connector's stdout (or right after the success response
of a move), before any live record. Taking the snapshot and switching to live relaying is one step in the session's
task, so no live record is skipped or sent twice. Order:

1. `compaction_start`, if a compaction is running;
2. the in-progress assistant message: its `message_start` and `message_update` records so far;
3. tools still running: each `tool_execution_start` and its `tool_execution_update` records so far;
4. the latest `queue_update`;
5. pending dialogs, each `extension_ui_request` with its original `id`;
6. the latest `setStatus` and `setWidget` per key.

pi.nvim holds these back until its `get_entries` response and deduplicates them against the snapshot, so with Piton
there is no "response in progress" placeholder and no invisible dialog.

### Session view (Swift clients)

Swift clients don't get raw RPC. The daemon keeps a structured model of each live session (`cli/src/session/view.rs`) and
sends a snapshot plus numbered changes. It's built from pi's entries (`get_entries`, active branch) at launch and after
`switch_session`/`new_session`/`fork`/`clone` and successful compactions, and kept current from the event stream.

- **Items** (flat list, conversation order, each with a daemon-assigned `id`):
  - `user {content}`: pi's content (string or blocks).
  - `assistant {content, streaming, stop_reason, error, model}`: pi's content blocks (`text`, `thinking`, `toolCall`).
  - `tool {call_id, message, name, args, status, preview, truncated}`: one per tool call, `message` = the assistant
    item that made it. `status`: `pending`, `running`, `done`, `error`. `preview` is the output's text tail (40 lines,
    4 KB); the full result via `session.tool_output`. `truncated` is also set when the output has images.
  - `summary {kind: compaction|branch, text}`, `notice {level, text}` (retries, failures, aborts, extension errors),
    `other {role, message}` (roles the view doesn't model, passed through).
  - System messages aren't shown.
- **Images:** no image data travels in items, changes, or `session.tool_output`. Each pi image block, at any depth,
  becomes `{type: "image", image: <id>, mime_type}`, and `session.image` returns one image's data. A pasted screenshot
  is about 1 MB of base64 (pi keeps prompt images up to about 4.5 MB): inline, a few of them would make every snapshot
  that large. Ids are per session and live until the next reset. The apps accept messages up to 16 MiB; URLSession's
  default, 1 MiB, failed the whole connection on one large `session.image` reply (2026-11-10).
- **Changes:** `append {item}`, `replace {item}`, `delta {item, index, field: text|thinking, text}` (streaming),
  `state {state}`, `reset` (conversation replaced; a new snapshot follows).
- **State:** `status` (`idle`, `working`, `needs_input`, `stopping`), workspace, mode, session id/file/name, `title`
  (name, else the first line of the first user message), model, thinking level, queue (`steering`, `follow_up`),
  pending dialogs (pi's `extension_ui_request` records).
- **Window:** the newest `limit` items (default 50, max 200); `session.history` pages back.
- **Resume:** every change has a `seq`. `session.watch {from}` sends the missed changes, or a snapshot when they aren't
  kept (5000 per session) or a reset happened since.

The same model drives the session list's titles and status; notifications next.

### The `view` protocol

One JSON object per line after the hello `{"protocol":"view","version":1,"client":"<name>"}`. Requests carry an
`id` and get exactly one `{"type":"reply","id","ok":true,"data"}` or `{"type":"reply","id","ok":false,"error"}`.
Pushed lines have no id: `sessions {sessions}` (after `sessions.subscribe`, then on every change),
`snapshot {session, seq, items, has_more, state}`, `change {session, seq, change}`.

Requests are taken in the order they arrive: what one does to a session (a message queued for pi, a watch) happens
before the next is read, so a client's messages reach pi in the order it sent them. Replies come when each request's
outcome is known, so a slow one (a stop waiting for pi to exit) doesn't hold up the others; replies can therefore
arrive out of request order.

| Request | Data |
| --- | --- |
| `sessions.list` | `{sessions}`: live sessions (key, id, file, workspace, checkout, mode, status, clients, pid, title) |
| `sessions.subscribe` | `{}`; then pushes `sessions` and `daemon {state}` lines on change |
| `daemon.state` | `{state}`: `host_id`, `keep_awake` (enabled), `awake` (held now), `sessions` (live count), `logging` (`policy`, `default`), `settings` (each with `key`, `scope` (`host`, `client`, `project`), `value`, `placement` (`shared`, `local`, or null), `local_choice`, `value_type`, `applies` (`live`, or `restart` with `pending`), `editable`), `settings_error` |
| `settings.set {scope, key, value, local?}` | `{state}`; the key in the global file of `scope`; `value: null` resets; `local` moves the key between the shared and `.local` file (refused for `client`, which has no local file) |
| `daemon.logging {policy}` | `{state}`; until it expires or the daemon restarts |
| `workspaces.browse {path?}` | `{entries}`; see "Workspace roots" |
| `session.start {workspace, mode?, prompt?}` | `{session}`; mode defaults to `sandbox`; `workspace` must be a checkout under a root |
| `session.stop {session}` | `{forced}` |
| `session.watch {session, from?, limit?}` | `{}`; then a `snapshot` (or the changes since `from`) and every change |
| `session.unwatch {session}` | `{}` |
| `session.history {session, before, limit?}` | `{items, has_more}`: items before item id `before` |
| `session.tool_output {session, call_id}` | pi's full tool result, images as references |
| `session.image {session, image}` | `{mime_type, data}`: one image's base64 data, by the id in its reference |
| `session.send {session, message_id, text, mode?}` | pi's response data. Always pi's `prompt` with a `streamingBehavior`, so pi, not the daemon's view of its status, decides whether a run is active: `prompt` (default) and `follow_up` start a run when idle and queue a follow-up during one; `steer` steers a run. pi gets one prompt at a time (see "pi version"). `message_id` is the client's id for the message: a repeat with the same id reaches pi once and gets the first reply (see "Lost replies") |
| `session.abort {session}` | `{}` |
| `session.answer {session, request_id, answer}` | `{}`; `answer` has pi's `value`, `confirmed`, or `cancelled`. Refused when already answered |

A session's `checkout` is `{kind: "source"}` (the workspace is the project) or `{kind: "worker", source, name}` for a
worker `<repo>--<name>` of the project at `source`. The daemon decides it the way it does for the sandbox (the worker's
`agent.source` git config, accepted only for the `<repo>--<name>` layout next to `<repo>`), so clients never parse
folder names: a repository whose own name contains `--` stays a project. The apps group the session list by project:
projects ordered by their most urgent session, the project's own checkout first, then its workers by name. Cards share
one alignment and spacing, with `origin` or the worker slug at top left and status at top right; there are no separate
worker headers. Selection changes only the card border to cyan, without the native Mac row highlight.

#### Lost replies

A connection can drop after the daemon passed a message to pi but before the reply arrived (a phone losing signal),
so the client can't tell whether it was delivered. The apps keep every message in an outbox until it's confirmed and,
after reconnecting, send it again with the same `message_id`. The session remembers its last 256 message ids: a repeat
of one pi is still answering waits for that answer, a repeat of an accepted one gets the same reply without reaching
pi again, and a refused one is forgotten so a retry tries again. Abort and dialog answers need no id: aborting twice
is harmless and a second answer is already refused.

A view client counts as a session client (writer rule, empty-pi cleanup). Not yet: saved sessions in `sessions.list`,
`session.open`, workspaces, devices/presence, images in `send`.

### Remote access (tailnet WebSocket)

The `view` protocol also runs over WebSocket on this Mac's tailnet addresses (`[tailnet] port`, default 7357), one
text frame per message, plain `ws://` (Tailscale encrypts the traffic). `cli/src/apps/tailnet.rs`,
`cli/src/apps/websocket.rs`.

- **Client transport:** the apps use the system proxy settings, as any app does (diagnostics log which proxy iOS
  picks; see `docs/logging.md`). Traffic still uses Tailscale's VPN routing/encryption; it isn't ordinary public web
  traffic. Every terminal path
  invalidates its URLSession (failure, deadline, cancellation, remote close, or explicit close), releasing the delegate
  before another reconnect can accumulate retained sessions.
- **Who gets in:** before the WebSocket handshake, `tailscale whois --json <peer>` identifies the device. Accepted only
  when it belongs to the same Tailscale user as this Mac, isn't tagged, isn't shared in from another tailnet, and
  isn't this Mac itself. Refusals are logged with the reason; the peer sees the connection close.
- **Why not this Mac:** sandboxed agents can open network connections, and through the tailnet address they'd look
  like the owner. Local clients use the Unix socket (`0600`; the sandbox denies connecting to it). Never add a localhost or
  token path to the WebSocket: the identity check is the only way in.
- **Only `view`:** an `rpc` or `control` hello over the WebSocket is closed (no remote `piton attach`).
- **Dead connections:** a phone that loses its network sends no close, and its TCP connection would linger for many
  minutes. The daemon pings every client (`[tailnet] heartbeat_ms`, default 10 s) and drops one that answers none of
  three pings in a row. The apps ping too (every 15 s) and reconnect when a pong doesn't arrive within 10 s.
- **Unnoticed drops:** a connection can die while the phone is suspended in the background or when its network changes,
  and through a local proxy the phone's side stays open: writes seem to succeed and requests wait for the next pong
  timeout (up to 25 s). So the iPhone app checks the connection when it becomes active and when the network path
  changes: a ping with a 2 s deadline, reconnecting at once when it isn't answered. A store waiting to retry retries
  immediately.
- Without Tailscale (not installed, not running) the daemon logs it once and serves local clients only, trying again
  every `[tailnet] retry_ms` (default 10 s): at login launchd can start it before Tailscale is up. Once listening, it
  checks every minute whether this Mac's tailnet addresses changed and listens on the new ones.
- Tests use a fake `tailscale` CLI (`cli/tests/fixtures/fake-tailscale.sh`) that puts the listener on loopback.
- macOS firewalls (Little Snitch) hold incoming connections until allowed. `mise run cli:install` signs
  `~/.cargo/bin/piton` with the team's Apple Development certificate (identifier `com.alex35mil.piton.daemon`), so its
  code identity stays the same across rebuilds and a rule for that path keeps matching. Allow incoming TCP on port
  7357 from the tailnet ranges (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`); the identity check (Tailscale `whois`) is
  Piton's own, the firewall rule only lets the connection reach it. Debug builds (`cli/target/debug/piton`, tests)
  aren't signed and don't need the rule (tests listen on loopback).

### Protocols

Both run over the Unix socket (local, `0600`); `view` also over the tailnet WebSocket (see "Remote access"). Framing
is one JSON object per message (JSONL on the socket, one per frame on the WebSocket).

Every connection starts with a hello declaring its protocol:

```json
{"type": "hello", "protocol": "rpc", "version": 1, "workspace": "/abs/path", "client": {"name": "nvim"}}
{"type": "hello", "protocol": "view", "version": 1, "client": {"name": "iPhone", "device": "…"}}
```

- **`rpc`:** after the hello, raw pi RPC for one session. Used by `piton attach`. Possible later over the tailnet.
- **`view`:** Piton's control and view protocol. Draft operations:
  - `sessions.list` (live and saved, with status), `sessions.subscribe` (list changes);
  - `session.start {workspace}`, `session.open {sessionPath}` (attach if live, else launch pi on it),
    `session.stop {id}`;
  - `session.watch {id, from?}` → snapshot + changes, `session.unwatch`, `session.history {id, before, limit}`;
  - `session.send {id, message_id, text, images?, mode: prompt|steer|follow_up}`, `session.abort {id}`,
    `session.answer {id, requestId, value|confirmed|cancelled}`;
  - `workspaces.browse {path?}`, `workspaces.recent`, `workspaces.search {query}`, `devices.register {apnsToken}`, `presence {id?, foreground}`.

  Commands map to pi RPC commands, so pi sees the same thing whichever client sent it.

Versioned from the start; schemas are defined in Rust and mirrored by hand in Swift. The daemon test `fixtures` writes
real protocol messages that PitonKit must decode, and fails when they're stale, so drift between the two shows up in
tests (see `docs/development.md`).

### Notifications

Attention is derived from the event stream:

| Trigger | Notification |
| --- | --- |
| `agent_settled` | finished, with the first line of the last reply |
| dialog `extension_ui_request` | needs input, with the dialog title |
| run failure, retries exhausted | failed |
| pi exited unexpectedly | exited |

- Channels are pluggable: APNs first (direct from the Mac, no relay; needs an Apple developer account), Telegram
  optional.
- iOS notifications offer an inline reply that sends a follow-up or answers a dialog without opening the app.
- A notification is skipped when the session has an active client: a `piton attach` connection that sent recently, or a
  Swift client in the foreground on that session.

### Keep-awake

- While keep-awake is on and any session is live (working or idle), the daemon holds a prevent-idle-sleep power
  assertion (`caffeinate -i`), so the phone can reach sessions it might want to steer; released when the last
  session ends. Idle sessions count: a Mac that slept with only idle sessions made the phone lose access
  (2026-11-08). The display may still sleep and the screen lock; the daemon and Tailscale keep running.
- On/off: `keep_awake` in `config.host.toml` (default on), or `config.host.local.toml` to keep frequent toggles out
  of a dotfiles diff; the apps toggle it (`settings.set` on `host` `keep_awake`, the macOS menu bar item and Settings >
  General on both platforms), and a hand edit applies at once.
- Lid closed on battery without an external display sleeps regardless. For that, `piton-sleepd` toggles
  `pmset disablesleep` while agents work: it accepts XPC only from the signed daemon, restores normal sleep when the
  connection drops, and enforces a maximum duration and a low-battery/thermal cutoff.
- The helper needs the app bundle; the bare daemon gets only the assertion.

### Logging and agent access

The Info level records essential events; opt-in Debug adds detailed traces and expires after an hour by default unless
explicitly kept on. The apps retain bounded files alongside Apple's unified logger. Host logging (the
Mac app, the daemon, and a separate paired-iPhone collector) follows the daemon: `logging` in `config.host.toml`
is the default, and `daemon.logging` (the Mac app) or `piton logging` switches it until it expires or the daemon restarts.
The policy is part of the pushed daemon state. The phone's capture setting is a device setting, local
to the phone, and works offline. No logging control changes pi's settings, uses its RPC channel, or restarts sessions.

Settings ownership: each setting lives in exactly one place; see "Configuration and state" below.

Settings UI: one view per section, shared by both apps; only the container differs. The Mac has a Settings window (⌘,,
also in the menu bar item) with a sidebar: General (keep-awake, daemon version), Appearance, Logging (host level). It
follows System Settings' geometry: a fixed width (715 pt, sidebar 215 pt) and resizable height, and Esc closes it, after
anything inside that uses Esc first (a search field, an open picker). The phone has a Settings tab next to Sessions:
General, Connection (the Mac's address), Appearance, Logging (the phone's capture). Before an address is set, the phone
opens on Connection. The tab bar shows only at the top level, not inside a conversation. The iPad, when its window is
wide enough for two columns, has no tab bar: one split view whose sidebar switches between the sessions and the
settings sections (the phone's) with a Sessions | Settings switcher at its bottom, and whose content is the selected
session or section. Each keeps its selection while the other is shown, and the session stays watched. A narrow iPad
window (Slide Over, a narrow Split View) gets the phone's tabs. Settings use the system's
type, and each row carries its description under its title, as System Settings does, instead of section footers.

Conversation: an assistant message shows its text, never its thinking. While the newest block of a streaming message
is thinking, an `ellipsis.bubble` in the dim color sits next to the model name; while it streams text or a tool call,
a spinner does; nothing once it finishes. The composer is a multi-line field: on a hardware keyboard (Mac, iPad),
Return sends (steers while the session works) and Shift+Return or Option+Return breaks the line; the on-screen
keyboard's Return breaks the line and the send button sends. Images show as thumbnails under a count
(`photo.on.rectangle.angled`, "N images"); pi's notes for the model about resizing or converting an image (`[Image:
original WxH, displayed at …]`) are hidden. A clicked image opens at its natural size in points (pixels over the
screen's scale), fitted to 80% of the screen's visible area on the Mac and to the whole screen on the phone. Both show
it the same way: one viewer over a dimmed backdrop, holding the message's images, with a counter ("2 / 3") and a close
button on top. On the Mac the viewer covers the whole screen in a borderless window above everything, so stepping
swaps the image in place; the image is centered across and a little above center vertically, but never higher than a
full-size (80%) image, which sits exactly centered. ← and →, or the arrow buttons at the screen's edges (shown for more
than one image), step through the images; Esc, the close button, or a click
outside the image closes. On the phone the viewer covers the app: swipe across to page, pinch or double-tap to zoom,
tap to hide or show the controls (counter, close button, status bar), swipe down to close, as in Photos: the image follows
the finger and shrinks a little while the backdrop and controls fade in place; released far or fast enough, they vanish
where they are and the image carries on off the bottom. Images are shown as pi stored them: pi shrinks an image
larger than 2000 × 2000 px or 4.5 MB (base64) before the model sees it and keeps only that copy (`images.autoResize`,
a model's `inputLimits.images.resize`), so a Retina full-screen screenshot opens at about 40% of the Mac's screen.

Look: one built-in theme (`PitonUI/Sources/Shared/Theme.swift`), no theme switch. A dark gray canvas (#272B30) with a faint grid, panels a step lighter (#2D3238; #34393F for the user's messages and
dialogs) with
hairline borders and chamfered corners, neon cyan as the only accent (green for done, red for errors and stopping,
yellow for needs-input and host mode), and three type voices: small uppercase letter-spaced labels and data in the code
font, content in the text font. Views take colors, fonts, and shapes only from the theme. The text font as default and
the roomier reading line spacing apply to the session list and conversations, not to Settings. Liquid Glass stays on
system chrome (bars, alerts, menus): iOS 27 has no opt-out.

Fonts are client preferences (Settings > Appearance): a text font and a code font, each the system font or any family
available to the app, scaled with Dynamic Type. Each is shared through the host (`config.client.toml`,
the same on every device, cached for offline start) unless Local keeps a value on this device; a shared font a
device lacks shows as the system font there. Settings > Host shows every host setting with where it comes from. The Mac lists its installed fonts. On the phone, fonts the user owns
(a paid one like Berkeley Mono can't be bundled) are imported from .otf/.ttf files: copied into the app's Application
Support and registered for the process at launch. Fonts installed system-wide through configuration-profile apps need
the "Use Installed Fonts" entitlement, which the app doesn't have.

The independent collector uses Apple's paired-device file service, not Piton's WebSocket: it can retrieve logs of a
failed Piton connection when CoreDevice can still reach the phone. At Info it makes no device calls. It
preserves the last good snapshot with explicit freshness metadata, and agents get read-only access to the log-only
directory, never the control socket or device tooling. See [logging.md](logging.md) for limits and setup.

### Configuration and state

Status: implemented in #206. Earlier layouts were converted once with a migration command that has since been
removed; files they left behind are simply not read.

**Hosts and clients.** A host runs the daemon: it runs pi, holds the projects, and owns the state database. A client
is an app connected to a host (the Mac app, the iPhone and iPad apps). A Mac can be a host, a client of another host,
or both; the Mac app asks on first launch and installs the daemon and the sleep helper only on a host. Each host has
a generated `host_id`, sent in `daemon.state` (pushed to every subscribed client).

Four stores, each with one job; every value has exactly one home.

| Store | Where | Owns | Written by |
| --- | --- | --- | --- |
| Config files | `.piton/` and `.agents/` in the repository and its parent folders; the global tier `$XDG_CONFIG_HOME/piton/` | what you decide, preferences included; travels through dotfiles and repositories | the user; the daemon for app edits to the global files and for approved protected writes |
| State database | `~/.local/state/piton/piton.db` | facts about this host and what happened on it: `host_id`; later sessions, drafts, recent projects. No preferences | the daemon |
| Client storage | each app's container | hosts it connects to, a device's own preference values, imported fonts, log capture, unsent work | that app |
| Memory | the daemon | live pis, session views, temporary switches, pending protected writes | the daemon |

Piton never touches pi's own files (`<agent-dir>`: settings, auth, sessions, trust).

#### Config files

```
$XDG_CONFIG_HOME/piton/          # global tier (default ~/.config/piton/)
  config[.local].toml            # the chain's farthest level: defaults every project inherits
  config.host[.local].toml       # how the daemon runs on this host, this host's preferences, [development]
  config.client.toml             # the apps' preferences, served to every client (no local file)
  mcp[.local].json               # MCP servers, in the format other harnesses read
  policies.md
  agents/rules/, agents/skills/  # global agent instructions
<ancestor>/.piton/, .agents/     # shared by every repository below
<repo>/
  .piton/
    config.toml                  # committed: [sandbox] [workers] [permissions] [branches] [tracker] [forge]
    config.local.toml            # not committed: the same tables, plus [branches] username and [web_search] api_key
    mcp[.local].json, policies.md
    .local/                      # workflow artifacts (gitignored); a worker links its source's
  .agents/rules/, skills/
  AGENTS.md
```

Three families of files, each read by its own consumer:

- **`config.toml` / `config.local.toml`, every level:** a project's settings, read along the chain by launches, pi,
  and workflows. The global pair is the chain's farthest level, appended from its fixed location. `.local` means "not
  committed": this machine's values, secrets, or anything you don't want in a diff (`/.piton/*.local.*` keeps it out
  of Git).
- **`config.host.toml` / `config.host.local.toml`, global only:** read by the daemon. Top-level keys: `socket`, `log`,
  `roots`, `favorites`, `logging`, `default_attach_mode`, `empty_grace_ms`, `keep_awake`; tables `[pi]`, `[env]`, `[stop]`, `[tailnet]`, and `[development]` (switches for a
  development install, in [development.md](development.md)), which the pi package reads through
  `piton config resolve --global` on whichever machine pi runs.
- **`config.client.toml`, global only:** the apps' preferences (`text_font`, `code_font`; themes and keybindings
  later), served by the host to every client. It has no local file: a device keeps its own value in its app.

Keys are snake_case. A project file's tables are each parsed on their own, so a bad value in `[tracker]` never stops
permission checks; a syntax error, or an unknown top-level key, fails every table of that file, and the message names
the line. A host or client file has one schema and is read whole.

- **Chains.** `.piton/` is read from the source checkout (a worker's source; the checkout around a subfolder), then
  each ancestor strictly below the home folder, then the global tier. `.agents/` starts from the workspace's own
  checkout, as pi does, so instructions apply where they're edited, and ends at the global tier's `agents/`. The home
  folder itself is not a level. A level that exists but can't be read is an error. `.piton/.local/` is never
  configuration.
- **Locating the global tier** can't depend on the workspace, whose `mise.toml` can set `HOME` or `XDG_CONFIG_HOME`.
  The daemon resolves it once from its login-shell environment and sets `PITON_CONFIG_HOME` for every pi after the
  mise environment; a pi started outside Piton falls back to `XDG_CONFIG_HOME`, then `~/.config`.
- **Where a key may be set** is fixed by Piton, declared next to each table's type, and enforced by the reader (a
  file that breaks it fails that table, naming the key):
  - `sandbox.expose` (lifting a built-in credential deny) and the `mise` preset's `install = "allow"` (writing the
    tools the host runs): the global `config.toml` only, so a repository can't open them to itself.
  - `[tracker]`, `[forge]`: a project or parent folder only, never the global tier, where they'd become every
    repository's.
  - `web_search.api_key`: a `config.local.toml` only (a secret).
  - `branches.username`: a project's `config.local.toml`, where `config.toml` is committed; either file in the global
    tier, which is yours alone.
  - Everything else, keep-awake included: either file, any level.
- **Merging,** farthest to nearest; within a level the shared file, then the `.local` one.
  - `[sandbox]`: lists (`presets`, `allow`, `allow_file`, `deny`, `expose`) only add; a preset named at several
    levels takes each option from the nearest level that sets it. The daemon's `launch/sandbox`
    module, the only code that knows the backend, resolves the merged table with the built-in policy into each
    launch's rules ("Agent access").
  - `[permissions]`, then the runtime's `[permissions.host]` or `[permissions.sandbox]`, per file: rules union, deny
    and ask beat allow; `default_mode` and `skill_grants` from the nearest table that sets them.
  - `mcp.json`: the nearer entry replaces the whole entry of the same server.
  - `[branches]`, `[web_search]`, the host file's keys: the nearer value per key.
  - `[tracker]` and `[forge]`: each the nearest table, whole. Both are optional; `provider = "none"` turns one off,
    including one a farther level sets (the pi package reads it).
  - `policies.md` and rules: combined, farthest first.
- **Writes.** The apps and `piton config set` change keys with `toml_edit`, keeping comments and layout
  (`--global` for the global `config.toml`, `--scope host|client` for the host and client files). The pi package
  writes through `piton config set --dry-run`, which returns the file's new text; a protected file then goes through
  the daemon's confirmation. Nothing else writes these files.
- Paths in every file accept `~` and `$HOME`.
- **When read.** The global files at daemon start and on every change (watched; keys that need a restart are
  reported as pending); the project chain at every launch and `worker create`; the pi package's files at pi start and
  `/reload`. `piton config check` validates the workspace's chain and the host and client files, lists the files
  read, and warns about what doesn't travel (below); `piton config get` serves one resolved part to other tools (the
  Neovim plugin's queue reads `tracker`, `forge`, and `mcp`).
- **One resolver.** The daemon crate's `config` module is the only implementation (layout below). The pi package and
  the Neovim plugin call `piton config resolve` / `piton config get`, which read files and don't need the daemon
  running. The daemon passes the binary's path in `PITON_BIN` after the mise environment; the pi package caches the
  result and calls again only when a watched file changed. The result is per part: one per table of `config.toml`,
  named as the table, plus MCP, the global tier's MCP alone, policies, and rule and skill folders. Each is its value or
  its error, so a broken table fails only the tools that read it. `--global` returns the host file's `[development]`.
- **Code layout.** `cli/src/config/` is the only code that reads or writes these files. Its `mod.rs` is the API;
  the four layers are private:
  - `model/`: what configuration is, with no I/O: paths and scopes, levels and chains, one file per project table
    (its schema, where its keys may be set, checks, merge rule), the host and client files, and the daemon's options
    with defaults.
  - `read/`: one file into its parts, a chain into one value or error per part, the global tier's own files.
  - `write/`: key edits that keep comments and layout, and the one write path (which file, the change as a dotted key
    and a JSON value, checked before writing) for `piton config set` and the apps.
  - `check/`: read-only diagnostics such as the portability warnings.

  Outside it, and using its API: `commands/config.rs` (the `piton config` command), `apps/settings.rs` (the apps' settings
  list), `daemon/` (reloading the global files: the daemon's options from the host file alone, so a broken client
  file stops only the apps' list), `launch/sandbox/` (`[sandbox]` into a launch's rules), and `state/` (the state
  database).

#### Config and state together

- **Config or state.** Config is what you decide, preferences included: roots, favorites, appearance, keep-awake,
  the logging default, sandbox grants, permissions. State is facts and history on this host: its identity, and later
  sessions, drafts, recent projects. The database holds no preferences.
- **Shared unless Local.** Each preference has one Local checkbox where its file has a local one. Unchecked, a host
  preference lives in `config.host.toml` (every host through dotfiles); checked, in `config.host.local.toml` (this
  host). A client preference unchecked lives in `config.client.toml`, served to every client (which caches it for
  offline start); checked, in that app (this device). A shared client choice a client can't honor (a font it lacks)
  falls back there only.
- **The apps' settings list** (`daemon.state` `settings`) shows global config keys, each with `key` as written in its
  file, `scope` (`host`, `client`, or `project` for the global `config.toml`), `placement` (`shared`, `local`, or
  unset), whether its scope has a local file, its type, whether it applies live or after a restart, and whether the
  apps may change it. `settings.set {scope, key, value, local?}` writes it. The daemon edits with `toml_edit`, keeping
  comments and order, writes through a dotfiles symlink, and rereads before writing so a hand edit isn't lost. The
  apps edit the host file's keys (`socket` shown read-only), the client file, and the global `[sandbox]` and
  `[workers]`; `[development]`, `[permissions]`, `[branches]`, `[web_search]`, `[tracker]`, and `[forge]` are edited
  in the file. The apps never write repository or parent-folder files. A change is checked against the schema before
  it's written. The daemon watches the global folder (and a symlink's target folder): edits apply at once; `log`,
  `[env]`, and `[tailnet]` wait for a restart and show as pending; a file that can't be used keeps its last good
  values and `settings_error` says why.

#### State database

SQLite (`rusqlite`, bundled), WAL, owned by the daemon alone, in its `0700` state folder. Tables, each added by the
work that needs it: `host` (`host_id`); later `projects` (canonical source path, last used); `sessions` (Piton's
session identity and its state `draft`, `creating`, `live`, `failed`, title, pi session file); `drafts` and
`draft_attachments` (size-bounded). Rows decode into Rust sum types at the boundary.

- `PRAGMA user_version` is the schema version; ordered migrations run at start in one transaction, after a
  `VACUUM INTO` backup of the previous version. A database newer than the binary refuses to start.
- Deleting it resets this Mac's state; configuration is unaffected.
- `piton state export <file>` and `piton state import <file>` move it to a replacement Mac (Migration Assistant
  copying `~/.local/state/piton/` does the same), keeping `host_id` so clients see the same host. Import refuses over a
  database that already has sessions unless forced. Sessions reopen where pi's own session files were copied too.

#### Client storage

The hosts it connects to and local client preferences in `UserDefaults`; the cached shared client preferences;
imported fonts in Application Support; the phone's log capture in its `settings.json`; durable unsent work (outbox,
local drafts) as versioned Codable files in Application Support, moved aside when they don't decode.

#### A new or second Mac

A host: clone the dotfiles (the global tier) and repositories, then run `piton setup` (the Mac app runs it on first
launch). It's idempotent: it checks `pi`, `mise`, and `tailscale`; validates the global configuration and
that its `.local` files stay out of Git; installs the `piton` command and creates the database; and reports the
tailnet address for clients. Registering the daemon to run at login is the Mac app's step. A client needs only a host's address. What doesn't travel is by design: this host's
state (a replacement Mac imports it, above), `.local` files (secrets, per-host values, local preferences; recreated
by hand), and workflow artifacts, which resume from the tracker. `piton config check` warns about absolute paths
under another user's home and secrets in tracked files.

Several hosts in the clients are a later follow-up: a list of hosts keyed by `host_id`, with either a switch or one
session list grouped by host. Hosts stay independent; nothing syncs between them.

#### Agent access

The sandbox follows what Claude Code, Codex, and Gemini CLI converged on: read broadly, write narrowly, deny
credentials. Unlike them, Piton sandboxes the whole pi process, not each shell command: pi's file tools, every
extension and pi package, local MCP servers, and whatever they start are inside one OS boundary, where per-command
sandboxing would leave them all with the user's access behind application-level checks (OpenCode's experimental
whole-agent sandbox still trusts its plugins and MCP servers). The cost: pi's own credentials are readable inside,
and the network can't be closed without cutting pi off from its model provider.

Each sandboxed launch gets an ordered list of rules (`launch/sandbox/`), the last matching one deciding, as Seatbelt
does; `piton sandbox show` prints them and `piton sandbox why` names the one that decides an access:

1. **Base:** everything is readable, nothing is writable, the network is open (pi has to reach its model provider;
   fetch, MCP, and installs need it too).
2. **Writes:** the workspace, `<source>/.piton/.local/`, the temp folders and `/dev`, the `presets`, `allow`, and
   `allow_file`. Not user caches (`~/.cache`, `~/Library/Caches`): host tools reuse what's in them without checking
   (the Go build cache is linked straight into host builds), so a writable cache would run sandbox-written code on
   the host. Each preset (`mise`, `rust`, `node`, `swift`) grants one toolchain's caches and state as an informed
   opt-in, documented as such. A preset is a name or `{ name = "...", <options> }`; options are typed per preset.
   With its default options no preset writes what the host executes or trusts (a unit test lists those paths):
   `mise` writes its cache and install bookkeeping, never its installed tools or trusted configs. Its `install`
   option is `"ask"` by default (installs approved per command and run on the host, through #241's confirmation gate;
   until then they fail in the sandbox), `"deny"`, or `"allow"` (global only: the sandbox writes the installed tools).
3. **Credentials:** nono's deny groups (`deny_credentials`, keychains, browser data, macOS private data, shell
   history and rc files), vendored verbatim in `nono-deny-groups.json` at the `nono` version; `mise run
   cli:nono-policy-check` reports upstream changes, and a test fails when the crate is bumped without re-vendoring.
   Read, write, exec, and Unix connect are denied; metadata stays visible. `expose` (global only) lifts one, readable
   then, writable with `allow`; exposing `~/Library/Keychains` also reopens the keychain Mach services the library
   always closes.
4. **pi's agent directory:** read-only, except its runtime state (`sessions/`, `auth.json`, `models-store.json`,
   `mcp-auth.json`, `crashes.json`, `mcp.log` and its rotation, their locks, and MCP's per-server token-refresh
   locks: credentials, catalogs, and logs). Its settings, project trust (`trust.json`), MCP servers, extensions,
   skills, prompts, themes, managed binaries, and packages decide what a later pi loads, host sessions included.
   pi locks `settings.json` and `trust.json` even to read them, so their locks (`<file>.lock/`) are writable; the
   files aren't. `docs/pi-findings.md` lists every write pi makes there, from its source. A workspace's `mise.toml` can
   set `PI_CODING_AGENT_DIR`, so the launch is refused when it's the home folder or above it, inside the host's own
   agent directory (the daemon's environment names it) or a checkout's `.pi/`, or overlaps a configuration level,
   the state folder, or a denied path. The host's agent directory stays closed whichever one the launch uses.
5. **The user's `deny`.**
6. **Configuration:** every level of both chains, and what their entries link to (a dotfiles repository), is
   write-denied; protected files (below) too. The workspace's own `.agents/` stays writable.
7. **The daemon:** its state folder is closed (read, write, connect) except `logs/` for reading and `agent.sock` for
   connecting; the control socket (`piton.sock`, or the configured `socket`) is denied to connect wherever it is.
   With the network open, Seatbelt would otherwise let a sandboxed agent connect to it and act as a client:
   approve its own protected writes, start host sessions.

**Symlinks.** Seatbelt compares real paths, so each rule also gets its real-path form (`/var` is `/private/var`). A
deny follows every symlink: it closes what a link points to as well. A grant follows only links no sandboxed session
of the project could have made, since one it made would move the next launch's grant wherever it pointed (pi's
`crashes.json` replaced with a link to `~/.zshrc`). A grant never follows its own last component, and never a link
inside the launch's checkout or source checkout (a session in the whole checkout writes there, even when this
launch's workspace is a subfolder) or covered by another write grant, as written or resolved, computed to a fixed
point. Below a link it doesn't follow, it keeps the rest as written, which matches nothing while the link is there.
The daemon logs each such grant and `piton sandbox show` warns about it; a user who links a granted folder elsewhere
grants the real path. A link outside the checkout made by a session elsewhere whose `allow` was wider is still
followed when this launch's grants don't cover it. A sandboxed launch is refused when the workspace path given goes
through a symlink inside a git checkout that leads out of it; the user's own links outside any checkout work.

`PITON_RUNTIME`, `PITON_CONFIG_HOME`, and `PITON_SESSION_TOKEN` are set by the daemon after the mise environment.

Known limits: the network isn't restricted, so anything readable can leave (domain filtering:
[#228](https://github.com/alex35mil/Piton/issues/228)); pi's own credentials are readable
([#229](https://github.com/alex35mil/Piton/issues/229)); macOS only.

**Protected files** are everything a later pi loads from the workspace that could widen what an agent may do without
asking, or run code outside the sandbox: the source checkout's `.piton/` (except `.local/`); pi's project `.pi/`
folders (settings, MCP servers, extensions, skills, system prompts), which pi reads from the working folder. `.pi/` is
protected at any depth in the workspace's checkout, since a session can start in a subfolder. `AGENTS.md`, rules, and
`.agents/skills/` aren't protected: they change what the agent is told, not what it may do. A skill's `allowed-tools`
grants nothing unless the user opts in with `skill_grants = true` in `[permissions]`; with it on, an agent that edits a
skill can grant itself the tools it lists. Without protection, a sandboxed agent in the source checkout could add `allow = ["bash(*)"]` to
`[permissions.host]` in `.piton/config.toml`, and a host session in the same checkout would run any command without asking from its
next tool call. Changes are gated where they're written:

1. **OS sandbox.** Each sandboxed launch carries raw Seatbelt rules: `(deny file-write* ...)` with regexes
   for `<source>/.piton` (excluding `<source>/.piton/.local`), and `.pi` at any depth in the checkout (excluding
   the `.pi/settings.json.lock` folder itself, which pi creates to read the project's settings). Every
   protected name matches in any case (`.PI`, `.PITON`), since APFS is case-insensitive by
   default; the daemon's own path check compares the same way. So every write by any process in the sandbox fails:
   shell commands, scripts, `git checkout`, MCP servers, pi itself. The library's typed grants can't express a
   read-only folder inside a writable one; its platform rules can, and it emits them after its allows so the deny
   wins. `agent/protected.rs` is the one definition; `piton sandbox show` prints the rules, and
   `cli/tests/probe/protected.sh` checks them on the host (writes, creates, deletes, renames, hard links, symlinks,
   other cases, and the rest of the policy).
2. **Writes through the daemon.** `write`, `edit`, and project setup write protected files through one operation in
   the pi package. In every session the daemon launched, sandboxed or on the host, it sends the path and contents
   over `~/.local/state/piton/agent.sock` (identified by the session token); the daemon checks the path, shows the confirmation with the diff in the apps
   and in pi.nvim (as an extension dialog it inserts), and writes only on approval. The agent can request but never
   approve: answers come only from clients. One pending request per session; it ends when the agent stops waiting or
   its run ends. Every protected write asks. The dialog carries the file's current content, so the
   apps show a diff without reading the file. On approval the daemon walks the path from the checkout without
   following any symlink, so a folder the agent swapped for a link while the user decided can't redirect the write.
   Only a pi running outside Piton (no session token) has the permissions extension ask itself and write directly,
   whatever `defaultMode` or auto-accept says.

A rejected change never reaches disk. Limits: shell writes in host sessions aren't gated (as in Claude Code without
its sandbox); the rule is macOS-only. Codex and Claude Code don't need step 2 because they sandbox each shell command and run their
edit tools unsandboxed; Piton sandboxes the whole pi process.

**Sandbox backend.** The `nono` Rust crate (Apache-2.0), linked into `piton`: the daemon starts a sandboxed pi as
`<piton> sandbox-exec -- pi ...`, which applies the policy with `Sandbox::apply_auto` and execs pi. There's no nono
CLI, PATH lookup, or profile file. Everything is readable through one typed grant of `/`; every other rule is a
platform rule. The library has no policy of its own (groups and profiles are nono's CLI's); Piton vendors only the
credential deny groups, since broad reads make the ~120 system-path allow entries unnecessary. A typed write-deny
with nested re-allow is requested upstream ([nono#2310](https://github.com/nolabs-ai/nono/issues/2310)). A custom
Seatbelt launcher is rejected: keeping the credential lists correct across macOS releases is security work nono
shares.

### Deploying from a sandbox

Status: planned. Today installs are host-only (`mise run cli:install`, `apps:mac`, `apps:iphone`, `apps:ipad`).

An agent in a sandboxed session may deploy what it built: the daemon, the Mac app, or the iOS app on the iPhone or
iPad. This doesn't widen what the agent can do: the code it wrote runs on the host once deployed, whoever runs the
deploy, so a person approving each one adds no safety. What deploying must not do is disrupt work in progress, and
that's what gates it: the daemon decides when a deploy may take effect, and an agent can't skip that check.

The sandbox keeps every restriction it has. A deploy is a request, and the host does the work:

- **Deploy socket.** The daemon listens on a second Unix socket (`~/.local/state/piton/deploy.sock`, `0600`), the
  only Piton socket the sandbox allows besides `agent.sock`. It speaks one request, `{deploy: daemon | mac | iphone | ipad,
  checkout}`, and nothing else: no session control, no host-mode launches, no paths or arguments to run. The control
  socket stays out of the sandbox.
- **Checkout.** The daemon builds from the checkout the request names, accepted only when it's a source or a worker
  that resolves under a known project (as for launches, "Workers"). It runs the existing `mise` task there, on the
  host, with the user's environment: signing, `/Applications`, `launchctl`, and `devicectl` happen in the daemon's
  context.
- **One at a time.** Deploys run in order: they share build folders and devices. Build output goes to
  `logs/deploy/`, which the sandbox can read.
- **CLI.** `piton deploy daemon|mac|iphone|ipad [--from <checkout>]` sends the request (over the deploy socket in a
  sandbox, over the control socket on the host) and prints the outcome; `piton deploy status` shows a pending one.
  Refusals are typed and exit 4, like `piton worker`.

#### Gates

A deploy builds and installs at once when that disrupts nothing, and otherwise waits for a safe moment or is refused,
per target:

- **Daemon.** Replacing the daemon restarts it, and a restart ends every pi (see "Deferred": sessions don't survive
  restarts; they continue from their files, and an in-progress turn is lost). So the daemon is swapped only when no
  session is busy: `working` (a run or a compaction), `needs_input` (a pending dialog), or `stopping`. Idle sessions
  don't hold it back; their clients reconnect (pi.nvim reattaches, the apps resume from their sessions' files).
  - The request builds and signs the new binary right away and stages it next to the installed one. A build failure
    is reported and nothing changes.
  - The swap then waits until every session is idle. That includes the requesting agent's own session: its turn ends
    before the restart, so the agent never cuts itself off mid-turn. The reply says the daemon is staged and which
    sessions it waits for.
  - While a swap waits, the daemon state carries it (`deploy: {target, from, waiting_for: [sessions]}`), the apps
    show it, and the user can restart now (Mac app, or `piton deploy daemon --now` on the host). `--now` isn't
    accepted on the deploy socket. A newer request replaces a staged one.
  - Swap: the old daemon stops every pi as on shutdown, moves the staged binary into place, keeps the previous one,
    and asks launchd to restart it. Before exiting it starts the previous binary as a watchdog: if the new daemon
    doesn't answer on its socket within 30 s, the watchdog restores the previous binary and restarts it, and the
    failure is logged. A broken daemon can't be fixed by a deploy through itself, so it must never stay installed.
- **Mac app.** Replacing it quits and relaunches Piton.app. That ends no session: the app reconnects and resumes
  every watched session from its last `seq`. Not gated by sessions. Gated by the app's outbox: a message not yet
  confirmed lives only in the app's memory, so the app is replaced only when its outbox is empty (it reports this in
  its hello and on change; an app that doesn't report it is replaced at once).
- **iPhone, iPad.** Installing relaunches the app on the device, which ends nothing on the Mac. The device must be
  paired and reachable; otherwise the request is refused (`device_unavailable`). Gated like the Mac app when that
  device is connected to the daemon.

The gates apply to every deploy, the user's included: on the host, `mise run cli:install` goes through the same
request while a daemon is running (and installs directly when none is). `--now` is the user's override, never an
agent's.

## Scope

### v1

1. Daemon: launching pi with the mise environment, registry, Unix socket, `piton attach` relay with its exit codes, id
   multiplexing, command interception and the move sequence, replay, empty-pi cleanup, writer, `piton stop`, `piton ls`.
2. Keep-awake assertion.
3. Session view model and the `view` protocol over the tailnet WebSocket.
4. Piton.app: session list, session view, send/steer/follow-up/abort, dialogs, start/open sessions.
5. APNs notifications with inline reply.
6. LaunchAgent registration, sleep helper.
7. Deploying from a sandbox, with its gates.

Order: 1–2 first, verified with a scripted client against real pi and then with pi.nvim; 3–6 after. Progress is
tracked in the tracker, not here.

### Deferred

- Sessions surviving daemon restarts (a shim process per session owning pi's stdio). v1 keeps per-session state
  self-contained so moving it into a shim is mechanical. Until then, restarting Piton ends all sessions; they continue
  from their files (an in-progress turn is lost).
- A "dialog resolved" record so other clients close a dialog answered elsewhere.
- Diff review on the phone (needs workspace file access, cf. pi.nvim #29).
- Choosing host mode from pi.nvim.

## Upstream watch

Upstream pi has been building multi-client support: a "server mode" (badlogic in pi#4737) and PR pi#7409 (merged
2026-07-31, `packages/client`/`packages/server` with shared/exclusive session leases; those packages are no longer on
`main`). None of it is in pi 1.5.2. Keep Piton's pi-facing side behind one adapter so a future server mode replaces it
rather than the daemon.

`pi-durable` (experimental, in the same repository) is a library for building your own agent harness in your own
process, with its own storage; it doesn't use the pi coding agent's sessions, settings, extensions, or RPC. Piton
stays on the pi coding agent for now (2026-11-13), but a later switch to a pi-durable runner is a real option: it
would bring restart-durable runs, Piton-owned provider login, and a Piton-owned Neovim client. So Piton's interfaces
are designed for the runtime to be replaced:

- **Clients never see pi.** The `view` protocol, `settings.set`, and `daemon.state` speak Piton's own model
  (sessions, items, dialogs, settings); no pi RPC record, pi setting name, or pi file path crosses to a client.
  `piton attach` (pi.nvim's raw RPC) is the one pi-shaped surface, and it's kept separate.
- **One runtime boundary in the daemon.** Launching, the session task's pi I/O, and the RPC-to-view translation
  stay together behind one adapter; nothing else in the daemon reads pi records.
- **Configuration is Piton's.** The resolver's parts (permissions, MCP, rules, skills, policies, workflows,
  settings) are runtime-neutral; only their delivery is pi-specific (a pi extension today, hooks and sections in a
  pi-durable runner). Settings Piton adds (daemon, sandbox, host and client preferences, and what Piton's
  extensions read) live in Piton's files, never in pi's `settings.json`. pi's own configuration (its
  `settings.json`: default model, theme, packages; `auth.json`, keybindings, sessions, a project's `.pi/`) stays
  pi's, and Piton doesn't write it. If the apps ever change something pi owns (the default model, say), it's a
  Piton setting the adapter applies to pi.
- **pi-specific pieces are named as such:** the protected `.pi/` folder, skill discovery through
  `resources_discover`, `PI_CODING_AGENT_DIR`, and pi's session files. They're the parts a switch removes.

## Open questions

1. A collision caused by a `switch_session` Piton can't see (extension, pi's `/resume`): can Piton detect it, and
   should it report only or also stop one side? Researched in #210.
