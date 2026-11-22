// End-to-end tests of #241 on the host: a sandboxed session's bash calls run through the daemon, each in its own
// sandbox; a rerun the agent asks for is asked of the user and runs with everything but the floor; a persisted grant
// runs the next matching call without asking; a blocked process that isn't a bash call is offered as a sandbox entry.
// Real pi with the faux extension, sandbox mode.
//
// Usage (in cli/): cargo build && node tests/e2e/blocks.mjs, or `mise run cli:test`. Sandboxes don't nest and `log`
// refuses inside one, so in a sandboxed session (PITON_RUNTIME=sandbox) every test is skipped.
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join, resolve } from "node:path";
import assert from "node:assert/strict";

import { writeConfig } from "./config.mjs";

const root = resolve(import.meta.dirname, "../..");
const piton = join(root, "target/debug/piton");
const ext = join(root, "tests/fixtures/faux-extension.ts");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const nested = process.env.PITON_RUNTIME === "sandbox";
// The block question's title in either runtime.
const QUESTION = /^Run (again in the sandbox|on the host, with no sandbox)\?/;

function setup() {
    const dir = realpathSync(mkdtempSync(join(tmpdir(), "piton-blocks-")));
    const ws = join(dir, "ws");
    const agent = join(dir, "agent");
    mkdirSync(join(ws, ".git"), { recursive: true });
    mkdirSync(agent);
    mkdirSync(join(dir, "state"));
    // Outside every grant: the temp folder is writable, so the blocked writes go to the home folder, under names this
    // test removes.
    const outside = join(homedir(), `piton-e2e-blocks-${process.pid}`);
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
    });
    const env = { ...process.env, PITON_CONFIG_HOME: config, PI_CODING_AGENT_DIR: agent };
    return { dir, ws, config, env, outside };
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

const tests = {
    async "a plain call runs through the daemon in the session's sandbox"(ctx) {
        const a = attach(ctx);
        const ok = await a.bash("bash echo inside > out.txt && cat out.txt");
        assert.match(ok, /inside/);
        assert.ok(existsSync(join(ctx.ws, "out.txt")), "the workspace is writable");
        const blocked = await a.bash(`bash touch ${ctx.outside}-plain`);
        assert.match(blocked, /Operation not permitted/);
        assert.ok(!existsSync(`${ctx.outside}-plain`), "outside the grants it's refused");
        await a.close();
    },

    async "a rerun is asked, runs at all, and keeps the floor"(ctx) {
        const a = attach(ctx);
        const target = `${ctx.outside}-rerun`;
        // The floor: a protected file stays closed even at `all`.
        const command = `touch ${target}; touch .pi/x 2>&1 || echo floor-held`;
        const dialog = a.waitNew((r) => r.type === "extension_ui_request" && r.method === "select" && QUESTION.test(r.title));
        const done = a.bash(`rerun needs home|${command}`);
        const first = await dialog;
        assert.match(first.title, /needs home/);
        // The first option decides nothing: the same question comes back.
        assert.equal(first.options[0], "Choose an answer below");
        const again = a.waitNew((r) => r.type === "extension_ui_request" && r.method === "select" && r.id !== first.id);
        a.send({ type: "extension_ui_response", id: first.id, value: first.options[0] });
        const req = await again;
        assert.equal(req.title, first.title);
        a.send({ type: "extension_ui_response", id: req.id, value: req.options.find((o) => o.startsWith("Allow once in the sandbox")) });
        const result = await done;
        assert.ok(existsSync(target), "allowed once: it ran outside the sandbox");
        assert.match(result, /floor-held/, "a protected file stays closed");
        assert.match(result, /widened sandbox/, "the result says where it ran");
        await a.close();
    },

    async "the switch asks again on the host; a host run passes the floor and starts a sandbox of its own"(ctx) {
        const a = attach(ctx);
        const target = join(ctx.ws, ".pi", "host-wrote");
        // Only a host run writes a protected file, and only a host run can start a sandbox (they don't nest).
        const command = `mkdir -p .pi && touch ${target} && ${piton} sandbox-exec -- /usr/bin/true && echo nested-ok`;
        const dialog = a.waitNew((r) => r.type === "extension_ui_request" && r.method === "select" && QUESTION.test(r.title ?? ""));
        const done = a.bash(`rerun host tools|${command}`);
        const sandboxed = await dialog;
        assert.match(sandboxed.title, /^Run again in the sandbox\?/);
        const switched = a.waitNew((r) => r.type === "extension_ui_request" && r.method === "select" && r.id !== sandboxed.id);
        a.send({ type: "extension_ui_response", id: sandboxed.id, value: sandboxed.options.find((o) => o.startsWith("Switch to running on the host")) });
        const host = await switched;
        assert.match(host.title, /^Run on the host, with no sandbox\?/);
        a.send({ type: "extension_ui_response", id: host.id, value: host.options.find((o) => o.startsWith("Run once on the host")) });
        const result = await done;
        assert.ok(existsSync(target), "on the host: no floor");
        assert.match(result, /nested-ok/, "no sandbox around it");
        assert.match(result, /on the host/, "the result says where it ran");
        await a.close();
    },

    async "denying leaves it unrun; persisting saves a grant that later calls use"(ctx) {
        const a = attach(ctx);
        const target = `${ctx.outside}-persist`;
        let dialog = a.waitNew((r) => r.type === "extension_ui_request" && QUESTION.test(r.title ?? ""));
        let done = a.bash(`rerun x|touch ${target}`);
        let req = await dialog;
        a.send({ type: "extension_ui_response", id: req.id, value: "Deny" });
        assert.match(await done, /denied/);
        assert.ok(!existsSync(target));

        dialog = a.waitNew((r) => r.type === "extension_ui_request" && QUESTION.test(r.title ?? ""));
        done = a.bash(`rerun x|touch ${target}`);
        req = await dialog;
        const everywhere = req.options.find((o) => o.startsWith("Allow this command everywhere"));
        a.send({ type: "extension_ui_response", id: req.id, value: everywhere });
        await done;
        assert.ok(existsSync(target));
        const saved = readFileSync(join(ctx.config, "config.toml"), "utf8");
        assert.match(saved, /\[\[sandbox\.commands\]\]/);
        assert.ok(saved.includes(`bash(touch ${target})`), saved);
        assert.match(saved, /runtime = "sandbox"/);
        rmSync(target);
        await a.close();

        // A new session reads the grant: the same command runs widened without asking.
        const b = attach(ctx);
        const result = await b.bash(`bash touch ${target}`);
        assert.doesNotMatch(result, /Operation not permitted/);
        assert.ok(existsSync(target), "the grant ran it outside the sandbox");
        assert.ok(!b.records.some((r) => r.type === "extension_ui_request" && QUESTION.test(r.title ?? "")));
        await b.close();
    },

    async "a blocked process that isn't a bash call is offered as a sandbox entry"(ctx) {
        // pi's own process writes outside its grants (an extension's tool): the kernel's record carries the session's
        // tag, never a call's, so it's offered as the entry that lifts it.
        const a = attach(ctx);
        const target = `${ctx.outside}-process`;
        const offered = a.waitNew(
            (r) => r.type === "extension_ui_request" && r.method === "select" && /blocked/.test(r.title ?? ""),
            30000,
        );
        a.send({ type: "prompt", message: `write ${target}` });
        const req = await offered;
        // A file that doesn't exist yet may be a folder's create: offered as `write` on it.
        assert.ok(req.title.includes(`= [\"${target}\"]`), req.title);
        assert.equal(req.options[0], "Choose an answer below");
        assert.ok(req.options.includes("Not now"));
        // The run ends before an answer: the question is asked again, not lost.
        await a.waitNew((r) => r.type === "agent_settled", 30000).catch(() => {});
        const again = await a.waitNew(
            (r) => r.type === "extension_ui_request" && r.method === "select" && r.title === req.title && r.id !== req.id,
            15000,
        );
        a.send({ type: "extension_ui_response", id: again.id, value: "Not now" });

        // A second block queued behind a command question is shown once that question is answered.
        const second = `${ctx.outside}-process2`;
        const blocked = a.waitNew((r) => r.type === "extension_ui_request" && QUESTION.test(r.title ?? ""));
        a.send({ type: "prompt", message: `rerun x|touch ${ctx.outside}-rerun2` });
        const command = await blocked;
        a.send({ type: "prompt", message: `write ${second}`, streamingBehavior: "followUp" });
        await sleep(2000);
        assert.ok(
            !a.records.some((r) => r.type === "extension_ui_request" && (r.title ?? "").includes(second)),
            "one question at a time",
        );
        const next = a.waitNew((r) => r.type === "extension_ui_request" && (r.title ?? "").includes(second), 30000);
        a.send({ type: "extension_ui_response", id: command.id, value: "Deny" });
        const shown = await next;
        a.send({ type: "extension_ui_response", id: shown.id, value: "Not now" });
        await a.close();
    },

    async "git's control files are reported, not asked; deny everywhere silences a path"(ctx) {
        const a = attach(ctx);
        // pi's own process writes the workspace's .git/config: no entry lifts it, so a notice, never a question.
        const notice = a.waitNew(
            (r) => r.type === "extension_ui_request" && r.method === "notify" && /Git's hooks and configuration/.test(r.message ?? ""),
            30000,
        );
        a.send({ type: "prompt", message: `write ${join(ctx.ws, ".git/config")}` });
        const shown = await notice;
        assert.match(shown.message, /git preset/);
        await a.waitNew((r) => r.type === "agent_settled", 30000).catch(() => {});
        assert.ok(
            !a.records.some((r) => r.type === "extension_ui_request" && r.method === "select" && (r.title ?? "").includes(".git/config")),
            "no question",
        );

        // Deny everywhere: saved as `silence`, so a new session doesn't ask.
        const target = `${ctx.outside}-silenced`;
        const offered = a.waitNew((r) => r.type === "extension_ui_request" && r.method === "select" && (r.title ?? "").includes(target), 30000);
        a.send({ type: "prompt", message: `write ${target}` });
        const req = await offered;
        a.send({ type: "extension_ui_response", id: req.id, value: "Deny everywhere" });
        await a.waitNew((r) => r.type === "agent_settled", 30000).catch(() => {});
        const saved = readFileSync(join(ctx.config, "config.toml"), "utf8");
        assert.match(saved, /silence = \[/);
        assert.ok(saved.includes(target), saved);
        await a.close();

        const b = attach(ctx);
        b.send({ type: "prompt", message: `write ${target}` });
        await b.waitNew((r) => r.type === "agent_settled", 30000).catch(() => {});
        await sleep(2000);
        assert.ok(
            !b.records.some((r) => r.type === "extension_ui_request" && (r.title ?? r.message ?? "").includes(target)),
            "a silenced path is only logged",
        );
        await b.close();
    },
};

if (nested) {
    console.log("skip  blocks.mjs: sandboxes don't nest and `log` refuses inside one");
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
        for (const suffix of ["-plain", "-rerun", "-rerun2", "-persist", "-process", "-process2", "-silenced"]) {
            rmSync(`${ctx.outside}${suffix}`, { force: true });
        }
        rmSync(ctx.dir, { recursive: true, force: true });
    }
}
process.exit(failed ? 1 : 0);
