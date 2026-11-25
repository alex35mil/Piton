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
  it. It never changes pi's settings, agent directory, session files, or extensions, except to write a change the user
  approved as a diff ("Protected files"), and never computes pi's session folder itself. Per-folder agent directories (set through mise) keep working because Piton makes no assumption about
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
  `view` connections), keep-awake, reloading the global files, the local socket's messages (`protocol`), and the
  kernel's record of sandbox refusals (`refusals`).
- `session/`: one pi process, and the conversation model the apps render (`view`).
- `launch/`: pi's environment and command line, and the sandbox (`sandbox`: the built-in policy, one launch's
  resolved rules, and applying them with the `nono` library; the only code that knows the backend).
- `apps/`: serving the native apps: the `view` protocol, the tailnet WebSocket, browsing roots, the Settings list.
- `agent/`: the bridge between a sandboxed agent and the daemon: which files are protected and their Seatbelt rules
  (`protected`), `agent.sock` (`socket`), a protected write asked of the user (`write`), bash calls the daemon runs
  (`run`) and the per-command grants it matches (`command`), the block question (`block`), and `open` (`open`).
- `project/`: a workspace's source or worker relation, and worker clones.
- `integrations/`: the external programs piton runs (mise, fnox, tailscale) and whether each is used.
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
the bundled binary: tools (pi, required; mise, fnox, and tailscale, optional, with their state), the global configuration, the CLI link
(`~/.local/bin/piton` to the bundled `piton`), start at login, the state database, and remote access (this host's
tailnet name and port). Each is ok, missing with its fix, or failed: `piton setup apply <step>` does the steps
Piton can (create the global folder, link the CLI, create the database), the app registers the daemon, and the rest
are instructions. A development build has no bundled binary and uses the linked CLI; its daemon comes from `piton
service install`. DMG packaging, signing, and notarization are tracked separately.

### Launching pi

For every pi it starts, Piton:

0. decides the repository's trust ("Repository trust"): a launch a person started is refused with `TrustNeeded`
   until the user decides, and pi gets `--approve` or `--no-approve` from the decision;
1. builds the environment: the user's login-shell environment captured at daemon start (launchd's environment is
   minimal), plus the workspace's environment tools (`mise env -C <workspace> --json-extended`, when mise is used),
   computed once per launch. A sandboxed launch runs them inside its own sandbox (`<piton> sandbox-exec`, under the launch's policy
   with the host's agent directory), since a sandboxed agent can edit the workspace's `mise.toml` and its `exec()`
   templates, `_.source` scripts, and includes run commands. A host launch runs them on the host. When `piton` itself
   already runs sandboxed (an agent running `piton sandbox show`), the tools run inside that sandbox: macOS doesn't
   reliably start a sandbox inside another one, and nothing inside can widen it. The kernel says which
   (`sandbox_check`), never the environment;
2. reports an untrusted or failing mise config instead of launching. In a sandboxed launch that includes a config
   that reads a secret manager while `mise env` runs (`exec(op read …)`): the manager isn't reachable from the
   sandbox;
3. runs pi in the workspace, in the launch's mode (below).

The environment tools are one enum (`launch/env.rs`) matched exhaustively, and a sandboxed launch's environment is a
type only the sandboxed runner builds, carrying the file each value came from (mise reports it), so a later tool
(direnv, Nix) can't skip the sandbox or the provenance check below.

**pi's environment is visible to the agent** and to every command it runs, in either mode: the login shell's, the
workspace's mise environment, provider keys. In sandboxed sessions the `[tools.permissions]` rules steer the agent but
aren't the boundary: a rule that hides `mise.local.toml` doesn't stop another command from printing the environment.
Keep secrets out of the mise environment; with the credential proxy (below), pi holds placeholders instead.

**Integrations.** mise, fnox, and tailscale are optional. `integrations/` decides each one's state once at daemon
start from `[integrations.<name>] enabled` and the login shell's PATH: unset means used when installed and off
otherwise, `false` is off, and `true` uses it and is an error when it's missing. An enabled integration that's missing
is never run: the apps' Settings show a warning on its setting, and every operation that needs it fails with a typed
error naming the setting (a launch, a worker, remote access); a worker refusal carries that message, so clients show it
as is. `Integration` is one enum matched exhaustively, so a new one (direnv, Nix) answers every decision. The apps are
served on the tailnet exactly while Tailscale is used: `[integrations.tailscale]` holds the tailnet settings, with no
second switch.

**Credential proxy (fnox, experimental).** When fnox is used and `fnox proxy rules` in the workspace finds a
`[proxy]` table, the launch's command line (either mode) is wrapped as `fnox --non-interactive proxy run
--no-sandbox -- ...` (`launch/fnox.rs`): pi's environment holds placeholders and fnox, outside the sandbox, injects
the real secrets into the requests its rules allow. Before that:

- fnox runs, and its checks run, with the daemon's base environment only: the workspace's mise environment can set
  `HOME`, `XDG_CONFIG_HOME`, or `FNOX_*`, which pick the config fnox loads. pi's environment travels past fnox as
  JSON in `PITON_LAUNCH_ENV` (`launch/handoff.rs`); `piton sandbox-exec` (or `piton launch-exec` for a host launch)
  unpacks it and starts pi with it plus every change fnox made to the base (placeholders, proxy and certificate
  variables, removed credentials). `FNOX_*` in pi's environment come only from the base.
- `fnox proxy rules` validates the config without resolving secrets; any failure other than "no `[proxy]`" refuses
  the launch with fnox's message.
- For a sandboxed launch (`prepare_sandbox`), the proxy is prepared first, and `fnox config-files` lists what fnox
  loads. The policy write-denies each listed file and each folder entry on its path, both as listed and at the real
  path (`Source::FnoxConfig`, floor), so an agent can neither edit one nor swap a folder above it, even when the global
  config links into the workspace. The launch is refused only by a policy that can read
  `~/Library/Keychains` or reopens the keychain services: fnox runs inside the sandbox too, and could read its
  provider credentials from the keychain without a prompt. Project fnox files are protected files ("Agent access").
- The daemon's Git on a source checkout (`SourceGit`, for worker updates) is resolved through the same
  `prepare_sandbox`, so fnox's files are closed to it too, it's refused where launches are, and hooks never run in a
  sandbox that can read the keychain.
  That Git doesn't run behind the proxy: it authenticates as an agent's Git does without fnox.

fnox starts the command even when a secret can't be resolved, and refuses only the requests that need it. The
session reads fnox's startup warnings from stderr (`StartupLog`; the format of fnox 1.48, pinned by a unit test) and
shows the unresolved secrets where the user looks: a warning `notice` in the session view, kept after the loaded
history through rebuilds (it isn't in pi's session file); and for attached clients a `notify` warning plus a
`setStatus` entry, both in pi's `extension_ui_request` format, so pi.nvim shows them unchanged and replay carries the
status to later clients.

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
- **`piton worker create <name> [--from <checkout>] [--label] [--issue] [--approve-source-update <sha>]`:** the
  source must be clean on its default branch; Piton fast-forwards it (see "Updating the source" below), clones
  everything except the skip list and `.piton/.local` (one `clonefile(2)` per entry), links `.piton/.local`, and sets `agent.source`, `agent.label`, `agent.issue`. When mise trusts the
  source's own config, Piton trusts the worker's too (`mise trust <worker>`): its config files are the source's,
  cloned, so this grants nothing the user hasn't. An untrusted source gives an untrusted worker, and a launch there
  reports it (see "Launching pi"). It writes no pi.nvim
  descriptor: pi.nvim's global spec attaches through Piton, and a workspace descriptor would override that and bypass
  Piton (no phone access, no keep-awake). The worker's sandbox comes from Piton at launch (see "Launch mode").
- **`piton worker ls`:** the source's workers with their live sessions.
- **`piton worker delete <path> [--force] [--approve-source-update <sha> | --skip-source-update]`:** refuses with
  the uncommitted changes and unpushed commits unless forced, and plans the source's update, both before changing
  anything; then stops the worker's sessions, drops mise's trust in the folder, unlinks `.piton/.local`, deletes the
  folder, and fast-forwards the source. A source that isn't clean on its default branch, or can't be fetched, is left
  as it is, with a warning.
- **Updating the source.** A sandboxed agent may have worked in the source checkout, so what Git runs there (hooks,
  `core.fsmonitor`, and the files they read, including ones Git ignores, like husky's `.husky/_/`) may be the agent's.
  The daemon fetches the default branch and fast-forwards inside the sandbox a launch in the source gets, with every
  hook on: the user's hooks keep running, contained, and no Git that could run them runs on the host. The fetch
  authenticates the way the agent's Git does (SSH through the SSH agent with `~/.ssh/config` and `known_hosts`
  readable, as the `standard` profile grants; HTTPS with the keychain readable, a `"read"` entry in
  `[sandbox.filesystem]`); when it can't, creation is refused naming that. The target is the commit `git ls-remote`
  reports, never a remote-tracking ref an agent could have rewritten. Read-only Git commands run on the host with
  hooks and fsmonitor off.
  - An update that changes protected files (`.piton/`, `.pi/`) needs the user, since the sandbox can't write them. The
    daemon refuses with `protected_update` (the checkout, `upstream`, the `target` commit, and the `files`) before
    changing anything. The client asks and retries with `--approve-source-update <target>`; deletion can instead
    `--skip-source-update`. An approval of a commit the remote has since moved past asks again.
  - Approved, the daemon writes exactly the approved files itself: each path's blob from the target commit (by hash,
    with its mode), or its removal, through the no-follow writer protected writes use, staged with `git update-index`.
    Then the merge runs in the sandbox with every rule in place, finding those paths done. Nothing is lifted, so a
    hook can't write any other protected file (`.piton/config.local.toml`, which no diff shows). A symlink or
    submodule among them refuses (change it by hand); a merge that fails puts the paths back from `HEAD`.
  - The user's permission rules can answer before anyone is asked: `piton(git-pull)` in the source checkout's
    `[tools.permissions]` (the host's rules: `[tools.permissions]` and `[tools.permissions.host]`). `allow` applies the update's
    protected files without asking, through the same approval path; `deny` refuses creation (`update_denied`) and, on
    deletion, deletes the worker and leaves the source as it is, with a warning; `ask` or no rule asks. A deny or ask
    rule wins over an allow, and `*` counts.
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
  `leftovers`, `invalid_configuration`, `source_fetch`, `protected_update`, `failed`) with what a caller needs to
  explain or answer them, and exit 4.

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
  pending dialogs (pi's `extension_ui_request` records), and pending Piton questions (`questions`, typed; today the
  block question for a command, `kind: "command"`, see "The block question").
- **Window:** the newest `limit` items (default 50, max 200); `session.history` pages back.
- **Resume:** every change has a `seq`. `session.watch {from}` sends the missed changes, or a snapshot when they aren't
  kept (5000 per session) or a reset happened since.

The same model drives the session list's titles and status; notifications next.

### The `view` protocol

One JSON object per line after the hello `{"protocol":"view","version":1,"client":"<name>","device":"mac"|"iphone"|"ipad"}`
(`device` required: it decides which client settings the connection sees). Requests carry an
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
| `daemon.state` | `{state}`: `host_id`, `keep_awake` (enabled), `awake` (held now), `sessions` (live count), `logging` (`policy`, `default`), `settings` (each with `key`, `scope` (`host`, `client`, `project`), `value` (a client setting's on this connection's device), `location` (`file` with `placement` `shared`, `local`, or null; or, for a client setting, `devices` with `sections`, the value of each section the file sets, and `section`, the one in effect on this device or null), `value_type` (`bool`, `text`, `text_list`, `integer`, `duration` with `never`, `choice` and `choice_list` with `options`, `map` with `values`), `applies` (`live`, or `restart` with `pending`), `editable`), `settings_error` |
| `settings.set {scope, key, value, local?, section?}` | `{state}`; the key in the global file of `scope`; `value: null` resets. `host`, `project`: `local` moves the key between the shared and `.local` file; `section` is refused. `client`: `section` (`default`, `mac`, `mobile`, `iphone`, `ipad`) is required and `local` refused; a value sets that section and removes the narrower sections that apply to this connection's device, so it's the one in effect there; `null` removes only that section |
| `daemon.logging {policy}` | `{state}`; until it expires or the daemon restarts |
| `workspaces.browse {path?}` | `{entries}`; see "Workspace roots" |
| `session.start {workspace, mode?, prompt?}` | `{session}`, or `{trust_needed: {source, grants, added, resources: {pi, mcp_servers}}}` when the repository needs the user's trust first (nothing launched); mode defaults to `sandbox`; `workspace` must be a checkout under a root |
| `trust.decide {workspace, decision, grants}` | `{trust}`: `decision` is `trusted` or `declined`, for the `grants` the user was shown; refused when the repository's grants changed since |
| `session.stop {session}` | `{forced}` |
| `session.watch {session, from?, limit?}` | `{}`; then a `snapshot` (or the changes since `from`) and every change |
| `session.unwatch {session}` | `{}` |
| `session.history {session, before, limit?}` | `{items, has_more}`: items before item id `before` |
| `session.tool_output {session, call_id}` | pi's full tool result, images as references |
| `session.image {session, image}` | `{mime_type, data}`: one image's base64 data, by the id in its reference |
| `session.send {session, message_id, text, mode?}` | pi's response data. Always pi's `prompt` with a `streamingBehavior`, so pi, not the daemon's view of its status, decides whether a run is active: `prompt` (default) and `follow_up` start a run when idle and queue a follow-up during one; `steer` steers a run. pi gets one prompt at a time (see "pi version"). `message_id` is the client's id for the message: a repeat with the same id reaches pi once and gets the first reply (see "Lost replies") |
| `session.abort {session}` | `{}` |
| `session.answer {session, request_id, answer}` | `{}`; `answer` has pi's `value`, `confirmed`, or `cancelled`. Refused when already answered |
| `session.decide {session, question, answer}` | `{}`; a typed answer to a pending Piton question. The block question takes `{"once": "sandbox" \| "host"}`, `"deny"`, or `{"always": {runtime, scope: "project" \| "global", pattern}}`. Refused when already answered, expired, or invalid |

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

The `view` protocol also runs over WebSocket on this Mac's tailnet addresses (`[integrations.tailscale] port`, default 7357), one
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
  minutes. The daemon pings every client (`[integrations.tailscale] heartbeat`, default 10 s) and drops one that answers none of
  three pings in a row. The apps ping too (every 15 s) and reconnect when a pong doesn't arrive within 10 s.
- **Unnoticed drops:** a connection can die while the phone is suspended in the background or when its network changes,
  and through a local proxy the phone's side stays open: writes seem to succeed and requests wait for the next pong
  timeout (up to 25 s). So the iPhone app checks the connection when it becomes active and when the network path
  changes: a ping with a 2 s deadline, reconnecting at once when it isn't answered. A store waiting to retry retries
  immediately.
- Without Tailscale (not installed, not running) the daemon logs it once and serves local clients only, trying again
  every `[integrations.tailscale] retry` (default 10 s): at login launchd can start it before Tailscale is up. Once listening, it
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
{"type": "hello", "protocol": "view", "version": 1, "client": {"name": "iPhone"}, "device": "iphone"}
```

- **`rpc`:** after the hello, raw pi RPC for one session. Used by `piton attach`. Possible later over the tailnet.
- **`view`:** Piton's control and view protocol. Draft operations:
  - `sessions.list` (live and saved, with status), `sessions.subscribe` (list changes);
  - `session.start {workspace}`, `session.open {sessionPath}` (attach if live, else launch pi on it),
    `session.stop {id}`;
  - `session.watch {id, from?}` → snapshot + changes, `session.unwatch`, `session.history {id, before, limit}`;
  - `session.send {id, message_id, text, images?, mode: prompt|steer|follow_up}`, `session.abort {id}`,
    `session.answer {id, requestId, value|confirmed|cancelled}`, `session.decide {id, question, answer}` (a Piton
    question's typed answer);
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
Mac app, the daemon, and a separate paired-iPhone collector) follows the daemon: `[debugging] log_level` in `config.host.toml`
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
available to the app, scaled with Dynamic Type. Each is set through the host (`config.client.toml`, for every device
or per device section, the value for this device cached for offline start) or kept on this device only; a font a
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
| State database | `~/.local/state/piton/piton.db` | facts about this host and what happened on it: `host_id`, the user's trust decision per repository; later sessions, drafts, recent projects. No preferences | the daemon |
| Client storage | each app's container | hosts it connects to, a device's own preference values, imported fonts, log capture, unsent work | that app |
| Memory | the daemon | live pis, session views, temporary switches, pending protected writes | the daemon |

Piton never touches pi's own files (`<agent-dir>`: settings, auth, sessions, trust), except to write a change to one
that the user approved as a diff ("Protected files").

#### Config files

```
$XDG_CONFIG_HOME/piton/          # global tier (default ~/.config/piton/)
  config[.local].toml            # the chain's farthest level: defaults every project inherits
  config.host[.local].toml       # how the daemon runs on this host, this host's preferences, [debugging]
  config.client.toml             # the apps' preferences, per device section (no local file)
  mcp[.local].json               # MCP servers, in the format other harnesses read
  policies.md
  agents/rules/, agents/skills/  # global agent instructions
<ancestor>/.piton/, .agents/     # shared by every repository below
<repo>/
  .piton/
    config.toml                  # committed: [sandbox] [workers] [tools] [branches] [tracker] [forge]; grants gated by trust
    config.local.toml            # not committed: the same tables, plus [branches] username, [web_search] api_key, [trust]
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
  `roots`, `favorites`, `default_attach_mode`, `stop_empty_session_after`, `approval_timeout`, `keep_awake`; tables
  `[pi]`, `[env]`, `[stop]`, `[integrations]` (above, with Tailscale's tailnet settings), and `[debugging]` (the host
  log level, and switches for a development install in [development.md](development.md), which the pi package reads
  through `piton config resolve --global` on whichever machine pi runs). Durations are strings: a whole number and
  one unit (`"500ms"`, `"10s"`, `"10m"`, `"1h"`); `approval_timeout` also takes `"never"`.
- **`config.client.toml`, global only:** the apps' preferences, grouped (`[appearance]` `text_font`, `code_font`;
  themes and keybindings later). Each is one value for every device or a table of device sections (`default`, `mac`,
  `mobile` for iPhone and iPad, `iphone`, `ipad`); the most specific section that applies to a device wins, and the
  host serves each client the values for the device its hello names. Sections are named by device, never by OS. It
  has no local file: a device keeps a value of its own in its app.

Keys are snake_case. A project file's tables are each parsed on their own, so a bad value in `[tracker]` never stops
permission checks; a syntax error, or an unknown top-level key, fails every table of that file. Every error names the
line, and a table's error also names the dotted key at fault (`line 42, tracker.fields.status.values.in_progress:
...`). A host or client file has one schema and is read whole.

`[tracker]` and `[forge]` are typed like every other table (`model/tables/tracker/`, `forge.rs`): a sum type over
`provider` (`none`, `github`, `linear` for the tracker; `none`, `github` for the forge), with the rules the schema can't
express checked there too: distinct Status and Phase options, distinct Project fields, the reserved `kind` and `gate`
label groups, unique label names where an item sees them (the GitHub repository; each Linear catalog, Phase labels
included), Linear's reserved names, GitHub's 100-character label descriptions, and keep and retired lists that name
each unmanaged label once. Their rules have no other implementation: setup checks a table before changing anything at
a provider with `piton config validate tracker|forge <json>`, which reads one table's value with the same code, no
file or placement rules involved.

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
  - `[tracker]`, `[forge]`: a project or parent folder only, never the global tier, where they'd become every
    repository's.
  - `web_search.api_key`: a `config.local.toml` only (a secret).
  - `branches.username` and `trust.reconfirm`: the user's own files, never a repository's committed `config.toml`
    (a personal value; a repository can't choose how its own grants are re-checked).
  - Everything else, every `[sandbox]` and `[tools.permissions]` key included: either file, any level. What a
    repository's committed file grants is decided by trust instead (below).
- **Repository trust** (#381). A repository's committed `config.toml` (the source checkout's `.piton/config.toml`;
  parent folders' files, the global tier, and every `config.local.toml` are the user's own) always restricts; what it
  grants applies only once the user trusts the repository:
  - Restrictions: `"deny"` in every sandbox map, `false` switches, `silence`; tool-permission `ask` and `deny`,
    `default_mode = "ask"`/`"deny"`, `skill_grants = false`.
  - Grants (`config::GrantEntry`, one canonical line each): `"read"`, `"write"`, `"allow"` entries, `true` switches,
    every preset, `profile`, `[[sandbox.commands]]`; tool-permission `allow`, `default_mode = "allow"`,
    `skill_grants = true`, in each of the three permission tables.
  - The decision is the user's, in the state database's `trust` table, keyed on the canonical source checkout (workers
    share it): `trusted` or `declined`, with the grants shown when it was given. `crate::trust` decides a repository's
    state from its grants, what pi would load (the checkout's `.pi/` resources pi's trust gates, `.agents/skills`, the
    repository's `.piton/mcp.json` servers), the record, and `[trust] reconfirm`, a sum type matched where it's used:
    `NothingToTrust` (no grants, no pi resources, no record: no question; grants committed later stay out),
    `Trusted`, `Declined`, or `Needed` (never asked; or, with `reconfirm = "on-change"`, grants the user didn't
    trust).
  - `reconfirm = "never"` (default): trusted stays trusted, every grant applies, until `piton trust deny|reset`.
    `"on-change"`: only the trusted grants apply; a launch that finds new ones asks again, showing them. Removing a
    grant or adding a restriction never asks. Only the grant entries are compared; `.pi/` and skills are listed in
    the question but never fingerprinted.
  - Every resolution takes a `GrantFilter` (`All`, `Only(grants)`, `None`) and leaves out, and lists as `untrusted`,
    the repository's grants it doesn't keep. A committed `"write"` still never reopens a credential or the user's own
    `"deny"`.
  - A launch a person started (`session.start`, `piton start`, `piton attach`) is refused with `TrustNeeded` while
    the state is `Needed`; the client asks, records the answer (`trust.decide`, `piton trust allow|deny`), and starts
    again. A decision is recorded only for the grants the user was shown. A launch the daemon starts on its own (a
    restart, a switch inside a session) never asks: it keeps what was decided. With #208, opening a project runs the
    same check before a draft exists.
  - pi follows the same decision: the daemon passes `--approve` or `--no-approve` on every launch, so pi's own
    `trust.json` is never used or written. The launch's filter goes to pi in `PITON_TRUST` (set after the workspace's
    environment), so `piton config resolve` inside the session keeps exactly what the launch kept.
  - Worker Git and the source-update rule use the source's decision too.
- **Merging,** farthest to nearest; within a level the shared file, then the `.local` one.
  - `[sandbox]`: one map per resource (`filesystem`, `network.connections`, `network.unix_socket_connections`,
    `network.unix_socket_bindings`, `macos.services`); the nearest value wins per key, each entry keeping the file that
    set it and whether that file is the user's own (`Origin`); the nearest `profile` and `allow_*` switch win;
    presets add, a preset named at several levels taking each option from the nearest level that sets it. The daemon's `launch/sandbox` module, the only code that knows the backend, resolves
    them with the base policy and the profile's entries into each launch's rules ("Agent access").
  - `[tools.permissions]`, then the runtime's `[tools.permissions.host]` or `[tools.permissions.sandbox]`, per file:
    rules union, deny and ask beat allow; `default_mode` and `skill_grants` from the nearest table that sets them.
  - `mcp.json`: the nearer entry replaces the whole entry of the same server.
  - `[branches]`, `[web_search]`, the host file's keys: the nearer value per key.
  - `[tracker]` and `[forge]`: each the nearest table, whole. Both are optional; `provider = "none"` turns one off,
    including one a farther level sets. Each resolves to a role: `on` with its file and validated config, or `off`
    because no level has the table (`absent`) or the nearest says `provider = "none"` (`disabled`, with its file).
  - `policies.md` and rules: combined, farthest first.
- **Writes.** The apps and `piton config set` change keys with `toml_edit`, keeping comments and layout
  (`--global` for the global `config.toml`, `--scope host|client` for the host and client files). The pi package
  writes through `piton config set --dry-run`, which returns the file's new text; a protected file then goes through
  the daemon's confirmation. Nothing else writes these files. An array Piton writes is inline with one item and one
  item per line (indented, with a trailing comma) with two or more; items it keeps keep their comments, and arrays
  it doesn't change keep their layout.
- Paths in every file accept `~` and `$HOME`.
- **When read.** The global files at daemon start and on every change (watched; keys that need a restart are
  reported as pending); the project chain at every launch and `worker create`; the pi package's files at pi start and
  `/reload`. `piton config check` validates the workspace's chain and the host and client files, lists the files
  read, and warns about what doesn't travel (below); `piton config get` serves one resolved part to other tools (the
  Neovim plugin's queue reads `tracker`, `forge`, and `mcp`); `piton config validate` checks one table's value without
  a file.
- **One resolver.** The daemon crate's `config` module is the only implementation (layout below). The pi package and
  the Neovim plugin call `piton config resolve` / `piton config get`, which read files and don't need the daemon
  running. The daemon passes the binary's path in `PITON_BIN` after the mise environment; the pi package caches the
  result and calls again only when a watched file changed. The result is per part: one per table of `config.toml`,
  named as the table, plus MCP, the global tier's MCP alone, policies, and rule and skill folders. Each is its value or
  its error, so a broken table fails only the tools that read it. `--global` returns the host file's `[debugging]`.
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
- **Shared unless Local, or by device.** Each host preference has one Local checkbox where its file has a local one. Unchecked, a host
  preference lives in `config.host.toml` (every host through dotfiles); checked, in `config.host.local.toml` (this
  host). A client preference has a scope instead: a device section of `config.client.toml` (every device, every Mac,
  iPhone and iPad, every iPhone, every iPad; each client gets its device's value and caches it for offline start), or
  only this device, in that app. A client choice a client can't honor (a font it lacks) falls back there only.
- **The apps' settings list** (`daemon.state` `settings`) shows global config keys, each with `key` as written in its
  file, `scope` (`host`, `client`, or `project` for the global `config.toml`), its `location` (the file of the pair
  that sets it; for a client setting, every device section set and the one in effect on the asking device), its type,
  whether it applies live or after a restart, and whether the apps may change it. `settings.set {scope, key, value,
  local?, section?}` writes it (`section` for a client setting). The daemon edits with `toml_edit`, keeping
  comments and order, writes through a dotfiles symlink, and rereads before writing so a hand edit isn't lost. The
  apps edit the host file's keys (`socket` shown read-only), the client file, and the global `[sandbox]` and
  `[workers]`; `[debugging] session_analysis`, `[tools.permissions]`, `[branches]`, `[web_search]`, `[tracker]`, and `[forge]` are edited
  in the file. The apps never write repository or parent-folder files. A change is checked against the schema before
  it's written. The daemon watches the global folder (and a symlink's target folder): edits apply at once; `log`,
  `[env]`, and `[integrations]` wait for a restart and show as pending; a file that can't be used keeps its last good
  values and `settings_error` says why.

#### State database

SQLite (`rusqlite`, bundled), WAL, owned by the daemon alone, in its `0700` state folder. Tables, each added by the
work that needs it: `host` (`host_id`); `trust` (per canonical source checkout: `trusted` or `declined`, the grants it
was given for, when; see "Repository trust"); later `projects` (canonical source path, last used); `sessions` (Piton's
session identity and its state `draft`, `creating`, `live`, `failed`, title, pi session file); `drafts` and
`draft_attachments` (size-bounded). Rows decode into Rust sum types at the boundary.

- `PRAGMA user_version` is the schema version; ordered migrations run at start in one transaction, after a
  `VACUUM INTO` backup of the previous version. A database newer than the binary refuses to start.
- Deleting it resets this Mac's state; configuration is unaffected.
- `piton state export <file>` and `piton state import <file>` move it to a replacement Mac (Migration Assistant
  copying `~/.local/state/piton/` does the same), keeping `host_id` so clients see the same host. Import refuses over a
  database that already has sessions unless forced. Sessions reopen where pi's own session files were copied too.

#### Client storage

The hosts it connects to and this device's own client preferences in `UserDefaults`; the cached host values for
this device;
imported fonts in Application Support; the phone's log capture in its `settings.json`; durable unsent work (outbox,
local drafts) as versioned Codable files in Application Support, moved aside when they don't decode.

#### A new or second Mac

A host: clone the dotfiles (the global tier) and repositories, then run `piton setup` (the Mac app runs it on first
launch). It's idempotent: it checks `pi` and reports `mise` and `tailscale` (optional) with their state; validates the global configuration and
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
and the network can be closed only to everything but pi's own destinations (the gate, below).

**The model.** The base is the floor: everything that can be closed is closed. `[sandbox]` is one map per resource,
Codex-style (#379): `filesystem` (a path to `"read"`, `"write"`, or `"deny"`), `network.connections` (a host,
`host:port`, `localhost:PORT`, `localhost:*`, `*:PORT`, or `*`), `network.unix_socket_connections` and
`network.unix_socket_bindings` (a path), and `macos.services` (a name, `name.*`, or `*`), each to `"allow"` or
`"deny"`; three switches cover what has nothing to list (`allow_tcp_listening`, `allow_git_control_files`,
`allow_launching_apps`). Keys are typed at parse time (`PathKey`, `NetworkTarget`, `ServiceKey`); paths have no
wildcards, so an entry is a path and everything below it, which a Linux sandbox (Landlock, bubblewrap) can enforce one
to one. A profile (`strict`, `standard` by default, `yolo`) and the presets are the base layer of the same maps
(`launch/sandbox/policy.rs`: `profile_entries`, `preset_grants`), all opening entries; the files come after, the
nearest value winning per key. Then the most specific matching entry decides, so any entry closes what a profile or a
broader entry opens. Every entry keeps where it came from (a profile, a preset, or the file that set it, as
`sandbox.<table>."<key>"`), carried in each rule's source (`Source::Config`, `Source::Preset` with a `Provenance`),
so `piton sandbox why` names it. Three states follow: an opening entry opens; nothing listed is closed and asked
about; the user's own `"deny"`, a `"read"` written to, or a `false` switch is closed and only logged
(`DeniedBy::UserDeny`).

**Permissions and the sandbox are separate layers.** `[tools.permissions]` decides whether the agent may make a call,
before it runs; the sandbox decides what a running process may touch. Neither feeds the other. Claude Code merges
`Read`/`Edit` denies into its sandbox, but its sandbox wraps only shell commands; Piton's wraps pi itself, its MCP
servers, and the daemon's `mise env`, so a permission deny made a sandbox deny would hide pi's own `auth.json` from
pi or a `mise.local.toml` from mise. A path no process may touch is a filesystem `"deny"`; secrets belong in a secret
manager behind fnox, never in plain files.

Each sandboxed launch gets an ordered list of rules (`launch/sandbox/`), the last matching one deciding, as Seatbelt
does; `piton sandbox show` prints them and `piton sandbox why` names the one that decides an access. The entry layer
(2 and 3) is one list sorted by path depth, least specific first, so the last match is the most specific entry; at
the same depth a base root comes first, then built-in denies, base-layer entries, the user's allows, and the user's
denies:

1. **Base:** everything is readable, nothing is writable, and no Unix socket can be connected to or created
   (`(deny network-outbound (subpath "/"))` and `(deny network-bind (subpath "/"))`, first in the network rules). A
   path filter on a network operation matches only Unix sockets. A running Neovim runs commands sent to its socket,
   Docker's controls the host, and the SSH and gpg agents sign with the user's keys, so every socket opens only
   through an entry. TCP and UDP are decided after the socket rules, by `Network`. `Direct` (`*` allowed, and no
   named-host entry Seatbelt can't enforce) emits only the port entries, least specific first, each its own effect,
   and no gate runs. `Filtered` (`*` denied or absent, or a named host denied, or allowed under a port deny) emits a
   tagged `(deny network-outbound (remote ip "*:*"))`, then the port entries (`localhost:PORT`, `localhost:*`,
   `*:PORT`, least specific first; with `*` allowed, `localhost:*` too unless an entry says otherwise), then the
   gate's port (`(remote tcp "localhost:<port>")`). nono's own `(allow network-outbound)` comes before Piton's
   platform rules, so the deny wins. Seatbelt refuses a named host ("host must be * or localhost"), so named hosts
   are the gate's. TCP listening (`Listen`) is decided last: closed (by the base, or `Denied` by the user's `false`)
   emits a tagged `(deny network-inbound (local tcp "*:*"))`, which refuses every `listen()`; open
   (`allow_tcp_listening`: `standard`, `yolo`, the `pi-claude-code` preset, the user's own `true`) emits nothing, so
   nono's `(allow network-inbound)` stays. Seatbelt can't limit listening to loopback or a port
   (docs/pi-findings.md), so it's every address or none; `tcp`, not `ip`, because an `ip` deny also refuses UDP
   replies (DNS). Inbound connections can't be filtered by peer, and a listener's replies pass a filtered network.
2. **Base roots:** the workspace, `<source>/.piton/.local/`, the temp folders and `/dev` (read and write), and DNS
   (`/var/run/mDNSResponder`). They sort by depth with the entries, so a user entry inside the workspace (a `"read"`
   folder, a `"deny"`) applies, and a broader `"deny"` above it doesn't close it. Not user caches (`~/.cache`, `~/Library/Caches`): host tools reuse what's in them without checking
   (the Go build cache is linked straight into host builds), so a writable cache would run sandbox-written code on
   the host. Each preset (`mise`, `rust`, `node`, `swift`) grants one toolchain's caches and state as an informed
   opt-in, documented as such. A preset is its name, or a table with its name and its options (`Preset` parses
   both; options merge per option across levels, nearest wins, and the preset's provenance is the file that set an
   option, else the first that lists it). No preset writes what the host executes or trusts (a unit test lists those
   paths): `mise` writes its cache and install bookkeeping, never its installed tools or trusted configs; an install
   that needs them is asked about ("Asking when the sandbox blocks"). A preset's entries (`policy::preset_grants`:
   `"write"` and `"read"` paths, `"allow"` connections, `allow_tcp_listening`) join the base layer, named after the
   preset. A repository's committed presets apply only once the user trusts the repository ("Repository trust").
   The `pi-claude-code` preset grants the pi-claude-code extension's `claude`: the folder `PI_CLAUDE_CODE_CONFIG_DIR`
   names and its `<folder>.lock`, decided like pi's agent directory (accepted from the daemon's environment or a
   file the agent can't write, else only inside the checkout; never overlapping a configuration level, the daemon's
   state, a denied path, pi's agent directory, or the interactive Claude Code's folder), its log cache
   `~/Library/Caches/claude-cli-nodejs` (which doesn't follow `CLAUDE_CONFIG_DIR`), `localhost:*` and listening for
   the extension's relay, and with `login = "keychain"` a `"read"` of `~/Library/Keychains`. A folder it won't open, or a
   token login with no `CLAUDE_CODE_OAUTH_TOKEN` in pi's environment or fnox's rules, starts the session with a
   warning (`LaunchWarning`) instead of refusing it; resolution builds the policy without the folder first and judges
   the setting file against that. It also opens `claude`'s socket folder for writing and binding, never connecting
   (`ClaudeCodeSockets`): the user's `XDG_RUNTIME_DIR` or `CLAUDE_CODE_TMPDIR`, judged the same way and never temp,
   a write root, home, or a folder directly in home (else a warning), or a session's own folder that
   `launch/claude_sockets.rs` picks, the daemon creates at launch and sets as `CLAUDE_CODE_TMPDIR`, and removes when
   the session ends. Its silence entries cover `claude`'s probes there and, with the token login, the keychain
   lookups of `claude` and `security`.
3. **The entry layer:** nono's deny groups (`deny_credentials`, keychains, browser data, macOS private data, shell
   history and rc files), vendored verbatim in `nono-deny-groups.json` at the `nono` version (`mise run
   cli:nono-policy-check` reports upstream changes, and a test fails when the crate is bumped without re-vendoring),
   then every filesystem, socket, and binding entry of the base layer and the files. A deny closes read, write, exec,
   and Unix connect and bind; metadata stays visible. A `"read"` inside a credential reopens it for reading (and
   closes writing there); a `"write"` for both; a socket `"allow"` at or inside one opens just that socket, for
   connecting: Docker Desktop's socket, gpg-agent, and 1Password's SSH agent open when named, while a broader entry
   (yolo's `/`) stays beneath the credential denies. A `"read"` covering `~/Library/Keychains` also reopens the
   keychain Mach services the library always closes. `standard`'s `"read"` entries include every `*.pub` under
   `~/.ssh`, listed when the launch is resolved (ssh with `IdentitiesOnly yes` matches the agent's key through it).
   A committed file's `"read"`/`"write"` at or below a credential deny, or below the user's own `"deny"`, is left out
   (`Resolution::ignored`): only the user's own files reopen what's closed.
4. **pi's agent directory:** read-only, except its runtime state (`sessions/`, `auth.json`, `models-store.json`,
   `mcp-auth.json`, `crashes.json`, `mcp.log` and its rotation, their locks, and MCP's per-server token-refresh
   locks: credentials, catalogs, and logs). Its settings, project trust (`trust.json`), MCP servers, extensions,
   skills, prompts, themes, managed binaries, and packages decide what a later pi loads, host sessions included, so
   they change only through a write the user approves ("Protected files").
   pi locks `settings.json` and `trust.json` even to read them, so their locks (`<file>.lock/`) are writable; the
   files aren't. `docs/pi-findings.md` lists every write pi makes there, from its source. A workspace's `mise.toml` can
   set `PI_CODING_AGENT_DIR`, so the launch is refused when it's the home folder or above it, inside the host's own
   agent directory (the daemon's environment names it) or a checkout's `.pi/`, or overlaps a configuration level,
   the state folder, or a denied path. The host's agent directory stays closed whichever one the launch uses.
   It also depends on the file that set it: from the daemon's environment, or a file the sandbox can't write (the
   global mise config, a client folder's `mise.toml` above the checkout), it may name any folder the checks above
   allow; from a file a sandboxed session could write (the launch's policy lets it, as written or at its real path,
   or it's in the checkout or source checkout), or when mise names no file, it must lead into the checkout, or the
   launch is refused naming the file. A relative value is the workspace's, as pi resolves it. Limit: whether a file
   is writable is judged by this launch's policy, so a session opened in a parent folder can edit that folder's
   `mise.toml` for later launches in projects below it, as with links planted from another workspace.
5. **The files fnox loads** (below), then each socket an entry allows and every folder entry on its way, fixed in
   place (a literal write deny, as for fnox's files). The macOS SSH agent's socket is under
   `/private/tmp`, a write root, so without it a session could move the real socket aside and link Neovim's in its
   place. What's inside a listed folder (`$TMPDIR`) stays writable, so such an entry reaches any socket moved there.
6. **Configuration:** every level of both chains, and what their entries and pi's agent directories' entries link to
   (a dotfiles repository), is write-denied; protected files (below) too. Every folder on the way to a level, an agent
   directory, or a link target is fixed in place (a literal write deny on the folder, not what's in it, as for the
   files fnox loads): Seatbelt checks a rename only against the paths it names, so otherwise an agent in a dotfiles
   workspace could move a linked folder aside and put its own in its place. These files change only through a write
   the user approves ("Protected files"). The workspace's own `.agents/` stays writable. So are Git's hooks and
   configuration (`agent/git.rs`), which Git on the host runs: in any `.git` folder of the checkout and its source,
   `hooks/`, `config`, `config.worktree`, and `commondir` (which points Git at another git directory); the
   checkout's own `.git`, which can't be removed or replaced; and, for a checkout whose `.git` is a file (a linked
   worktree, a submodule), that file and the shared git directory's same entries. The rest of `.git` stays writable:
   commits, branches, fetches, pushes, rebases, `git worktree add`, and `git submodule update` of initialized
   submodules work. What writes a `config` or `hooks/` in a `.git` folder fails, and the user runs it on the host or
   outside the checkout: `git config --local`, `git remote add`, `husky install`, `git submodule init` (so
   `git submodule update --init` too), and `git clone` or `git init` into the checkout; `push -u` pushes but can't
   record tracking. Not covered, as in Claude Code: a `.git` entry created below the checkout's top level (a `gitdir:`
   file or a symlink to a git directory the agent built elsewhere), which makes Git run in that folder use it; denying
   it would also break `git worktree add` and checking out initialized submodules, which create such `.git` files. Nor
   the git directories Git keeps inside `.git` for linked worktrees (`worktrees/`) and submodules (`modules/`).
   Hook logic in tracked files (husky, lefthook) runs wherever Git runs; the daemon's Git that runs hooks runs in the
   sandbox ("Workers"). `allow_git_control_files = true` (in `yolo`, or the user's own) drops these Git rules, for users
   who'd rather let agents set up hooks; it's accepted only in the user's own files ("Where a key may be set"), so a repository can't turn the protection off for
   whoever clones it.
7. **The daemon:** its state folder (the socket's folder: `~/.local/state/piton/`) is closed (read, write, connect)
   except `logs/` for reading and `agent.sock` for connecting; the control socket (`piton.sock`) is denied to connect
   wherever it is. Only a debug build moves the socket, and the state folder with it (`socket`, `--socket`,
   `PITON_SOCKET`, for tests): a moved socket closes its whole folder, so a release build ignores the key (the daemon
   log and `piton config check` say so) and refuses the flag.
   Without it, a socket entry covering the folder (yolo's `/`) would let a sandboxed agent connect to it and act as
   a client: approve its own protected writes, start host sessions.
8. **Mach services and apps** (`Policy::services`, `Policy::apps`, emitted after the path rules). nono allows every
   lookup but the keychain's, and any app's Mach service is a way to drive that app outside the sandbox (Hammerspoon's
   `hs`, Keyboard Maestro, Shortcuts). `Services::Listed` emits a tagged `(deny mach-lookup)` and then each entry,
   least specific first (`*`, a shorter prefix, a longer one, an exact name; a deny after an allow), so the most
   specific decides: the built-in list (`policy::BASE_SERVICES`, Claude Code's list plus TLS, proxy settings, DNS
   configuration, and preferences, which pi, MCP servers, and their tools need), launching apps' services, the
   profile's `*` (`standard`), and the user's entries (names are checked only for what a Seatbelt string needs, since
   real ones include `com.apple.distributed_notifications@Uv3` and plain words). `Services::Every` is only an approved
   command's (it emits nothing). The keychain's five services decide after both, allowed only with a keychain
   `"read"`, so no prefix reopens them. `Policy::decide_service` answers a lookup the same way, for
   `sandbox why --service` and the block question. nono's `(deny default)` never allows `lsopen` (Launch Services'
   launch requests: `open`, an app's own launches) or `appleevent-send` (`osascript` scripting another app), so they're
   closed until `Apps::Open`, from `allow_launching_apps = true` (in `yolo`, or the user's own files), emits both
   (`Apps::Denied`, the user's `false`, closes yolo's); its services (Apple Events, quarantine, and what `open` looks up, from the kernel's record) join the
   list. The host probe runs git, TLS (curl, python, node), `id`, and `defaults` in `strict` with only the built-in
   list, and `open` of a link in `strict` with launching apps open. Verified in #249: with both closed, `open` of
   a script, an app, or a link and AppleScript to Terminal fail with -54, and nothing runs on the host.

**Symlinks.** Seatbelt compares real paths, so each rule also gets its real-path form (`/var` is `/private/var`). A
deny follows every symlink: it closes what a link points to as well. A grant follows only links no sandboxed session
of the project could have made, since one it made would move the next launch's grant wherever it pointed (pi's
`crashes.json` replaced with a link to `~/.zshrc`). A grant never follows its own last component, and never a link
inside the launch's checkout or source checkout (a session in the whole checkout writes there, even when this
launch's workspace is a subfolder) or covered by another write grant, as written or resolved, computed to a fixed
point. Below a link it doesn't follow, it keeps the rest as written, which matches nothing while the link is there.
The daemon logs each such grant and `piton sandbox show` warns about it; a user who links a granted folder elsewhere
grants the real path. A link outside the checkout made by a session elsewhere whose `write` was wider is still
followed when this launch's grants don't cover it. A sandboxed launch is refused when the workspace path given goes
through a symlink inside a git checkout that leads out of it; the user's own links outside any checkout work.

`PITON_RUNTIME`, `PITON_CONFIG_HOME`, and `PITON_SESSION_TOKEN` are set by the daemon after the mise environment.

**The network gate** (`launch/sandbox/network/`, `session/network.rs`, `pi/catalog.rs`). A session whose network is filtered gets a gate:
an HTTP proxy on 127.0.0.1 in the daemon (`CONNECT` any port, plus absolute-form plain HTTP). `piton sandbox-exec`
sets the proxy variables to it after applying the sandbox, over whatever the environment had, with
`NODE_USE_ENV_PROXY=1` and the `ssh` wrapper (`launch/sandbox/network/ssh.rs`: a link to `piton` in `<state>/net/bin`, readable and not
writable from the sandbox, which runs the real `ssh` with `ProxyCommand=piton gate-connect %h %p`) first on `PATH`.

- **Who asks.** Each sandbox the session starts gets a proxy user and a random credential: pi's (`pi`) at launch, and
  each bash call's (its tag) when the daemon runs it (`Network::issue`), forgotten when the call ends. A connection
  without a known user and its credential gets `407`. Seatbelt's port allow alone isn't enough: a `standard`
  session's `*`, a `localhost:*` entry, or a `*:PORT` reaches any gate's port, and a gate chained to fnox would hand
  another session that session's secrets; per-sandbox credentials also keep pi's process from borrowing a call's
  narrowed grant.
- **When.** The gate runs when the network is `Filtered`: `*` denied or absent, or `*` allowed with a named-host entry
  Seatbelt can't enforce (a host denied, or a host allowed under a port deny). An open network with only port entries
  is `Direct` and has no gate, as Claude Code's and Codex's networks have none until a domain rule needs their proxy.
- **Deciding.** The gate sends the session `NetworkAsk`; `network::Network::decide` checks, in order, that sandbox's
  most specific matching entry when it names a host (`Policy::network_entry`), pi's trusted hosts
  (`pi::catalog::Catalog`), the hosts allowed for the session, and a generic entry (`*`, `*:PORT`, `localhost…`), so
  `"*" = "deny"` refuses without asking but never cuts pi off from its providers; otherwise the connection waits on a
  question, one at a time in the session's slot, connections to the same destination joining it. Answers: allow for
  the session; allow and save (`"host:port" = "allow"`, `grant::Lift::Network`), which also joins every live
  sandbox's policy so it applies at once (Seatbelt doesn't change, so no restart); not now. No answer saves a denial,
  as in Codex. No answer in `approval_timeout`, or no client, refuses it with `403`.
- **pi's trusted hosts** (`pi/catalog.rs`): the hosts of every provider built into the installed pi and the sign-in hosts
  in its OAuth code, read by running `builtin.mjs` with Node against pi's package folder (found from its launcher,
  npm's link or aube's shim), once per package folder; `baseUrl`s in the agent directory's `models.json`; and
  remote MCP `url`s in the user's own MCP files (the global tier's `mcp.json` and `mcp.local.json`, every level's
  `mcp.local.json`). Never `models-store.json`: pi rewrites that cache at startup, so the sandbox can write it, and pi
  uses its `baseUrl`s with the user's keys (#362). Files are re-read when their modification time changes. The same
  module lists discovered models (untrusted) for the provider selector.
- **Resolved addresses** (`launch/sandbox/network/addresses.rs`). An allowed name is resolved once and an address that's loopback,
  unspecified, link-local, multicast, broadcast, cloud metadata, or one of this Mac's interface addresses (IPv4 inside
  IPv6 forms judged as the IPv4) isn't dialed; an IP the user listed is exempt.
- **Refusals** answer `403` with the reason as the status phrase and are recorded on the call (`gate`'s
  `network-outbound host:port`), so a failed call lists them without asking again. A direct connection the sandbox
  refused (a program that ignores the proxy variables) is the kernel's `network-outbound <ip>:<port>` record: an
  approved rerun lifts it (`grant::all` opens the network).
- **fnox.** Behind fnox's proxy, `piton sandbox-exec` reports fnox's `HTTPS_PROXY` over `agent.sock` with a one-time
  token (`PITON_GATE_TOKEN`, the helper's own variable) before applying the sandbox, and the gate then forwards each
  allowed tunnel as a `CONNECT` to fnox (`Route::Upstream`); connections wait up to 10 s for it. fnox 1.50 with
  permissive egress tunnels any port (checked: `CONNECT github.com:22` returns the SSH banner).
- **Git for workers** (`launch/git.rs`) gets a gate of its own while it runs, decided by the source's policy alone
  (nobody to ask), dialing directly.
- `piton sandbox gate` (hidden) starts a gate for a workspace's policy for the host probe.

Known limits: in `standard` and `yolo` the network is open, so anything readable can leave, and localhost TCP services are reachable; a trusted provider host is reachable by anything in pi's process; pi's own credentials are readable, except keys behind the
credential proxy ("Launching pi"); macOS only.

**Protected files** are everything a later pi loads from the workspace that could widen what an agent may do without
asking, or run code outside the sandbox: the source checkout's `.piton/` (except `.local/`); pi's project `.pi/`
folders (settings, MCP servers, extensions, skills, system prompts), which pi reads from the working folder; fnox's
config files (`fnox.toml`, `.fnox.toml`, `.config/fnox.toml`, and their `.local` and `fnox.<profile>.toml` forms),
which decide where secrets go and which commands fnox runs to fetch them. Both are protected at any depth in the
workspace's checkout, since a session (and fnox) can start in a subfolder. `AGENTS.md`, rules, and
`.agents/skills/` aren't protected: they change what the agent is told, not what it may do. A skill's `allowed-tools`
grants nothing unless the user opts in with `skill_grants = true` in `[tools.permissions]`; with it on, an agent that edits a
skill can grant itself the tools it lists. Without protection, a sandboxed agent in the source checkout could add `allow = ["bash(*)"]` to
`[tools.permissions.host]` in `.piton/config.toml`, and a host session in the same checkout would run any command without asking from its
next tool call.

Outside the checkout, every configuration file the user edits by hand is protected the same way, so an agent can help
set up a project or a dotfiles repository by proposing changes: the launch's configuration levels (the global tier
and its `agents/`, `.piton/` and `.agents/` in parent folders), pi's agent directories (the host's and a per-folder
one, except the runtime state the sandbox lets pi write: sessions, logins, catalogs, logs, locks), the files fnox
loads for the launch (`fnox config-files`), and the real targets of the symlinks in those levels and agent
directories (a global `config.toml` or pi's `settings.json` linked into dotfiles), shown and written at the real
path. Nothing else on the floor is ever proposable: the daemon's state folder and control socket, credentials, and
the folders on the way to these files. An agent directory or link target that holds the home folder or a checkout,
or overlaps the state folder, a credential deny, or a filesystem `"deny"`, is left out whole: a session in a
parent folder can edit that folder's `.agents/` (its own there), and a link it planted there to `~/.ssh` must not
make keys proposable from a session below. `agent/protected.rs` (`Protected`) is the one definition: the daemon builds
it at every launch, sandboxed or on the host, from the levels, the agent directories the launch resolved, and the
fnox files, and gives pi a copy in `PITON_PROTECTED` (set after the workspace's environment), which `piton config
protected` answers from. Outside a session it answers from the workspace (levels and `PI_CODING_AGENT_DIR` or
`~/.pi/agent`, no fnox files). Changing that copy only changes where pi sends a write: the sandbox still denies the
direct write, and the daemon checks the path against its own. The sandbox closes these files as rules 4 and 6 of
"Agent access" say; no sandboxed command approval ("Allow once" in the sandbox, `grant = "all"`) opens them. A command
the user chose to run on the host has no sandbox, so they're open to it like any program the user runs.

Changes are gated where they're written:

1. **OS sandbox.** Each sandboxed launch carries raw Seatbelt rules: `(deny file-write* ...)` with regexes
   for `<source>/.piton` (excluding `<source>/.piton/.local`), `.pi` at any depth in the checkout (excluding
   the `.pi/settings.json.lock` folder itself, which pi creates to read the project's settings), and fnox's config
   file names at any depth. Every
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
   apps show a diff without reading the file. Its `prompt` says what the file controls, since the diff is the user's
   only check: "can loosen the sandbox" (the global `config.toml`, any `config.local.toml`), "runs code in every
   session" (an agent directory's `extensions/`, `npm/`, `git/`, `bin/`, `settings.json`, `trust.json`, `mcp.json`; a
   project `.pi/`'s extensions, settings, and MCP servers; `config.host.toml` and its local file; a level's
   `mcp.json` and `mcp.local.json`), "decides where secrets go" (fnox files), a linked file by the path it's reached
   through. On approval the daemon walks the path from the file's root (the checkout, the level, the agent directory,
   or a linked file's real folder) without following any symlink, so a folder the agent swapped for a link while the
   user decided can't redirect the write; the folders above a root outside the checkout are fixed in place by the
   sandbox.
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

### Asking when the sandbox blocks

When the sandbox blocks something a sandboxed agent needs, the user is asked (#241), and nothing runs fully
unsandboxed unless the user chooses the host for a command (#335). Permissions decide whether the agent may make a
call; the sandbox decides what a running process may touch; when the sandbox blocks something, the user decides.

**The floor** is what no approval lifts: protected files, every configuration level and pi's agent directory (and
the folders on the way to them), the daemon's state and control socket, the user's own `"deny"` and `"read"` entries
and `false` switches, the files
fnox loads, and, while fnox is on, the credential denies (the sandbox could run `fnox get`). The configuration in it
changes only through a write the user approves ("Protected files"); the rest never. Only a command the user chose to
run on the host passes it, since a host run has no sandbox at all. `all` is the launch's policy with everything else open: writes
below every entry of `/` (nono refuses a write grant on `/` itself), every socket connect and bind allowed first, and
TCP listening open, then the launch's
rules with every liftable deny left out, so the floor still decides last
(`launch/sandbox/grant.rs`, where each deny's source and target say whether it's floor, matched exhaustively). Git's
control files and the credential denies (fnox off) aren't floor: `husky install` and Git's keychain sign-in work once
approved.

**Every bash call runs through the daemon.** A sandboxed process can't apply a new sandbox (pi-findings: "Sandboxes
don't nest"), and a refusal can't be traced to the command that caused it afterwards: macOS reports the refused
process's pid, which is usually gone, and hides another process's environment from a non-root reader. So in a
sandboxed session the daemon, outside any sandbox, starts every bash call and `!` command itself, through
`piton sandbox-exec` (`agent/run.rs`), each in its own sandbox: the session's policy, unless a grant or an approval
widens it, tagged with the call. pi, its extensions, and MCP servers stay in pi's sandbox, which carries the session's
tag; a process can't leave or widen its sandbox, so only the daemon can start one with more. Where a call runs is one
sum type the daemon decides and the runner matches (`Runtime`: the session's sandbox, a widened one with what widened
it, or the host with what put it there), so nothing runs on the host by default: a host run starts the shell itself,
with no helper and no policy, and only a saved `runtime = "host"` entry or the user's answer makes one.

- The permissions extension registers pi's own bash tool definition (rendering, truncation, session environment) with
  operations that send `run_command {token, call_id, command, cwd, timeout, env, reason?, rerun?}` over `agent.sock`
  and stream the answer (output chunks, then the exit, a timeout, or why it didn't run), and handles `user_bash` the
  same way (`call_id: "user:<n>"`). Outside a daemon-launched sandboxed session pi's own bash stays.
- The daemon accepts a request only for a bash call pi reported running (`tool_execution_start`) with exactly that
  command, once per call; a rerun only when that call's arguments asked for it (`rerun: {runtime}`). A typed
  command always runs at the session's policy. The daemon picks the shell and `shellCommandPrefix` from pi's settings
  files (the agent directory's and the workspace's `.pi/`, neither writable from the sandbox).
- At the session's own policy the command gets the environment the extension sends (pi's shell environment); widened,
  the session's launch environment, so a request can't point a granted command at its own `PATH`. Either reaches only
  the command: `piton sandbox-exec` runs outside any sandbox until it applies one, and the dynamic loader reads `DYLD_*`
  before `main`, so every sandboxed start (pi, environment tools, Git for workers, these commands) gives the helper only
  its own variables and carries the command's environment in `PITON_LAUNCH_ENV`, which it unpacks after applying the
  sandbox. On the host, the command gets the session's launch environment, the workspace's environment tools'
  values included (as Claude Code passes its own environment to an unsandboxed command), without `PITON_RUNTIME`,
  `PITON_SESSION_TOKEN`, and `PITON_AGENT_SOCKET`, so it isn't taken for the session's pi. At launch the daemon
  records which of those values came from a file the launch's sandbox can write (`launch/env.rs`,
  `SandboxEnv::agent_writable`; a tool that names no file counts as writable), and the block question shows them on its
  host side.
- When a call runs anywhere but the session's own sandbox, the daemon sends `{type: "ran", runtime: "sandbox" |
  "host", by: "grant" | "approval"}` before its output, and the extension adds a line saying where to the result. A
  sandbox run by a saved entry only says the entry widened it (its grant may be narrowed); an approved rerun says
  everything but the floor was open.
- As in pi's bash, the call ends when the shell exits and its background jobs keep running; the agent's stop (the
  connection closes) or the timeout kills the call's process group.
- Limit: commands the daemon runs don't go through fnox's proxy. And pi reports a call running before its permission
  gate decides, so another command could ask the daemon to run a call that's still waiting for an `ask` answer; only a
  command a per-command grant covers runs widened that way, and a rerun always asks the user.

**Per-command entries** (`[[sandbox.commands]]`, `match = "bash(...)"`, a required `runtime`: `"sandbox"` with
`grant = "all"` or a table of the `[sandbox]` grant lists, or `"host"` with no grant; in a repository's committed
file, only once the user trusts it;
`CommandRuntime` in the model, so a host entry with a grant can't be represented): a matching call runs where its
entry says from the start, never failing first. The
daemon matches with a Rust port of the permissions extension's Bash analysis and glob (`agent/command.rs`), held
identical by one fixture file run by both test suites (`pi/tests/fixtures/bash-matching.json`): every command node must
match the same entry, and a command that writes a file through a redirect gets no grant. A narrowed grant's policy is
the launch's resolved again with the grant's lists added, so it never reaches the floor either.

**Who asks.** Piton asks when it sees the block: when a call run in the session's own sandbox exits non-zero, the
socket asks the session (`SessionCmd::RunFailed`, decided by `Commands::after_failure`) whether the call has a
recorded block an approved rerun lifts (`grant::command_block` against `all`). The kernel's record usually arrives
within ~5 ms, so the socket looks every 10 ms for at most 50 ms, and stops at the first look that finds a block. A
success never waits. Credential reads (ssh's key probe on every SSH Git command), blocks the floor decides, and
silenced blocks are reported, not asked; so is a block while another question holds the slot. Every operation's
refusal reaches its call, since the default deny is tagged (below): a preferences write (`user-preference-write`, which
`all` lifts), a blocked `open` (`lsopen`, which `all` lifts), `osascript` scripting another app
(`file-issue-extension`, refused even with apps open), a signal to another process or a `sysctl` write (floor: only a
host run passes them). Refusals programs hit and recover from
(`grant::INCIDENTAL`: `open`'s and `osascript`'s `sysctl-write kern.grade_cputype` and connect to syslog's socket) never
ask, and are listed only when nothing else was recorded. For `open` and `osascript`, the target check below then
decides. Two refusals never reach a call: `job-creation` (`launchctl submit`, `at`) and `forbidden-exec-sugid` (a
setuid program: `ps`, `top`, `sudo`) aren't decided by the profile's default, so their records carry no message; the
agent's `rerun` covers them. When asked, the socket sends `blocks` and `blocked`; an approved rerun's output and exit become
the call's result, and a denial or no answer sends `not_rerun` and the first run's exit. A failed call's result ends
with the blocks recorded for it (`operation target`, with why one isn't asked about), even when nothing is asked. After
the user denies a block question (Piton's or the agent's), an agent `rerun` of the same command is refused for the rest
of that run. Records
later than 50 ms (node's first refusals can take seconds) aren't chased: they stay on the call, and the sandboxed bash
tool's system-prompt guideline tells the agent that such a failure is likely a block and to ask with `rerun`.

**The block question.** The agent asks for a rerun with `rerun: {runtime: "sandbox" | "host"}` (and a `reason`) after
a command failed in the sandbox and Piton didn't ask; `runtime` is only a hint for the side the question opens on, and
anything else in `rerun`, such as a grant, is refused. The question is Piton's own, a typed value the daemon holds
(`agent/question.rs`): the command, who asked (`Asker`: the agent with its reason, or Piton, which opens in the
sandbox), the refusals the kernel recorded for the call's last run, what `open` would do, the session's
agent-writable variables, and the runtime it opens on (the hint: sandbox at `all`, or the host; both are always
offered). Answers: once, deny, or always
for this command or an edited pattern, in this project (its source checkout's `.piton/config.local.toml`) or
everywhere (the global `config.toml`), each in the chosen runtime; always appends one `[[sandbox.commands]]` entry,
`runtime = "sandbox"` with `grant = "all"` or `runtime = "host"`, and new sessions use it. The daemon renders the
question per connection, by the protocol its hello chose, so the view protocol isn't shaped by pi's dialogs:

- `view` clients (the apps) get it typed in the session state (`questions`) and answer with `session.decide`. The apps
  draw the runtime as a switch (left and right arrows with a keyboard), the pattern inline, and on the host side the
  agent-writable variables in a disclosure folded by default.
- `rpc` clients (`piton attach`, so pi.nvim) get pi's dialog record (`select`), inserted like the protected-write
  dialog and kept out of the `view` state's `dialogs`. Its first option, "Choose an answer below", decides nothing and
  shows the question again (clients highlight the first option, and pi's `select` has no unselected state), so Enter
  alone never answers; its last switches the runtime and shows it again; a pattern is a second dialog, an `input`
  prefilled with the command and subcommand plus ` *`. On the host side the title lists the agent-writable variables,
  ten per file. A label of the other runtime's dialog denies.
- Both map to the same typed answer, and answering either closes both. This is the first Piton question; protected
  writes and process questions are still pi dialogs (#338 generalizes it).

Only a client answers. One
question at a time per session, shared with protected writes; no answer within `approval_timeout` (host
configuration, 10 minutes by default, or `"never"`), a dismissal, or the run ending means it doesn't run, and the agent is told which. `open` and `osascript` are decided by their target first
(`agent/open/`): `osascript` is refused; `open` of an `https://` link says it opens in the browser; another URL
scheme names the app Launch Services hands it to (`LSCopyDefaultApplicationURLForURL`, asked by the daemon) and is
refused when none does or the app is one the sandbox could have written; a file or app the sandbox could have written,
as a path or a `file:` URL (decoded to its path), is refused, since macOS would run it unsandboxed. These checks
guard both runtimes. A sandboxed rerun opens every Mach service and launching apps (`grant::all`), so an approved
`open` opens.

**Blocks with nothing to rerun** (an MCP server, an extension, pi itself). Every deny Piton emits carries
`(with message "piton-<tag>")`: `<session>-<random>` for pi's sandbox, `<session>-<random>-<n>` for a call's. A
leading tagged `(deny default)` replaces nono's untagged one (a profile has one default action, and the last wins), so
every refusal no rule decides carries the tag; a leading tagged `(deny file-write*)` labels writes outside every grant
(Piton asks nono for no write grants, so every later grant still wins); and the keychain's Mach denies are repeated
tagged. A blocked process's refusal an entry can't lift (a signal, a preferences write) is a notice; an incidental one
(`grant::INCIDENTAL`) is only logged.
The daemon runs one `log stream` (outside any sandbox; `log` refuses inside one) for the Sandbox kernel extension's
tagged reports and routes each to its session by tag (`daemon/refusals.rs`). A call's tag: kept for that call (shown
when it's rerun); a typed command's (`<session>-<random>-u`): the user saw it fail. The session's tag, or a call
forgotten since (its background job): a blocked process, every one brought to the user, in order, and each process,
operation, and target once. The session asks one question at a time (protected writes, block questions, process
questions); a process question waits for the slot, is shown as soon as it frees, and is asked again when its dialog
leaves unanswered with the run it showed in. It's offered as the `[sandbox]` entry that lifts it
(`grant::lift_for`), one key set in its table through the config writer's dotted keys (`KeyPath`, a path quoted):
`"<path>" = "write"` in `filesystem` for a write (a file or a folder alike), `"allow"` in
`network.unix_socket_connections` for a socket it connects to (one in a credential folder too, which that entry opens
for connecting) or in `network.unix_socket_bindings` for one it creates, `"<name>" = "allow"` in `macos.services`,
`allow_tcp_listening = true` for a TCP listen (`network-inbound local:*:<port>`). An entry is offered only when the
launch's configuration, resolved again with it as the nearest file's, allows the refused access. "Everywhere" saves the folder above the path's first component
that names the source or checkout as pi and Claude Code name per-folder state (the path with `/` made `-`), unless that
folder is `/`, home, directly in home, or holds a project. The question names each answer's exact entry: Allow in this
project or everywhere (saved, then a session restart, stopped and resumed from its file), or Not now (a dismissal
too). It saves no denial, as Codex's prompt doesn't: a block the user never wants is a `"deny"` they write, and a
block an entry of theirs decides (`DeniedBy::UserDeny`: a `"deny"`, a `"read"` written to, a `false` switch, a
service deny) is only logged, never asked or noticed. A path with the refusing process's pid as a whole
number in a component is never offered as that path: the folder above it is, when it isn't under a temp folder, the
home folder or directly in it, and lifts the block; otherwise `NoLift::PerProcess` makes it a notice. A process's
credential read (nono's deny groups), keychain block (`DeniedBy::Keychain`: `Readable` with fnox off,
`ClosedForFnox` with it on, matched exhaustively), and write to Git's control files are never asked about: the
deciding deny (`grant::DeniedBy`) makes them a warning notice (`session/notice.rs`), once per process and path.
Refusals by the floor, fnox's files, and operations no entry lifts are notices too. Every notice that can be quieted
ends with the entry that makes it the user's own (`session/notice.rs`, `Quiet`): a `"deny"` in the block's own table
for a path or a host (the folder, unless that's too broad; it closes it to approved commands too, and the notice says
so), `allow_git_control_files = false` for Git's files, and a `silence` entry for what has no path or host (a Mach
service or a TCP listen, matched by target and optionally the process name). Presets add `silence` entries of their
own (`SilenceSource::Preset`), a path among them for `claude`'s socket probes. Notices go to the session view
and, as a `notify` record, to attached pi.nvim clients. macOS reports
only a process's first refusal and folds repeats; refusals inside commands that still succeeded aren't surfaced.

Every request and its outcome is logged in the daemon log, never a command's output.

### Deploying from a sandbox

Status: planned. Today installs are host-only (`mise run cli:install`, `apps:mac`, `apps:iphone`, `apps:ipad`).

An agent in a sandboxed session may deploy what it built: the daemon, the Mac app, or the iOS app on the iPhone or
iPad. This doesn't widen what the agent can do: the code it wrote runs on the host once deployed, whoever runs the
deploy, so a person approving each one adds no safety. What deploying must not do is disrupt work in progress, and
that's what gates it: the daemon decides when a deploy may take effect, and an agent can't skip that check.

The sandbox keeps every restriction it has. A deploy is a request, and the host does the work:

- **Deploy socket.** The daemon listens on a second Unix socket (`~/.local/state/piton/deploy.sock`, `0600`). The
  sandbox closes the state folder, so this feature adds an allow for it, making it the only Piton socket the sandbox
  allows besides `agent.sock`. It speaks one request, `{deploy: daemon | mac | iphone | ipad,
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
