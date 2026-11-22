# Permissions Extension

Controls tool execution with configurable allow/deny/ask rules. Prompts the user for confirmation before executing tools that aren't explicitly allowed.

## Configuration

Settings: `[permissions]` and `[permissions.<runtime>]` of `config.toml` (then `config.local.toml`) in every level of Piton's `.piton/` chain, farthest first: the global folder `$XDG_CONFIG_HOME/piton/` (default `~/.config/piton/`), parent folders' `.piton/` strictly below home, then `<repo-root>/.piton/`. `piton config resolve` reads them.

### Runtime

`PITON_RUNTIME` says where Pi runs: unset (or empty) means `host`, `sandbox` means a sandboxed worker, such as a nono worker launched by piton. Any other value is invalid: tools are blocked until it's fixed, like invalid settings.

Only the current runtime's table loads. Each file contributes `[permissions]`, then its runtime table; `config.local.toml` comes after `config.toml`, and nearer levels later. `allow`, `ask`, and `deny` lists from every loaded table are combined, and deny or ask beats allow wherever it comes from. `default_mode` and `skill_grants` come from the nearest table that sets them.

For example, a global `[permissions.sandbox]` with `default_mode = "allow"` relaxes confirmations in every worker, unless a project's `[permissions]` sets its own `default_mode`. Project setup's `configure_permissions` writes only `[permissions]` of the project or local file; runtime tables are edited by hand.

`/permissions-settings` shows the runtime and the files that were loaded.

### Schema

```toml
[permissions]
default_mode = "ask"
allow = ["read", "bash(git *)"]
deny = ["bash(rm -rf *)"]
ask = ["write", "edit"]

[permissions.sandbox]       # only in a sandboxed worker; [permissions.host] for the host
default_mode = "allow"
```

### Rule Format

- `"read"` — blanket match on a tool name
- `"bash(git *)"` — match tool `bash` where command matches `git *`
- `"edit(/tmp/*)"` — match tool `edit` where path matches `/tmp/*`
- `"mcp(playwright, *)"` — match every tool from the `playwright` MCP server
- `"mcp(github, get_issue)"` — match one server-advertised MCP tool
- `"mcp(github, hint: readOnly)"` — match tools from `github` with `readOnlyHint: true`
- `"mcp(*, hint: readOnly)"` — match read-only-annotated tools from every MCP server

MCP selectors work in `allow`, `ask`, and `deny`. Server and tool selectors support either an exact name or the whole `*` wildcard. Names may contain only ASCII letters, digits, `_`, `.`, or `-`.

Only an explicit MCP `readOnlyHint: true` matches `hint: readOnly`; missing or false values do not. **Servers tell Pi whether tools are read-only; Pi cannot verify that claim. A hint rule can therefore change a tool from the default `deny` mode to `ask`. Only use hint rules with servers you trust.**

### Argument Matching

| Tool                    | Matched against                                                                   |
| ----------------------- | --------------------------------------------------------------------------------- |
| `bash`                  | each syntax-visible Bash command node, including commands nested in substitutions |
| `edit`, `write`, `read` | file path                                                                         |
| `grep`, `find`, `ls`    | path argument                                                                     |
| `fetch`                 | URL                                                                               |

### Evaluation Order

Without a session mode: `deny` > `ask` > built-in/configured `allow` > `default_mode` (default: `"ask"`). The strictest result across Bash command nodes wins.

Session modes (`/permissions-mode` and auto-accept) never weaken explicit `deny` or `ask` rules. With a session mode, the result is the stricter of the session mode and any matching explicit `deny`/`ask` rule. When no explicit rule matches, the session mode replaces allow rules and `default_mode`. So a session `deny` always blocks, an explicit `deny` always blocks, and an explicit `ask` still prompts under a session `allow`.

- **Bash:** when no `deny`/`ask` rule targets `bash`, a session mode skips Bash parsing and redirect checks. Otherwise every command node is checked against those rules, and a command that can't be parsed is denied. The redirect check is not an explicit rule, so a session `allow` still skips it, and so does an effective (merged) `default_mode` of `"allow"`.
- **MCP:** a tool without live catalog metadata can't be matched against rules. Under a session `allow` or `ask` it is denied when any `deny`/`ask` rule is `*` or an `mcp(...)` rule; otherwise the session mode applies. Without a session mode it is always denied.

Exact literal `true` and `false` Bash commands are built-in allow rules. Session modes and explicit deny or ask rules still take precedence.

**What a block does:**

- A `deny` blocks only that call and returns the cause to the model without ending the turn, so the model can recover. The reason names the matched rule, plus the command node for Bash, or the session mode, `default_mode`, an unparseable command, or missing MCP metadata.
- Rejecting an `ask` prompt ends the turn: it's a deliberate stop. A file-review rejection with notes returns the notes and keeps the turn.

### Skill-Derived Rules

With `skill_grants = true` in `[permissions]` (off unless set), trusted local skills (global + project) contribute allow rules for the whole session via YAML frontmatter:

```yaml
---
allowed-tools:
    - "bash(ls *)"
    - "read"
    - "mcp(github, get_issue)"
---
```

Skills aren't protected files, so while `skill_grants` is on, an agent that edits a skill can grant itself the tools it lists. Off, the field is metadata only, and the tools follow your rules like any other.

### Persistent MCP inspection

Project setup's `inspect_permissions` tool reuses this extension's persistent settings loader, MCP parser, and matcher to report covered rules, current matching tools, and ask/deny blockers with source paths. Inspection excludes session overrides and skill-derived allows; those temporary grants are not durable coverage. Equivalent and broader selectors cover narrower rules, but exact allows for today's read tools do not cover future annotated tools under a read-only selector. Unknown annotations never prove a read-only match.

Setup's `configure_permissions` merges approved MCP `allow` and `ask` lists into the selected project/local file, preserving existing constraints and unrelated settings. It skips covered, fully blocked, or unavailable allows and reports partial blockers without removing them. An MCP write allow grants every operation exposed by that tool across its credential scope, not just the workflow's current repository or operation. See [setup permissions](../../workflows/references/setup/permissions.md#integration-convenience-permissions) for recommendations and approval behavior.

## Bash in a sandboxed session under Piton

In a sandboxed session Piton's daemon launched (`PITON_RUNTIME=sandbox` with `PITON_AGENT_SOCKET` and
`PITON_SESSION_TOKEN`), the extension registers pi's own `bash` tool definition with operations that send each call to
the daemon, and handles `!` commands (`user_bash`) the same way: a sandboxed process can't start one with another
sandbox, so the daemon runs each in its own, tagged with the call (`sandbox.ts`). pi's rendering, truncation,
`shellPath`, `shellCommandPrefix`, and session environment stay. The tool takes two more parameters: `rerun`
(`{ runtime: "sandbox" | "host" }`: after the sandbox blocked this exact command, run it again; the user is asked,
the question opens on that runtime, and the user may choose the sandbox with everything but the floor, the host with
no sandbox, or deny) and `reason`. When a call ran anywhere but the session's own sandbox (a
per-command entry or the user's answer), the daemon says so before the output, and the result ends with one line
naming where (`structuredContent.ran`). Permission rules still decide first. Elsewhere pi's own bash stays. See
"Asking when the sandbox blocks" in `docs/design.md`.

The daemon matches `[[sandbox.commands]]` grants with a Rust port of this extension's Bash analysis and glob; both
run the cases in `pi/tests/fixtures/bash-matching.json`.

## Safety Features

- Bash is parsed with tree-sitter; command nodes are checked across newlines, lists, pipelines, compound statements, and nested substitutions
- Runtime indirection such as `eval`, `bash -c`, `source`, aliases, functions, and dynamic command names is checked as the outer invocation only
- Shell-state-only assignments/declarations emit no outer command, while syntax-visible commands nested inside them are still checked
- A leading `cd <dir> &&` is ignored only when `<dir>` resolves to the current directory
- Writable file redirects (`>`, `>|`, `>>`, `&>`, `&>>`, and filename-targeted `>&word`) escalate otherwise-allowed Bash calls to `"ask"`, unless the effective `default_mode` is `"allow"` or a session mode applies
- Exact `/dev/null` targets, input redirects, fd duplication/closing, and process-substitution targets do not add a redirect prompt; nested process-substitution commands are still checked
- Parser/WASM failures and trees containing `ERROR` or `MISSING` nodes are denied without fallback; this includes valid syntax unsupported by the grammar, such as `<>`
- Bash wildcard rules match across newlines within a command node
- Headless mode (no UI) blocks all `"ask"` calls
- A deny blocks the call and returns its matched rule or cause to the model without ending the turn; rejecting an ask prompt ends the turn
- Invalid JSON, malformed MCP rules, unknown hints, invalid MCP names, and legacy MCP rules fail closed
- With invalid settings, ordinary prompts and all agent tools are blocked; slash commands and user-entered `!` shell commands remain available for recovery
- Interactive sessions show a persistent error widget above the editor until valid settings are loaded

## Commands

| Command                           | Description                                                        |
| --------------------------------- | ------------------------------------------------------------------ |
| `/permissions-toggle-auto-accept` | Switch edit/write away from the settings' default for the session  |
| `/permissions-mode`               | Set permission mode for a specific tool (session only)             |
| `/permissions-settings`           | Show resolved settings, skill-derived rules, and session overrides |

### Auto-accept toggle

`/permissions-toggle-auto-accept` switches `edit` and `write` away from what the settings resolve to, for the current session. Toggling again returns to the settings' default.

- **Review default** (the usual host setup, where edits ask): the toggle turns auto-accept on.
- **Auto-accept default** (both `edit` and `write` allowed, for example a sandbox's `[permissions.sandbox]` with `allow = ["edit", "write"]`): the toggle turns diff review on.

The default comes from the merged settings, skill-derived allows included, using only rules that apply to every path: `"edit"`, `"edit(*)"`, or `"*"`, in the usual `deny` > `ask` > `allow` > `default_mode` order. It's auto-accept only when both tools resolve to `allow`. Path-scoped rules such as `edit(/tmp/*)` don't change the default and keep applying per call.

The toggle sets session modes, so explicit `ask` and `deny` rules still win either way: with `"ask": ["edit(*.lock)"]`, lock files still prompt while auto-accept is on. Toggling back clears every `edit`/`write` session mode, including ones set with `/permissions-mode`.

The status line shows only a switch away from the default; the default itself, review or auto-accept, shows nothing:

- `▶︎ Auto-accept edits`: the default is review, and the toggle has turned auto-accept on.
- `◉ Review edits`: the default is auto-accept, and the toggle has turned review on.
- Nothing: the default applies, the toggle has no effect (for example a blanket `ask: ["edit"]` beats it), or a session or blanket `deny` blocks edits.

## Neovim Integration

The extension includes support for pi.nvim: edit/write tool calls present Accept/Reject choices via `ctx.ui.select`, and the nvim plugin can respond with structured JSON containing the user's decision, optional modified file content, and optional review notes.

pi.nvim may return:

```json
{"result":"Accepted","notes":[...]}
{"result":"AcceptModified","content":"...","notes":[...]}
{"result":"Rejected","notes":[...]}
```

Review notes use this shape:

```ts
type ReviewNote = {
    path: string
    side: "current" | "proposed"
    lineStart: number
    lineEnd: number
    lines: string[]
    note: string
}
```

Native edit/write execution wrappers return success for editor-applied Accepted/AcceptModified outcomes without applying the change again. Ordinary acceptance delegates to native execution once. Permission and scoped-rule gates run before editor review or mutation.

Review notes from nested calls stop only the codemode script, preserve the child's outcome, and reach the model in full even when the script discards its result. Already completed effects are not rolled back. The model must quote each note completely and respond before further tool calls.

Rejection policy:

- Rejected with notes: file unchanged; the model continues to address the notes.
- Rejected without notes/cancel: abort the turn.
