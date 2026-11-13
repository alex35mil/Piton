// End-to-end tests of `piton setup` as the Mac app runs it: JSON from the real CLI over a fixture home, including a
// global configuration the strict loader would refuse (setup's job is to report it).
// Usage (in daemon/): cargo build && node tests/e2e/setup.mjs, or `mise run daemon:test`.
import { spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, realpathSync, readlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import assert from "node:assert/strict";

const root = resolve(import.meta.dirname, "../..");
const piton = join(root, "target/debug/piton");

function home() {
    const dir = realpathSync(mkdtempSync(join(tmpdir(), "piton-setup-")));
    const config = join(dir, "config");
    return {
        dir,
        config,
        args: ["--socket", join(dir, "state", "piton.sock")],
        link: join(dir, "bin", "piton"),
        env: { ...process.env, PITON_CONFIG_HOME: config, PI_CODING_AGENT_DIR: join(dir, "agent") },
    };
}

function run(h, ...args) {
    const out = spawnSync(piton, [...h.args, "setup", ...args, "--json", "--link", h.link], { env: h.env });
    return { status: out.status, json: JSON.parse(out.stdout.toString() || "null"), stderr: out.stderr.toString() };
}

const step = (report, name) => report.steps.find((s) => s.step === name);

const tests = {
    "a fresh host: every step reported, the fixable ones fixed"() {
        const h = home();
        const check = run(h, "check");
        assert.equal(check.status, 3, check.stderr);
        assert.equal(check.json.format, 1);
        assert.deepEqual(check.json.steps.map((s) => s.step), ["prerequisites", "configuration", "cli", "daemon", "state", "remote_access"]);
        assert.equal(step(check.json, "configuration").fix.kind, "apply");
        assert.equal(step(check.json, "daemon").fix.kind, "app");
        for (const name of ["configuration", "cli", "state"]) {
            const applied = run(h, "apply", name);
            assert.equal(applied.status, 0, applied.stderr);
            assert.equal(applied.json.step, name);
        }
        assert.equal(readlinkSync(h.link), realpathSync(piton));
        const refused = run(h, "apply", "daemon");
        assert.equal(refused.status, 1);
        assert.match(refused.json.error, /isn't something piton setup does itself/);
    },

    "a configuration the daemon would refuse is a step's problem, not a failed command"() {
        for (const [file, text, expected] of [
            ["config.toml", "roots = []\n", /piton migrate|old/],
            ["settings.toml", "[host\n", /settings\.toml/],
        ]) {
            const h = home();
            mkdirSync(h.config, { recursive: true });
            writeFileSync(join(h.config, file), text);
            const check = run(h, "check");
            assert.equal(check.status, 3, check.stderr);
            const configuration = step(check.json, "configuration");
            assert.equal(configuration.status, "missing");
            assert.match(configuration.problems.join("\n"), expected);
        }
    },
};

let failed = 0;
for (const [name, test] of Object.entries(tests)) {
    try {
        test();
        console.log(`ok   ${name}`);
    } catch (e) {
        failed++;
        console.log(`FAIL ${name}\n     ${e.stack?.split("\n").slice(0, 5).join("\n     ")}`);
    }
}
process.exit(failed ? 1 : 0);
