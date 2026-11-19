# Daemon and CLI

## Running it

```sh
mise run cli:build
cli/target/debug/piton daemon                # foreground; ctrl-c stops every pi
cli/target/debug/piton start ~/Projects/foo  # new session in the sandbox
cli/target/debug/piton start ~/Projects/foo --host --prompt "hello"
cli/target/debug/piton ls
cli/target/debug/piton stop --session <pi session id | session file | piton key>

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
an agent's Git does: over SSH with `~/.ssh/config` and `~/.ssh/known_hosts` in `expose`, over HTTPS with the keychain
exposed. When it can't, creating a worker fails naming that; deleting one leaves the source as it is, with a warning.

An update that changes protected files (`.piton/`, `.pi/`) needs your approval, since the sandbox can't write them.
Piton asks before changing anything, naming the files. Approved, Piton writes exactly those files from the upstream
commit itself, and the rest of the update, with your hooks, still runs in the sandbox, so a hook can't change any
other protected file. Declined, nothing changes (when deleting, you can delete without updating the source). The
Neovim plugin asks; `piton worker` refuses and prints the flag that answers it (`--approve-source-update <sha>`, or
`--skip-source-update` when deleting). Not asking at all comes with
[#241](https://github.com/alex35mil/Piton/issues/241).

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
or `PITON_CONFIG_HOME` for tests). Every key has the default shown.

- **`config.host.toml`** (and `config.host.local.toml`, not committed): how the daemon runs on this host and this
  host's preferences. Only this file can stop the daemon from starting; a typo in it does.
- **`config.client.toml`:** the apps' preferences, served to every client.
- **`config.toml`** (and `config.local.toml`): the defaults every project inherits, the chain's farthest level. The
  daemon reads its `[sandbox]` and `[workers]` for each launch.

Unknown keys are refused. The daemon watches the files: a change applies to the next session at once, and the apps
show `log`, `[env]`, `[tailnet]`, and `[tools]` changes as waiting for a restart (`piton service install`). An invalid edit keeps
that file's last good values and the apps say why. The apps change the keys below (`socket` shown read-only), keeping
your comments and order, writing through a symlink into a dotfiles repository, and moving a key to the `.local` file
when you check Local. The whole layout, including repository and parent-folder levels and where each key may be set,
is in "Configuration and state" in [design.md](design.md); `piton config check` validates a workspace's configuration
and the host and client files, lists the files it reads, and warns about a path under another user's home or a secret
in a file Git tracks; `piton config set KEY VALUE [--global | --scope host|client] [--local]` changes one key the same
way the apps do.

`config.host.toml`:

```toml
socket = "~/.local/state/piton/piton.sock"  # debug builds only, for tests: release builds ignore it (the sandbox
                                            # closes the socket's folder, and the Mac app uses this default)
log = "~/.local/state/piton/logs/daemon/current.log" # five files, 2 MiB each
roots = []                # where the apps may browse and start sessions; none until you list some,
                          # e.g. ["~/Projects"]; piton attach and the CLI aren't limited
favorites = []
default_attach_mode = "sandbox"   # mode for `piton attach` when none is given
empty_grace_ms = 10000    # stop a pi with no messages this long after its last client leaves
logging = "info"          # host logging level ("info" or "debug"); `piton logging` and the Mac app switch it
                          # until it expires or the daemon restarts
keep_awake = true         # hold the Mac awake while sessions are live; toggled from the apps
                          # (put it in config.host.local.toml to keep toggles out of a dotfiles diff)

[tailnet]
enabled = true            # the apps' WebSocket on this Mac's Tailscale addresses (needs the tailscale tool)
port = 7357
heartbeat_ms = 10000      # ping each app this often; drop it after three unanswered pings
retry_ms = 10000          # while Tailscale isn't up, try again this often

[pi]
command = ["pi"]
args = []                 # added after `--mode rpc`

[env]
login_shell = true        # base environment from `$SHELL -l`

[stop]
grace_ms = 15000          # after closing stdin, before SIGTERM
term_grace_ms = 3000      # after SIGTERM, before SIGKILL

[tools.mise]              # `mise env -C <workspace>` for every launch, trust for workers
enabled = true            # unset: used when installed, off otherwise
command = ["mise"]        # found through the login shell's PATH

[tools.fnox]              # experimental: every launch behind fnox's credential proxy (below)
enabled = true
command = ["fnox"]        # e.g. ["fnox", "-P", "agent"] for a profile

[tools.tailscale]         # remote access (`[tailnet]`)
enabled = true
command = ["tailscale"]
```

A pi gets `PITON_RUNTIME` (sandbox launches), `PITON_CONFIG_HOME`, and `PITON_BIN` after the workspace's mise
environment.

For a sandboxed launch, `mise env` runs inside the launch's sandbox, so whatever a workspace's mise configuration runs
(`exec()` templates, `_.source` scripts) is contained. A configuration that reads a secret manager while `mise env`
runs (`exec(op read …)`) fails there and the launch is refused with mise's error; keep secrets out of the mise
environment and use the credential proxy (below).

Everything in pi's environment is visible to the agent and every command it runs. `[permissions]` rules steer the
agent; in sandboxed sessions they aren't the boundary, the sandbox is.
### External tools

Piton runs mise, fnox, and tailscale when they're there and works without them. Each has a `[tools.<name>]` table:

- **Unset `enabled`:** used when its command is installed, off otherwise. A Mac with neither works with both off:
  pi gets the login shell's environment, and only this Mac connects.
- **`enabled = false`:** off.
- **`enabled = true`:** used, and an error when it isn't installed: piton never runs it, the apps' Settings show a
  warning on the setting, and each operation that needs it fails with an error naming the setting (a launch or a
  worker for mise; every launch for fnox; remote access for tailscale).

The daemon decides each tool's state when it starts, so changes apply after a restart. `piton setup check` requires
only pi and lists the tools with their state.

### Credential proxy (fnox, experimental)

With [fnox](https://fnox.jdx.dev/guide/proxy)'s credential proxy, pi never holds your secrets: its environment has
placeholders, and fnox, outside the sandbox, puts the real values (read from 1Password or another secret store) only
into the requests its rules allow. An agent that reads `GITHUB_PERSONAL_ACCESS_TOKEN` gets `fnox_proxy_...`.

When fnox is used (`[tools.fnox]`) and a fnox config with a `[proxy]` table applies to the workspace, every session,
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
CLAUDE_CODE_OAUTH_TOKEN = { provider = "op", value = "op://Agent/claude-token/credential", env = false }

[proxy]
egress = "permissive"   # fnox's default, "strict", refuses every host without a rule: pi's model provider,
                        # web fetches, installs. Use strict with `allow_domains` if you want an allowlist.

[[proxy.rules]]
secret = "GITHUB_PERSONAL_ACCESS_TOKEN"
domain = "api.githubcopilot.com"   # the GitHub MCP server (`Authorization: Bearer ${GITHUB_PERSONAL_ACCESS_TOKEN}`)
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
  keychain.
- **Move secrets out of `mise.local.toml`** and other files pi's environment comes from: a secret still set there
  reaches pi as a real value.
- **Restart sessions after changing a secret.** fnox reads secrets when it starts a session; a running session keeps
  the old value.
- **Project fnox files are protected.** `fnox.toml`, `.fnox.toml`, `.config/fnox.toml`, their `.local` and profile
  forms (`fnox.<profile>.toml`), at any depth in the checkout: an agent proposes changes and you approve each, as for
  `.pi/`. A sandboxed launch is refused when fnox would load any other file the sandbox lets the agent change (an
  `import` of a workspace file, a config under an `allow` path), naming it.
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
> `~/Library/Keychains` (and any folder containing it, like `~/Library`) from `[sandbox] expose` and `allow` in your
> configuration (the global `config.toml` or a `config.local.toml`; `piton config check` lists the files). With the
> Claude token behind the proxy, the sandbox doesn't need it. Git over HTTPS authenticates through the keychain, so
> with it closed, the sandbox's Git (an agent's, and worker updates) fetches over SSH.

Limits:

- Credentials pi manages itself (`auth.json`: `/login` providers, MCP OAuth) aren't covered.
- A program that ignores the proxy variables or fnox's certificate sends the placeholder, and its request fails. Go
  programs on macOS (`gh`, `terraform`) check certificates only against the system trust store and ignore
  `SSL_CERT_FILE`, so they refuse fnox's: `gh` behind fnox fails with "certificate is not trusted". Give such a tool
  its token another way, or run it on the host.
- Git over HTTPS doesn't work behind fnox: Git signs in with `Authorization: Basic base64(user:token)`, and fnox 1.48
  can't find a placeholder inside base64 (a proposal to add it, jdx/fnox#1151, was closed without merging). GitHub
  refuses the token as `Bearer`. With fnox on, use SSH remotes.
- fnox's daemon cache isn't used, since any process of yours can query it.

### Sandbox

A sandboxed pi works with no configuration. Piton's built-in policy, the same idea as Claude Code's, Codex's, and
Gemini CLI's:

- **Reads:** everything, except credentials and private data: SSH and GPG keys, cloud and registry credentials
  (`~/.aws`, `~/.config/gcloud`, `~/.kube`, `~/.docker`, `~/.npmrc`, `~/.netrc`, ...), keychains and password
  managers, browser profiles, Mail and Messages, shell history and rc files. The list is nono's deny groups, vendored.
- **Writes:** the workspace, the source's `.piton/.local`, temp folders, and pi's runtime state in its agent directory
  (sessions, logins, model catalogs, MCP auth and logs, and the locks pi takes even to read its settings and project
  trust). Not user caches: host tools reuse what's in them without checking, so a writable cache would let a
  sandboxed agent run code on the host. Not pi's settings, project trust, MCP servers, extensions, managed
  binaries, or packages, which host sessions load: changing a setting, trusting a project, adding an MCP server, or
  installing or updating a pi package from a sandboxed session fails, so do those in a host session. A session folder
  moved out of the agent directory (`sessionDir`, `PI_CODING_AGENT_SESSION_DIR`) needs `allow`.
- **Always closed:** protected files (`.piton/`, `.pi/`, except the lock pi takes to read `.pi/settings.json`) and
  every configuration level, the daemon's state folder (except `logs/` for reading and `agent.sock`), and the
  daemon's control socket.
- **Git:** hooks and configuration (`.git/hooks/`, `.git/config`, `.git/config.worktree`, `.git/commondir`, in any
  repository of the checkout), and replacing the checkout's `.git`, since Git on the host runs what they name. Commits,
  branches, fetches, pushes, worktrees, and updating initialized submodules work. What writes a repository's
  configuration or hooks fails in the sandbox: run `husky install`, `git config --local`, `git remote add`,
  `git submodule update --init`, and `git clone` or `git init` into the checkout on the host (or clone outside the
  checkout); `git push -u` pushes but can't record tracking. Known limit, as in Claude Code: an agent can create a
  `.git` file or symlink in a subfolder pointing at a repository it built elsewhere, whose settings Git uses when run
  in that folder; check a folder an agent created before running Git there. Hooks still run in the sandbox; only
  writing these files is closed. The `git` preset (below) lifts it.
- **Network:** open: pi has to reach its model provider, and fetch, web search, MCP, and installs need it too.

`[sandbox]` adds to that, in any `config.toml` of the chain; lists only add across levels:

```toml
[sandbox]
presets = []     # toolchains whose caches and state may be written: "mise", "rust", "node", "swift";
                 # host builds reuse what the sandbox writes there, turn on the ones you build with.
                 # "git": agents may write Git's hooks and configuration (see below)
allow = []       # more writable folders
allow_file = []  # more writable files
deny = []        # paths closed like the credentials: no read, write, or connect
expose = []      # lift a built-in deny for a path (readable then; writable with allow)
```

The knobs that loosen a protection are yours alone: `expose`, the `git` preset, and the `mise` preset's
`install = "allow"` are accepted in the global `config.toml` or any `config.local.toml`, never in a project's
`config.toml`, which travels with the repository: a repository can't open the sandbox for whoever clones it.

The `git` preset lets agents write Git's hooks and configuration, so `husky install`, `git remote add`, and
`git submodule update --init` work in the sandbox, and the next Git command on the host may run an agent's code.
Hooks run in the sandbox either way.

Paths are absolute, `~/...`, or `$VAR/...` from the daemon's login-shell environment (never the workspace's).

A preset is its name, or a table with its name and options. No preset, with its default options, lets the sandbox
write what the host runs or trusts (installed tools, `~/.cargo/bin`, rustup's toolchains, mise's trusted configs).
The `mise` preset writes mise's cache and its install bookkeeping; its `install` option says how a sandboxed agent
installs tools:

```toml
presets = ["rust", { name = "mise", install = "ask" }]
```

- `"ask"` (the default): each `mise install` is approved by you and run on the host. That needs the confirmation
  gate of [#241](https://github.com/alex35mil/Piton/issues/241); until it exists, installs fail in the sandbox and you
  run them on the host.
- `"deny"`: installs fail in the sandbox.
- `"allow"`: the sandbox may write mise's installed tools, so installs just work, and an agent can replace binaries
  your host shell runs. The global `config.toml` or a `config.local.toml` only.

Across levels, a preset's options are the nearest level's that sets them, so a project can turn `install` to `"deny"`.
The apps edit `presets` as a list of names; a value with options is shown read-only and edited in the file.

A recommended global configuration for a developer Mac:

```toml
[sandbox]
presets = ["mise", "rust", "node"]
expose = ["~/.ssh/config", "~/.ssh/known_hosts"]  # git over SSH; the agent socket is reachable already
```

And per project, in its `.piton/config.toml`, what its builds need, e.g. `presets = ["swift"]`.

`piton sandbox why PATH [--write | --connect] [--workspace DIR]` says whether a sandboxed pi there may read, write, or
connect to PATH, and which rule decides; `piton sandbox show` prints every rule.

A grant doesn't follow a symlink a sandboxed agent could have made: the path itself, or a link inside the project's
checkout or another grant. When a granted folder (say `~/Library/Developer/Xcode/DerivedData`, or pi's `sessions/`) is a
link to somewhere else, grant the real path; `piton sandbox show` warns about each grant it doesn't follow, and the
daemon logs it. Opening a sandboxed session through a symlink inside a checkout that leads out of it is refused: open
the real folder.

`PI_CODING_AGENT_DIR` set by a file a sandboxed session can write (the workspace's `mise.toml`, anything in the
checkout or a granted folder) must point inside the checkout; otherwise the launch is refused, naming the file. Set a
shared agent directory in your shell, your global mise configuration, or a `mise.toml` above the project that no
sandboxed session can write.

Known limits:

- The network isn't restricted: anything the agent can read can leave the machine. Filtering by domain is planned in
  [#228](https://github.com/alex35mil/Piton/issues/228); say so there if you need it.
- pi's own credentials (its `auth.json`, provider keys in its environment) are readable: the whole pi process is
  sandboxed, and pi needs them. Keys in its environment can be placeholders instead, with the credential proxy
  (above).
- macOS only.

Keys of the earlier nono-CLI backend (`base`, `groups`, `bypass`, `read`, `read_file`, `unix_socket`,
`[sandbox.network]`, and `[sandbox] command` in `config.host.toml`) are refused with what replaces them.

### State

Facts about this host, as opposed to what you decide, are in the daemon's SQLite database next to its socket
(`~/.local/state/piton/piton.db`): this host's identity (`host_id`), and later sessions and drafts. It holds no
preferences. Deleting it resets this host's state; configuration is unaffected. A database newer than the binary
refuses to start; an older one is migrated after a `.v<N>.bak` copy.

- `piton state export <file>` writes a copy (safe while the daemon runs).
- `piton state import <file> [--force]` replaces the database on a replacement Mac, keeping `host_id` so clients
  see the same host. Stop the daemon first. It refuses over a database that holds state unless forced.

## Logging

`piton logging status`, `piton logging debug [--until-disabled]`, and `piton logging info` switch host logging through
the daemon without restarting sessions, until the switch expires or the daemon restarts. `logging` in
`config.host.toml` is the default; debug expires after one hour unless kept on. The independent phone collector is installed with
`mise run logs:install` (host only) and removed with `piton logging uninstall`. App settings, collection, paths, and limits: [logging.md](logging.md).

## Tests

```sh
mise run cli:test                                 # unit tests + e2e against real pi (faux model, host mode)
cd cli && node tests/probe/probe.mjs <scenario>      # pi RPC probes, see pi-findings.md
```

The e2e tests use pi's faux provider through `cli/tests/fixtures/faux-extension.ts`, so they need no credentials.
They run pi in host mode; in a sandboxed session a sandboxed launch can't start (sandboxes don't nest), so the worker
test checks its sandbox through `piton sandbox show`. `sh cli/tests/probe/protected.sh` checks the applied sandbox on
the host. `mise run cli:nono-policy-check` compares the vendored deny groups with nono's at the `nono` version in
`Cargo.lock`; a unit test fails when the crate is bumped without re-vendoring.
