# Development

## Requirements

- macOS 27, Xcode 27 (Swift 6.4), Rust 1.99 (installed by mise).
- rustfmt from a pinned nightly, for formatting only: `rustup toolchain install nightly-2026-11-13 --profile minimal
  --component rustfmt`. The import rules in `cli/rustfmt.toml` (one `use` per crate; std, external crates, then this
  crate) are unstable options. Builds, tests, and clippy stay on stable; `cli:lint` and `cli:format` name the date.
- [mise](https://mise.jdx.dev): installs Rust and XcodeGen from `mise.toml` and runs every task.
- pi on the `PATH` (the user's global mise config pins it; this repository doesn't).
- Node (for the daemon's e2e tests and probes).

```sh
mise trust && mise install
```

## Layout

```
pi/                pi package (TypeScript): extensions, skills, workflows; tests in pi/tests
cli/               Rust crate: the `piton` binary (daemon + CLI)
  src/
  tests/e2e/       daemon + attach against real pi (faux model)
  tests/fixtures/  faux-extension.ts: credential-free scripted model for pi
  tests/probe/     pi RPC probes (docs/pi-findings.md)
apps/              Swift side
  Package.swift    all library code (PitonKit, PitonUI)
  project.yml      XcodeGen spec for the app targets; Piton.xcodeproj and Info.plist are generated, not committed
  packages/        PitonKit (protocol, connection, session store; no UI), PitonUI (shared SwiftUI)
  ios/             iOS/iPadOS app target (thin)
  macos/           macOS menu bar app target (thin)
nvim/              Neovim plugin (Lua): lua/piton/; public API and hooks in nvim/README.md, headless tests in tests/
docs/              confirmed documentation
```

## Tasks

| Command | What it does |
| --- | --- |
| `mise run cli:build` | Build the daemon and CLI (debug) |
| `mise run cli:test` | Unit tests, then the e2e tests against real pi |
| `mise run cli:lint` | Formatting (pinned nightly rustfmt), then clippy with warnings as errors |
| `mise run cli:format` | Format the Rust sources (pinned nightly rustfmt) |
| `mise run cli:install` | Install the stable binary and (re)load its launchd agent (host only; ends all sessions) |
| `mise run apps:generate` | Generate `apps/Piton.xcodeproj` from `apps/project.yml` |
| `mise run apps:build` | Build the iOS (simulator) and macOS apps into `apps/.build/xcode` |
| `mise run apps:test` | Swift package tests (protocol fixtures; integration tests skipped) |
| `mise run apps:e2e` | PitonKit's `Connection` against a real daemon and pi (faux model, host mode) |
| `mise run nvim:test` | Clean headless module-load and editor-integration checks (mocked dependencies) |
| `mise run logs:install` | Install the independent paired-iPhone log collector (host only) |
| `mise run build` / `test` / `lint` / `format` | Aggregate project tasks |

- `mise run test` runs the suites one after another: the e2e suites stream real pi and flake when they share the CPU.
- Protocol fixtures (`apps/packages/PitonKit/Tests/Fixtures`) are written by the daemon test `fixtures`
  (`cli/src/session/view.rs`), which fails when they're stale. After a protocol change, run
  `PITON_WRITE_FIXTURES=1 cargo test fixtures` in `cli/`, then fix PitonKit until `mise run apps:test` passes.

## Apps

- Minimum OS: iOS 27, macOS 27.
- One app per platform, "Piton": `com.alex35mil.piton.ios`, `com.alex35mil.piton.macos`, in every build configuration.
  Conversation state lives in the daemon; the apps keep local address/logging preferences, bounded logs, and an
  in-memory outbox. A separate dev variant protects no saved conversations. Shared IDs use the bare prefix:
  `group.com.alex35mil.piton`.
- Signing: Apple Development, team `Q62V4YP545`, automatic. Push (APNs), TestFlight, and the privileged
  helper need an active Apple Developer Program membership.
- If the macOS app later registers the daemon itself, a test build would replace the daemon your sessions run on;
  revisit a dev variant for macOS then.
- Apple-side conventions (project setup, signing, app structure, theming) follow the author's Kiddo app (private):
  align with its patterns, but don't copy its code or internals into this public repository.

## Getting the app on the iPhone and iPad

Two ways to install the same app, both host-only (signing uses the login keychain; installs and uploads need the
network):

| | `mise run apps:iphone` | `mise run apps:testflight` |
| --- | --- | --- |
| Reaches the phone | directly, cable or same Wi-Fi | TestFlight, from anywhere |
| Time | ~1 min | ~1 min upload + 5-15 min Apple processing |
| Lasts | 1 year (paid team) | 90 days per build |

Both install Piton (Release). `apps:iphone` is the normal way; TestFlight is only for updating away from the Mac.

### One-time setup

1. Active Apple Developer Program membership (team `Q62V4YP545`); Xcode → Settings → Accounts shows it as a paid
   team, not "Personal Team".
2. iPhone: connect it to the Mac once by cable and trust the Mac; then Settings → Privacy & Security → Developer Mode
   on (restart). After that, installs also work over Wi-Fi.
3. `xcrun devicectl list devices` → put the phone's name or identifier in `mise.local.toml`:

   ```toml
   [env]
   PITON_IPHONE = "Alex's iPhone"
   PITON_IPAD = "Alex's iPad"   # optional, the same steps as the phone
   ```

   Developer Mode appears in Settings only after Xcode has seen the device: if it's missing, open Xcode → Window →
   Devices and Simulators with the device connected and unlocked, or run the install once and let it fail.

4. For TestFlight:
   - App Store Connect → Apps → + → New App: iOS, name "Piton", bundle ID `com.alex35mil.piton.ios`, SKU
     `piton-ios`.
   - App Store Connect → TestFlight → Internal Testing: a group with yourself. Internal builds need no Apple review.
   - App Store Connect → Users and Access → Integrations → App Store Connect API → generate a key (role App Manager).
     Save the `.p8` outside the repo (it downloads once), e.g. `~/.appstoreconnect/AuthKey_<KEY_ID>.p8`, and add to
     `mise.local.toml`:

     ```toml
     [env]
     ASC_KEY_ID = "<KEY_ID>"
     ASC_ISSUER_ID = "<issuer id shown above the keys>"
     ASC_KEY_PATH = "~/.appstoreconnect/AuthKey_<KEY_ID>.p8"
     ```

   - Install the TestFlight app on the phone.

### Each update

- iPhone: `mise run apps:iphone` builds, installs, and launches Piton. iPad: `mise run apps:ipad`, the same (both run
  `apps/scripts/install-device.sh`).
- Away from the Mac: commit, then `mise run apps:testflight`. The build number is the commit count (always
  increasing, maps back to a commit); the marketing version (`MARKETING_VERSION` in `apps/project.yml`) changes only
  when you want it to. TestFlight notifies the phone when the build is ready; with automatic updates on in the
  TestFlight app, it installs by itself.
- macOS: `mise run apps:mac` builds Piton (Release), quits the running one, installs it in `/Applications`, and opens
  it.
- All: `mise run apps` runs `apps:mac`, `apps:iphone`, then `apps:ipad` when `PITON_IPAD` is set.

### Crashes

`mise run apps:crashes` copies Piton's crash logs (`Piton-<date>.ips`) from the phone into `apps/.build/crashes`. The
log's stack frames point into Piton's code once symbolicated against the build's dSYM
(`apps/.build/xcode/Build/Products/Release-iphoneos/Piton.app.dSYM`, kept until the next `apps:iphone`); Xcode's
Devices window (Window → Devices and Simulators → View Device Logs) symbolicates them automatically. On macOS the
logs are in `~/Library/Logs/DiagnosticReports`.

### Logging and diagnostics

The Info and Debug levels, local app controls, storage limits, and automatic paired-iPhone
collection are documented in [logging.md](logging.md). Agents read `~/.local/state/piton/logs/`; routine debugging does
not require manually copying Console output. A timeout alone doesn't prove Little Snitch or Tailscale is responsible:
use the attempt stages, error codes, event timestamps, and collector freshness as evidence.

## Sandboxed agent workers

- Every sandboxed pi gets `PITON_RUNTIME=sandbox` from the daemon. The Swift tasks then turn off SwiftPM's and the
  compiler's own sandboxes (`--disable-sandbox`, `OTHER_SWIFT_FLAGS=-disable-sandbox`), since macOS doesn't nest
  sandboxes. `mise run --dry-run <task>` shows the command.
- `[sandbox]` in `.piton/config.toml` turns on the `swift` and `rust` presets: SwiftPM's and clang's caches, Xcode's
  DerivedData, and cargo's registry and locks. Everything else this project needs (Xcode's preferences, Piton's logs)
  is readable by default; the daemon's sockets and database stay closed. It's read from the source checkout, so an
  agent can't widen its own sandbox. Without the `swift` preset, `mise run apps:build` fails at package resolution
  ("ManifestLoading couldn't be saved"). `piton sandbox why <path> [--write]` names the rule that decides.
- `.piton/config.local.toml` (ignored through `/.piton/*.local.*`) holds this machine's grants, for example a
  neighbouring checkout to write (`[sandbox]` `write = ["~/Projects/personal/tooling/pi.nvim"]`). Lists only add
  across files and levels. Applied to sessions started after a change.
- The hidden Xcode preference `IDEPackageSupportDisableManifestSandbox` must be set on the host, once per machine:
  `defaults write com.apple.dt.Xcode IDEPackageSupportDisableManifestSandbox -bool YES`.
- Sandboxes don't nest, so a sandboxed worker can't start a sandboxed pi: the e2e tests run pi in host mode and check
  a sandboxed launch's policy through `piton sandbox show`; `sh cli/tests/probe/protected.sh` checks the applied
  sandbox from a host shell. In a sandboxed session, `PITON_BIN` is the installed `piton`, which may be older than the
  code: run `apps:test` with `PITON_BIN=$PWD/cli/target/debug/piton` after changing the daemon's protocol.
- Installing the daemon and launching apps are host-only.

## Workflow reports and session analysis

The pi package knows it's a development install when it runs from a git checkout of Piton (its folder is the
checkout's `pi/`, next to `cli/`). Then:

- "Report this workflow issue" files Todo work in this checkout's tracker (`[tracker]` of `.piton/config.toml`)
  instead of handing the user a pre-filled GitHub issue.
- Every shipped Task or Gig, completed Epic, and shipped incoming PR ends with an in-chat analysis of the session for
  workflow problems. It costs tokens; turn it off in the global `config.host.toml`:

  ```toml
  [development]
  session_analysis = false
  ```

  The setting has no effect in an app install.
