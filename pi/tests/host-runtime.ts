/**
 * Every test file imports this first, so tests run in the host runtime no matter where the
 * runner was launched. A sandboxed session exports PITON_RUNTIME=sandbox, which would select
 * fixtures' `[permissions.sandbox]` rules. Tests that exercise the sandbox runtime set
 * PITON_RUNTIME themselves and restore it afterward.
 *
 * Configuration resolves through the `piton` binary: tests use this repository's debug build
 * (`mise run pi:test` builds it first) and an empty global tier of their own, never the user's.
 */

import { execFileSync } from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"

import { RUNTIME_ENV } from "../extensions/__lib/runtime.ts"
import { BIN_ENV, clearSettingsCache } from "../extensions/__lib/settings.ts"

delete process.env[RUNTIME_ENV]
// A session under piton has its own channel to the daemon; tests that need one set up a fake.
delete process.env.PITON_AGENT_SOCKET
delete process.env.PITON_SESSION_TOKEN

// Always this checkout's build: a session's PITON_BIN is the installed piton, which may be older than the code tested.
const built = path.resolve(import.meta.dirname, "../../cli/target/debug/piton")
if (!fs.existsSync(built)) throw new Error(`build piton first (mise run cli:build): ${built} is missing`)
process.env[BIN_ENV] = built

/** The test process's own global tier and pi agent folder. */
export const TEST_CONFIG_HOME = fs.mkdtempSync(path.join(os.tmpdir(), "piton-test-config-"))
process.env.PITON_CONFIG_HOME = TEST_CONFIG_HOME
// Never the user's own: piton refuses its old files there. Tests that need another set it themselves.
process.env.PI_CODING_AGENT_DIR = fs.mkdtempSync(path.join(os.tmpdir(), "piton-test-agent-"))

/** Writes `<global tier>/<name>` (JSON for objects) and forgets cached resolutions. */
export function writeGlobalSetting(name: string, value: unknown): string {
    const file = path.join(TEST_CONFIG_HOME, name)
    fs.mkdirSync(path.dirname(file), { recursive: true })
    fs.writeFileSync(file, typeof value === "string" ? value : JSON.stringify(value))
    clearSettingsCache()
    return file
}

/**
 * Writes settings to a configuration file through the binary, as the pi package does. `changes` has dotted keys as the
 * file names them (`"permissions.allow"`, `"tracker"`), objects as tables; each named key is replaced whole. `dir` is a
 * repository (its source checkout's `.piton/config.toml`), `"global"` for the test's global `config.toml`, or `"host"`
 * for its `config.host.toml` (`[development]`). `local` writes the pair's `.local` file. Returns the file.
 */
export function writeConfig(dir: string, changes: Record<string, unknown>, local = false): string {
    const args = ["config", "set", JSON.stringify(changes), "--changes"]
    if (dir === "global") args.push("--global")
    else if (dir === "host") args.push("--scope", "host")
    else args.push("--workspace", dir)
    if (local) args.push("--local")
    try {
        execFileSync(process.env[BIN_ENV]!, args, { encoding: "utf-8", stdio: ["ignore", "pipe", "pipe"] })
    } catch (error) {
        const failure = error as { stdout?: string; stderr?: string }
        throw new Error(`writeConfig: ${failure.stdout || failure.stderr}`)
    }
    clearSettingsCache()
    if (dir === "host") return path.join(TEST_CONFIG_HOME, local ? "config.host.local.toml" : "config.host.toml")
    const folder = dir === "global" ? TEST_CONFIG_HOME : path.join(fs.realpathSync(dir), ".piton")
    return path.join(folder, local ? "config.local.toml" : "config.toml")
}

/** Removes everything in the global tier. */
export function resetGlobalSettings(): void {
    for (const entry of fs.readdirSync(TEST_CONFIG_HOME)) {
        fs.rmSync(path.join(TEST_CONFIG_HOME, entry), { recursive: true, force: true })
    }
    clearSettingsCache()
}
