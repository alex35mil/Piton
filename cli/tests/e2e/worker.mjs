// End-to-end tests of `piton worker` and worker-aware sandbox launches, through the daemon.
// Usage (in cli/): cargo build && node tests/e2e/worker.mjs, or `mise run cli:test`.
//
// Sandboxes don't nest: in a sandboxed session (PITON_RUNTIME=sandbox) the sandboxed launch is checked through
// `piton sandbox show`, the daemon's own resolver, and the launch itself only on the host.
import { spawn, execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, realpathSync, readFileSync, existsSync, lstatSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import net from "node:net";
import assert from "node:assert/strict";

import { writeConfig } from "./config.mjs";

const root = resolve(import.meta.dirname, "../..");
const piton = join(root, "target/debug/piton");
const ext = join(root, "tests/fixtures/faux-extension.ts");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const git = (cwd, ...args) => execFileSync("git", ["-C", cwd, ...args], { stdio: "pipe" }).toString().trim();

function setup() {
    const dir = realpathSync(mkdtempSync(join(tmpdir(), "piton-worker-")));
    const agent = join(dir, "agent");
    mkdirSync(agent);
    // Source checkout with an origin, workflow artifacts, sandbox grants, and a skipped editor index.
    const origin = join(dir, "origin.git");
    const source = join(dir, "repo");
    mkdirSync(source);
    execFileSync("git", ["init", "-q", "--bare", "-b", "main", origin]);
    git(source, "init", "-q", "-b", "main");
    git(source, "config", "user.email", "t@t");
    git(source, "config", "user.name", "t");
    writeFileSync(join(source, ".gitignore"), "/apps/.build\n/.piton/.local\n");
    mkdirSync(join(source, ".piton"));
    writeFileSync(
        join(source, ".piton/config.toml"),
        '[workers]\nskip = ["apps/.build/index-build"]\n[sandbox.filesystem]\n"/source-grant" = "write"\n',
    );
    git(source, "add", ".");
    git(source, "commit", "-q", "-m", "init");
    git(source, "remote", "add", "origin", origin);
    git(source, "push", "-q", "-u", "origin", "main");
    git(source, "remote", "set-head", "origin", "main");
    mkdirSync(join(source, ".piton/.local"), { recursive: true });
    mkdirSync(join(source, "apps/.build/index-build"), { recursive: true });

    // The daemon's state folder is its socket's: apart from pi's agent directory, which a sandbox may write.
    mkdirSync(join(dir, "state"));
    const socket = join(dir, "state/piton.sock");
    const log = join(dir, "piton.log");
    const config = writeConfig(dir, {
        daemon: {
            socket,
            log,
            pi: { args: ["-e", ext, "--model", "faux/faux-1"] },
            env: { login_shell: false },
            integrations: { mise: { enabled: false }, tailscale: { enabled: false } },
            stop: { terminate_after: "3s", kill_after: "1s" },
        },
        // The apps' `session.start` (used to watch a session's dialogs) takes checkouts under a root.
        host: { default_attach_mode: "sandbox", stop_empty_session_after: "60s", roots: [dir] },
        project: { sandbox: { presets: ["rust"], filesystem: { "/global-grant": "write" } } },
    });
    const env = { ...process.env, PITON_CONFIG_HOME: config, PI_CODING_AGENT_DIR: agent };
    return { dir, source, log, env };
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

function cli(ctx, args, cwd) {
    return new Promise((res) => {
        const p = spawn(piton, args, { env: ctx.env, cwd: cwd ?? ctx.source, stdio: ["ignore", "pipe", "pipe"] });
        let out = "";
        let err = "";
        p.stdout.on("data", (d) => (out += d));
        p.stderr.on("data", (d) => (err += d));
        p.on("exit", (code) => res({ code, out, err, json: args.includes("--json") && out.trim() ? JSON.parse(out) : undefined }));
    });
}

/// A `view` client that watches one session and answers its dialogs.
function viewClient(socketPath) {
    const sock = net.connect(socketPath);
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
    sock.write(JSON.stringify({ protocol: "view", version: 1, client: "test", device: "mac" }) + "\n");
    c.wait = (pred, ms = 20000) => {
        const hit = c.lines.find(pred);
        if (hit) return Promise.resolve(hit);
        return new Promise((res, rej) => {
            c.waiters.push({ pred, resolve: res });
            setTimeout(() => rej(new Error(`timeout; last ${JSON.stringify(c.lines.slice(-3)).slice(0, 600)}`)), ms);
        });
    };
    c.waitNew = (pred, ms) => {
        const from = c.lines.length;
        return c.wait((m) => c.lines.indexOf(m) >= from && pred(m), ms);
    };
    c.req = async (type, args = {}) => {
        const id = `t-${++c.n}`;
        sock.write(JSON.stringify({ id, type, ...args }) + "\n");
        const reply = await c.wait((m) => m.type === "reply" && m.id === id);
        if (!reply.ok) throw new Error(`${type}: ${reply.error}`);
        return reply.data;
    };
    /// The next worker dialog shown: its id and options.
    c.dialog = async () => {
        const st = await c.wait((m) => m.type === "change" && m.change.op === "state" && m.change.state.dialogs.some((d) => d.id.startsWith("piton:worker:") && !c.answered?.has(d.id)));
        return st.change.state.dialogs.find((d) => d.id.startsWith("piton:worker:") && !c.answered?.has(d.id));
    };
    c.answered = new Set();
    c.answer = async (session, dialog, value) => {
        c.answered.add(dialog.id);
        await c.req("session.answer", { session, request_id: dialog.id, answer: { value } });
    };
    c.close = () => sock.end();
    return c;
}

/// One request on the agent socket; `onSent` gets the open socket (to close it early).
function agentRequest(socket, token, request, onSent) {
    return new Promise((resolve, reject) => {
        const sock = net.connect(socket);
        let buf = "";
        sock.on("data", (chunk) => (buf += chunk));
        sock.on("close", () => {
            try {
                resolve(buf.trim() ? JSON.parse(buf) : null);
            } catch (e) {
                reject(e);
            }
        });
        sock.on("error", reject);
        sock.write(JSON.stringify({ token, request }) + "\n");
        onSent?.(sock);
    });
}

/// Starts a host session in `workspace` and returns its key, pi's token, and the relay that sends as pi.
async function piSession(c, workspace) {
    const { session } = await c.req("session.start", { workspace, mode: "host" });
    const key = session.key;
    await c.req("session.watch", { session: key });
    await c.req("session.send", { session: key, message_id: "e", text: "env" });
    const done = await c.wait((m) => m.type === "change" && m.change.op === "state" && m.change.state.status === "idle" && c.lines.some((l) => l.session === key && l.change?.item?.type === "tool" && l.change.item.status === "done"));
    assert.ok(done);
    const tool = c.lines.find((l) => l.session === key && l.change?.item?.type === "tool" && l.change.item.status === "done").change.item;
    const { token, relay } = JSON.parse((await c.req("session.tool_output", { session: key, call_id: tool.call_id })).content[0].text);
    return { key, token, relay };
}

const tests = {
    async "create, launch in the sandbox with the source's settings, delete"(ctx) {
        // The source's committed grant applies only once the user trusts the source; its workers share the decision.
        const needsTrust = await cli(ctx, ["start", ctx.source]);
        assert.equal(needsTrust.code, 1);
        assert.match(needsTrust.err, /needs your trust before a launch/);
        const untrusted = JSON.parse((await cli(ctx, ["sandbox", "show"])).out);
        assert.ok(untrusted.untrusted.some((g) => g.includes("/source-grant")), "left out until trusted");
        // A decision for grants other than the repository's now is refused, naming the change (exit 4).
        const stale = await cli(ctx, ["trust", "allow", "--grants", "[]"]);
        assert.equal(stale.code, 4, stale.err);
        assert.match(stale.err, /changed since they were shown/);
        const status = await cli(ctx, ["trust", "status", "--json"]);
        assert.equal(status.json.trust.state, "needed");
        const allowed = await cli(ctx, ["trust", "allow", "--grants", JSON.stringify(status.json.trust.grants)]);
        assert.equal(allowed.code, 0, allowed.err);
        assert.match(allowed.out, /trusted/);
        const created = await cli(ctx, ["worker", "create", "Fix X", "--item-id", "KID-5", "--json"]);
        assert.equal(created.code, 0, created.err);
        assert.equal(created.json.type, "worker");
        const worker = created.json.worker.path;
        assert.equal(worker, join(ctx.dir, "repo--fix-x"));
        assert.equal(created.json.worker.item_id, "KID-5");
        assert.ok(lstatSync(join(worker, ".piton/.local")).isSymbolicLink());
        assert.ok(!existsSync(join(worker, "apps/.build/index-build")), "skip list honored");
        // An agent edits its worker's grants to widen its sandbox: ignored.
        writeFileSync(join(worker, ".piton/config.toml"), '[sandbox.filesystem]\n"/" = "write"\n');

        // The worker's sandbox, as the daemon resolves it: the global and source grants and the source's artifacts,
        // never the worker's edit; the worker's checkout is the protected `.pi/` root.
        const shown = await cli(ctx, ["sandbox", "show", "--workspace", worker]);
        assert.equal(shown.code, 0, shown.err);
        const rules = JSON.parse(shown.out).policy.rules;
        const writes = rules.filter((r) => r.effect === "allow" && r.access.includes("write")).map((r) => r.target.path);
        for (const path of ["/global-grant", "/source-grant", join(ctx.source, ".piton/.local"), worker]) {
            assert.ok(writes.includes(path), `${path} is writable`);
        }
        assert.ok(rules.some((r) => r.source.kind === "preset" && r.source.preset === "rust"), "the global preset");
        assert.ok(!writes.includes("/"), "the worker's own grants are ignored");
        const protectedRule = rules.find((r) => r.target.kind === "protected");
        assert.deepEqual(protectedRule.target, { kind: "protected", source: ctx.source, checkout: worker });
        const why = await cli(ctx, ["sandbox", "why", "--workspace", worker, "--write", join(worker, ".pi/settings.json")]);
        assert.equal(why.code, 4, why.out);
        assert.match(why.out, /protected file/);

        // Start a sandboxed session through this binary's `sandbox-exec`. In a sandboxed session the helper can't
        // apply a second sandbox, so pi doesn't come up there; on the host it answers.
        const started = await cli(ctx, ["start", worker, "--prompt", "hello"]);
        assert.equal(started.code, 0, started.err);
        await sleep(1500);
        const log = readFileSync(ctx.log, "utf8");
        assert.ok(log.includes(`worker of ${ctx.source}`), "launch logs the relation");
        const nested = process.env.PITON_RUNTIME === "sandbox";

        const listed = await cli(ctx, ["worker", "ls", "--json"]);
        assert.equal(listed.json.workers.length, 1);
        if (nested) {
            assert.match(log, /sandbox-exec: can't apply the sandbox|pi exited/, "the nested launch fails visibly");
        } else {
            assert.equal(listed.json.workers[0].sessions.length, 1, "the worker's live session is listed");
            // The session says which project it belongs to, for the apps' grouping.
            assert.deepEqual(listed.json.workers[0].sessions[0].checkout, {
                kind: "worker",
                source: ctx.source,
                name: "fix-x",
            });
        }

        // Leftovers (the worker's own config.toml edit) refuse a delete; --force deletes and stops the session.
        const refused = await cli(ctx, ["worker", "delete", worker, "--json"]);
        assert.equal(refused.code, 4);
        assert.equal(refused.json.refusal.kind, "leftovers");
        const deleted = await cli(ctx, ["worker", "delete", worker, "--force", "--json"]);
        assert.equal(deleted.code, 0, deleted.err);
        assert.equal(deleted.json.stopped, nested ? 0 : 1);
        assert.ok(!existsSync(worker));
        assert.ok(existsSync(join(ctx.source, ".piton/.local")), "source's artifacts survive");
        const after = await cli(ctx, ["ls", "--json"]);
        assert.equal(after.json.length, 0);
    },

    async "workers get no pi.nvim descriptor (a spawned one would bypass piton)"(ctx) {
        const created = await cli(ctx, ["worker", "create", "plain", "--json"]);
        assert.equal(created.code, 0, created.err);
        const worker = created.json.worker.path;
        assert.ok(!existsSync(join(worker, ".agents/pi.nvim.json")));
        assert.equal(git(worker, "status", "--porcelain"), "", "a fresh worker is clean");
        const old = await cli(ctx, ["worker", "create", "old", "--descriptor", ".agents/pi.nvim.json", "--json"]);
        assert.notEqual(old.code, 0, "--descriptor is gone");
    },

    async "refusals: dirty source, existing name, not a worker"(ctx) {
        await cli(ctx, ["worker", "create", "one", "--json"]);
        const exists = await cli(ctx, ["worker", "create", "one", "--json"]);
        assert.equal(exists.code, 4);
        assert.equal(exists.json.refusal.kind, "exists");
        writeFileSync(join(ctx.source, "dirty"), "x");
        const dirty = await cli(ctx, ["worker", "create", "two", "--json"]);
        assert.equal(dirty.json.refusal.kind, "source_dirty");
        const notWorker = await cli(ctx, ["worker", "delete", ctx.source, "--json"]);
        assert.equal(notWorker.json.refusal.kind, "not_a_worker");
    },

    // The fast-forward of the source runs in its sandbox, which can't write protected files: an update that changes
    // them waits for the user's approval of that exact commit, before anything changes.
    async "an update that changes protected files asks first"(ctx) {
        const upstream = join(ctx.dir, "upstream");
        execFileSync("git", ["clone", "-q", join(ctx.dir, "origin.git"), upstream]);
        git(upstream, "config", "user.email", "t@t");
        git(upstream, "config", "user.name", "t");
        const push = (file, content) => {
            git(upstream, "pull", "-q", "origin", "main");
            writeFileSync(join(upstream, file), content);
            git(upstream, "add", ".");
            git(upstream, "commit", "-q", "-m", file);
            git(upstream, "push", "-q", "origin", "HEAD:main");
            return git(upstream, "rev-parse", "HEAD");
        };
        const head = () => git(ctx.source, "rev-parse", "HEAD");

        // An ordinary update: fast-forwarded without asking.
        const ordinary = push("README", "hello");
        const one = await cli(ctx, ["worker", "create", "one", "--json"]);
        assert.equal(one.code, 0, one.err);
        assert.equal(head(), ordinary);

        const before = head();
        const target = push(".piton/config.toml", '[workers]\nskip = ["apps/.build/index-build"]\n');
        const refused = await cli(ctx, ["worker", "create", "two", "--json"]);
        assert.equal(refused.code, 4, refused.err);
        assert.equal(refused.json.refusal.kind, "protected_files_update");
        assert.equal(refused.json.refusal.target, target);
        assert.deepEqual(refused.json.refusal.files, [".piton/config.toml"]);
        assert.equal(head(), before, "the source is unchanged");
        assert.ok(!existsSync(join(ctx.dir, "repo--two")));
        const human = await cli(ctx, ["worker", "create", "two"]);
        assert.match(human.err, new RegExp(`--approve-source-update ${target}`));
        assert.match(human.err, /runs in the sandbox/);
        assert.doesNotMatch(human.err, /outside the sandbox/);

        // Deleting asks too, before deleting.
        const worker = one.json.worker.path;
        const asked = await cli(ctx, ["worker", "delete", worker, "--json"]);
        assert.equal(asked.json.refusal.kind, "protected_files_update");
        assert.ok(existsSync(worker), "nothing deleted before the answer");

        // The user's post-merge hook tries to write a protected file the user never saw: the daemon wrote the approved
        // file itself, and the merge (with the hook) runs in the sandbox, which denies the hook's write. Only on the
        // host: in a sandboxed session the merge runs in this session's sandbox, whose protected files are elsewhere.
        const local = join(ctx.source, ".piton/config.local.toml");
        writeFileSync(
            join(ctx.source, ".git/hooks/post-merge"),
            `#!/bin/sh\nprintf '[sandbox]\\nallow_git_control_files = true\\n' > '${local}' 2>/dev/null\nexit 0\n`,
            { mode: 0o755 },
        );
        const approved = await cli(ctx, ["worker", "create", "two", "--approve-source-update", target, "--json"]);
        assert.equal(approved.code, 0, approved.err);
        assert.equal(head(), target);
        assert.equal(readFileSync(join(ctx.source, ".piton/config.toml"), "utf8"), '[workers]\nskip = ["apps/.build/index-build"]\n');
        if (process.env.PITON_RUNTIME !== "sandbox") {
            assert.ok(!existsSync(local), "the hook couldn't write a protected file");
        }
        const skipped = await cli(ctx, ["worker", "delete", worker, "--skip-source-update", "--json"]);
        assert.equal(skipped.code, 0, skipped.err);
        assert.ok(!existsSync(worker));
    },

    // A session's agent asks the daemon over its channel; the daemon decides from `[workers.permissions]` and asks the
    // user, and only a client answers.
    async "a session creates, lists, and deletes workers after the user approves"(ctx) {
        const trusted = await cli(ctx, ["trust", "status", "--json"]);
        await cli(ctx, ["trust", "allow", "--grants", JSON.stringify(trusted.json.trust.grants)]);
        const c = viewClient(join(ctx.dir, "state/piton.sock"));
        const { key, token, relay } = await piSession(c, ctx.source);
        const ask = (request, onSent) => agentRequest(relay, token, request, onSent);

        // Listing never asks.
        assert.deepEqual(await ask({ type: "worker_list" }), { type: "workers", workers: [] });

        // Asked: denied, then allowed once. A second request while the first question is open waits for it, and is
        // asked once it's answered.
        const denied = ask({ type: "worker_create", name: "one" });
        const first = await c.dialog();
        const waiting = ask({ type: "worker_create", name: "waited" });
        await sleep(300);
        const shown = c.lines.filter((m) => m.type === "change" && m.change.op === "state").at(-1).change.state.dialogs;
        assert.deepEqual(shown.map((d) => d.id), [first.id], "one question at a time");
        await c.answer(key, first, "Deny");
        assert.equal((await denied).type, "not_done");
        const second = await c.dialog();
        assert.notEqual(second.id, first.id);
        await c.answer(key, second, "Deny");
        assert.equal((await waiting).type, "not_done");
        assert.ok(!existsSync(join(ctx.dir, "repo--one")));
        const once = ask({ type: "worker_create", name: "one", item_id: "github:7" });
        const dialog = await c.dialog();
        assert.match(dialog.title, /create a worker/);
        assert.match(dialog.title, /Item: github:7/);
        await c.answer(key, dialog, "Allow once");
        const created = await once;
        assert.equal(created.type, "worker_created", JSON.stringify(created));
        assert.equal(created.worker.item_id, "github:7");
        assert.equal(git(created.worker.path, "config", "--local", "agent.itemId"), "github:7");

        // A call that stops waiting after the user approved still gets its worker.
        const abandoned = ask({ type: "worker_create", name: "two" }, (sock) => (c.abandon = sock));
        await c.answer(key, await c.dialog(), "Allow for this session");
        c.abandon.destroy();
        await abandoned;
        for (let i = 0; i < 100 && !existsSync(join(ctx.dir, "repo--two", ".git")); i++) await sleep(100);
        await sleep(500);
        assert.ok(existsSync(join(ctx.dir, "repo--two")), "created after the call left");
        // For this session: no more questions for creating.
        const third = await ask({ type: "worker_create", name: "three" });
        assert.equal(third.type, "worker_created");

        // Outside the project: refused without asking.
        const outside = await ask({ type: "worker_delete", path: ctx.source });
        assert.equal(outside.type, "worker_refused");
        assert.equal(outside.refusal.kind, "not_a_worker");

        // Leftovers: "Delete anyway" after the delete question.
        writeFileSync(join(ctx.dir, "repo--three", "wip"), "x");
        const deleting = ask({ type: "worker_delete", path: join(ctx.dir, "repo--three") });
        await c.answer(key, await c.dialog(), "Allow once");
        const leftovers = await c.dialog();
        assert.match(leftovers.title, /isn't pushed/);
        await c.answer(key, leftovers, "Delete anyway (this work is lost)");
        assert.equal((await deleting).type, "worker_deleted");
        assert.ok(!existsSync(join(ctx.dir, "repo--three")));

        // A setting decides first: deny refuses, allow runs without asking.
        writeFileSync(join(ctx.source, ".git/info/exclude"), "/.piton/*.local.*\n");
        writeFileSync(join(ctx.source, ".piton/config.local.toml"), '[workers.permissions]\ndelete_by_agent = "deny"\n');
        const off = await ask({ type: "worker_delete", path: join(ctx.dir, "repo--two") });
        assert.equal(off.type, "not_done");
        assert.match(off.reason, /delete_by_agent/);
        writeFileSync(join(ctx.source, ".piton/config.local.toml"), '[workers.permissions]\ndelete_by_agent = "allow"\n');
        assert.equal((await ask({ type: "worker_delete", path: join(ctx.dir, "repo--two") })).type, "worker_deleted");
        c.close();
    },

    async "a session deletes the worker it runs in"(ctx) {
        const created = await cli(ctx, ["worker", "create", "self", "--json"]);
        const worker = created.json.worker.path;
        const trusted = await cli(ctx, ["trust", "status", "--json"]);
        await cli(ctx, ["trust", "allow", "--grants", JSON.stringify(trusted.json.trust.grants)]);
        const c = viewClient(join(ctx.dir, "state/piton.sock"));
        const { key, token, relay } = await piSession(c, worker);
        const deleting = agentRequest(relay, token, { type: "worker_delete", path: worker });
        const dialog = await c.dialog();
        assert.match(dialog.title, /This session runs in it/);
        await c.answer(key, dialog, "Allow once");
        // pi stops, so the agent may hear nothing; the worker goes anyway.
        await deleting.catch(() => null);
        for (let i = 0; i < 100 && existsSync(worker); i++) await sleep(100);
        assert.ok(!existsSync(worker), "deleted though the requester ended");
        c.close();
    },
};

let failed = 0;
for (const [name, fn] of Object.entries(tests)) {
    const ctx = setup();
    const daemon = await startDaemon(ctx);
    try {
        await fn(ctx);
        console.log(`ok   ${name}`);
    } catch (e) {
        failed++;
        console.log(`FAIL ${name}\n     ${e.stack?.split("\n").slice(0, 6).join("\n     ")}`);
    } finally {
        daemon.kill("SIGTERM");
        await daemon.exited;
    }
}
process.exit(failed ? 1 : 0);
