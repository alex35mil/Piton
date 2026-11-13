# piton: design

Status: draft. Decisions below were settled in discussion; open questions are listed at the end.

## Problem

A pi RPC process lives exactly as long as the client that started it: Pi 1.3.0 RPC has one client per process, the
client owns pi's stdin, and closing stdin shuts pi down (still true on 1.5.2). Today that client is pi.nvim inside Neovide, so every session
dies with the editor and is unreachable away from the laptop.

piton separates where pi runs from who is looking at it:

- pi processes run under a daemon on the laptop and outlive any client;
- pi.nvim becomes a client that connects and disconnects without interrupting a session;
- an iOS/iPadOS app (and a macOS menu bar app) connects over Tailscale to read and steer sessions;
- the user is notified by push when an agent needs them.

piton is personal tooling for one user on one MacBook Pro. pi.nvim is a public, provider-neutral library; piton is one
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

- **Transparent to pi.** piton runs `pi --mode rpc` in the workspace with the environment the user's shell would give
  it. It never changes pi's settings, agent directory, session files, or extensions, and never computes pi's session
  folder itself. Per-folder agent directories (set through mise) keep working because piton makes no assumption about
  them.
- **Build on pi's protocol, don't replace it.** pi.nvim gets raw pi RPC. piton types only the fields it acts on
  (command ids and types, event types, extension UI requests); everything else passes through as JSON.
- **One live pi per session file.** pi doesn't lock session files; two processes appending to one file corrupt its
  tree. piton enforces the rule for every pi it owns. A pi started outside piton (a terminal) is out of scope.
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
the bundled binary: tools (pi, mise, nono, tailscale), the global configuration, the CLI link
(`~/.local/bin/piton` to the bundled `piton`), start at login, the state database, and remote access (this host's
tailnet name and port). Each is ok, missing with its fix, or failed: `piton setup apply <step>` does the steps
piton can (create the global folder, link the CLI, create the database), the app registers the daemon, and the rest
are instructions. A development build has no bundled binary and uses the linked CLI; its daemon comes from `piton
service install`. DMG packaging, signing, and notarization are tracked separately.

### Launching pi

For every pi it starts, piton:

1. builds the environment: the user's login-shell environment captured at daemon start (launchd's environment is
   minimal), plus `mise env -C <workspace> --json`, computed once per launch;
2. reports an untrusted or failing mise config instead of launching;
3. runs pi in the workspace, in the launch's mode (below).

Sessions started from nvim and from the phone get identical environments, and each worker clone gets its own mise
environment (today a `:tcd` worker inherits the Neovide launch folder's environment).

### Launch mode: sandbox or host

The mode is chosen per launch, not per workspace. nono is the only sandbox.

- **Sandbox (default):** `nono run -s --profile <generated> --allow-cwd -- pi --mode rpc [...]`, with piton's
  environment passed through. The daemon generates the profile for each launch from the configuration chain's
  `[sandbox]` tables (see "Configuration and state"); it grants the source's `.piton/.local` and keeps protected files
  read-only. See "Workers" for what "source" means.
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
- **`[workers] skip`** in `.piton/daemon.toml` lists paths not copied into new workers, such as editor indexes that
  break tooling in a clone (`apps/.build/index-build` here). It only removes things from the copy.
- **`piton worker create <name> [--from <checkout>] [--label] [--issue]`:** the source must be clean on its default
  branch; piton fast-forwards it, clones everything except the skip list and `.piton/.local` (one `clonefile(2)` per
  entry), links `.piton/.local`, and sets `agent.source`, `agent.label`, `agent.issue`. When mise trusts the
  source's own config, piton trusts the worker's too (`mise trust <worker>`): its config files are the source's,
  cloned, so this grants nothing the user hasn't. An untrusted source gives an untrusted worker, and a launch there
  reports it (see "Launching pi"). It writes no pi.nvim
  descriptor: pi.nvim's global spec attaches through piton, and a workspace descriptor would override that and bypass
  piton (no phone access, no keep-awake). The worker's sandbox comes from piton at launch (see "Launch mode").
- **`piton worker ls`:** the source's workers with their live sessions.
- **`piton worker delete <path> [--force]`:** refuses with the uncommitted changes and unpushed commits unless forced;
  stops the worker's sessions, drops mise's trust in the folder, unlinks `.piton/.local`, deletes the folder, and
  fast-forwards the source.
- **`--json`:** every `piton worker` command prints the daemon's response; refusals are typed (`kind`: `exists`,
  `source_dirty`, `source_off_default`, `unknown_default`, `invalid_name`, `not_a_repository`, `not_a_worker`,
  `leftovers`, `failed`) with what a caller needs to explain or force them, and exit 4.

The editor keeps its UI (tabs, picker, tracker queue, dialogs) and calls `piton worker` for the rest.

### pi version

piton targets pi 1.5.2. pi is pinned in the user's global mise config and bumped centrally across tools, so this
repository doesn't pin pi itself (piton launches whatever `pi` the workspace's environment resolves); the pi package's
minimum is in `pi/README.md`. The RPC protocol docs (`rpc-commands.md`, `json.md`, `rpc-extension-ui.md`) are
identical on 1.3.0, 1.5.0, 1.5.2, and `main` (checked 2026-11-07, 1.5.2 on 2026-11-11). Differences between
versions that shaped piton:

- **Stopping pi.** On 1.3.0, a signal that arrives while pi is shutting down after stdin EOF cuts its cleanup short
  (for example a pending OAuth token save). So piton stops a pi by closing its stdin only, waiting the grace period, and
  only then sending SIGTERM, and SIGKILL after a further short wait. The default grace is longer than pi.nvim's 5 s
  (proposed 15 s) because pi waits for pending work. This sequence is also correct on newer pi, where SIGTERM starts an
  orderly shutdown too.
- **Unrequested exits.** Newer pi exits at once when an extension calls `ctx.shutdown()` while idle (1.3.0 waits for
  the next command). Either way piton must treat an exit it didn't request with code 0 as "pi ended" (connections exit
  3), not as a crash, in status and notifications.
- **Session writes.** 1.5.0 fixed failed session writes leaving entries in memory that never reached disk. On 1.3.0 a
  failed write can still make the live pi and its file disagree; piton doesn't depend on the file matching the live pi
  except when resuming after a daemon restart.
- **Prompts sent together.** From 1.5, pi drops a `prompt` that arrives while the previous one is still being
  prepared, after answering it `"started"` (docs/pi-findings.md, "Queued prompts"). So the session task writes one
  `prompt` at a time, from every source (rpc clients, the apps, `start --prompt`), and holds the next until pi
  answered the previous one. Held prompts reach pi even if their client left.

### Workspace roots

piton's config lists workspace roots (`roots`; `daemon/src/workspaces.rs`). There are none by default: until the owner
lists some, `workspaces.browse` and `session.start` from a `view` client refuse with "no workspace roots configured",
and the apps say so. A root is a tree of grouping folders with repositories at varying depth, e.g. `~/Projects/personal/apps/app` and its worker clone `~/Projects/personal/apps/app--fix-login`
next to it. A **workspace** is a folder under a root that
contains `.git`; piton doesn't descend into a workspace, so subfolders of a repository aren't shown. Hidden folders are
skipped.

- `workspaces.browse {path?}` returns one level as `{entries}`: the direct children of `path` (the roots when
  omitted), each either `{kind: "group", name, path}` (a folder without `.git`, browsable further) or
  `{kind: "workspace", name, path, source?}` (`source` for a worker). Groups first, by name. The phone loads a level
  when you open it, so there is no depth limit and no full scan. Paths outside the roots are refused (canonicalized
  first, so `..` can't climb out).
- Not yet: `workspaces.recent` (workspaces piton launched pi in, newest first), `workspaces.search {query}`, and live
  session counts per workspace.
- Worker clones are ordinary workspaces in that tree. Grouping them under their main repository is a later refinement.
- `session.start` from a `view` client rejects a path that isn't a git checkout under a root.
- `piton attach` and `piton start` are not limited by roots: they run locally, in a workspace you chose yourself.

### Session registry

piton keeps one record per live pi: workspace, pi process, session id and file (from `get_state`, refreshed after
`switch_session`, `new_session`, `fork`, `clone`), status, connected clients, writer, pending dialogs, and the session
view (below).

Saved sessions that aren't running are listed from the session folder pi reports. Finding that folder for a workspace
with no live pi is part of the session creation redesign (#208).

### Attaching from pi.nvim

pi.nvim knows nothing about piton: it supports any attached connector (its README, "Attached pi"), and piton is one.
piton relies on that generic contract:

- **Restore:** after connecting, after an automatic reconnect, and after every successful `switch_session` and
  `new_session`, pi.nvim rebuilds the chat from `get_state` and `get_entries`, holding back records that arrive before
  the `get_entries` response and dropping the ones the snapshot already has. piton's replay (below) fills in what
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
diagnostics go to stderr. The connector's environment is not passed to pi; piton builds pi's environment itself.

| pi.nvim action | What pi.nvim sends | What piton does |
| --- | --- | --- |
| `:Pi` | runs the connector | launches a new pi in the workspace |
| `:PiContinue`, `:PiResume` | `switch_session <path>` | see "Commands piton intercepts" |
| `:PiNewSession` | `new_session` (no `abort`) | see "Commands piton intercepts" |
| `:PiAbort` | `abort` | passes through, even on a shared pi |
| `:PiDetach`, tab close, Neovim exit | closes the connector's stdin | drops the client, exits 0; pi keeps running |
| `:PiStop` | confirms, runs `terminate` | `piton stop --session <id>`: see below |
| automatic reconnect | runs the connector, then `switch_session <previous file>` | new pi, then the move rule brings it back |

#### Commands piton intercepts

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

Not interceptable: an extension's `switch_session` or pi's own `/new` and `/resume` handled inside pi. piton doesn't
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

When the last client leaves a pi that has no messages, no run, and no pending dialog, piton stops it after about 10 s
(so a quick reconnect lands on it). This covers `:Pi` then closing the tab, a cancelled `:PiResume` in a fresh tab,
the pi left behind by a move, and the same cases from Swift clients.

### Multiple clients on one session

Pi has one stdin, so the daemon multiplexes:

- **Command ids:** each client's ids are rewritten to unique ids on the way to pi and restored on the way back, so
  responses reach only the client that asked. Events go to everyone.
- **Writer:** updated on every steering command from any client.
- **Dialogs:** piton tracks every pending `extension_ui_request` (dialog methods) until a client answers, its timeout
  passes, or pi exits. Answers after the first are dropped by piton (pi ignores them anyway). A dialog answered on
  another client stays open in pi.nvim until dismissed, since pi sends nothing when a dialog resolves.
- **Order:** each pi's stdout has one reader; every connection gets records in pi's order. Responses to other clients'
  commands are filtered out, never reordered.

#### Replay

piton keeps the temporary state that neither `get_state` nor `get_entries` contains, and sends it to an `rpc` client as
pi's original records, unchanged, as the first records on the connector's stdout (or right after the success response
of a move), before any live record. Taking the snapshot and switching to live relaying is one step in the session's
task, so no live record is skipped or sent twice. Order:

1. `compaction_start`, if a compaction is running;
2. the in-progress assistant message: its `message_start` and `message_update` records so far;
3. tools still running: each `tool_execution_start` and its `tool_execution_update` records so far;
4. the latest `queue_update`;
5. pending dialogs, each `extension_ui_request` with its original `id`;
6. the latest `setStatus` and `setWidget` per key.

pi.nvim holds these back until its `get_entries` response and deduplicates them against the snapshot, so with piton
there is no "response in progress" placeholder and no invisible dialog.

### Session view (Swift clients)

Swift clients don't get raw RPC. The daemon keeps a structured model of each live session (`daemon/src/view.rs`) and
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
| `daemon.state` | `{state}`: `host_id`, `keep_awake` (enabled), `awake` (held now), `sessions` (live count), `logging` (`policy`, `default`), `settings` (each with `key`, `value`, `kind` (`config` with `file` and `placement`, or `state`), `value_type`, `applies` (`live`, or `restart` with `pending`), `editable`), `settings_error` |
| `settings.set {key, value, local?}` | `{state}`; state to the database, configuration to its global file; `value: null` resets; `local` moves the key between the shared and `.local` file |
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
text frame per message, plain `ws://` (Tailscale encrypts the traffic). `daemon/src/tailnet.rs`,
`daemon/src/websocket.rs`.

- **Client transport:** the apps use the system proxy settings, as any app does (diagnostics log which proxy iOS
  picks; see `docs/logging.md`). Traffic still uses Tailscale's VPN routing/encryption; it isn't ordinary public web
  traffic. Every terminal path
  invalidates its URLSession (failure, deadline, cancellation, remote close, or explicit close), releasing the delegate
  before another reconnect can accumulate retained sessions.
- **Who gets in:** before the WebSocket handshake, `tailscale whois --json <peer>` identifies the device. Accepted only
  when it belongs to the same Tailscale user as this Mac, isn't tagged, isn't shared in from another tailnet, and
  isn't this Mac itself. Refusals are logged with the reason; the peer sees the connection close.
- **Why not this Mac:** sandboxed agents can open network connections, and through the tailnet address they'd look
  like the owner. Local clients use the Unix socket (`0600`, outside the `piton` sandbox). Never add a localhost or
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
- Tests use a fake `tailscale` CLI (`daemon/tests/fixtures/fake-tailscale.sh`) that puts the listener on loopback.
- macOS firewalls (Little Snitch) hold incoming connections until allowed. `mise run daemon:install` signs
  `~/.cargo/bin/piton` with the team's Apple Development certificate (identifier `com.alex35mil.piton.daemon`), so its
  code identity stays the same across rebuilds and a rule for that path keeps matching. Allow incoming TCP on port
  7357 from the tailnet ranges (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`); the identity check (Tailscale `whois`) is
  piton's own, the firewall rule only lets the connection reach it. Debug builds (`daemon/target/debug/piton`, tests)
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
- **`view`:** piton's control and view protocol. Draft operations:
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
- On/off: state in the host's database (default on); the apps toggle it (`settings.set` `keep_awake`, the macOS menu bar
  item and Settings > General on both platforms), and the choice survives restarts. A `keep_awake` key in
  `daemon.toml` is refused: it's state, not configuration.
- Lid closed on battery without an external display sleeps regardless. For that, `piton-sleepd` toggles
  `pmset disablesleep` while agents work: it accepts XPC only from the signed daemon, restores normal sleep when the
  connection drops, and enforces a maximum duration and a low-battery/thermal cutoff.
- The helper needs the app bundle; the bare daemon gets only the assertion.

### Logging and agent access

The Info level records essential events; opt-in Debug adds detailed traces and expires after an hour by default unless
explicitly kept on. The apps retain bounded files alongside Apple's unified logger. Host logging (the
Mac app, the daemon, and a separate paired-iPhone collector) follows the daemon: `[host] logging` in `settings.toml`
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

Fonts are a device setting (Settings > Appearance): a text font and a code font, each the system font or any family
available to the app, scaled with Dynamic Type. The Mac lists its installed fonts. On the phone, fonts the user owns
(a paid one like Berkeley Mono can't be bundled) are imported from .otf/.ttf files: copied into the app's Application
Support and registered for the process at launch. Fonts installed system-wide through configuration-profile apps need
the "Use Installed Fonts" entitlement, which the app doesn't have.

The independent collector uses Apple's paired-device file service, not Piton's WebSocket: it can retrieve logs of a
failed Piton connection when CoreDevice can still reach the phone. At Info it makes no device calls. It
preserves the last good snapshot with explicit freshness metadata, and agents get read-only access to the log-only
directory, never the control socket or device tooling. See [logging.md](logging.md) for limits and setup.

### Configuration and state

Status: settled in #206, being implemented. Sections above that name `.piton.toml`, `.nono*.json`, `.project/`, or
`~/.config/piton/config.toml` describe the layout this replaces.

**Hosts and clients.** A host runs the daemon: it runs pi, holds the projects, and owns the state database. A client
is an app connected to a host (the Mac app, the iPhone and iPad apps). A Mac can be a host, a client of another host,
or both; the Mac app asks on first launch and installs the daemon and the sleep helper only on a host. Each host has
a generated `host_id`, sent in `daemon.state` (pushed to every subscribed client).

Four stores, each with one job; every value has exactly one home.

| Store | Where | Owns | Written by |
| --- | --- | --- | --- |
| Config files | `.piton/` and `.agents/` in the repository and its parent folders; the global tier `$XDG_CONFIG_HOME/piton/` | what you decide; travels through dotfiles and repositories | the user; the daemon for app edits to the global files and for approved protected writes |
| State database | `~/.local/state/piton/piton.db` | this host's state: sessions, drafts, recent projects, keep-awake, `host_id` | the daemon |
| Client storage | each app's container | hosts it connects to, imported fonts, log capture, local client preferences, unsent work | that app |
| Memory | the daemon | live pis, session views, temporary switches, pending protected writes | the daemon |

piton never touches pi's own files (`<agent-dir>`: settings, auth, sessions, trust).

#### Config files

```
$XDG_CONFIG_HOME/piton/          # global tier (default ~/.config/piton/)
  daemon[.local].toml            # how the daemon runs, and [sandbox] (was config.toml and the base nono profile)
  settings[.local].toml          # preferences: [host] roots, favorites, logging default...; [client] theme, fonts
  permissions*.json, mcp.json, web-search.json, workflows.json   # were in pi's agent folder
  agents/rules/, agents/skills/  # global agent instructions
<ancestor>/.piton/, .agents/     # shared by every repository below
<repo>/
  .piton/
    daemon[.local].toml          # [sandbox] grants, [workers] skip (was .nono*.json and .piton.toml)
    workflows[.local].json, integrations.json, policies.md
    permissions[.<runtime>][.local].json, mcp[.local].json, web-search[.local].json
    .local/                      # workflow artifacts (gitignored); a worker links its source's
  .agents/rules/, skills/
  AGENTS.md
```

- **Chains.** `.piton/` is read from the source checkout (a worker's source; the checkout around a subfolder), then
  each ancestor strictly below the home folder, then the global tier. `.agents/` starts from the workspace's own
  checkout, as pi does, so instructions apply where they're edited, and ends at the global tier's `agents/`. The home
  folder itself is not a level. A level that exists but can't be read is an error. `.piton/.local/` is never
  configuration.
- **Locating the global tier** can't depend on the workspace, whose `mise.toml` can set `HOME` or `XDG_CONFIG_HOME`.
  The daemon resolves it once from its login-shell environment and sets `PITON_CONFIG_HOME` for every pi after the
  mise environment; a pi started outside piton falls back to `XDG_CONFIG_HOME`, then `~/.config`.
- **Scopes.** Mac-wide and personal keys (the daemon's socket, roots, tailnet, pi and sandbox commands;
  `workflowProject`, `sessionAnalysis`; permission keybindings) are accepted only in the global tier. Elsewhere they
  refuse the file.
- **Merging,** farthest to nearest; within a level the base file, its runtime file, then their `.local` files.
  `.local` files are allowed at every level.
  - `daemon.toml` `[sandbox]`: grant lists (`allow`, `read`, `read_file`, `allow_file`, `deny`, `unix_socket`) only
    add. `sandbox.base` (the backend's base profile) and `bypass` are global-only. The daemon's `sandbox` module, the
    only code that knows the backend, translates the merged table and the protected-file rules into a nono profile
    for each launch.
  - `permissions*.json`: rules union, deny and ask beat allow; `defaultMode` from the nearest file that sets it.
  - `mcp.json`: the nearer entry replaces the whole entry of the same server.
  - other `daemon.toml` keys, `workflows.json`, `web-search.json`: the nearer value per key.
  - `settings.toml`: global only; `settings.local.toml` wins per key.
  - `integrations.json`: the nearest file wins as a whole.
  - `policies.md` and rules: combined, farthest first.
- Paths in every file accept `~` and `$HOME`.
- **When read.** The global files at daemon start and on every change (watched; keys that need a restart are
  reported as pending); the project chain at every launch and `worker create`; the
  pi package's files at pi start and `/reload`. `piton config check` validates and shows each value's source;
  `piton config get` serves resolved files to other tools (the Neovim plugin's tracker queue).
- **One resolver.** The Rust `settings` module is the only implementation. The pi package and the Neovim plugin call
  `piton config resolve` / `piton config get`, which read files and don't need the daemon running. The daemon passes
  the binary's path in `PITON_BIN` after the mise environment; the pi package caches the result and calls again only
  when a watched file changed. The result is per part (permissions, MCP, the global tier's MCP alone, web search,
  workflows, integrations, policies, rule and skill folders), each its value or its error, so a broken or old-layout
  file fails only the tools that read it: a malformed `integrations.json` stops workflows, never permission checks.
  `--global` resolves the global tier alone, for personal settings (`workflowProject`, `sessionAnalysis`) that a
  consuming project's files mustn't block.

#### Config and state together

- **Config or state.** Config is what you decide: roots, favorites, appearance, the logging default, sandbox grants,
  permissions. State is what happened on this host: sessions, drafts, recent projects, keep-awake. Each value is one
  or the other; a config key for a state value is refused with a message saying where it's set.
- **Shared unless Local.** Each preference has one Local checkbox. Unchecked, it lives in `settings.toml`: host
  preferences apply on every host through dotfiles, client preferences are served by the host to every client (which
  caches them for offline start). Checked, it moves to `settings.local.toml` (this host) or into the app (this
  client). A shared client choice a client can't honor (a font it lacks) falls back there only.
- **The apps change both.** `settings.set {key, value, local?}` goes to the database for state and to the global
  `settings` or `daemon` file for config. The daemon edits with `toml_edit`, keeping comments and order, writes
  through a dotfiles symlink, and rereads before writing so a hand edit isn't lost. Every global key is editable
  except `socket`, a test and development override the apps don't show. The apps never write repository or
  parent-folder files. A change is checked against the schema before it's written. The daemon watches the global
  folder (and a symlink's target folder): edits apply at once; `log`, `[env]`, and `[tailnet]` wait for a restart
  and show as pending; an invalid file keeps the last good values and `settings_error` says why.
- `daemon.state` carries each setting with its value, kind (config with its file, or state), whether it's local,
  whether the apps may change it, and any pending restart. Clients never see file contents.

#### State database

SQLite (`rusqlite`, bundled), WAL, owned by the daemon alone, in its `0700` state folder. Tables, each added by the
work that needs it: `host` (`host_id`); `settings` (state values such as keep-awake); `projects` (canonical source
path, last used); `sessions` (piton's session identity and its state `draft`, `creating`, `live`, `failed`, title,
pi session file); `drafts` and `draft_attachments` (size-bounded). Rows decode into Rust sum types at the boundary.

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
launch). It's idempotent: it checks `pi`, `nono`, `mise`, and `tailscale`; validates configuration and lists
old-layout files and `.local` files this host lacks; installs the service and creates the database; and prints the
tailnet address for clients. A client needs only a host's address. What doesn't travel is by design: this host's
state (a replacement Mac imports it, above), `.local` files (secrets, per-host values, local preferences; recreated
by hand), and workflow artifacts, which resume from the tracker. `piton config check` warns about absolute paths
under another user's home and secrets in tracked files.

Several hosts in the clients are a later follow-up: a list of hosts keyed by `host_id`, with either a switch or one
session list grouped by host. Hosts stay independent; nothing syncs between them.

#### Agent access

A sandboxed agent reads its whole chain (the daemon adds read grants for the source's `.piton/`, each parent level,
and the global tier) and writes only its workspace and `<source>/.piton/.local/`. It never reaches the state folder
except `logs/` and `agent.sock`. `PITON_RUNTIME`, `PITON_CONFIG_HOME`, and `PITON_SESSION_TOKEN` are set by the
daemon after the mise environment.

**Protected files** are everything a later pi loads from the workspace that could widen what an agent may do without
asking, or run code outside the sandbox: the source checkout's `.piton/` (except `.local/`); pi's project `.pi/`
folders (settings, MCP servers, extensions, skills, system prompts), which pi reads from the working folder; and
`.agents/skills/` (their `allowed-tools` become allow rules). The last two are protected at any depth in the
workspace's checkout, since a session can start in a subfolder. `AGENTS.md` and rules aren't protected: they change what the
agent is told, not what it may do. Without protection, a sandboxed agent in the source checkout could add `{"allow": ["bash(*)"]}` to
`.piton/permissions.host.json`, and a host session in the same checkout would run any command without asking from its
next tool call. Changes are gated where they're written:

1. **OS sandbox.** The daemon adds raw Seatbelt rules to each sandboxed launch (`(deny file-write* (require-all
   (subpath "<source>/.piton") (require-not (subpath "<source>/.piton/.local"))))`, and regex rules for `.pi` and
   `.agents/skills` at any depth), so
   every write by any process in the sandbox fails: shell commands, scripts, `git checkout`, MCP servers, pi itself.
   nono's typed rules can't express a read-only folder inside a writable one; its raw-rule field
   (`unsafe_macos_seatbelt_rules`) can, and nono emits it after the allows so the deny wins.
2. **Writes through the daemon.** `write`, `edit`, and project setup write protected files through one operation in
   the pi package. In a sandboxed session it sends the path and contents over `~/.local/state/piton/agent.sock`
   (identified by the session token); the daemon checks the path, shows the confirmation with the diff in the apps
   and in pi.nvim (as an extension dialog it inserts), and writes only on approval. The agent can request but never
   approve: answers come only from clients. One pending request per session. A skill edit that leaves
   `allowed-tools` unchanged is written without asking. In a host session, or a pi outside piton, the permissions
   extension asks itself and writes directly, whatever `defaultMode` or auto-accept says.

A rejected change never reaches disk. Limits: shell writes in host sessions aren't gated (as in Claude Code without
its sandbox); the rule is macOS-only; and its effect on renames and hard links, and the pi.nvim dialog, are checked
first when implemented. Codex and Claude Code don't need step 2 because they sandbox each shell command and run their
edit tools unsandboxed; piton sandboxes the whole pi process.

**Sandbox backend.** The nono CLI on PATH for development. The `nono` Rust crate (the sandbox core: `CapabilitySet`,
`Sandbox::apply_auto`, `add_platform_rule`) could be linked into the daemon through a `piton sandbox-exec` helper,
removing the PATH dependency and the profile file; but nono's policy groups and profile inheritance live in its CLI,
so the daemon would carry and sync about 120 path entries itself. That's the step to take before piton is
distributed (#224). A typed write-deny with nested re-allow, which would replace the raw rules, is requested upstream
([nono#2310](https://github.com/nolabs-ai/nono/issues/2310)). A custom Seatbelt launcher is rejected: keeping system and credential path lists correct across macOS releases is
security work with no upstream sharing it.

#### Old layout

No compatibility period. `piton migrate` (run on the host; a sandboxed agent can't write `.piton/`) moves a
repository's files (`git mv` for committed ones), converts `.nono*.json` into `[sandbox]` tables, and updates the
ignore rules. Every consumer refuses the old paths with a message naming each one and its new place, in the parts that read
them: the daemon at start and launch, the pi package in the tools that need them, the Neovim plugin for its queue. Global-tier
files are the user's dotfiles: `piton config check` lists what to move.

### Deploying from a sandbox

Status: planned. Today installs are host-only (`mise run daemon:install`, `apps:mac`, `apps:phone`, `apps:ipad`).

An agent in a sandboxed session may deploy what it built: the daemon, the Mac app, or the iOS app on the iPhone or
iPad. This doesn't widen what the agent can do: the code it wrote runs on the host once deployed, whoever runs the
deploy, so a person approving each one adds no safety. What deploying must not do is disrupt work in progress, and
that's what gates it: the daemon decides when a deploy may take effect, and an agent can't skip that check.

The sandbox keeps every restriction it has. A deploy is a request, and the host does the work:

- **Deploy socket.** The daemon listens on a second Unix socket (`~/.local/state/piton/deploy.sock`, `0600`), the
  only piton path the `piton` nono profile allows. It speaks one request, `{deploy: daemon | mac | iphone | ipad,
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

The gates apply to every deploy, the user's included: on the host, `mise run daemon:install` goes through the same
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
  self-contained so moving it into a shim is mechanical. Until then, restarting piton ends all sessions; they continue
  from their files (an in-progress turn is lost).
- A "dialog resolved" record so other clients close a dialog answered elsewhere.
- Diff review on the phone (needs workspace file access, cf. pi.nvim #29).
- Choosing host mode from pi.nvim.

## Upstream watch

Upstream pi has been building multi-client support: a "server mode" (badlogic in pi#4737) and PR pi#7409 (merged
2026-07-31, `packages/client`/`packages/server` with shared/exclusive session leases; those packages are no longer on
`main`). None of it is in pi 1.5.2. Keep piton's pi-facing side behind one adapter so a future server mode replaces it
rather than the daemon.

`pi-durable` (experimental, in the same repository) is a library for building your own agent harness in your own
process, with its own storage; it doesn't use the pi coding agent's sessions, settings, extensions, or RPC. piton
stays on the pi coding agent for now (2026-11-13), but a later switch to a pi-durable runner is a real option: it
would bring restart-durable runs, piton-owned provider login, and a piton-owned Neovim client. So piton's interfaces
are designed for the runtime to be replaced:

- **Clients never see pi.** The `view` protocol, `settings.set`, and `daemon.state` speak piton's own model
  (sessions, items, dialogs, settings); no pi RPC record, pi setting name, or pi file path crosses to a client.
  `piton attach` (pi.nvim's raw RPC) is the one pi-shaped surface, and it's kept separate.
- **One runtime boundary in the daemon.** Launching, the session task's pi I/O, and the RPC-to-view translation
  stay together behind one adapter; nothing else in the daemon reads pi records.
- **Configuration is piton's.** The resolver's parts (permissions, MCP, rules, skills, policies, workflows,
  settings) are runtime-neutral; only their delivery is pi-specific (a pi extension today, hooks and sections in a
  pi-durable runner). Settings piton adds (daemon, sandbox, host and client preferences, and what piton's
  extensions read) live in piton's files, never in pi's `settings.json`. pi's own configuration (its
  `settings.json`: default model, theme, packages; `auth.json`, keybindings, sessions, a project's `.pi/`) stays
  pi's, and piton doesn't write it. If the apps ever change something pi owns (the default model, say), it's a
  piton setting the adapter applies to pi.
- **pi-specific pieces are named as such:** the protected `.pi/` folder, skill discovery through
  `resources_discover`, `PI_CODING_AGENT_DIR`, and pi's session files. They're the parts a switch removes.

## Open questions

1. A collision caused by a `switch_session` piton can't see (extension, pi's `/resume`): can piton detect it, and
   should it report only or also stop one side? Researched in #210.
