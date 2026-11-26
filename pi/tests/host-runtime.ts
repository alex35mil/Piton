/**
 * Every test file imports this first, so tests run outside any session, in the host runtime, no matter where the runner
 * was launched: a session's channel would make `piton` answer with the runner's own session's facts. Tests that need a
 * session set up a fake daemon.
 *
 * Configuration resolves through the `piton` binary: tests use this repository's debug build (`mise run pi:test` builds
 * it first), a global tier and a state folder of their own, never the user's.
 */

import { execFileSync } from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { DatabaseSync } from "node:sqlite"

import { BIN_ENV, clearSettingsCache } from "../extensions/__lib/settings.ts"

delete process.env.PITON_AGENT_SOCKET
delete process.env.PITON_AGENT_TOKEN

// Always this checkout's build: a session's PITON_BIN is the installed piton, which may be older than the code tested.
const built = path.resolve(import.meta.dirname, "../../cli/target/debug/piton")
if (!fs.existsSync(built)) throw new Error(`build piton first (mise run cli:build): ${built} is missing`)
process.env[BIN_ENV] = built

/** The test process's own global tier (`$XDG_CONFIG_HOME/piton`) and pi agent folder. */
const xdg = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "piton-test-xdg-")))
export const TEST_CONFIG_HOME = path.join(xdg, "piton")
fs.mkdirSync(TEST_CONFIG_HOME)
process.env.XDG_CONFIG_HOME = xdg
// Never the user's own: piton refuses its old files there. Tests that need another set it themselves.
process.env.PI_CODING_AGENT_DIR = fs.mkdtempSync(path.join(os.tmpdir(), "piton-test-agent-"))

/**
 * The test process's own state folder: the debug build takes the daemon's socket, and so its state folder, from the
 * host file. Trust decisions live in its `piton.db`.
 */
export const TEST_STATE = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "piton-test-state-")))
/** The host file's line that keeps the state folder the test's; a test that rewrites the file keeps it. */
export const TEST_HOST_FILE = `socket = ${JSON.stringify(path.join(TEST_STATE, "piton.sock"))}\n`
fs.writeFileSync(path.join(TEST_CONFIG_HOME, "config.host.toml"), TEST_HOST_FILE)

/**
 * Records the repository at `dir` (its source checkout) as trusted, as `piton trust allow` does, so its committed grants
 * apply, as in a session of a trusted repository.
 */
export function trustRepository(dir: string): void {
    const db = new DatabaseSync(path.join(TEST_STATE, "piton.db"))
    try {
        db.exec(`CREATE TABLE IF NOT EXISTS host (id INTEGER PRIMARY KEY CHECK (id = 1), host_id TEXT NOT NULL);
            INSERT OR IGNORE INTO host VALUES (1, 'a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0');
            CREATE TABLE IF NOT EXISTS trust (source TEXT PRIMARY KEY, decision TEXT NOT NULL
                CHECK (decision IN ('trusted', 'declined')), grants TEXT NOT NULL, decided_at TEXT NOT NULL);
            PRAGMA user_version = 2;`)
        db.prepare("INSERT OR REPLACE INTO trust VALUES (?, 'trusted', '[]', '2026-01-01T00:00:00Z')").run(
            fs.realpathSync(dir),
        )
    } finally {
        db.close()
    }
    clearSettingsCache()
}

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
 * file names them (`"tools.permissions.allow"`, `"tracker"`), objects as tables; each named key is replaced whole. `dir` is a
 * repository (its source checkout's `.piton/config.toml`), `"global"` for the test's global `config.toml`, or `"host"`
 * for its `config.host.toml` (`[debugging]`). `local` writes the pair's `.local` file. Returns the file.
 */
export function writeConfig(dir: string, changes: Record<string, unknown>, local = false): string {
    const args = ["config", "set", JSON.stringify(changes), "--changes"]
    if (dir === "global") args.push("--global")
    else if (dir === "host") args.push("--scope", "host")
    else {
        args.push("--workspace", dir)
        // A committed file's grants apply only in a trusted repository.
        if (!local) trustRepository(sourceOf(dir))
    }
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

/**
 * The `facts` line a fake daemon answers with: a session in `runtime` whose workspace is `workspace`, keeping every
 * grant, with the workspace's protected files (and none outside it) and the test's global tier.
 */
export function factsLine(runtime: "host" | "sandbox", workspace: string) {
    const real = fs.realpathSync(workspace)
    return {
        type: "facts",
        runtime,
        grants: { kind: "all" },
        protected: { source: real, checkout: real, areas: [], global: TEST_CONFIG_HOME, excluded: [] },
        global: TEST_CONFIG_HOME,
        workspace: { path: real, checkout: real, relation: { kind: "source" } },
    }
}

/**
 * Runs `body` as inside a session whose daemon answers `facts` with `runtime` (and nothing else): a fake `agent.sock` in
 * a worker thread, so a `piton` the test runs synchronously can ask it too. Restores the environment afterwards.
 */
export async function withSessionFacts<T>(
    runtime: "host" | "sandbox",
    body: () => Promise<T> | T,
    workspace?: string,
): Promise<T> {
    const { Worker } = await import("node:worker_threads")
    const { clearFactsCache } = await import("../extensions/__lib/agent.ts")
    const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "piton-facts-")))
    const socket = path.join(dir, "agent.sock")
    const facts = factsLine(runtime, workspace ?? dir)
    const worker = new Worker(
        `const net = require("node:net"); const { parentPort, workerData } = require("node:worker_threads");
        net.createServer((c) => c.once("data", () => c.end(JSON.stringify(workerData.facts) + "\\n")))
            .listen(workerData.socket, () => parentPort.postMessage("listening"))`,
        { eval: true, workerData: { socket, facts } },
    )
    await new Promise((resolve) => worker.once("message", resolve))
    const previous = { socket: process.env.PITON_AGENT_SOCKET, token: process.env.PITON_AGENT_TOKEN }
    process.env.PITON_AGENT_SOCKET = socket
    process.env.PITON_AGENT_TOKEN = "test"
    clearFactsCache()
    clearSettingsCache()
    try {
        return await body()
    } finally {
        if (previous.socket === undefined) delete process.env.PITON_AGENT_SOCKET
        else process.env.PITON_AGENT_SOCKET = previous.socket
        if (previous.token === undefined) delete process.env.PITON_AGENT_TOKEN
        else process.env.PITON_AGENT_TOKEN = previous.token
        clearFactsCache()
        clearSettingsCache()
        await worker.terminate()
        fs.rmSync(dir, { recursive: true, force: true })
    }
}

/** The source checkout of `dir`: a worker's (`agent.source`), else the Git checkout it's in, else `dir`. */
function sourceOf(dir: string): string {
    const git = (args: string[]) => {
        try {
            return execFileSync("git", args, {
                cwd: dir,
                encoding: "utf-8",
                stdio: ["ignore", "pipe", "ignore"],
            }).trim()
        } catch {
            return ""
        }
    }
    return git(["config", "--local", "agent.source"]) || git(["rev-parse", "--show-toplevel"]) || dir
}

/** Removes everything in the global tier but the test's state folder. */
export function resetGlobalSettings(): void {
    for (const entry of fs.readdirSync(TEST_CONFIG_HOME)) {
        fs.rmSync(path.join(TEST_CONFIG_HOME, entry), { recursive: true, force: true })
    }
    fs.writeFileSync(path.join(TEST_CONFIG_HOME, "config.host.toml"), TEST_HOST_FILE)
    clearSettingsCache()
}
