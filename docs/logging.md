# Logging and unattended diagnostics

Logging is independent of pi's settings and session files. The apps use Apple's unified logger and a bounded file
sink; the daemon uses bounded file logs. The phone collector is separate host-side development tooling, not part of
the daemon or the app's connection path.

## Levels and controls

Records are `info` or `debug`, and the logging level decides which are kept.

**Info** is the default: essential lifecycle/connection events and failures only. These also go to small rotating files
so agents can investigate an earlier incident. Detailed timing, network-path, ping/pong, snapshot, and outbox traces are
omitted. The collector makes **no device calls** at Info.

**Debug** adds those detailed traces and turns on phone-log collection. Enable it for **one hour** (the default) or
**until disabled**. Expiration is an absolute timestamp, so re-reading or reconnecting never extends the hour. Switching
back to Info preserves existing files.

**Host logging** (the Mac app, the daemon, and the collector) has one source of truth: `logging` in Piton's
`config.host.toml` is the default (`info` unless set), and the daemon holds the policy in effect. Switching it from the Mac
app or the CLI is **temporary**: it lasts until it expires or the daemon restarts, which returns to the config. Nothing
else stores it. The daemon pushes the policy to the apps (including when a timed switch expires), the Mac app's own
capture follows it, and the collector asks the daemon before every poll and transfer.

**The phone** keeps its own capture policy in its app container (`Library/Logs/Piton/settings.json`), so it works while
disconnected. There, the expiration persists across app restarts; an expired or malformed file falls back to Info.

- **Apps:** **Settings > Logging** (⌘, on the Mac; the Settings tab on the iPhone). On the iPhone it sets the phone's
  own capture. On the Mac it switches host logging through the daemon, shows the configured default, and
  needs the daemon connection.
- **CLI, on the host:**

  ```sh
  piton logging status
  piton logging debug                    # one hour
  piton logging debug --until-disabled
  piton logging info
  ```

  These go through the daemon's control socket and fail when the daemon isn't running. No sessions are restarted. A
  transfer already in flight may finish (at most 15 seconds), but switching back or expiry prevents it from replacing
  the published snapshot. To make debug the default, set it in `config.host.toml` (or `config.host.local.toml` to
  keep it out of your dotfiles) and restart the daemon:

  ```toml
  logging = "debug"   # or "info" (the default)
  ```
- **Clearing:** each app's Logging screen can clear its own file ring without changing the level. It does not clear
  collected copies. Files naturally age out by size as new records are written; no time-based retention is promised.

Switch **the phone and the Mac** to Debug to capture and collect full phone traces. Host collection can also retrieve
the phone's Info records without debug on the phone. Debug can't reconstruct old detailed events that weren't
captured.

## Files agents can read

Default host root: `~/.local/state/piton/logs/`:

```
daemon/current.log              info records, plus debug records while at Debug
macos/current.log               Mac app capture
collector/current.log           collector failures
iphone/status.json              collection status (collected, disabled, no daemon, unavailable), timestamps, failure
iphone/snapshot/current.log     last successfully copied iPhone capture
```

Swift archives are `archive-1.log` through `archive-4.log`. Rust archives are `current.log.1` through `current.log.4`.
The phone stores its file ring and local `settings.json` at `Library/Logs/Piton/` in its own app container. Copies live
under `iphone/snapshot/`; status timestamps describe **collection time**, not when the last event occurred. Always
check both the collector status and event timestamps before treating a capture as current.

The `log` key of `config.host.toml` overrides the daemon's file path. Its default moved from
`~/.local/state/piton/piton.log` to `logs/daemon/current.log`. Old files are not migrated or deleted. New LaunchAgents
use the bounded file writers instead of appending a duplicate unbounded `launchd.log`. A foreground daemon's info
records still go to stderr. Early configuration failures leave a sanitized startup marker in the default daemon log.

The sandbox should grant **read-only access to the logs directory**, not its parent (which contains the control
socket). This project's `.piton/config.toml` grants it. For agents in other projects, add it to the global
`config.toml` (`~/.config/piton/`) in the dotfiles:

```toml
[sandbox]
read = ["~/.local/state/piton/logs"]
```

Grant lists only add across levels. Grants apply to newly launched sandbox processes, not existing ones.
Agents receive no CoreDevice access, settings write access, daemon control socket access, or host command execution.

`PITON_LOGS_DIR` can select a different host root for development and tests; the Mac app and CLI understand it. All test
tasks use temporary roots so they never change the owner's capture policy or logs. Installed services use the default
root unless their launchd environment is explicitly configured otherwise.

## Install the independent phone collector (host only)

Requirements: Xcode and its command-line tools, a paired/trusted iPhone, access to Piton's app data container, and
`PITON_IPHONE` configured as for `apps:iphone`. Start with the development-signed installation from `apps:iphone`;
container access on other distribution/signing variants must be verified rather than assumed.

```sh
mise run cli:install       # install updated binary; this step ends live sessions
mise run apps:iphone
mise run apps:mac
mise run logs:install         # uses the installed piton; does NOT restart the daemon
piton logging debug
```

Switch the phone's Logging screen to Debug as needed. The collector LaunchAgent is
`com.alex35mil.piton.logs`. It runs the installed binary's `piton logging collect --device <device>`, starts at login,
and is supervised independently of the daemon. To stop/remove it: `piton logging uninstall`.

While the host is at Debug, collection uses `xcrun devicectl device copy from`, restricted to Piton's
`Library/Logs/Piton` app-container directory. It works over USB or Xcode's paired-device wireless connection, not
Piton's WebSocket or Tailscale. Successful polls are 30 seconds apart; failures back off to five minutes. Each transfer
has a 12-second tool deadline and 15-second outer deadline. It does not launch the app or request iOS background
execution. Transfers still have a network/battery cost, which is why there's no polling at Info, for daily use.

The collector retains the last good snapshot on failures, validates its fixed file names/sizes, and rejects symlinks.
If the phone is unreachable, locked against container access, or iOS/CoreDevice refuses it, `status.json` reports the
failure and last-success time. No collector can retrieve new logs from a completely unreachable phone. The phone
keeps capturing locally; copying resumes when Apple's paired-device channel becomes available again.

**Observed working on the owner's development-signed iPhone (2026-11-09):** the collector retrieved live phone logs
while Piton's WebSocket connection was failing. Other hosts/signing variants still need their first successful copy
checked. Automated tests use a fake device tool and cannot verify Apple's pairing/container-access behavior.

## Reading connection failures

For example, `http_status=503 proxy=true` with `NSURLErrorDomain:-1011` means the WebSocket upgrade failed through a
proxy/relay path; it does not establish that the daemon returned 503. Compare with daemon-side TCP/upgrade traces.
The app leaves the system proxy settings in effect, as apps normally do. `ws.handshake_begin` records
`system_proxies=`, the proxies the system configuration gives for the Mac's URL in the order they're tried (`none` is
a direct connection; a PAC script is logged by URL, not evaluated). Compare it with `proxy=` in `ws.metrics` to tell a
configured proxy from a connection iOS reports as proxied for another reason.

Resource cleanup is as important as log limits: every WebSocket terminal path invalidates its URLSession. Apple
retains session delegates until invalidation; closing just a task or finishing the event stream leaks them across
retries. Integration tests exercise deallocation after real failed handshakes and explicit closes.

For a possible collector-related device slowdown, pause device polling with host `piton logging info`. Phone
capture is a separate setting and can remain enabled. A small file size doesn't prove CoreDevice service work is
cheap; use a controlled comparison and Instruments when the issue reproduces.

## Resource and privacy limits

- Each file is capped at **2 MiB**, with five files per component/device: **10 MiB retained** per ring. The collector
  keeps one phone snapshot, plus one temporary transfer/previous snapshot during replacement. Those temporary copies
  can briefly add up to another 20 MiB; OS unified-log storage is managed separately by Apple.
- File writes are serialized away from UI/network/session paths. Admission queues are capped at 256 records; overload
  drops events instead of consuming unbounded memory or blocking work. Swift captures at most 20 events/second; Rust
  captures at most 100, including at most 20 detailed events. Records are length-bounded too.
- Disk-full/write errors do not break sessions, sending, or connections. Swift file-write error notices are limited
  to once per minute. Log files are diagnostic evidence, not a guaranteed-delivery audit trail.
- No prompts, assistant text, RPC payloads, dialog answers, credentials, or arbitrary error descriptions are recorded.
  Network errors retain domains/codes, including underlying POSIX errors. Raw pi stderr and full launch arguments are
  not persisted; pi stderr still reaches attached clients as before. Device names and host filesystem paths can appear
  in daemon metadata, so logs should still be treated as private local data.

For manual Apple-log inspection, use subsystem `com.alex35mil.piton`, categories `connection` and `lifecycle`. Agents
normally read the files above; Console copying is not the routine collection workflow.
