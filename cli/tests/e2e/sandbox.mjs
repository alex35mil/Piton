// End-to-end tests of each sandbox profile on the host (#211): real pi in the real sandbox, launched in `strict`,
// `standard`, and `yolo` with a scratch configuration. Each launch checks pi's own startup there (it answers, writes its
// session file, loads its global settings and packages, starts in a project with `.pi/` resources), then runs the same
// probes and checks what the profile grants and refuses. A refused probe must also have been reported (a block
// question, answered "Deny" or "Not now" here, or the result's list of blocks).
//
// The daemon takes the home folder from the user database, not `$HOME`, so the policy's home entries are the user's
// real ones. The probes never change a user file there, and read only what isn't secret, with the output discarded:
// the credential probe lists `~/.ssh` (a missing file reads "No such file" before the sandbox decides, so it must name
// something that exists, and a listing opens no file), and the probe of what `standard` grants reads SSH's config,
// known hosts, and one public key, checking only that the read wasn't refused. Writes outside every grant go to
// the run's folder under /private/var/tmp (`away()`), never the home folder. The SSH agent and the other socket are
// listeners this test starts in its scratch folder, never the user's.
//
// Usage (in cli/): cargo build && node tests/e2e/sandbox.mjs, or `mise run cli:test`. Sandboxes don't nest, so in a
// sandboxed session (PITON_RUNTIME=sandbox) every test is skipped. TCP probes reach 1.1.1.1:80.
import { spawn } from "node:child_process";
import { existsSync, lstatSync, mkdirSync, readdirSync, rmSync } from "node:fs";
import { createServer } from "node:net";
import { homedir } from "node:os";
import { join } from "node:path";
import assert from "node:assert/strict";

import { attach, away, runSuite, setup, sleep, trust } from "./sandboxed.mjs";

/// Paths outside every grant: in the run's folder, made on first use (after the suite's sandboxed-session skip).
const outside = () => join(away(), "sandbox");
const refusal = /Operation not permitted|PermissionError|EPERM/;

/// What each profile grants among the probes that differ by profile (policy.rs `profile_entries`). Every profile
/// refuses a credential read and a write outside the grants, and allows the workspace and temp.
const GRANTS = {
    strict: { sshFiles: false, sshAgent: false, otherSocket: false, tcpOut: false, tcpListen: false, gitHooks: false },
    standard: { sshFiles: true, sshAgent: true, otherSocket: false, tcpOut: true, tcpListen: true, gitHooks: false },
    yolo: { sshFiles: true, sshAgent: true, otherSocket: true, tcpOut: true, tcpListen: true, gitHooks: true },
};

/// A public key under `~/.ssh` (links not followed, a few levels deep), found outside the sandbox; `standard` reads
/// every one.
function publicKey() {
    const walk = (dir, depth) => {
        if (depth > 3) return undefined;
        let names;
        try {
            names = readdirSync(dir);
        } catch {
            return undefined;
        }
        for (const name of names) {
            const path = join(dir, name);
            const stat = lstatSync(path);
            if (stat.isFile() && name.endsWith(".pub")) return path;
            if (stat.isDirectory()) {
                const found = walk(path, depth + 1);
                if (found) return found;
            }
        }
        return undefined;
    };
    return walk(join(homedir(), ".ssh"), 0);
}

async function waitFor(path, what) {
    for (let i = 0; i < 100 && !existsSync(path); i++) await sleep(50);
    assert.ok(existsSync(path), `${what} didn't start`);
}

async function prepare(name) {
    const profile = name.split(":")[0];
    const ctx = setup("sandbox", {
        // pi's own default declines a project in RPC mode; Piton's decision (`--approve`, from `piton trust allow` in
        // `startup`) decides instead. pi still reads trust.json under its lock.
        settings: { defaultProjectTrust: "never" },
        files: {
            ".piton/config.local.toml": `[sandbox]\nprofile = "${profile}"\n`,
            ".pi/settings.json": "{}\n",
            ".pi/prompts/e2e-check.md": "---\ndescription: e2e check\n---\nhello\n",
        },
    });
    mkdirSync(join(ctx.ws, ".git/hooks"), { recursive: true });
    ctx.profile = profile;

    // A scratch SSH agent with no keys: `standard`'s `$SSH_AUTH_SOCK` entry names it.
    ctx.agentSock = join(ctx.dir, "agent.sock");
    ctx.sshAgent = spawn("ssh-agent", ["-D", "-a", ctx.agentSock], { stdio: "ignore" });
    await waitFor(ctx.agentSock, "ssh-agent");
    ctx.env.SSH_AUTH_SOCK = ctx.agentSock;

    // Another socket in the scratch folder: only `yolo` reaches every socket.
    ctx.otherSock = join(ctx.dir, "other.sock");
    ctx.server = createServer((s) => s.end("hi\n"));
    await new Promise((r) => ctx.server.listen(ctx.otherSock, r));
    return ctx;
}

function cleanup(ctx) {
    ctx.sshAgent?.kill("SIGTERM");
    ctx.server?.close();
    for (const suffix of ["-cmd", "-pi"]) rmSync(`${outside()}${suffix}`, { force: true });
}

/// Declines every question as it comes, and counts them: a refused probe must be reported.
function session(ctx) {
    let asked = 0;
    const a = attach(ctx, {
        onRecord(rec, c) {
            if (rec.type !== "extension_ui_request" || rec.method !== "select") return;
            const decline = rec.options?.find((o) => o === "Deny" || o === "Not now");
            if (!decline) return;
            asked++;
            c.send({ type: "extension_ui_response", id: rec.id, value: decline });
        },
    });
    a.asked = () => asked;
    return a;
}

/// Runs `command` as a bash call; returns its result text and whether the sandbox refused it. A refusal must have been
/// reported, unless `reported: false`: for a probe macOS doesn't reliably log (#374).
async function probe(a, command, { reported = true } = {}) {
    const before = a.asked();
    const text = await a.bash(`bash ${command}`);
    // Some programs say nothing when refused (`nc`): the result's list of blocks counts too.
    const listed = /The sandbox blocked/.test(text);
    const refused = refusal.test(text) || listed;
    if (refused && reported) {
        assert.ok(a.asked() > before || listed, `refused but not reported: ${command}\n${text}`);
    }
    return { text, refused };
}

/// pi's own process writes `path` (the `faux_write` tool); returns the tool's result text.
async function processWrite(a, path) {
    const ended = a.waitNew((r) => r.type === "tool_execution_end" && r.toolName === "faux_write", 60000);
    a.send({ type: "prompt", message: `write ${path}` });
    const end = await ended;
    await a.waitNew((r) => r.type === "agent_settled", 30000).catch(() => {});
    return JSON.stringify(end.result);
}

function expect(profile, what, granted, { text, refused }) {
    assert.equal(!refused, granted, `${profile}: ${what} should be ${granted ? "allowed" : "refused"}:\n${text}`);
}

async function startup(ctx, a) {
    const settled = a.waitNew((r) => r.type === "agent_settled", 30000);
    a.send({ type: "prompt", message: "hello" });
    await settled;
    assert.ok(JSON.stringify(a.records).includes("echo: hello"), "pi answers");

    const state = await a.call({ type: "get_state" });
    const file = state.data?.sessionFile;
    assert.ok(file?.startsWith(ctx.agent), `the session file is in pi's agent directory: ${file}`);
    assert.ok(existsSync(file), "the session file is written");

    // Piton's pi package (global settings, read under settings.json.lock) and the project's prompt template (trust.json
    // read under its lock, then the project's .pi/).
    const commands = (await a.call({ type: "get_commands" })).data?.commands?.map((c) => c.name) ?? [];
    assert.ok(commands.includes("rules"), `the global settings' package loads: ${commands.join(", ")}`);
    assert.ok(commands.includes("e2e-check"), `the project's .pi/ loads: ${commands.join(", ")}`);
}

async function probes(ctx, a) {
    const grants = GRANTS[ctx.profile];
    const p = ctx.profile;

    expect(p, "a credential read", false, await probe(a, "ls ~/.ssh >/dev/null"));
    const pub = publicKey();
    const sshFiles = ["~/.ssh/config", "~/.ssh/known_hosts", ...(pub ? [pub] : [])];
    expect(p, "reading SSH's config, known hosts, and public keys", grants.sshFiles,
        await probe(a, sshFiles.map((f) => `cat ${f} >/dev/null`).join("; ")));
    const agent = await probe(a, `SSH_AUTH_SOCK=${ctx.agentSock} ssh-add -l`);
    expect(p, "the SSH agent", grants.sshAgent, agent);
    if (grants.sshAgent) assert.match(agent.text, /no identities/, agent.text);

    // `nc`, not python3: macOS doesn't log python3's refused socket connects, so they'd never be reported.
    expect(p, "another socket", grants.otherSocket, await probe(a, `nc -U ${ctx.otherSock} </dev/null`));
    expect(p, "TCP out", grants.tcpOut, await probe(a, "nc -z -w 10 1.1.1.1 80"));
    // python3, since `nc -l` waits for a connection when it's allowed. macOS logs python3's refused listens only
    // sometimes (#374), so the report isn't required here.
    const listen = "python3 -I -c 'import socket; s = socket.socket(); s.bind((\"0.0.0.0\", 0)); s.listen(); print(\"listening\")'";
    expect(p, "listening on TCP", grants.tcpListen, await probe(a, listen, { reported: false }));

    expect(p, "the workspace and temp", true, await probe(a, `touch ws-probe && t=$(mktemp) && rm "$t" && echo wrote`));
    assert.ok(existsSync(join(ctx.ws, "ws-probe")));
    expect(p, "a write outside the grants", false, await probe(a, `touch ${outside()}-cmd`));
    assert.ok(!existsSync(`${outside()}-cmd`));
    expect(p, "Git's hooks", grants.gitHooks, await probe(a, "touch .git/hooks/x"));

    // pi's own process: the same policy, where the startup failures were.
    const inside = await processWrite(a, join(ctx.ws, "pi-wrote"));
    assert.match(inside, /written/, `${p}: pi writes the workspace: ${inside}`);
    // Its refusal is reported too: a question (declined by `session`) or a notice naming the path. The kernel's record
    // can take seconds to arrive, so the wait starts before the write and allows for it, as in blocks.mjs.
    const target = `${outside()}-pi`;
    const reported = a.waitNew(
        (r) => r.type === "extension_ui_request" && `${r.title ?? ""}${r.message ?? ""}`.includes(target),
        45000,
    );
    const home = await processWrite(a, target);
    assert.match(home, refusal, `${p}: pi's write outside the grants is refused: ${home}`);
    assert.ok(!existsSync(target));
    await reported.catch((e) => assert.fail(`${p}: pi's refused write wasn't reported: ${e.message}`));
}

const profileTest = async (ctx) => {
    // The workspace has pi project files: decided first, so pi gets `--approve` and loads them.
    await trust(ctx);
    const a = session(ctx);
    try {
        await startup(ctx, a);
        await probes(ctx, a);
    } finally {
        await a.close();
    }
};

const tests = {
    "strict: pi starts and works; everything outside the base is refused": profileTest,
    "standard: pi starts and works; SSH, the network, and listening are open": profileTest,
    "yolo: pi starts and works; every socket and Git's hooks are open too": profileTest,
};

await runSuite("sandbox.mjs", tests, { prepare, cleanup });
