# Daemon and CLI

## Running it

```sh
mise run cli:build
cli/target/debug/piton daemon                # foreground; ctrl-c stops every pi
cli/target/debug/piton start ~/Projects/foo  # new session in the sandbox
cli/target/debug/piton start ~/Projects/foo --host --prompt "hello"
cli/target/debug/piton ls
cli/target/debug/piton stop --session <pi session id | session file | piton key>
cli/target/debug/piton trust status ~/Projects/foo  # whether its committed grants apply (Repository trust, below)

mise run cli:install                         # stable binary in ~/.cargo/bin + launchd agent (host only)
piton service status | piton service uninstall
piton --version                                 # package version and the git commit it was built from
piton setup check                               # what this host needs, each step with its fix (--json: the Mac app's)
piton setup apply cli                           # do a step piton can: configuration, cli, or state
tail -f ~/.local/state/piton/logs/daemon/current.log  # bounded daemon log
```

## Workers

Copy-on-write clones for agents; see "Workers" in [design.md](design.md).

```sh
piton worker create "Fix login" --issue KID-5   # from the current checkout: ../<repo>--fix-login
piton worker ls
piton worker delete ../<repo>--fix-login [--force]
```

Per-project settings live in `.piton/config.toml` in the source checkout (read only from there, never from a worker):
`[workers] skip` lists paths not copied into new workers, `[sandbox]` adds grants for sandboxed pis (see "Sandbox"
below).

Creating or deleting a worker fast-forwards its source checkout. The fetch and the fast-forward run in the sandbox a
session in the source gets, with your hooks on; nothing of it runs outside the sandbox. The fetch authenticates the way
an agent's Git does: over SSH through your SSH agent with `~/.ssh/config` and `~/.ssh/known_hosts` readable (the
`standard` profile's entries), over HTTPS with the keychain readable (`"~/Library/Keychains" = "read"` in
`[sandbox.filesystem]`). When it can't, creating a worker fails naming that; deleting one leaves the source as it is, with a warning.

An update that changes protected files (`.piton/`, `.pi/`) needs your approval, since the sandbox can't write them.
Piton asks before changing anything, naming the files. Approved, Piton writes exactly those files from the upstream
commit itself, and the rest of the update, with your hooks, still runs in the sandbox, so a hook can't change any
other protected file. Declined, nothing changes (when deleting, you can delete without updating the source). The
Neovim plugin asks; `piton worker` refuses and prints the flag that answers it (`--approve-source-update <sha>`, or
`--skip-source-update` when deleting). To answer once for all updates, add `piton(git-pull)` to the source's
`[tools.permissions]`: in `allow`, the files are applied without asking; in `deny`, creating a worker is refused and deleting
one leaves the source as it is.

## pi.nvim

```lua
process = {
    kind = "attached",
    cmd = { "piton", "attach" },
    terminate = { "piton", "stop", "--session", "${{SESSION_ID}}" },
}
```

`piton attach` launches a fresh pi in the tab's workspace (sandboxed), relays pi RPC, and exits 0 on detach, 3 when pi
ended, 1 when the connection was lost. `:PiContinue` / `:PiResume` onto a session that's live in Piton attach to it.

## Configuration

Optional, in the global configuration folder `$XDG_CONFIG_HOME/piton/` (default `~/.config/piton/`; `--config-home`
for tests). Inside a session, `piton` uses the session's (it asks the daemon). Every key has the default shown.

- **`config.host.toml`** (and `config.host.local.toml`, not committed): how the daemon runs on this host and this
  host's preferences. Only this file can stop the daemon from starting; a typo in it does.
- **`config.client.toml`:** the apps' preferences, grouped (`[appearance]`), each for every device or per device
  (below); the host serves each client the values for its device.
- **`config.toml`** (and `config.local.toml`): the defaults every project inherits, the chain's farthest level. The
  daemon reads its `[sandbox]` and `[workers]` for each launch.

Unknown keys are refused. The daemon watches the files: a change applies to the next session at once, and the apps
show `log`, `[env]`, and `[integrations]` changes as waiting for a restart (`piton service install`). An invalid edit keeps
that file's last good values and the apps say why. The apps change the keys below (`socket` shown read-only), keeping
your comments and order, writing through a symlink into a dotfiles repository, and moving a key to the `.local` file
when you check Local. A list they write goes one item per line once it has two items, and its items keep their
comments. The whole layout, including repository and parent-folder levels and where each key may be set,
is in "Configuration and state" in [design.md](design.md); `piton config check` validates a workspace's configuration
(`[tracker]` and `[forge]` included) and the host and client files, naming each error's line and key, lists the files
it reads, and warns about a path under another user's home or a secret in a file Git tracks; `piton config set KEY
VALUE [--global | --scope host|client] [--local]` changes one key the same way the apps do; `piton config validate
TABLE VALUE` checks one table's JSON value (`piton config validate forge '{"provider": "none"}'`) without a file.

`config.host.toml`:

```toml
socket = "~/.local/state/piton/piton.sock"  # debug builds only, for tests: release builds ignore it (the sandbox
                                            # closes the socket's folder, and the Mac app uses this default)
log = "~/.local/state/piton/logs/daemon/current.log" # five files, 2 MiB each
roots = []                # where the apps may browse and start sessions; none until you list some,
                          # e.g. ["~/Projects"]; piton attach and the CLI aren't limited
favorites = []
default_attach_mode = "sandbox"   # mode for `piton attach` when none is given
stop_empty_session_after = "10s"  # stop a pi with no messages this long after its last client leaves
approval_timeout = "10m"  # an approval (a blocked command, a held connection) waits this long for your answer;
                          # then the action doesn't happen. "never" waits until it's answered, dismissed, or the
                          # run ends, holding the session's one question slot meanwhile
keep_awake = true         # hold the Mac awake while sessions are live; toggled from the apps
                          # (put it in config.host.local.toml to keep toggles out of a dotfiles diff)

[pi]
command = ["pi"]
args = []                 # added after `--mode rpc`

[env]
login_shell = true        # base environment from `$SHELL -l`

[stop]
terminate_after = "15s"   # after closing stdin, before SIGTERM
kill_after = "3s"         # after SIGTERM, before SIGKILL

[integrations.mise]       # `mise env -C <workspace>` for every launch, trust for workers
enabled = true            # unset: used when installed, off otherwise
command = ["mise"]        # found through the login shell's PATH

[integrations.fnox]       # experimental: every launch behind fnox's credential proxy (below)
enabled = true
command = ["fnox"]        # e.g. ["fnox", "-P", "agent"] for a profile

[integrations.tailscale]  # remote access: while it's used, the apps are served on this Mac's tailnet addresses
enabled = true
command = ["tailscale"]
port = 7357
heartbeat = "10s"         # ping each app this often; drop it after three unanswered pings
retry = "10s"             # while Tailscale isn't up, try again this often

[debugging]
log_level = "info"        # host logging level ("info" or "debug"); `piton logging` and the Mac app switch it
                          # until it expires or the daemon restarts
session_analysis = true   # development installs only (development.md)
```

Durations are a whole number and one unit: `ms`, `s`, `m`, or `h` (`"500ms"`, `"10s"`, `"10m"`, `"1h"`). Only
`approval_timeout` also takes `"never"`; zero isn't a duration.

`config.client.toml`:

```toml
[appearance]
text_font = "Inter"       # one value: every device
code_font = { default = "Iosevka", mobile = "Fira Code", ipad = "Berkeley Mono" }
```

A client setting is one value for every device, or a table of device sections: `default` (every device), `mac`,
`mobile` (iPhone and iPad), `iphone`, `ipad`. The most specific section that applies wins: a Mac reads `mac`, then
`default`; an iPhone `iphone`, then `mobile`, then `default`; an iPad likewise with `ipad`. With no section for a
device, the app's default applies. Unset, the fonts are the system font. Settings > Appearance sets a font's scope
(Every device, Every Mac, iPhone and iPad, Every iPhone or iPad, or only this device, kept in the app); choosing a
section there also removes the narrower sections that would hide it on that device.

A pi gets `PITON_AGENT_SOCKET`, `PITON_AGENT_TOKEN`, and `PITON_BIN` after the workspace's mise environment. What
else a session needs to know (its runtime, kept grants, protected files, global tier, workspace) `piton` and the pi
package ask the daemon for (`facts`, see "The channel to the daemon" in `docs/design.md`).

For a sandboxed launch, `mise env` runs inside the launch's sandbox, so whatever a workspace's mise configuration runs
(`exec()` templates, `_.source` scripts) is contained. A configuration that reads a secret manager while `mise env`
runs (`exec(op read …)`) fails there and the launch is refused with mise's error; keep secrets out of the mise
environment and use the credential proxy (below).

Everything in pi's environment is visible to the agent and every command it runs. `[tools.permissions]` rules steer the
agent; in sandboxed sessions they aren't the boundary, the sandbox is.
### Integrations

Piton runs mise, fnox, and tailscale when they're there and works without them. Each has an `[integrations.<name>]`
table:

- **Unset `enabled`:** used when its command is installed, off otherwise. A Mac with neither works with both off:
  pi gets the login shell's environment, and only this Mac connects.
- **`enabled = false`:** off.
- **`enabled = true`:** used, and an error when it isn't installed: piton never runs it, the apps' Settings show a
  warning on the setting, and each operation that needs it fails with an error naming the setting (a launch or a
  worker for mise; every launch for fnox; remote access for tailscale).

Remote access has no switch of its own: the apps are served on the tailnet exactly while Tailscale is used.

The daemon decides each integration's state when it starts, so changes apply after a restart. `piton setup check`
requires only pi and lists the integrations with their state.

### Credential proxy (fnox, experimental)

With [fnox](https://fnox.jdx.dev/guide/proxy)'s credential proxy, pi never holds your secrets: its environment has
placeholders, and fnox, outside the sandbox, puts the real values (read from 1Password or another secret store) only
into the requests its rules allow. An agent that reads `GITHUB_PERSONAL_ACCESS_TOKEN` gets `fnox_proxy_...`.

When fnox is used (`[integrations.fnox]`) and a fnox config with a `[proxy]` table applies to the workspace, every session,
sandboxed or on the host, starts as `fnox --non-interactive proxy run --no-sandbox -- <the usual command>`. fnox finds
its config itself: the global `~/.config/fnox/config.toml`, and `fnox.toml` files in the workspace and its parents.
Where no `[proxy]` applies, launches run without it. `--no-sandbox` turns fnox's own sandbox off: Piton's applies.

A global config for a Mac whose secrets live in 1Password:

```toml
# ~/.config/fnox/config.toml
[providers.keychain]
type = "keychain"
service = "fnox"

[providers.op]
type = "1password"
token = { secret = "OP_SERVICE_ACCOUNT_TOKEN" }  # a service account (below)

[secrets]
OP_SERVICE_ACCOUNT_TOKEN = { provider = "keychain", value = "OP_SERVICE_ACCOUNT_TOKEN", env = false }
GITHUB_PERSONAL_ACCESS_TOKEN = { provider = "op", value = "op://Agent/github-token/credential", env = false }
GH_TOKEN = { provider = "op", value = "op://Agent/github-token/credential", env = false }   # the same token, for gh
CLAUDE_CODE_OAUTH_TOKEN = { provider = "op", value = "op://Agent/claude-token/credential", env = false }

[proxy]
egress = "permissive"   # fnox's default, "strict", refuses every host without a rule: pi's model provider,
                        # web fetches, installs. Use strict with `allow_domains` if you want an allowlist.

[[proxy.rules]]
secret = "GITHUB_PERSONAL_ACCESS_TOKEN"
domain = "api.githubcopilot.com"   # the GitHub MCP server (`Authorization: Bearer ${GITHUB_PERSONAL_ACCESS_TOKEN}`)
header = "authorization"

[[proxy.rules]]
secret = "GITHUB_PERSONAL_ACCESS_TOKEN"
domain = "api.github.com"          # the workflows' GitHub API calls, with the MCP server's token
header = "authorization"

[[proxy.rules]]
secret = "GH_TOKEN"
domain = "api.github.com"          # gh
header = "authorization"

[[proxy.rules]]
secret = "CLAUDE_CODE_OAUTH_TOKEN"
domain = "api.anthropic.com"
header = "authorization"
placeholder = "sk-ant-oat01-placeholder"   # claude checks the token's shape
```

- **A service account, not the desktop app.** The 1Password app asks for approval for each new process, so a session
  the daemon starts while you're away would wait for it. A [service account](https://developer.1password.com/docs/service-accounts/)
  with access to one vault (`Agent` above) needs no prompt: store its token in the keychain once (`fnox set --global
  --provider keychain OP_SERVICE_ACCOUNT_TOKEN`, which prompts for it), and fnox reads it at each launch (4-5 s). With several 1Password
  accounts and the desktop app, set `account` on the provider, or `op` asks which one.
- **Claude subscription.** `claude setup-token` makes a long-lived token for scripts; with it in
  `CLAUDE_CODE_OAUTH_TOKEN` behind the proxy, the claude binary (and pi's claude-code models) work without reading the
  keychain. In a sandboxed session, the `pi-claude-code` preset (Sandbox, below) gives pi's `claude` what it writes.
- **`gh`.** `gh auth login` keeps its token in the keychain, which stays closed to the sandbox, so in a sandboxed
  session `gh` goes out without a token. `gh` reads `GH_TOKEN` before its own login: declared as a secret with a rule
  for `api.github.com` (above), it gives `gh` a placeholder, and fnox sends the real token. With `env = false` it
  stays out of your shell, so `gh` in a terminal keeps its own login. This needs `gh` 2.104 or later (Limits, below).
- **Move secrets out of `mise.local.toml`** and other files pi's environment comes from: a secret still set there
  reaches pi as a real value.
- **Restart sessions after changing a secret.** fnox reads secrets when it starts a session; a running session keeps
  the old value.
- **Project fnox files are protected.** `fnox.toml`, `.fnox.toml`, `.config/fnox.toml`, their `.local` and profile
  forms (`fnox.<profile>.toml`), at any depth in the checkout: an agent proposes changes and you approve each, as for
  `.pi/`. Every other file fnox loads for a launch (`fnox config-files`: the global config, imports) is read-only in
  that launch's sandbox, at its real path through any symlink, and the folders on its path can't be renamed or
  replaced. A global config linked into a dotfiles repository is closed in sessions in that repository, which still
  start. An agent proposes changes to these files too (written at the real path once you approve), but no command
  approval opens them, and their folders are never writable.
- **What happens when something's wrong.** fnox enabled but not installed: no session starts. A broken fnox config:
  the launch is refused with fnox's error. A secret fnox can't read (1Password unreachable, a wrong token, a missing
  item): the session starts, and requests that need that secret are refused. The apps show a warning after the
  conversation ("fnox couldn't read these secrets when this session started: NAME (reason)"), and pi.nvim a
  notification and a status entry ("fnox: N secrets unavailable"). fnox retries a missing secret on the next request
  that needs it; the warning clears when the session restarts.
- fnox itself runs with the daemon's environment (the login shell's), never the workspace's: a repository's
  `mise.toml` can't pick fnox's profile or config, or the `HOME` it finds its config in. pi still gets the
  workspace's environment, with fnox's placeholders and proxy settings on top, except `FNOX_*` variables, which come
  only from the daemon's environment.

> [!WARNING]
> **Keep the keychain closed to the sandbox.** fnox runs inside the sandbox like any program, and macOS lets it read
> the keychain items it created without asking. If the sandbox can read your keychain, an agent can run `fnox get` and
> read every secret your service account can. Piton refuses a sandboxed launch behind fnox when the sandbox can read
> the keychain. So does the source update of a worker's creation, which runs your hooks in that sandbox (a deletion
> leaves the source as it is, with a warning): remove
> `~/Library/Keychains` (and any folder containing it, like `~/Library`) from `"read"` and `"write"` entries in your
> configuration (the global `config.toml` or a `config.local.toml`; `piton config check` lists the files), and the
> `pi-claude-code` preset's `login = "keychain"`; the refusal names what opened it. With the Claude token behind the
> proxy, the sandbox doesn't need it. Git over HTTPS authenticates through the keychain, so
> with it closed, the sandbox's Git (an agent's, and worker updates) fetches over SSH.

Limits:

- Credentials pi manages itself (`auth.json`: `/login` providers, MCP OAuth) aren't covered.
- A program that ignores the proxy variables or fnox's certificate sends the placeholder, and its request fails. Go
  programs on macOS read `SSL_CERT_FILE` only from Go 1.27: those built with Go 1.26 or older check certificates
  only against the system trust store, so they refuse fnox's ("certificate is not trusted"). `gh` 2.104 and later
  are built with Go 1.27; `go version -m "$(command -v gh)"` shows the Go version of a binary. fnox's certificate
  file includes the system's root certificates, so hosts fnox doesn't intercept still verify. Give an older tool its
  token another way, or run it on the host.
- Git over HTTPS doesn't work behind fnox: Git signs in with `Authorization: Basic base64(user:token)`, and fnox 1.48
  can't find a placeholder inside base64 (a proposal to add it, jdx/fnox#1151, was closed without merging). GitHub
  refuses the token as `Bearer`. With fnox on, use SSH remotes.
- fnox's daemon cache isn't used, since any process of yours can query it.

### Sandbox

A sandboxed pi works with no configuration. The sandbox is built in layers:

1. **The base**, the floor every launch gets (below). Everything that can be closed is closed.
2. **A profile and presets:** `strict`, `standard` (the default), or `yolo`, and the toolchain presets you turn on:
   built-in entries of the maps below.
3. **Your entries** in `[sandbox]`, in any `config.toml` of the chain: one map per resource.

The nearest file's value wins per entry, and the most specific matching entry decides, so any entry can close what a
profile or a broader entry opens (`"*" = "deny"` closes `standard`'s network). Host mode is the no-sandbox option.

**Two layers, separate.** `[tools.permissions]` rules decide whether the agent may make a call (run it, ask first, or refuse)
before it runs; the sandbox decides what a running process may touch. Neither changes the other. The whole pi process
is sandboxed (pi, its extensions, MCP servers, `mise env`), so a permission rule like `read(*.env)` stops pi's `read`
tool, not `cat .env` in bash, and turning it into a sandbox rule would block pi and your tools too. A path no process
may touch is a `"deny"` in `[sandbox.filesystem]`. Secrets don't belong in plain files at all: keep them in a secret
manager behind fnox's credential proxy (below), with the sandbox keeping its credential folders closed.

The base, the same idea as Claude Code's, Codex's, and Gemini CLI's:

- **Reads:** everything, except credentials and private data: SSH and GPG keys, cloud and registry credentials
  (`~/.aws`, `~/.config/gcloud`, `~/.kube`, `~/.docker`, `~/.npmrc`, `~/.netrc`, ...), keychains and password
  managers, browser profiles, Mail and Messages, shell history and rc files. The list is nono's deny groups, vendored.
- **Writes:** the workspace, the source's `.piton/.local`, temp folders, and pi's runtime state in its agent directory
  (sessions, logins, model catalogs, MCP auth and logs, and the locks pi takes even to read its settings and project
  trust). Not user caches: host tools reuse what's in them without checking, so a writable cache would let a
  sandboxed agent run code on the host. Not pi's settings, project trust, MCP servers, extensions, managed
  binaries, or packages, which host sessions load: pi changing a setting, trusting a project, adding an MCP server,
  or installing or updating a pi package from a sandboxed session fails, so do those in a host session. The agent
  can propose a change to those files with its edit or write tool (below). A session folder moved out of the agent
  directory (`sessionDir`, `PI_CODING_AGENT_SESSION_DIR`) needs a `"write"` entry.
- **Always closed:** protected files (`.piton/`, `.pi/`, except the lock pi takes to read `.pi/settings.json`), every
  configuration level, pi's agent directory except its runtime state, the files fnox loads, what their symlinks point
  to (a dotfiles repository), and the folders on the way to each, so none can be moved aside and replaced. Also the
  daemon's state folder (except `logs/` for reading and `agent.sock`), and the daemon's control socket.
- **Proposed changes:** every configuration file you edit by hand (the protected files above, outside the checkout
  too: the global tier, `.piton/` and `.agents/` in parent folders, pi's agent directory, fnox's files, and a linked
  file at its real path) can be changed by the agent with its `edit` or `write` tool, which sends the change to the
  daemon. You get the diff in pi.nvim or the apps, and the daemon writes it only if you approve. The prompt says when
  the file can loosen the sandbox (the global `config.toml`, a `config.local.toml`), runs code in every session (pi's
  extensions, packages, settings, and trust; MCP server lists; `config.host.toml`), or decides where secrets go
  (fnox). No command approval opens these files, and the daemon's state, its control socket, and credentials are
  never proposable. A level folder that doesn't exist yet isn't proposable: create it first.
- **Git:** hooks and configuration (`.git/hooks/`, `.git/config`, `.git/config.worktree`, `.git/commondir`, in any
  repository of the checkout), and replacing the checkout's `.git`, since Git on the host runs what they name. Commits,
  branches, fetches, pushes, worktrees, and updating initialized submodules work. What writes a repository's
  configuration or hooks fails in the sandbox: run `husky install`, `git config --local`, `git remote add`,
  `git submodule update --init`, and `git clone` or `git init` into the checkout on the host (or clone outside the
  checkout); `git push -u` pushes but can't record tracking. Known limit, as in Claude Code: an agent can create a
  `.git` file or symlink in a subfolder pointing at a repository it built elsewhere, whose settings Git uses when run
  in that folder; check a folder an agent created before running Git there. Hooks still run in the sandbox; only
  writing these files is closed. `allow_git_control_files = true` (below) lifts it.
- **Unix sockets:** closed, except DNS (`mDNSResponder`) and `agent.sock`, the channel to the daemon (each process
  there has a token of its own, which the daemon accepts only from that process: a bash call can't send pi's
  requests): no process may connect to a socket (a
  running Neovim, Docker, the SSH and gpg agents) or create one, unless a `[sandbox.network.unix_socket_connections]`
  or `[sandbox.network.unix_socket_bindings]` entry (below) opens it.
- **Network:** filtered. A process reaches only DNS, the port entries, and Piton's gate (below), which decides named
  hosts. pi's own hosts pass without asking: the model providers built into pi and the hosts it signs in through,
  `baseUrl`s in its `models.json`, and the remote MCP servers in your own MCP files. You're asked about any other host
  while the connection waits. `standard` and `yolo` allow `"*"`, which keeps the network open.
- **TCP listening:** closed, on every address, loopback included: a dev server or a test daemon can't listen.
  `allow_tcp_listening = true` (below) opens it, on every address: the sandbox can't limit listening to this Mac
  (macOS's sandbox has no filter for it; Claude Code's `allowLocalBinding` and Codex's `allow_local_binding` open every
  address too), so a listener is reachable from your LAN and tailnet, and a filtered network doesn't stop its replies.
  UDP stays open (closing it would refuse DNS replies). `standard`, `yolo`, and the `pi-claude-code` preset open it.
- **Mach services:** only a built-in list (logging, user and group names, fonts, TLS checks, proxy settings, DNS
  configuration, preferences, Launch Services lookups; `piton sandbox why --service NAME` names any one). Any app's
  service is a way to drive that app, which runs outside the sandbox: with Hammerspoon's `ipc` module loaded, `hs -c`
  runs shell commands on the host. A `[sandbox.macos.services]` entry (below) opens one; `standard` opens every
  service. The keychain's services stay closed unless a filesystem `"read"` opens the keychain.
- **Launching apps:** closed. `open` (a file, an app, a link) and `osascript` scripting another app fail, as in
  Claude Code: what macOS starts runs outside the sandbox, a script the agent wrote included.
  `allow_launching_apps = true` (below) opens them; an approved command (below) runs with them open.

The format:

```toml
[sandbox]
profile = "standard"                   # strict | standard | yolo; the nearest file that sets it wins
presets = ["rust", "pi-claude-code"]   # mise, rust, node, swift: toolchain caches and state;
                                       # pi-claude-code: what pi's Claude Code needs (below)
allow_tcp_listening = true             # listening on TCP, on every address
allow_git_control_files = false        # agents may write Git's hooks and configuration (below)
allow_launching_apps = false           # launching apps and Apple Events (below)

[sandbox.filesystem]                   # a path: "read", "write", or "deny"
"~/Library/Developer/Xcode/DerivedData" = "write"
"~/secrets" = "deny"

[sandbox.network.connections]          # a host, host:port, localhost:PORT, localhost:*, *:PORT, or *: "allow" or "deny"
"*" = "deny"                           # closes standard's open network
"github.com" = "allow"

[sandbox.network.unix_socket_connections]   # a socket, or a folder of them: "allow" or "deny"
"$SSH_AUTH_SOCK" = "allow"

[sandbox.network.unix_socket_bindings]      # where processes may create sockets: "allow" or "deny"
"~/project/run" = "allow"

[sandbox.macos.services]               # a Mach service, a prefix ("com.example.*"), or "*": "allow" or "deny"
"com.example.agent" = "allow"
```

- **Paths** are absolute, `~/…`, or `$VAR/…` from the daemon's login-shell environment (never the workspace's). An
  entry covers the path and everything below it. There are no wildcards.
- **Values:** `"read"` is readable (and runnable), not writable; `"write"` is readable and writable; `"deny"` closes
  reading, writing, connecting, and creating sockets. The other tables open or close their one operation.
- **Merging:** the profile and presets first, then the global files, parent folders, and the repository; within a
  level `config.toml`, then `config.local.toml`. The nearer value wins per entry. Then the most specific matching
  entry decides: a longer path over a shorter one, across the filesystem and socket tables (a socket `"allow"` inside
  a filesystem `"deny"` opens that one socket); for connections, a host over `localhost` over `*`, a longer domain
  over a shorter, a port over any port; for services, a name over a longer prefix over a shorter one over `*`. When
  two entries tie, a deny wins.
- **Three states:** an entry that opens something opens it; something no entry opens is closed and asked about (the
  block question, below); a `"deny"` (or a `"read"` path something writes to, or a `false` switch) is closed and only
  logged, never asked about. The question saves only the entry that opens; a deny you write by hand.
- **Switches:** the nearest file's `allow_*` wins. A `false` you write wins over the profile and the presets too:
  `allow_tcp_listening = false` closes listening even with `pi-claude-code`, whose relay then can't run.
- **`piton config set`** takes the same dotted keys, a path quoted:
  `piton config set --global 'sandbox.filesystem."~/.cache/tool"' write`.
- **macOS:** `[sandbox.macos]` holds what only macOS has. The rest is meant to carry over to a Linux daemon later.

The profiles, as entries, each holding the one before it:

```toml
# strict: nothing

# standard (the default): Git over SSH through your SSH agent (keys used, never read), the network, creating
# Unix sockets anywhere and listening on TCP (test and dev servers, IPC), and every Mach service but the keychain's
allow_tcp_listening = true
[sandbox.filesystem]
"~/.ssh/config" = "read"
"~/.ssh/known_hosts" = "read"          # and every public key (*.pub) under ~/.ssh, found at launch
[sandbox.network.connections]
"*" = "allow"
[sandbox.network.unix_socket_connections]
"$SSH_AUTH_SOCK" = "allow"
[sandbox.network.unix_socket_bindings]
"/" = "allow"
[sandbox.macos.services]
"*" = "allow"

# yolo: standard, plus every Unix socket (except the ones in credential folders), Git's control files, and launching
# apps: close to host mode
allow_git_control_files = true
allow_launching_apps = true
[sandbox.network.unix_socket_connections]
"/" = "allow"
```

A profile entry whose variable isn't set (no `$SSH_AUTH_SOCK`) is left out. Any entry can close one: `"*" = "deny"` in
`[sandbox.network.connections]` gives `standard` a filtered network, `"/" = "deny"` in a socket table closes yolo's
sockets. Toolchain presets are in no profile: host builds reuse what the sandbox writes there, so turn on the ones you
build with.

Connections: ports and `*` are enforced by the sandbox itself; a named host only by Piton's gate, since the macOS
sandbox sees addresses, never names, and only programs that use the proxy variables reach the gate.

| Entry | Matches |
| --- | --- |
| `localhost:PORT` | a port on this Mac |
| `localhost:*` | every port on this Mac: every local service (databases, dev servers) too |
| `*:PORT` | a port on any host |
| `*` | any host, any port |
| `github.com`, `github.com:22`, `*.npmjs.org`, `10.0.0.5` | a named host or address, any port or one |

When `"*"` is allowed, the network stays open and no gate runs, unless an entry needs the gate: a named host denied
(`"evil.com" = "deny"`), or allowed under a port deny (`"*:22" = "deny"` with `"github.com:22" = "allow"`). Then
direct connections close except to this Mac and the port entries, and everything else goes through the gate, which
passes every host but the denied ones without asking. Programs that ignore the proxy variables (most database
clients, Java) then need a port entry (`"*:5432" = "allow"`); Git over SSH goes through Piton's `ssh` wrapper. Denying
only ports keeps the network open. No entry reopens the daemon's state and control socket.

`silence` keeps blocks with no path or host (a Mach service lookup, a TCP listen) from showing a notice. Each entry is
a table; `process` is optional and matches the blocked process's name exactly:

```toml
silence = [
  { process = "claude", target = "com.apple.SecurityServer" }, # a Mach service, by name
  { process = "node", target = "tcp-listen" },                 # node's TCP listens, on any port
]
```

A path or a host takes a `"deny"` in its table instead. Presets add `silence` entries of their own; `piton sandbox
show` lists them all with their source. The apps' settings don't list `silence`: edit it in the file.

### The network gate

When the network is filtered (`"*"` not allowed, or a named-host entry the sandbox can't enforce, above), each session
gets a gate: a proxy on 127.0.0.1 that the sandbox's processes reach through the proxy variables Piton sets
(`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` and their lowercase forms, and `NODE_USE_ENV_PROXY=1` for Node's built-in
`fetch`). It tunnels any port, not only HTTPS, and decides each connection in this order:

1. a named-host entry (`github.com`, `*.npmjs.org`) that matches, allowed or denied (a bash call's own
   `[[sandbox.commands]]` grant included);
2. pi's own hosts: the providers built into the installed pi and their sign-in hosts, `baseUrl`s in the agent
   directory's `models.json`, and remote MCP servers' `url`s in your own MCP files (the global tier's `mcp.json` and
   `mcp.local.json`, and every level's `mcp.local.json`; never a repository's committed `.piton/mcp.json`, which
   travels with the repository). pi's model catalog cache (`models-store.json`) is never trusted: the sandbox can
   write it. Files are read again when they change. A `baseUrl` on this Mac (Ollama's `http://localhost:11434`)
   isn't the gate's: allow it as `"localhost:11434"`;
3. hosts you allowed for this session;
4. a generic entry that matches (`*`, `*:PORT`, `localhost:…`): `"*" = "deny"` refuses without asking, but never cuts
   pi off from its own hosts;
5. otherwise you're asked, in the apps and pi.nvim, while the connection waits: **Allow for this session**, **Allow in
   this project** or **everywhere** (saves `"host:port" = "allow"` in `[sandbox.network.connections]` of the
   project's `config.local.toml` or the global `config.toml`; applies at once, no restart), or **Not now** (refused this
   time). There's no Deny: a host you never want is a `"deny"` you write. No answer within `approval_timeout`
   refuses it.

A refused connection gets `403` with the reason, which curl and most clients print, and a failed bash call lists it
(`network-outbound host:port (refused by the network gate)`).

- Before dialing an allowed name, the gate resolves it and won't connect to loopback, link-local, multicast, cloud
  metadata, or this Mac's own addresses (LAN, tailnet): an allowed domain can't be pointed at local services. An IP
  entry (`"127.0.0.1:3000" = "allow"`) is a choice and exempt.
- Each sandbox (pi's, each bash call's) has its own proxy credential: a process can't use another call's grant, and
  another session can't use this session's gate. A process that can read another process's environment (same user)
  can still take its credential while that call runs.
- SSH goes through the gate too: an `ssh` first on the sandbox's `PATH` (a link to `piton` in the daemon's state
  folder) runs the real `ssh` with the gate as its `ProxyCommand`, so Git over SSH, `scp`, and `rsync` reach listed
  hosts. It overrides a `ProxyCommand` or `ProxyJump` in `~/.ssh/config`. The SSH agent is still a
  `[sandbox.network.unix_socket_connections]` entry.
- With fnox's credential proxy on, the gate forwards every allowed connection through fnox, so fnox still adds
  secrets and only sees what the gate allowed; programs know only the gate.
- Programs that ignore the proxy variables (most database clients, Java, tools with their own SSH code) are refused
  even for an allowed host: they need a port entry (`"*:5432" = "allow"`) or an approved rerun (which opens the
  network). SOCKS and
  Java support: [#359](https://github.com/alex35mil/Piton/issues/359),
  [#360](https://github.com/alex35mil/Piton/issues/360).
- The gate matches the host as the program names it: an IP entry doesn't cover a connection by name, or the reverse.
- UDP and QUIC are refused, except DNS through macOS's resolver; clients fall back to TCP. Listening on TCP isn't
  restricted yet ([#361](https://github.com/alex35mil/Piton/issues/361)).

Hosts a typical `strict` setup allows besides pi's own: package registries (`registry.npmjs.org`, `*.npmjs.org`,
`pypi.org`, `files.pythonhosted.org`, `crates.io`, `static.crates.io`, `index.crates.io`), forges (`github.com`,
`*.githubusercontent.com`, `api.github.com`, `github.com:22` for Git over SSH), and the hosts your MCP servers that
run locally call. `piton sandbox why --connect host:port` says what decides one; `piton sandbox show` lists pi's hosts
(`pi_hosts`).

Sockets inside a credential folder (`~/.ssh`, `~/.gnupg`, `~/.docker`, 1Password's folders, and the rest of the
built-in list) open only when an entry names a path at or inside that folder, and then only for connecting: nothing
there becomes readable. A broader entry (yolo's `/`, `~`) leaves them closed. For example:

```toml
[sandbox.network.unix_socket_connections]
"~/.docker/run/docker.sock" = "allow"   # Docker Desktop
"~/.gnupg/S.gpg-agent" = "allow"        # gpg-agent: signing commits
"~/Library/Group Containers/2BUA8C4S2C.com.1password/t/agent.sock" = "allow"   # 1Password's SSH agent
```

standard's `$SSH_AUTH_SOCK` opens 1Password's agent the same way when it points there.

A socket an entry allows, and each folder on its way, can't be written, renamed, or replaced by the sandbox, so no
other socket can be moved there (the macOS SSH agent's socket is under `/private/tmp`, which the sandbox writes). An
entry naming a folder the sandbox writes (`$TMPDIR`) still reaches every socket in it, Neovim's and any moved or linked
there: name the socket itself when you can.

Every Docker socket (Docker Desktop's, OrbStack's `~/.orbstack/run/docker.sock`, `/var/run/docker.sock`) gives whoever
connects control of this Mac: allowing one lets the agent run anything as you.

Sockets a command creates and connects to itself in temp (this repository's e2e tests start a daemon there) are
blocked too: the command meets the question below, or takes a per-command grant:

```toml
[[sandbox.commands]]
match = "bash(mise run cli:test*)"
runtime = "sandbox"
grant = { network = { unix_socket_connections = { "$TMPDIR" = "allow" } } }
```

Socket bindings and TCP listening are separate because a Unix socket is checked by its path and a TCP listen isn't:
creating a socket in a folder makes a new server and reaches nothing that's there, while TCP listening can only be open
on every address or closed. Neither lets a process connect to what's already listening; that's the connection tables.

A repository's committed `config.toml` travels with the repository, so what it grants applies only once you trust the
repository ("Repository trust", below); its restrictions always apply. A committed `"write"` never reopens a
credential folder or a path your own files deny, trusted or not; `piton sandbox show` lists each one it left out.

`allow_git_control_files = true` lets agents write Git's hooks and configuration, so `husky install`, `git remote add`,
and `git submodule update --init` work in the sandbox, and the next Git command on the host may run an agent's code.
Hooks run in the sandbox either way.

`allow_launching_apps = true` lets processes launch apps and send Apple Events: `open` of a file, app, or link,
`osascript` scripting another app, and an app opening a browser for a sign-in. What macOS starts runs outside the
sandbox, so an agent can run a script it wrote on the host (`open x.command`).

A preset is its name, or a table with its name and options (`{ name = "pi-claude-code", login = "keychain" }`); a
nearer file's options win one by one. The apps show a preset list with options read-only: edit it in the file. No
preset lets the sandbox write what the host runs or trusts (installed tools, `~/.cargo/bin`, rustup's toolchains,
mise's trusted configs). The `mise` preset writes mise's cache and its install bookkeeping; `mise install` itself is
asked about when the sandbox blocks it (below), or granted per command.

Git over SSH in `standard`: ssh reads `~/.ssh/config`, `known_hosts`, and the public keys, and asks your SSH agent to
sign, never reading a private key. Every key loaded in the agent works for every host, not just the Git host. To
narrow it, give Git a separate agent holding only that host's keys (`IdentityAgent` in `~/.ssh/config`, then that
socket in `[sandbox.network.unix_socket_connections]` instead), or use a deploy key per repository. With
`IdentitiesOnly yes`, ssh picks the agent's key through the public key beside the configured `IdentityFile`
(`<file>.pub`). The first-run failures: a key not loaded in the agent (run `ssh-add` on the host; the key file stays
unreadable), and a host missing from `known_hosts` (connect once from a terminal). `strict` opens none of this: allow
the agent's socket and `"read"` the files.

A recommended global configuration for a developer Mac (the `standard` profile covers Git over SSH):

```toml
[sandbox]
presets = ["mise", "rust", "node"]
```

And per project, in its `.piton/config.toml`, what its builds need, e.g. `presets = ["swift"]`.

#### pi's Claude Code

The pi-claude-code extension runs the `claude` CLI for every Claude turn. `claude` writes its state in its config
folder and its logs in `~/Library/Caches/claude-cli-nodejs`, and signs in. Don't grant it the folder the interactive
Claude Code uses (`~/.claude`, or `$CLAUDE_CONFIG_DIR`): that one loads hooks, plugins, and MCP servers, so an agent
that writes there runs code on the host the next time you start Claude Code. Give pi's `claude` a folder of its own,
signed in with a token:

1. Set `PI_CLAUDE_CODE_CONFIG_DIR` to a new folder (e.g. `~/.config/pi/claude`) in your shell or global mise
   configuration. The extension passes it to `claude` as `CLAUDE_CONFIG_DIR`.
2. Run `claude setup-token` and put the token in `CLAUDE_CODE_OAUTH_TOKEN` behind fnox (above), or in pi's
   environment. A token wins over a saved login.
3. In the global `config.toml` or a `config.local.toml`:

   ```toml
   [sandbox]
   presets = ["pi-claude-code"]
   ```

4. Keep the keychain and the interactive folder out of `"read"` and `"write"` entries in `[sandbox.filesystem]`.

The preset makes writable that folder and its lock beside it (`<folder>.lock`), the log cache, and connecting to any
port on this Mac (`localhost:*`; the extension's relay listens on a random one, and it reaches Anthropic from pi's
process). It also opens TCP listening (`allow_tcp_listening`), for that relay: on every address, as the sandbox can't
open only this Mac's. Your own `allow_tcp_listening = false` closes it anyway, and the relay can't run. The folder is read like pi's agent directory: from your shell's environment, a file the agent can't write,
or, when the workspace's own `mise.toml` sets it, only inside the checkout.

It also gives pi's `claude` a socket folder of its own. Every `claude` serves a messaging socket at
`<folder>/cc-socks/<pid>.sock` (`<folder>` is `$XDG_RUNTIME_DIR`, else `$CLAUDE_CODE_TMPDIR`, else `/tmp`) and first
probes a stale one with its pid. The extension ends `claude` with SIGKILL, which leaves every socket behind
([pi-claude-code#50](https://github.com/earendil-works/pi-claude-code/issues/50)), so the shared `/tmp/cc-socks` fills
up and the probe meets the sandbox. When you set neither variable, each session gets a fresh private folder
(`$TMPDIR/piton-cc-<daemon>-<random>`, or under `/private/tmp` when `$TMPDIR` is too deep for the socket path), set
as `CLAUDE_CODE_TMPDIR` in pi's environment and removed when the session ends; the daemon removes its own leftovers
when it starts. When you set one, that folder is used, accepted like `PI_CLAUDE_CODE_CONFIG_DIR` and never `/tmp`,
`$TMPDIR`, a write root, your home folder, or a folder directly in it; otherwise the session starts with a warning.
The folder is open for writing and creating sockets, never for connecting (the agent can create entries there too,
so a connect grant would reach any socket hard-linked in); `claude`'s probes of its own stale sockets there are
silenced.

With the token login, the preset also silences `claude`'s startup lookup of a stored login in the keychain (its own,
and the `security` it runs): `claude` signs in with the token. A keychain lookup by `security` from anything else in
the session is silenced too, since the blocked process can't be told apart; the tool reports its own error.

`login = "keychain"` (`presets = [{ name = "pi-claude-code", login = "keychain" }]`) signs in with a
`claude auth login` saved in the keychain instead: the sandbox can then read the keychain. Run `claude auth login`
with `CLAUDE_CONFIG_DIR` set to the same folder; each folder has its own keychain entry. Refused while fnox applies,
since the sandbox could then run `fnox get`.

The session still starts, with a warning after the conversation (and a pi.nvim notification), when the folder isn't
set, is the interactive Claude Code's, would open what the sandbox closes (your home, a configuration level, a
credential, pi's agent directory), or was set by a file a sandboxed session can write and leads out of the checkout;
and when the token login finds no `CLAUDE_CODE_OAUTH_TOKEN` in pi's environment or fnox's proxy rules. `claude`'s
writes to a folder the preset didn't open are blocked and asked about like any other. `piton sandbox show` prints the
same warnings.

`piton sandbox why PATH [--write | --connect | --unix-bind | --listen | --service] [--workspace DIR]` says whether a
sandboxed pi there may read, write, connect to, or create a socket at PATH, listen on TCP (`--listen 127.0.0.1:3000`;
every address is decided alike), or look up the Mach service PATH names, and which rule decides: built in, or the
entry that decides (`sandbox.filesystem."~/x"`, from a profile, a preset, or the file that sets it). `--connect
host:port` checks a network destination: the entry or pi's own host that decides it, or that the gate would ask.
`piton sandbox show` prints the profile (and the file that chose it), the network's mode (`open` or `filtered`), the
`silence` entries with their source (yours and the presets'), every rule with where it came from, TCP listening, the
committed entries it left out (`ignored`, each with why), the repository's grants left out because you haven't trusted
them (`untrusted`), pi's trusted hosts (`pi_hosts`), and the models pi
discovered (`pi_discovered_models`, never trusted).

A grant doesn't follow a symlink a sandboxed agent could have made: the path itself, or a link inside the project's
checkout or another grant. When a granted folder (say `~/Library/Developer/Xcode/DerivedData`, or pi's `sessions/`) is a
link to somewhere else, grant the real path; `piton sandbox show` warns about each grant it doesn't follow, and the
daemon logs it. Opening a sandboxed session through a symlink inside a checkout that leads out of it is refused: open
the real folder.

`PI_CODING_AGENT_DIR` set by a file a sandboxed session can write (the workspace's `mise.toml`, anything in the
checkout or a granted folder) must point inside the checkout; otherwise the launch is refused, naming the file. Set a
shared agent directory in your shell, your global mise configuration, or a `mise.toml` above the project that no
sandboxed session can write.

#### When the sandbox blocks something

In a sandboxed session, the daemon runs every bash command and `!` command, each in its own sandbox with the same rules
as pi's. When a command fails and Piton saw the sandbox block something an approval would lift, you're asked right
away in the apps or pi.nvim, without waiting for the agent. That covers every kind of block: a file or socket, a
preferences write (`defaults write`), a blocked `open` or `osascript`. Two can't be seen: `launchctl submit` (and `at`),
and setuid programs (`ps`, `top`, `sudo`); macOS logs those without Piton's tag. Piton waits
at most 50 ms after the failure for the kernel's record. A block it didn't see in time, or didn't see at all, is left to
the agent, which can ask to run the command again, hinting where (the sandbox or the host). Either way the agent's
result lists the blocks Piton recorded.

Some blocks aren't asked about, only listed for the agent: credential reads (ssh opens your private keys on every SSH
Git command and recovers), what the floor protects (signals to other processes and `sysctl` writes included: only a
host run passes them), and silenced paths. A few refusals programs always hit and recover from (`open`'s CPU-type
`sysctl` write) are only logged. After you deny a question about a command, the agent can't ask again for the same
command in that turn.

The question opens in the sandbox when Piton asks, and on the agent's hint when the agent asks. You choose where it runs
again, then answer:

- **In the sandbox:** everything open except the floor: protected files, configuration, pi's agent
  directory, Piton's state, your own `"deny"` and `"read"` entries and `false` switches, and credentials while fnox
  is on. Every other Mach service and launching apps are open too, so an approved `open` opens.
- **On the host:** no sandbox at all, as you. For commands no sandbox can run: anything that starts a sandbox of its
  own (`piton sandbox-exec`, `sh cli/tests/probe/protected.sh`), `log`, and tools that must write the floor. It
  gets the session's environment, the workspace's `mise env` included, so it runs whatever the agent wrote into the
  files it runs and into the files that set its environment (`mise.toml` can set `PATH` or `BASH_ENV`). The question
  lists those variables, by file (folded in the apps).

Then: **Allow once** (or **Run once on the host**); **for this session**, which runs it and then runs exactly this
command there without failing or asking until the session ends, saving nothing; **Deny** (it stays failed); or
**always** for this command or an edited pattern, in this project or everywhere, which saves a per-command entry for
that runtime and runs it. The whole command runs again, so its side effects repeat. The agent's result says where it ran. When Piton asked, the
rerun's output and exit are the agent's result; after a denial, it gets the failed run's.

The apps show a switch (left and right arrows with a keyboard) and the pattern inline. pi.nvim shows pi's dialog: its
first option, **Choose an answer below**, decides nothing and shows the question again, so Enter alone never answers;
its last option switches to the other runtime and asks again.

No answer within `approval_timeout` (10 minutes by default): it doesn't run. The agent can ask, never approve.
`osascript` is never offered, and neither is `open` of a file or app the sandbox could have written, as a path or a
`file:` URL. `open` of a web link says it opens in the browser; another URL scheme names the app macOS hands it to,
and is refused when no app handles it or the app is one the sandbox could have written. A blocked Mach service of a
process that isn't a command (an MCP server, an extension) is asked about with `"<name>" = "allow"` in
`[sandbox.macos.services]`.

Per-command entries, in any `config.toml` or `config.local.toml` (a repository's committed one only once you trust it).
`runtime` is required: `"sandbox"` with a `grant`, or `"host"` without one.

```toml
[[sandbox.commands]]
match = "bash(husky install *)"
runtime = "sandbox"
grant = "all"                               # everything except the floor (what "always" in the sandbox saves)

[[sandbox.commands]]
match = "bash(git push *)"
runtime = "sandbox"
grant = { network = { unix_socket_connections = { "$SSH_AUTH_SOCK" = "allow" } } }
                                            # narrowed by hand: [sandbox]'s maps, switches, and presets

[[sandbox.commands]]
match = "bash(sh cli/tests/probe/protected.sh)"
runtime = "host"                            # no sandbox (what "always" on the host saves)
```

A saved host entry runs its command on the host without asking, so whatever the agent writes into a file that command
runs (a script, a Makefile, a package script, a `mise` task) or into a file that sets its environment runs on the host
too, and no question shows the variables first. Claude Code's docs say the same about `excludedCommands`. Prefer a
sandbox entry, narrowed if you can, and save a host entry only for a command whose files the agent can't change.

A matching command runs where its entry says from the start, matched as `[tools.permissions]` rules match (every part
of a compound command must match the same entry; a command that writes a file with `>` gets none). Your
`[tools.permissions]` rules still decide first whether it runs at all.

When something other than a command is blocked (an MCP server, an extension, pi), you're asked once per process and
path, one question at a time. On macOS, pi's running sandbox is widened on your answer, with no restart: the daemon
writes a small addon for this to `<state>/native/lift.node` when it starts, and pi loads it from there.

- **Allow until the end of this turn:** open now, closed when the agent hands the turn back to you (pi's
  `agent_settled`: retries and queued follow-ups stay inside the turn). The same block
  in a later run is asked about again.
- **Allow for this session:** open until the session ends; nothing saved.

While a path is open, the agent's write and edit tools can change it too; the question says so. Bash commands can't:
each runs in a sandbox of its own.
These two answers are offered only for pi's own process: a background job a bash call left running can't be widened
while it runs.

- **Always in this project / everywhere:** saves the entry that lifts it and opens it now; the session's later bash
  calls get it too, and new sessions read it. The entry: `"<path>" = "write"` in
  `[sandbox.filesystem]` for a file or folder it writes (what it writes inside isn't blocked again),
  `"<path>" = "allow"` in `[sandbox.network.unix_socket_connections]` for a socket it connects to (in a credential
  folder too, for that socket only) or in `[sandbox.network.unix_socket_bindings]` for one it creates,
  `allow_tcp_listening = true` for a TCP listen (pi's OAuth sign-in callback, an MCP server's). "Everywhere" saves
  the folder above a path's per-project part (Claude Code's `~/Library/Caches/claude-cli-nodejs/<project>` saves
  `claude-cli-nodejs`), so it helps every project. An entry is offered only when it actually lifts the block. Git's
  control files and the keychain have no entry to save, so they get only the first two answers.

A path named after the blocked process itself (its pid is a whole number in a component: Claude Code's
`cc-socks/64421.sock`, Neovim's `nvim.64421.0`) is never offered as that path, since the entry would never match
again. The question offers the folder above it when that folder isn't temp (`/tmp`, `$TMPDIR`, and what's below them,
where other processes keep their sockets), the home folder or a folder directly in it, and listing it lifts the block;
otherwise it's a notice naming that folder.
- **Not now** (or dismissing it): it stays blocked; it's asked again later.

There's no Deny: like Codex's prompt, a question never saves a denial. A block you never want is an entry you write,
and a block decided by your own entry (a `"deny"`, a `"read"` path something writes to, a `false` switch) is only
logged, never asked about or shown.

Some blocks are never asked about, only shown as a warning in the session, once per process and path. Each warning
ends with the entry that turns it into one you decided (a `"deny"` of the folder, `allow_git_control_files = false`
for Git's files, a `silence` entry for a Mach service or a listen), so you can quiet the ones you consider noise one at
a time:

- **A credential read** (`~/.ssh`, `~/.aws`, and the rest of the built-in list). These are nearly always probes a
  program recovers from. A `"read"` entry would lift one, but it exposes the credential to every sandboxed process, so
  the warning names it only as a last resort for a program that fails without it. The `"deny"` it names to quiet the
  warning closes that folder to commands you approve too. ssh, for example, opens each private
  key to read its public half before signing through your SSH agent, so every SSH Git command shows one. To keep ssh
  from opening private keys at all, point `IdentityFile` at the `.pub` file in `~/.ssh/config`, with
  `IdentitiesOnly yes`; ssh then signs through the agent (which must hold the key: `ssh-add -l`).
- **The keychain** (its services, and `~/Library/Keychains`), when pi's sandbox can't be widened while it runs. With
  fnox off, the warning names `"~/Library/Keychains" = "read"`; in pi's own process its services are asked about
  instead. With fnox on it says the keychain stays closed: the credentials of fnox's secret sources live there, and a
  sandbox that can read it isn't started. For `claude`, it names the `pi-claude-code` preset with a setup token.
- **Git's hooks and configuration** (`.git/hooks/`, `.git/config`), when pi's sandbox can't be widened while it runs
  (otherwise pi's own write is asked about): what Git skipped (`git push -u` sets an upstream; `git branch -D` drops a
  branch's section; a hook isn't installed). Git carries on.
- What the floor blocks, and what nothing can lift.

A command the agent runs keeps the rerun question for every block, credentials and Git's files included.

Known limits:

- Commands the daemon runs don't go through fnox's credential proxy.
- In `standard` and `yolo` the network is open: anything the agent can read can leave the machine, and services
  listening on localhost TCP are reachable. `strict` closes both. In any profile, pi's own provider hosts are
  reachable by anything in pi's process, so an agent could send data to one with a key it supplies (as with Claude
  Code's API host).
- pi's own credentials (its `auth.json`, provider keys in its environment) are readable: the whole pi process is
  sandboxed, and pi needs them. Keys in its environment can be placeholders instead, with the credential proxy
  (above).
- macOS only.

### Repository trust

A repository's committed `.piton/config.toml` is written by whoever commits to it. Its **restrictions always apply**:
`"deny"` entries, `false` switches, `silence`, and tool-permission `ask`, `deny`, and `default_mode = "ask"`/`"deny"`.
Its **grants apply only once you trust the repository**: `"read"`, `"write"`, and `"allow"` entries, `true` switches,
presets, `profile`, `[[sandbox.commands]]`, and tool-permission `allow`, `default_mode = "allow"`, and
`skill_grants = true`. Parent folders' files, the global files, and every `config.local.toml` are your own and never
gated.

The same decision tells pi whether to load the repository's own files: its `.pi/` extensions, settings, skills,
prompts, themes, and MCP servers, `.agents/skills`, and the servers in `.piton/mcp.json`. Piton passes pi
`--approve` or `--no-approve` on every launch, so pi never asks itself and `~/.pi/agent/trust.json` doesn't matter in
Piton's sessions.

**When you're asked.** Before the first session in a repository that has grants or such files: the apps show the
question when you start a session, the Neovim plugin before it opens an agent's tab, and `piton start` in the terminal
(or, with no terminal, it fails and names `piton trust allow`). `piton attach` (pi.nvim on its own) can't ask: it fails
with the same message. The question lists every grant and what pi would load; the answer is kept for the grants it
showed, per source checkout, so workers share it. Until you answer, nothing launches; if you don't trust it, sessions
start with its restrictions only.

```toml
[trust]                  # your own files only: the global config.toml, a config.local.toml, a parent folder
reconfirm = "never"      # or "on-change"
```

- `"never"` (the default, as in Claude Code and Codex): a trusted repository stays trusted, whatever its grants
  become, until you change it.
- `"on-change"`: only the grants you trusted apply; when the committed file gains one, the next launch asks again and
  marks the new ones. Removing a grant or adding a restriction never asks.

```sh
piton trust status [<path>] [--json]   # trusted, declined, or not decided; the grants and pi's files
piton trust allow [<path>]             # trust the grants as they are now
piton trust deny [<path>]              # restrictions only, and pi loads nothing of the project's
                                       # --grants '<json list>': for the grants you were shown (status --json);
                                       # refused, exit 4, when they changed since
piton trust reset [<path>]             # forget it: the next launch asks
```

`piton config check` says whether the repository is trusted and lists the grants left out; `piton sandbox show` lists
them as `untrusted`.

**What trust lets in, and what it doesn't check:**

- Every committed grant: paths the sandbox may read or write, hosts and sockets it may reach, toolchain presets,
  commands that run on the host or widened (`[[sandbox.commands]]`), and tool calls the agent makes without asking.
- pi's project files: `.pi/` extensions run code in pi's process, outside any tool-permission rule; `.pi/settings.json`
  can add packages (more extensions); skills and prompts instruct the agent; MCP servers start programs.
- After you trust a repository, a pull can change its `.pi/` files and skills without a question, in either mode:
  only the grant entries are compared. In a session, an agent can't change them without your approval (they're
  protected files), but a commit you pull can.
- With `"never"`, a pull can also add grants without a question.
- Declining keeps the repository's restrictions and the rest of your configuration; context files (`AGENTS.md`) still
  load, as in pi.

### State

Facts about this host, as opposed to what you decide, are in the daemon's SQLite database next to its socket
(`~/.local/state/piton/piton.db`): this host's identity (`host_id`), your trust decision per repository (above), and
later sessions and drafts. It holds no
preferences. Deleting it resets this host's state; configuration is unaffected. A database newer than the binary
refuses to start; an older one is migrated after a `.v<N>.bak` copy.

- `piton state export <file>` writes a copy (safe while the daemon runs).
- `piton state import <file> [--force]` replaces the database on a replacement Mac, keeping `host_id` so clients
  see the same host. Stop the daemon first. It refuses over a database that holds state unless forced.

## Logging

`piton logging status`, `piton logging debug [--until-disabled]`, and `piton logging info` switch host logging through
the daemon without restarting sessions, until the switch expires or the daemon restarts. `[debugging] log_level` in
`config.host.toml` is the default; debug expires after one hour unless kept on. The independent phone collector is installed with
`mise run logs:install` (host only) and removed with `piton logging uninstall`. App settings, collection, paths, and limits: [logging.md](logging.md).

## Tests

```sh
mise run cli:test                                 # unit tests + e2e against real pi (faux model)
cd cli && node tests/probe/probe.mjs <scenario>      # pi RPC probes, see pi-findings.md
```

The e2e tests use pi's faux provider through `cli/tests/fixtures/faux-extension.ts`, so they need no credentials, and
a scratch configuration, never yours. Most run pi in host mode. Three run it in the real sandbox on the host, sharing
`cli/tests/e2e/sandboxed.mjs`: `blocks.mjs` (the block question, reruns, saved grants), `network.mjs` (`strict`'s
filtered network), and `sandbox.mjs`, which launches pi in each profile and checks its startup (it answers, writes its
session file, loads global settings and packages and a project's `.pi/`) and what the profile grants and refuses. None
of them writes to your home folder: a write outside every grant goes to a folder of the run's own under
`/private/var/tmp`. `sandbox.mjs` reads only what isn't secret there (SSH's config, known hosts, and one public key,
output discarded). In a sandboxed session these three skip, since a sandboxed launch can't start (sandboxes don't
nest), and the worker test checks its sandbox through `piton sandbox show`. `sh cli/tests/probe/protected.sh` checks
the applied sandbox with a shell on the host, with the same run folder. `mise run cli:nono-policy-check` compares the vendored deny groups with nono's at the `nono` version in
`Cargo.lock`; a unit test fails when the crate is bumped without re-vendoring.
