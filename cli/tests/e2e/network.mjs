// End-to-end tests of #228 on the host: a strict session's filtered network. A bash call connecting to a host no
// `connect` entry lists waits while the user is asked; allowing lets it through, denying refuses it, and "allow in this
// project" saves the entry so later calls pass without asking. A listed host passes without a question. Real pi in
// sandbox mode with Piton's pi package and the faux extension, as in blocks.mjs. It reaches public hosts (example.com,
// www.iana.org) over plain HTTP: a local server wouldn't do, since the gate refuses names that resolve to this Mac.
//
// Usage (in cli/): cargo build && node tests/e2e/network.mjs, or `mise run cli:test`. Sandboxes don't nest, so in a
// sandboxed session (PITON_RUNTIME=sandbox) every test is skipped.
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import assert from "node:assert/strict";

import { writeConfig } from "./config.mjs";

const root = resolve(import.meta.dirname, "../..");
const piton = join(root, "target/debug/piton");
const ext = join(root, "tests/fixtures/faux-extension.ts");
const pkg = resolve(root, "../pi");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const nested = process.env.PITON_RUNTIME === "sandbox";
const NETWORK = /wants to connect to/;

/// The destinations: public hosts the test reaches through the gate. `example.com` is listed; the others are asked about.
const LISTED = "example.com";

function setup() {
    const dir = realpathSync(mkdtempSync(join(tmpdir(), "piton-network-")));
    const ws = join(dir, "ws");
    const agent = join(dir, "agent");
    mkdirSync(join(ws, ".git"), { recursive: true });
    mkdirSync(join(ws, ".piton"), { recursive: true });
    mkdirSync(agent);
    writeFileSync(join(agent, "settings.json"), JSON.stringify({ packages: [pkg] }));
    mkdirSync(join(dir, "state"));
    const config = writeConfig(dir, {
        daemon: {
            socket: join(dir, "state/piton.sock"),
            log: join(dir, "piton.log"),
            tailnet: { enabled: false },
            pi: { args: ["-e", ext, "--model", "faux/faux-1"] },
            env: { login_shell: false },
            tools: { mise: { enabled: false } },
            stop: { grace_ms: 3000, term_grace_ms: 1000 },
        },
        host: { default_attach_mode: "sandbox", empty_grace_ms: 60000 },
        project: { permissions: { default_mode: "allow" } },
    });
    // strict, with one host listed (the user's own file: loosening keys).
    writeFileSync(
        join(ws, ".piton/config.local.toml"),
        `[sandbox]\nprofile = "strict"\nconnect = ["${LISTED}"]\n`,
    );
    const env = { ...process.env, PITON_CONFIG_HOME: config, PI_CODING_AGENT_DIR: agent };
    return { dir, ws, config, env };
}

async function startDaemon(ctx) {
    const d = spawn(piton, ["daemon"], { env: ctx.env, stdio: ["ignore", "ignore", "pipe"] });
    let err = "";
    d.stderr.on("data", (c) => (err += c));
    for (let i = 0; i < 100 && !err.includes("listening"); i++) await sleep(50);
    assert.ok(err.includes("listening"), "daemon didn't start: " + err);
    d.exited = new Promise((r) => d.on("exit", (code) => r(code)));
    return d;
}

function attach(ctx) {
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

const curl = (host) => `bash curl -s -o /dev/null -w 'code=%{http_code}' --max-time 20 http://${host}/ ; echo " exit=$?"`;

const tests = {
    async "a listed host passes without a question; the network is otherwise closed"(ctx) {
        const a = attach(ctx);
        const ok = await a.bash(curl(LISTED));
        assert.match(ok, /code=(200|30\d)/, ok);
        assert.ok(!a.records.some((r) => NETWORK.test(r.title ?? "")), "no question");
        // Straight to an address, past the gate: the sandbox refuses it.
        const direct = await a.bash(`bash python3 -I -c 'import socket; socket.create_connection(("1.1.1.1", 80), 5)' ; echo exit=$?`);
        assert.match(direct, /exit=1/, direct);
        await a.close();
    },

    async "an unlisted host waits for the user: allowing for the session lets it through, and asks once"(ctx) {
        const a = attach(ctx);
        const dialog = a.waitNew((r) => r.type === "extension_ui_request" && NETWORK.test(r.title ?? ""));
        const done = a.bash(curl("www.iana.org"));
        const q = await dialog;
        assert.match(q.title, /`curl .*` wants to connect to www\.iana\.org:80/, q.title);
        assert.equal(q.options[0], "Choose an answer below");
        a.send({ type: "extension_ui_response", id: q.id, value: "Allow for this session" });
        const result = await done;
        assert.match(result, /code=(200|30\d)/, result);
        // Again in the same session: no question.
        const before = a.records.length;
        const again = await a.bash(curl("www.iana.org"));
        assert.match(again, /code=(200|30\d)/, again);
        assert.ok(!a.records.slice(before).some((r) => NETWORK.test(r.title ?? "")), "asked once");
        await a.close();
    },

    async "denying refuses it with the gate's reason; allowing in this project saves the entry"(ctx) {
        const a = attach(ctx);
        let dialog = a.waitNew((r) => r.type === "extension_ui_request" && NETWORK.test(r.title ?? ""));
        let done = a.bash(curl("www.iana.org"));
        let q = await dialog;
        a.send({ type: "extension_ui_response", id: q.id, value: "Not now" });
        const refused = await done;
        assert.match(refused, /code=403/, refused);
        assert.match(refused, /refused by the network gate/, "the agent sees why");

        dialog = a.waitNew((r) => r.type === "extension_ui_request" && NETWORK.test(r.title ?? ""));
        done = a.bash(curl("www.iana.org"));
        q = await dialog;
        a.send({ type: "extension_ui_response", id: q.id, value: "Allow in this project" });
        const allowed = await done;
        assert.match(allowed, /code=(200|30\d)/, allowed);
        const saved = readFileSync(join(ctx.ws, ".piton/config.local.toml"), "utf8");
        assert.match(saved, /"www\.iana\.org:80"/, saved);
        await a.close();
    },
};

if (nested) {
    console.log("skip  network.mjs: sandboxes don't nest");
    process.exit(0);
}
let failed = 0;
for (const [name, test] of Object.entries(tests)) {
    const ctx = setup();
    const daemon = await startDaemon(ctx);
    try {
        await test(ctx);
        console.log(`ok    ${name}`);
    } catch (e) {
        failed++;
        console.log(`FAIL  ${name}\n${e.stack}`);
        try {
            console.log(readFileSync(join(ctx.dir, "piton.log"), "utf8").split("\n").slice(-30).join("\n"));
        } catch {}
    } finally {
        daemon.kill("SIGTERM");
        await daemon.exited;
        rmSync(ctx.dir, { recursive: true, force: true });
    }
}
process.exit(failed ? 1 : 0);
