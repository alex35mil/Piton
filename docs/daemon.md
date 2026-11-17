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
show `log`, `[env]`, and `[tailnet]` changes as waiting for a restart (`piton service install`). An invalid edit keeps
that file's last good values and the apps say why. The apps change the keys below (`socket` shown read-only), keeping
your comments and order, writing through a symlink into a dotfiles repository, and moving a key to the `.local` file
when you check Local. The whole layout, including repository and parent-folder levels and where each key may be set,
is in "Configuration and state" in [design.md](design.md); `piton config check` validates a workspace's configuration
and the host and client files, lists the files it reads, and warns about a path under another user's home or a secret
in a file Git tracks; `piton config set KEY VALUE [--global | --scope host|client] [--local]` changes one key the same
way the apps do.

`config.host.toml`:

```toml
socket = "~/.local/state/piton/piton.sock"  # for tests and development only: the Mac app always uses this default
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
enabled = true            # the apps' WebSocket on this Mac's Tailscale addresses
port = 7357
cli = "tailscale"
heartbeat_ms = 10000      # ping each app this often; drop it after three unanswered pings
retry_ms = 10000          # while Tailscale isn't up, try again this often

[pi]
command = ["pi"]
args = []                 # added after `--mode rpc`

[env]
login_shell = true        # base environment from `$SHELL -l`
mise = true               # plus `mise env -C <workspace>`

[stop]
grace_ms = 15000          # after closing stdin, before SIGTERM
term_grace_ms = 3000      # after SIGTERM, before SIGKILL
```

A pi gets `PITON_RUNTIME` (sandbox launches), `PITON_CONFIG_HOME`, and `PITON_BIN` after the workspace's mise
environment.

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
- **Network:** open: pi has to reach its model provider, and fetch, web search, MCP, and installs need it too.

`[sandbox]` adds to that, in any `config.toml` of the chain; lists only add across levels:

```toml
[sandbox]
presets = []     # toolchains whose caches and state may be written: "mise", "rust", "node", "swift".
                 # Host builds reuse what the sandbox writes there; turn on the ones you build with.
allow = []       # more writable folders
allow_file = []  # more writable files
deny = []        # paths closed like the credentials: no read, write, or connect
expose = []      # global config.toml only: lift a built-in deny for a path (readable then; writable with allow)
```

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
  your host shell runs. Global `config.toml` only.

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

Known limits:

- The network isn't restricted: anything the agent can read can leave the machine. Filtering by domain is planned in
  [#228](https://github.com/alex35mil/Piton/issues/228); say so there if you need it.
- pi's own credentials (its `auth.json`, provider keys in its environment) are readable: the whole pi process is
  sandboxed, and pi needs them ([#229](https://github.com/alex35mil/Piton/issues/229)).
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
