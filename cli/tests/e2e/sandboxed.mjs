// What the suites that run real pi in the real sandbox share (blocks.mjs, network.mjs, sandbox.mjs): a scratch
// configuration with the faux model and Piton's pi package, the debug daemon, an attached client, and the loop that runs
// each test against a daemon of its own. Sandboxes don't nest, so in a sandboxed session (PITON_RUNTIME=sandbox) a suite
// skips every test.
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, realpathSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import assert from "node:assert/strict";

import { writeConfig } from "./config.mjs";

export const root = resolve(import.meta.dirname, "../..");
export const piton = join(root, "target/debug/piton");
export const ext = join(root, "tests/fixtures/faux-extension.ts");
export const pkg = resolve(root, "../pi");
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
export const nested = process.env.PITON_RUNTIME === "sandbox";

/// Where writes outside every grant go: no profile's write root, and not the user's home folder. Each run makes a folder
/// of its own there (`away()`); `runSuite` removes it, and removes earlier runs' folders older than a day first, since a
/// run killed with SIGKILL leaves its own behind.
const AWAY_ROOT = "/private/var/tmp";
const AWAY_PREFIX = "piton-test.";
let awayDir;
export function away() {
    awayDir ??= mkdtempSync(join(AWAY_ROOT, AWAY_PREFIX));
    return awayDir;
}
function sweepAway() {
    const dayAgo = Date.now() - 24 * 60 * 60 * 1000;
    for (const name of readdirSync(AWAY_ROOT)) {
        if (!name.startsWith(AWAY_PREFIX)) continue;
        const path = join(AWAY_ROOT, name);
        try {
            if (statSync(path).mtimeMs < dayAgo) rmSync(path, { recursive: true, force: true });
        } catch {}
    }
}

/// A scratch folder: a workspace (`ws/`, a Git checkout), pi's agent directory with Piton's pi package, the daemon's
/// state folder, and the global configuration (sandbox attach mode, `[tools.permissions] default_mode = "allow"` so the
/// permissions extension doesn't ask before each call). `settings` adds to the agent directory's `settings.json`;
/// `files` writes workspace files (relative path to contents), such as `.piton/config.local.toml`.
export function setup(prefix, { settings = {}, files = {} } = {}) {
    const dir = realpathSync(mkdtempSync(join(tmpdir(), `piton-${prefix}-`)));
    const ws = join(dir, "ws");
    const agent = join(dir, "agent");
    mkdirSync(join(ws, ".git"), { recursive: true });
    mkdirSync(agent);
    writeFileSync(join(agent, "settings.json"), JSON.stringify({ packages: [pkg], ...settings }));
    for (const [path, contents] of Object.entries(files)) {
        mkdirSync(dirname(join(ws, path)), { recursive: true });
        writeFileSync(join(ws, path), contents);
    }
    mkdirSync(join(dir, "state"));
    const config = writeConfig(dir, {
        daemon: {
            socket: join(dir, "state/piton.sock"),
            log: join(dir, "piton.log"),
            pi: { args: ["-e", ext, "--model", "faux/faux-1"] },
            env: { login_shell: false },
            integrations: { mise: { enabled: false }, tailscale: { enabled: false } },
            stop: { terminate_after: "3s", kill_after: "1s" },
        },
        host: { default_attach_mode: "sandbox", stop_empty_session_after: "60s" },
        project: { tools: { permissions: { default_mode: "allow" } } },
    });
    // The faux `lift` tool imports Piton's pi package's lift library from this checkout.
    const env = {
        ...process.env,
        PITON_CONFIG_HOME: config,
        PI_CODING_AGENT_DIR: agent,
        PITON_FAUX_LIFT_LIB: join(pkg, "extensions/__lib/lift.ts"),
    };
    return { dir, ws, agent, config, env };
}

export async function startDaemon(ctx) {
    const d = spawn(piton, ["daemon"], { env: ctx.env, stdio: ["ignore", "ignore", "pipe"] });
    let err = "";
    d.stderr.on("data", (c) => (err += c));
    for (let i = 0; i < 100 && !err.includes("listening"); i++) await sleep(50);
    assert.ok(err.includes("listening"), "daemon didn't start: " + err);
    d.exited = new Promise((r) => d.on("exit", (code) => r(code)));
    return d;
}

/// `piton trust allow|deny` for the workspace: a repository with grants or pi project files needs a decision first.
export function trust(ctx, decision = "allow") {
    return new Promise((res, rej) => {
        const p = spawn(piton, ["trust", decision, ctx.ws], { env: ctx.env, stdio: ["ignore", "pipe", "pipe"] });
        let err = "";
        p.stderr.on("data", (d) => (err += d));
        p.on("exit", (code) => (code === 0 ? res() : rej(new Error(`piton trust ${decision}: ${err}`))));
    });
}

/// `piton attach` in the workspace. `onRecord` sees every record as it arrives (to answer questions as they come).
export function attach(ctx, { onRecord } = {}) {
    const child = spawn(piton, ["attach"], { cwd: ctx.ws, env: ctx.env, stdio: ["pipe", "pipe", "pipe"] });
    const c = { child, records: [], waiters: [], stderr: "", n: 0 };
    let buf = "";
    child.stdout.on("data", (chunk) => {
        buf += chunk;
        let nl;
        while ((nl = buf.indexOf("\n")) >= 0) {
            const line = buf.slice(0, nl);
            buf = buf.slice(nl + 1);
            if (!line.trim()) continue;
            const rec = JSON.parse(line);
            c.records.push(rec);
            onRecord?.(rec, c);
            for (const w of [...c.waiters]) if (w.pred(rec)) { c.waiters.splice(c.waiters.indexOf(w), 1); w.resolve(rec); }
        }
    });
    child.stderr.on("data", (d) => (c.stderr += d));
    c.exited = new Promise((r) => child.on("exit", () => r()));
    c.send = (cmd) => child.stdin.write(JSON.stringify({ ...cmd, id: cmd.id ?? `c-${++c.n}` }) + "\n");
    c.waitNew = (pred, ms = 20000) => {
        const from = c.records.length;
        return new Promise((res, rej) => {
            const w = { pred: (r) => c.records.indexOf(r) >= from && pred(r), resolve: res };
            c.waiters.push(w);
            setTimeout(() => rej(new Error(`timeout; last ${JSON.stringify(c.records.slice(-4).map((r) => r.type))}; ${c.stderr}`)), ms);
        });
    };
    /** Sends an RPC command and returns its response. */
    c.call = async (cmd, ms = 20000) => {
        const id = `c-${++c.n}`;
        const answered = c.waitNew((r) => r.type === "response" && r.id === id, ms);
        c.send({ ...cmd, id });
        return answered;
    };
    /** Sends a prompt and waits for the bash call it makes to end; returns its result text. */
    c.bash = async (prompt) => {
        const ended = c.waitNew((r) => r.type === "tool_execution_end" && r.toolName === "bash", 60000);
        c.send({ type: "prompt", message: prompt });
        const end = await ended;
        await c.waitNew((r) => r.type === "agent_settled", 30000).catch(() => {});
        return JSON.stringify(end.result);
    };
    c.close = () => {
        child.stdin.end();
        return c.exited;
    };
    return c;
}

/// Runs each test against a daemon of its own: `prepare(name)` builds the test's context (`setup` and anything the test
/// needs running before the daemon starts), `cleanup(ctx)` removes what it left outside the scratch folder and `away()`.
/// Exits 1
/// when any test failed; prints the daemon log's tail for a failure.
export async function runSuite(file, tests, { prepare, cleanup = () => {}, skip = "sandboxes don't nest" }) {
    if (nested) {
        console.log(`skip  ${file}: ${skip}`);
        process.exit(0);
    }
    sweepAway();
    let failed = 0;
    for (const [name, test] of Object.entries(tests)) {
        const ctx = await prepare(name);
        let daemon;
        try {
            daemon = await startDaemon(ctx);
            await test(ctx);
            console.log(`ok    ${name}`);
        } catch (e) {
            failed++;
            console.log(`FAIL  ${name}\n${e.stack}`);
            try {
                console.log(readFileSync(join(ctx.dir, "piton.log"), "utf8").split("\n").slice(-30).join("\n"));
            } catch {}
        } finally {
            if (daemon) {
                daemon.kill("SIGTERM");
                await daemon.exited;
            }
            await cleanup(ctx);
            rmSync(ctx.dir, { recursive: true, force: true });
        }
    }
    if (awayDir) rmSync(awayDir, { recursive: true, force: true });
    process.exit(failed ? 1 : 0);
}
