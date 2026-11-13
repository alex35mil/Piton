# Daemon and CLI

## Running it

```sh
mise run daemon:build
daemon/target/debug/piton daemon                # foreground; ctrl-c stops every pi
daemon/target/debug/piton start ~/Projects/foo  # new session in the nono sandbox (profile piton)
daemon/target/debug/piton start ~/Projects/foo --host --prompt "hello"
daemon/target/debug/piton ls
daemon/target/debug/piton stop --session <pi session id | session file | piton key>

mise run daemon:install                         # stable binary in ~/.cargo/bin + launchd agent (host only)
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

Per-project settings live in `.piton/daemon.toml` in the source checkout (read only from there, never from a worker):
`[workers] skip` lists paths not copied into new workers, `[sandbox]` adds grants for sandboxed pis.

## pi.nvim

```lua
process = {
    kind = "attached",
    cmd = { "piton", "attach" },
    terminate = { "piton", "stop", "--session", "${{SESSION_ID}}" },
}
```

`piton attach` launches a fresh pi in the tab's workspace (sandboxed), relays pi RPC, and exits 0 on detach, 3 when pi
ended, 1 when the connection was lost. `:PiContinue` / `:PiResume` onto a session that's live in piton attach to it.

## Configuration

Optional, in the global configuration folder `$XDG_CONFIG_HOME/piton/` (default `~/.config/piton/`; `--config-home`
or `PITON_CONFIG_HOME` for tests); every key has the default shown. Unknown keys are refused, so a typo stops the
daemon from starting. The daemon watches the files: a change applies to the next session at once, and the apps show
`log`, `[env]`, and `[tailnet]` changes as waiting for a restart (`piton service install`). An invalid edit keeps
the last good values and the apps say why. The apps change every key except `socket`, keeping your comments and
order, writing through a symlink into a dotfiles repository, and moving a key to the `.local` file when you check
Local. The
whole layout, including repository and parent-folder levels, is in "Configuration and state" in
[design.md](design.md); `piton config check` validates a workspace's configuration and lists the files it reads.

`daemon.toml` (and `daemon.local.toml`, this Mac's overrides), how the daemon runs:

```toml
socket = "~/.local/state/piton/piton.sock"  # for tests and development only: the Mac app always uses this default
log = "~/.local/state/piton/logs/daemon/current.log" # five files, 2 MiB each

[tailnet]
enabled = true            # the apps' WebSocket on this Mac's Tailscale addresses
port = 7357
cli = "tailscale"
heartbeat_ms = 10000      # ping each app this often; drop it after three unanswered pings
retry_ms = 10000          # while Tailscale isn't up, try again this often

[pi]
command = ["pi"]
args = []                 # added after `--mode rpc`

[sandbox]
command = ["nono"]
base = "piton"            # the nono profile it extends: your ~/.config/nono/profiles/piton.json, or a built-in
                          # one such as "mise-dev" once its grants are in this table
groups = []               # extra nono groups, e.g. ["git_config"]
allow = []                # read-write grants; also read, read_file, allow_file, deny, unix_socket
bypass = []               # lift a base-profile deny for a path granted above
[sandbox.network]
open_port = []            # e.g. [0]: every localhost port
# profile = false         # no proxy filtering, or a nono network profile name

[env]
login_shell = true        # base environment from `$SHELL -l`
mise = true               # plus `mise env -C <workspace>`

[stop]
grace_ms = 15000          # after closing stdin, before SIGTERM
term_grace_ms = 3000      # after SIGTERM, before SIGKILL
```

`settings.toml` (and `settings.local.toml`, this Mac's values), the preferences the apps change:

```toml
[host]
roots = []                # where the apps may browse and start sessions; none until you list some,
                          # e.g. ["~/Projects"]; piton attach and the CLI aren't limited
attach_mode = "sandbox"   # mode for `piton attach`
empty_grace_ms = 10000    # stop a pi with no messages this long after its last client leaves
logging = "info"          # host logging level ("info" or "debug"); `piton logging` and the Mac app switch it
                          # until it expires or the daemon restarts
```

Every sandboxed launch gets a nono profile the daemon generates in its state folder: `sandbox.base` extended with the
merged `[sandbox]` grants of every level, read access to the configuration chain, the source's `.piton/.local`, and
the protected-file rules. A pi gets `PITON_RUNTIME` (sandbox launches), `PITON_CONFIG_HOME`, and `PITON_BIN` after
the workspace's mise environment.

### State

What happened on this host, as opposed to what you decide, is in the daemon's SQLite database next to its socket
(`~/.local/state/piton/piton.db`): this host's identity (`host_id`) and keep-awake (on by default; toggled in the
apps and kept across restarts). Deleting it resets this host's state; configuration is unaffected. A database newer
than the binary refuses to start; an older one is migrated after a `.v<N>.bak` copy.

- `piton state export <file>` writes a copy (safe while the daemon runs).
- `piton state import <file> [--force]` replaces the database on a replacement Mac, keeping `host_id` so clients
  see the same host. Stop the daemon first. It refuses over a database that holds state unless forced.

### Moving from the old layout

Every tool refuses the files of the layout before `.piton/` (`.project/`, `.agents/{permissions,mcp,web-search}*.json`,
`.nono*.json`, `.piton.toml`, the global `config.toml`, and the files in pi's agent folder), naming each file and
its new place; only the parts that read a file refuse it.

- `piton migrate [path] [--dry-run]`, on the host, in a source checkout: moves `.project/` to `.piton/`
  (`config[.local].json` becomes `workflows[.local].json`), `.agents/` settings files to `.piton/`, and converts
  `.nono.json` and `.piton.toml` into `.piton/daemon.toml` (`.nono.local.json` into `daemon.local.toml`). Committed
  files move with Git, staged for you to commit. It rewrites the ignore rules to `/.piton/.local` and
  `/.piton/*.local.*`, and relinks every worker's `.piton/.local`. It plans everything first and refuses before
  changing anything: on a file already in the way, or a nono key `[sandbox]` doesn't model (named in the message).
- `piton config check` prints the global tier's conversion for your dotfiles: `config.toml` split into `daemon.toml`
  and `settings.toml`, the base nono profile as `[sandbox]` (refused by key name when it uses one `[sandbox]` doesn't
  model), and the files and `rules/` to move out of pi's agent folder. It writes nothing.

## Logging

`piton logging status`, `piton logging debug [--until-disabled]`, and `piton logging info` switch host logging through
the daemon without restarting sessions, until the switch expires or the daemon restarts. `[host] logging` in
`settings.toml` is the default; debug expires after one hour unless kept on. The independent phone collector is installed with
`mise run logs:install` (host only) and removed with `piton logging uninstall`. App settings, collection, paths, and limits: [logging.md](logging.md).

## Tests

```sh
mise run daemon:test                                 # unit tests + e2e against real pi (faux model, host mode)
cd daemon && node tests/probe/probe.mjs <scenario>   # pi RPC probes, see pi-findings.md
```

The e2e tests use pi's faux provider through `daemon/tests/fixtures/faux-extension.ts`, so they need no credentials.
They run in host mode because nono can't start inside a nono sandbox.
