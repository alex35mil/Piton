// End-to-end tests of launching pi behind fnox's credential proxy, with a fake fnox (tests/fixtures/fake-fnox.sh)
// against real pi with the faux extension. Host mode: a sandboxed session can't start a sandboxed pi, so the sandbox
// checks (writable fnox files, the keychain) are unit tests.
// Usage (in cli/): cargo build && node tests/e2e/fnox.mjs [test-name...], or `mise run cli:test`.
import { execFileSync, spawn } from "node:child_process";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync, realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import net from "node:net";
import assert from "node:assert/strict";

import { writeConfig } from "./config.mjs";

const root = resolve(import.meta.dirname, "../..");
const piton = join(root, "target/debug/piton");
const ext = join(root, "tests/fixtures/faux-extension.ts");
const fakeFnox = join(root, "tests/fixtures/fake-fnox.sh");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/// `fnox`: the `[tools.fnox]` table. `files`: the workspace's files, which steer the fake fnox. `project`: the global
/// `config.toml`. `mise`: run mise for the workspace's environment (its `mise.toml` in `files`).
function setup({ fnox = { enabled: true, command: [fakeFnox] }, files = {}, project = {}, mise = false } = {}) {
    const dir = realpathSync(mkdtempSync(join(tmpdir(), "piton-fnox-")));
    const projects = join(dir, "Projects");
    const ws = join(projects, "ws");
    const agent = join(dir, "agent");
    mkdirSync(join(ws, ".git"), { recursive: true });
    mkdirSync(agent);
    for (const [name, content] of Object.entries(files)) writeFileSync(join(ws, name), content);
    // The daemon's state folder is its socket's: apart from pi's agent directory, which a sandbox may write.
    mkdirSync(join(dir, "state"));
    const socket = join(dir, "state", "piton.sock");
    const config = writeConfig(dir, {
        daemon: {
            socket,
            log: join(dir, "piton.log"),
            tailnet: { enabled: false },
            pi: { args: ["-e", ext, "--model", "faux/faux-1"] },
            env: { login_shell: false },
            tools: { mise: { enabled: mise }, fnox },
            stop: { grace_ms: 3000, term_grace_ms: 1000 },
        },
        host: { default_attach_mode: "host", empty_grace_ms: 60000, roots: [projects] },
        project,
    });
    // The daemon's own FNOX_PROFILE reaches pi; a workspace's would not (mise is off here, so only the base sets it).
    const env = {
        ...process.env,
        PITON_CONFIG_HOME: config,
        PI_CODING_AGENT_DIR: agent,
        PITON_FAUX_TPS: "200",
        FNOX_PROFILE: "agent",
        MISE_TRUSTED_CONFIG_PATHS: dir,
    };
    delete env.PITON_WORKSPACE_MARKER;
    return { dir, ws, socket, env };
}

async function startDaemon(ctx) {
    const d = spawn(piton, ["daemon"], { env: ctx.env, stdio: ["ignore", "ignore", "pipe"] });
    let err = "";
    d.stderr.on("data", (c) => (err += c));
    for (let i = 0; i < 100 && !err.includes("listening"); i++) await sleep(50);
    assert.ok(err.includes("listening"), "daemon didn't start: " + err);
    d.exited = new Promise((r) => d.on("exit", (code) => r(code)));
    d.err = () => err;
    return d;
}

/// A view client (the apps' protocol): requests, replies, pushed lines, and the session model they build.
function viewClient(ctx, name) {
    const sock = net.connect(ctx.socket);
    const c = { lines: [], waiters: [], n: 0 };
    let buf = "";
    sock.on("data", (chunk) => {
        buf += chunk;
        let nl;
        while ((nl = buf.indexOf("\n")) >= 0) {
            const line = buf.slice(0, nl);
            buf = buf.slice(nl + 1);
            if (!line.trim()) continue;
            const msg = JSON.parse(line);
            c.lines.push(msg);
            for (const w of [...c.waiters]) if (w.pred(msg)) { c.waiters.splice(c.waiters.indexOf(w), 1); w.resolve(msg); }
        }
    });
    sock.write(JSON.stringify({ protocol: "view", version: 1, client: name }) + "\n");
    c.wait = (pred, ms = 15000) => {
        const hit = c.lines.find(pred);
        if (hit) return Promise.resolve(hit);
        return new Promise((res, rej) => {
            c.waiters.push({ pred, resolve: res });
            setTimeout(() => rej(new Error(`${name}: timeout; last ${JSON.stringify(c.lines.slice(-3)).slice(0, 600)}`)), ms);
        });
    };
    c.reqRaw = async (type, args = {}) => {
        const id = `${name}-${++c.n}`;
        sock.write(JSON.stringify({ id, type, ...args }) + "\n");
        return c.wait((m) => m.type === "reply" && m.id === id);
    };
    c.req = async (type, args = {}) => {
        const reply = await c.reqRaw(type, args);
        if (!reply.ok) throw new Error(`${type}: ${reply.error}`);
        return reply.data;
    };
    c.items = (session) => {
        let items = [];
        for (const m of c.lines) {
            if (m.session !== session) continue;
            if (m.type === "snapshot") items = structuredClone(m.items);
            else if (m.type === "change" && m.change.op === "append") items.push(structuredClone(m.change.item));
            else if (m.type === "change" && m.change.op === "replace") items = items.map((i) => (i.id === m.change.item.id ? m.change.item : i));
            else if (m.type === "change" && m.change.op === "reset") items = [];
        }
        return items;
    };
    c.close = () => sock.end();
    return c;
}

/// `piton attach` (pi.nvim's way in): pi's records, and the ones the daemon adds.
function attach(ctx) {
    const child = spawn(piton, ["attach"], { cwd: ctx.ws, env: ctx.env, stdio: ["pipe", "pipe", "pipe"] });
    const a = { records: [], waiters: [], stderr: "", n: 0 };
    let buf = "";
    child.stdout.on("data", (chunk) => {
        buf += chunk;
        let nl;
        while ((nl = buf.indexOf("\n")) >= 0) {
            const line = buf.slice(0, nl);
            buf = buf.slice(nl + 1);
            if (!line.trim()) continue;
            const rec = JSON.parse(line);
            a.records.push(rec);
            for (const w of [...a.waiters]) if (w.pred(rec)) { a.waiters.splice(a.waiters.indexOf(w), 1); w.resolve(rec); }
        }
    });
    child.stderr.on("data", (d) => (a.stderr += d));
    a.exited = new Promise((r) => child.on("exit", (code) => r(code)));
    a.wait = (pred, ms = 15000) => {
        const hit = a.records.find(pred);
        if (hit) return Promise.resolve(hit);
        return new Promise((res, rej) => {
            a.waiters.push({ pred, resolve: res });
            setTimeout(() => rej(new Error(`attach: timeout; stderr ${a.stderr}`)), ms);
        });
    };
    a.call = async (cmd) => {
        const id = `a-${++a.n}`;
        child.stdin.write(JSON.stringify({ ...cmd, id }) + "\n");
        return a.wait((r) => r.type === "response" && r.id === id);
    };
    a.close = () => {
        child.stdin.end();
        return a.exited;
    };
    return a;
}

/// Runs the faux `env` tool in `session` and returns what pi's environment held.
async function piEnv(c, session) {
    await c.req("session.send", { session, message_id: "env", text: "env" });
    const done = (i) => i.type === "tool" && i.status === "done";
    await c.wait(() => c.items(session).some(done), 20000);
    const tool = c.items(session).find(done);
    const out = await c.req("session.tool_output", { session, call_id: tool.call_id });
    return JSON.parse(out.content[0].text);
}

const tests = {
    async "pi starts behind the proxy with the daemon's fnox variables"(ctx) {
        const c = viewClient(ctx, "mac");
        const { session } = await c.req("session.start", { workspace: ctx.ws, mode: "host" });
        await c.req("session.watch", { session: session.key });
        const env = await piEnv(c, session.key);
        assert.equal(env.fnox, "1", "pi runs under fnox");
        assert.equal(env.githubToken, "fnox_proxy_0000", "pi holds the placeholder");
        assert.equal(env.fnoxProfile, "agent");
        const args = readFileSync(join(ctx.ws, "fake-fnox-args"), "utf8").split("\n");
        assert.deepEqual(args.slice(0, 2), ["--no-sandbox", "--"]);
        assert.ok(!c.items(session.key).some((i) => i.type === "notice"), "no warning when every secret resolves");
        c.close();
    },

    async "no [proxy] for the workspace: pi starts without fnox"(ctx) {
        writeFileSync(join(ctx.ws, "fake-fnox-no-proxy"), "");
        const c = viewClient(ctx, "mac");
        const { session } = await c.req("session.start", { workspace: ctx.ws, mode: "host" });
        await c.req("session.watch", { session: session.key });
        const env = await piEnv(c, session.key);
        assert.equal(env.fnox, null);
        c.close();
    },

    async "a broken fnox config refuses the launch with fnox's error"(ctx) {
        writeFileSync(join(ctx.ws, "fake-fnox-broken"), "");
        const c = viewClient(ctx, "mac");
        const reply = await c.reqRaw("session.start", { workspace: ctx.ws, mode: "host" });
        assert.equal(reply.ok, false);
        assert.match(reply.error, /fnox's configuration .* is invalid: .*unknown field `hosts`/s);
        c.close();
    },

    async "unresolved secrets: the session starts with a warning after the history and in the status"(ctx) {
        writeFileSync(join(ctx.ws, "fake-fnox-missing"), "GITHUB_TOKEN\nCLAUDE_CODE_OAUTH_TOKEN\n");
        // A conversation to resume, so the warning has history to follow.
        const first = viewClient(ctx, "first");
        const { session: s1 } = await first.req("session.start", { workspace: ctx.ws, mode: "host", prompt: "hello" });
        await first.req("session.watch", { session: s1.key });
        await first.wait(() => first.items(s1.key).some((i) => i.type === "assistant" && !i.streaming), 20000);
        const file = (await first.wait((m) => m.type === "change" && m.change.op === "state" && m.change.state.session_file)).change.state.session_file;
        await first.req("session.stop", { session: s1.key });
        first.close();

        // pi.nvim resumes it: a warning notification and the status entry.
        const a = attach(ctx);
        const sw = await a.call({ type: "switch_session", sessionPath: file });
        assert.equal(sw.success, true);
        const notify = await a.wait((r) => r.type === "extension_ui_request" && r.method === "notify" && /CLAUDE_CODE_OAUTH_TOKEN/.test(r.message ?? ""));
        assert.equal(notify.notifyType, "warning");
        assert.match(notify.message, /GITHUB_TOKEN \(op: not signed in\)/);
        const status = await a.wait((r) => r.type === "extension_ui_request" && r.method === "setStatus" && r.statusText === "fnox: 2 secrets unavailable");
        assert.equal(status.statusKey, "piton-fnox");

        // An app watching that pi sees the warning after the resumed history.
        const st = await a.call({ type: "get_state" });
        const c = viewClient(ctx, "mac");
        const { sessions } = await c.req("sessions.list");
        const live = sessions.find((s) => s.id === st.data.sessionId);
        await c.req("session.watch", { session: live.key });
        await c.wait(() => c.items(live.key).some((i) => i.type === "notice"));
        const items = c.items(live.key);
        const kinds = items.map((i) => i.type);
        assert.deepEqual(kinds.slice(-1), ["notice"], JSON.stringify(kinds));
        assert.ok(kinds.indexOf("user") < kinds.indexOf("notice"), "after the history");
        const notice = items.at(-1);
        assert.equal(notice.level, "warning");
        assert.match(notice.text, /GITHUB_TOKEN \(op: not signed in\); CLAUDE_CODE_OAUTH_TOKEN/);

        // A client that attaches later gets the status and the notification too.
        const late = attach(ctx);
        await late.call({ type: "switch_session", sessionPath: file });
        await late.wait((r) => r.method === "setStatus" && r.statusKey === "piton-fnox");
        await late.wait((r) => r.method === "notify" && /GITHUB_TOKEN/.test(r.message ?? ""));
        await Promise.all([a.close(), late.close()]);
        c.close();
    },
};

/// The global configuration exposes the keychain to the sandbox.
const keychainExposed = {
    async "keychain exposed: sandboxed launches and the source's Git for workers are refused"(ctx) {
        const c = viewClient(ctx, "mac");
        const reply = await c.reqRaw("session.start", { workspace: ctx.ws, mode: "sandbox" });
        assert.equal(reply.ok, false);
        assert.match(reply.error, /the sandbox can read your keychain/);
        c.close();
        // Creating a worker runs the source's Git in a launch's sandbox: refused the same way, before any Git runs.
        rmSync(join(ctx.ws, ".git"), { recursive: true });
        execFileSync("git", ["-C", ctx.ws, "init", "-q", "-b", "main"]);
        execFileSync("git", ["-C", ctx.ws, "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "init"]);
        const created = await new Promise((res) => {
            const p = spawn(piton, ["worker", "create", "x", "--json"], { env: ctx.env, cwd: ctx.ws, stdio: ["ignore", "pipe", "pipe"] });
            let out = "";
            p.stdout.on("data", (d) => (out += d));
            p.on("exit", (code) => res({ code, out }));
        });
        assert.notEqual(created.code, 0);
        assert.match(created.out, /the sandbox can read your keychain/);
    },
};

/// The workspace's mise environment sets a variable (as it could set HOME or XDG_CONFIG_HOME, which pick fnox's config).
const workspaceEnvironment = {
    async "fnox runs with the daemon's environment; pi gets the workspace's"(ctx) {
        const c = viewClient(ctx, "mac");
        const { session } = await c.req("session.start", { workspace: ctx.ws, mode: "host" });
        await c.req("session.watch", { session: session.key });
        const env = await piEnv(c, session.key);
        assert.equal(env.workspaceMarker, "from-mise", "pi gets the workspace's environment");
        assert.equal(env.githubToken, "fnox_proxy_0000", "fnox's changes reach pi");
        assert.equal(env.fnoxProfile, "agent", "the daemon's FNOX_PROFILE reaches pi");
        assert.equal(readFileSync(join(ctx.ws, "fake-fnox-env"), "utf8").trim(), "FNOX_SEES_MARKER=unset");
        c.close();
    },
};

const enabledButMissing = {
    async "fnox enabled but not installed: no session starts"(ctx) {
        const c = viewClient(ctx, "mac");
        const reply = await c.reqRaw("session.start", { workspace: ctx.ws, mode: "host" });
        assert.equal(reply.ok, false);
        assert.match(reply.error, /fnox is enabled \(\[tools\.fnox\] enabled = true\)/);
        c.close();
    },
};

const only = process.argv.slice(2);
let failed = 0;
const suites = [
    [tests, {}],
    [keychainExposed, { project: { sandbox: { read: ["~/Library/Keychains"] } } }],
    [
        workspaceEnvironment,
        { mise: true, files: { "mise.toml": '[env]\nPITON_WORKSPACE_MARKER = "from-mise"\nFNOX_PROFILE = "repo"\n' } },
    ],
    [enabledButMissing, { fnox: { enabled: true, command: ["piton-no-such-fnox"] } }],
];
for (const [suite, options] of suites) {
    for (const [name, fn] of Object.entries(suite)) {
        if (only.length && !only.some((o) => name.includes(o))) continue;
        const ctx = setup(options);
        const daemon = await startDaemon(ctx);
        const t = Date.now();
        try {
            await fn(ctx, daemon);
            console.log(`ok   ${name} (${Date.now() - t} ms)`);
        } catch (e) {
            failed++;
            console.log(`FAIL ${name}\n     ${e.stack?.split("\n").slice(0, 4).join("\n     ")}\n     daemon: ${daemon.err().slice(-800)}`);
        } finally {
            if (daemon.exitCode === null) {
                daemon.kill("SIGTERM");
                await daemon.exited;
            }
        }
    }
}
process.exit(failed ? 1 : 0);
