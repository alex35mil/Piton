# Piton for Neovim

Agent workers, a tracker queue, and source/worker tabs over `piton worker`. The daemon owns cloning, source updates,
sandbox settings, pi processes, and deletion. This plugin owns the editor UI, not a second implementation of that work.

## Requirements

- Neovim 0.12 (the tested version).
- `piton` on Neovim's PATH and the daemon running.
- [snacks.nvim](https://github.com/folke/snacks.nvim), with its picker enabled.
- [pi.nvim](https://github.com/alex35mil/pi.nvim), configured to attach through Piton (see
  [daemon setup](../docs/daemon.md#pinvim)).
- For the queue: a GitHub or Linear tracker for queued work, and a GitHub forge for incoming pull requests: the
  nearest `[tracker]` and `[forge]` of Piton's configuration chain (`config.toml` in the source's `.piton/` or its
  parent folders'; never the global folder), read through `piton config get`, which validates them; the plugin reads
  no configuration file itself. Each role reads with its own MCP registration (`mcp_server`), which must specify
  `Authorization: Bearer ${ENV_VAR}`. curl reads that variable from its own environment, which
  [`queue.with_credential`](#queue-credentials) supplies (by default, the environment Neovim was started with); the
  credential is never read into Neovim. Either role alone is enough. Without a tracker or a GitHub forge, named agents
  still work and the queue shows as unavailable.

## Setup

The plugin root is `nvim/` in this repository, **not** the repository root. For example, with lazy.nvim:

```lua
{
    name = "piton",
    dir = vim.fn.expand("~/Projects/personal/tooling/Piton/nvim"),
    dependencies = { "folke/snacks.nvim", "alex35mil/pi.nvim" },
    config = function()
        require("piton").setup()
    end,
}
```

Reuse your existing pi.nvim configuration if you already have one. Piton does not configure pi.nvim's process,
settings, or keybindings.

No keymaps, commands, or globals are installed. Map the actions yourself, for example:

```lua
local piton = require("piton")
vim.keymap.set({ "n", "i", "v", "t" }, "<C-S-n>", piton.new, { desc = "New agent" })
vim.keymap.set({ "n", "i", "v", "t" }, "<C-S-t>", piton.pick, { desc = "Agent picker" })
```

Loading modules and calling `setup()` do not load pi.nvim or Snacks. Those dependencies are used when an action needs
them. All notifications use `vim.notify` with title `Piton`.

## Public API

- **`setup(opts?)`** configures editor integration. Omitted options use defaults; another call replaces the previous
  configuration, rather than retaining old hooks. Unknown options and wrong types are rejected before changing it.
- **`new()`** opens the new-agent picker: Todo, then Backlog, then incoming pull requests, with a named agent always
  available. Queued work already assigned to a worker is excluded. Creation opens its tab and pre-fills the
  appropriate workflow command; it never sends the prompt automatically. When fast-forwarding the source changes
  protected files (`.piton/`, `.pi/`), which the daemon's sandboxed Git can't write, it asks first whether to apply
  them (Piton writes those files; the rest still runs in the sandbox); no creates nothing and leaves the source as it
  was.
  - With a GitHub forge, the pull-request section lists every open PR of `forge.repository` that the authenticated
    user didn't author, whatever the tracker is. Selecting one creates a worker with `/pr #NN Title` pre-filled.
    `/pr` reviews it in that worker.
  - With a GitHub Project tracker too, PRs on the board come first, in board order with their board Status, then the
    rest; a PR on the board and on the forge is listed once. Board PRs that aren't open PRs of the forge's repository
    aren't listed.
  - Without a forge (or with it turned off), there is no pull-request section; tracker work still loads.
- **`pick()`** lists the current repository's source and workers, with live pi status. Selecting an existing tab focuses
  it; otherwise it opens a tab. With no selection, typed text can create a named worker.
- **Trust.** Before an agent's tab opens, a repository that needs the user's trust (committed grants or pi project
  files, never decided) is asked about with `vim.ui.select`: the grants (new ones marked) and what pi would load.
  Trust or Don't trust records it for the grants shown (`piton trust allow|deny --grants`; when they changed while the
  question was open, it asks again) and opens the tab; Cancel or dismissing records nothing and opens nothing. pi.nvim
  started on its own in such a repository can't ask: its attach fails naming `piton trust allow` (#390).
- **`get(path)`** returns the agent containing a path, or nil. The record has `path`, `source`, `label`, and optional
  `issue`. Use this from your own tab-close action to distinguish workers from ordinary tabs.
- **`close_tab(agent)`** offers “Close tab” or “Delete agent”. Closing only closes the editor tab: the daemon's pi keeps
  running. Deleting uses the daemon, which checks everything before changing anything, and confirms each question in
  turn: discarding modified buffers, a dirty/unpushed checkout, and, when the source's update changes protected files,
  applying them or else deleting without updating the source. Answering no to all keeps the
  agent. Its tab stays open until the deletion succeeds; then it closes and the worker's buffers are removed.

## Editor integration

All configuration is optional:

```lua
require("piton").setup({
    tabs = {
        new = function()
            vim.cmd("tabnew")
        end,
        set_label = function(label)
            vim.t.piton_label = label
        end,
    },
    icons = { agent = "󰙅", source = "" },
    confirm = function(message)
        return vim.fn.confirm(message, "&Yes\n&No", 2) == 1
    end,
    queue = {
        with_credential = function(command, credential)
            return command
        end,
    },
})
```

- **`tabs.new()`** must synchronously create and enter a new tab. This is where a caller dismisses a dashboard or
  welcome screen **before** `tabnew`. Piton then sets the tab-local cwd to the worker/source path.
- **`tabs.set_label({icon, name})`** runs in that tab after its cwd is set, before opening a corresponding file and chat.
  Agent names are shortened to 40 characters at a word boundary, with an ellipsis. A custom hook can rename a tabline
  tab and/or store its own metadata. The default just sets `vim.t.piton_label`; it installs no tabline.
- **`icons`** are shared by tab labels and the agent picker; either can be overridden independently.
- **`confirm(message)`** synchronously returns **true** to proceed or **false** to keep the worker. Adapt a dialog
  wrapper here. Do not pass `vim.fn.confirm` directly: its numeric result is rejected, not treated as truthy.
- **`queue.with_credential(command, credential)`** supplies the queue's credential; see below.

### Queue credentials

The queue reads the tracker and forge with `curl`, run with the source checkout as its working directory. curl
imports the variable the MCP registration names (`--variable %VAR`) and expands it into the `Authorization` header
itself, so the token never passes through Neovim's environment, memory, or logs. `with_credential` decides what
environment curl runs in: it receives curl's argv and `{ variable, source }` (the variable's name and the source
checkout) and returns the command to run, usually curl's argv behind a prefix that runs a command with the secret set.
It never sees the value. A command that can't supply the variable makes the queue refuse with the variable's name.

- **Environment variable (the default):** export it in the shell that starts Neovim; curl inherits it.

- **mise:** the variable is set by the checkout's mise configuration (for example `mise.local.toml`):

  ```lua
  queue = {
      with_credential = function(command)
          return vim.list_extend({ "mise", "exec", "--" }, command)
      end,
  }
  ```

- **fnox:** the secret is in fnox's configuration (global, a parent folder's `fnox.toml`, or the checkout's
  `fnox.local.toml`). Mark it `env = "exec"` so `fnox exec` passes the real value while sessions behind
  `fnox proxy` keep the placeholder. `--only` resolves just that secret:

  ```lua
  queue = {
      with_credential = function(command, credential)
          return vim.list_extend({ "fnox", "exec", "--only", credential.variable, "--" }, command)
      end,
  }
  ```

Any other secret manager works the same way, as long as its runner sets the variable for the command it runs.

After opening a worker and its corresponding file (when present), Piton calls pi.nvim's existing public API:
`require("pi").show({ layout = "float", draft = draft })`. Opening a source tab doesn't open chat. Reusing an existing
tab doesn't rename it or reopen chat. No hooks for copying or recreating pi.nvim, tabline, or dialog implementations
are needed.

## Checks

From the repository root:

```sh
mise run nvim:test
```

This starts clean headless Neovim, loads every module without the user's configuration or installed plugin
dependencies, and tests the configuration/API, tab and file handling, picker integration, notifications, and deletion
confirmations. pi.nvim, Snacks, daemon calls, and tracker and forge requests are mocked. Tests never contact the installed
daemon or real providers. The task is also included in `mise run test`.
