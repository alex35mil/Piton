/**
 * Every test file imports this first, so tests run in the host runtime no matter where the
 * runner was launched. A sandboxed worker exports PITON_RUNTIME=sandbox, and inheriting it
 * would make fixture repositories fail the sandbox artifact guard. Tests that exercise the
 * sandbox runtime set PITON_RUNTIME themselves and restore it afterward.
 *
 * Configuration resolves through the `piton` binary: tests use this repository's debug build
 * (`mise run pi:test` builds it first) and an empty global tier of their own, never the user's.
 */

import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"

import { RUNTIME_ENV } from "../extensions/__lib/runtime.ts"
import { BIN_ENV, clearSettingsCache } from "../extensions/__lib/settings.ts"

delete process.env[RUNTIME_ENV]

const built = path.resolve(import.meta.dirname, "../../daemon/target/debug/piton")
if (!process.env[BIN_ENV]) {
    if (!fs.existsSync(built)) throw new Error(`build piton first (mise run daemon:build): ${built} is missing`)
    process.env[BIN_ENV] = built
}

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

/** Removes everything in the global tier. */
export function resetGlobalSettings(): void {
    for (const entry of fs.readdirSync(TEST_CONFIG_HOME)) {
        fs.rmSync(path.join(TEST_CONFIG_HOME, entry), { recursive: true, force: true })
    }
    clearSettingsCache()
}
