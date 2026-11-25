// End-to-end tests of how a sandboxed launch builds pi's environment with real mise: the workspace's mise
// configuration runs inside the sandbox, and a `PI_CODING_AGENT_DIR` it sets is accepted only inside the checkout.
// Usage (in cli/): cargo build && node tests/e2e/environment.mjs, or `mise run cli:test`.
//
// Checked through `piton sandbox show|why`, which prepare a launch exactly as the daemon does. Sandboxes don't nest:
// in a sandboxed session (PITON_RUNTIME=sandbox) mise runs inside that session's sandbox instead of the launch's, so
// the containment check runs only on the host.
import { spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, realpathSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import assert from "node:assert/strict";

import { writeConfig } from "./config.mjs";

const root = resolve(import.meta.dirname, "../..");
const piton = join(root, "target/debug/piton");
const nested = process.env.PITON_RUNTIME === "sandbox";

function setup() {
    const dir = realpathSync(mkdtempSync(join(tmpdir(), "piton-env-")));
    const repo = join(dir, "repo");
    mkdirSync(join(repo, ".git"), { recursive: true });
    // What a sandboxed agent must not write, though it's in the temp folder the sandbox may write.
    const guarded = join(dir, "guarded");
    mkdirSync(guarded);
    mkdirSync(join(dir, "state"));
    const config = writeConfig(dir, {
        daemon: {
            socket: join(dir, "state/piton.sock"),
            env: { login_shell: false },
            integrations: { mise: { enabled: true } },
        },
        project: { sandbox: { filesystem: { [guarded]: "deny" } } },
    });
    const env = {
        ...process.env,
        PITON_CONFIG_HOME: config,
        PI_CODING_AGENT_DIR: join(dir, "agent"),
        MISE_TRUSTED_CONFIG_PATHS: dir,
    };
    return { dir, repo, guarded, env };
}

function cli(ctx, args) {
    const r = spawnSync(piton, args, { env: ctx.env, cwd: ctx.repo, encoding: "utf8" });
    return { code: r.status, out: r.stdout, err: r.stderr };
}

const tests = {
    async "the workspace's mise configuration runs inside the sandbox"(ctx) {
        if (nested) return;
        const marker = join(ctx.guarded, "ran");
        writeFileSync(join(ctx.repo, "mise.toml"), `[env]\nX = "{{ exec(command='touch ${marker} && echo v') }}"\n`);
        const shown = cli(ctx, ["sandbox", "show", "--workspace", ctx.repo]);
        assert.notEqual(shown.code, 0, "the launch is refused when mise fails");
        assert.match(shown.err, /mise env failed/);
        assert.ok(!existsSync(marker), "mise's command ran outside the sandbox");
    },

    async "an agent directory the workspace sets out of the checkout is refused"(ctx) {
        writeFileSync(join(ctx.repo, "mise.toml"), `[env]\nPI_CODING_AGENT_DIR = "${join(ctx.dir, "elsewhere")}"\n`);
        const shown = cli(ctx, ["sandbox", "show", "--workspace", ctx.repo]);
        assert.equal(shown.code, 3, shown.err);
        assert.match(shown.err, /PI_CODING_AGENT_DIR/);
        assert.ok(shown.err.includes(join(ctx.repo, "mise.toml")), shown.err);
    },

    async "an agent directory the workspace sets inside the checkout is granted"(ctx) {
        writeFileSync(join(ctx.repo, "mise.toml"), '[env]\nPI_CODING_AGENT_DIR = "./agent"\n');
        const why = cli(ctx, ["sandbox", "why", "--workspace", ctx.repo, "--write", join(ctx.repo, "agent/crashes.json")]);
        assert.equal(why.code, 0, why.out + why.err);
        assert.match(why.out, /agent directory/);
    },

    // A file above the checkout the sandbox can't write is trusted (unit tests); this one is in the temp folder, which
    // the sandbox may write, so it counts as the agent's.
    async "one set by a file above the checkout that the sandbox may write is refused"(ctx) {
        writeFileSync(join(ctx.dir, "mise.toml"), `[env]\nPI_CODING_AGENT_DIR = "${join(ctx.dir, "shared")}"\n`);
        const shown = cli(ctx, ["sandbox", "show", "--workspace", ctx.repo]);
        assert.equal(shown.code, 3, shown.err);
        assert.ok(shown.err.includes(join(ctx.dir, "mise.toml")), shown.err);
    },
};

let failed = 0;
for (const [name, fn] of Object.entries(tests)) {
    const ctx = setup();
    try {
        await fn(ctx);
        console.log(`ok   ${name}${nested && name.includes("inside the sandbox") ? " (skipped: nested sandbox)" : ""}`);
    } catch (e) {
        failed++;
        console.log(`FAIL ${name}\n     ${e.stack?.split("\n").slice(0, 6).join("\n     ")}`);
    }
}
process.exit(failed ? 1 : 0);
