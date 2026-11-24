// End-to-end tests of #241 on the host: a sandboxed session's bash calls run through the daemon, each in its own
// sandbox; a rerun the agent asks for is asked of the user and runs with everything but the floor; a persisted grant
// runs the next matching call without asking; a blocked process that isn't a bash call is offered as a sandbox entry.
// Real pi in sandbox mode, loading Piton's pi package (`pi/`, whose permissions extension sends bash to the daemon;
// its dependencies come from `mise run pi:install`) and the faux extension (the scripted model and its test tools).
// The test's `[tools.permissions] default_mode = "allow"` keeps the extension from asking before each call.
//
// Usage (in cli/): cargo build && node tests/e2e/blocks.mjs, or `mise run cli:test`. Sandboxes don't nest and `log`
// refuses inside one, so in a sandboxed session (PITON_RUNTIME=sandbox) every test is skipped.
//
// macOS may not report a process's first sandbox refusals for some seconds (docs/pi-findings.md), so the faux tool
// that blocks pi's own process keeps trying for 10 s until the kernel's record reaches the daemon.
import { existsSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import assert from "node:assert/strict";

import { attach, away, piton, runSuite, setup, sleep } from "./sandboxed.mjs";

// The block question's title in either runtime.
const QUESTION = /^Run (again in the sandbox|on the host, with no sandbox)\?/;

function prepare() {
    // Outside every grant: the temp folder is writable, so the blocked writes go to the run's folder under
    // /private/var/tmp, under names this test removes.
    const outside = join(away(), "blocks");
    return { ...setup("blocks"), outside };
}

const tests = {
    async "a plain call runs through the daemon in the session's sandbox"(ctx) {
        const a = attach(ctx);
        const ok = await a.bash("bash echo inside > out.txt && cat out.txt");
        assert.match(ok, /inside/);
        assert.ok(existsSync(join(ctx.ws, "out.txt")), "the workspace is writable");
        // A block Piton sees is asked about without the agent's `rerun`; a denial keeps the failed run and lists it.
        const dialog = a.waitNew((r) => r.type === "extension_ui_request" && r.method === "select" && QUESTION.test(r.title ?? ""));
        const done = a.bash(`bash touch ${ctx.outside}-plain`);
        const req = await dialog;
        assert.match(req.title, /The sandbox blocked this command/);
        a.send({ type: "extension_ui_response", id: req.id, value: "Deny" });
        const blocked = await done;
        assert.match(blocked, /Operation not permitted/);
        assert.match(blocked, /The sandbox blocked: file-write-create/);
        assert.match(blocked, /declined/);
        assert.ok(!existsSync(`${ctx.outside}-plain`), "outside the grants it's refused");
        await a.close();
    },

    async "piton asks about a blocked call itself; allowing runs it, and the rerun is the result"(ctx) {
        const a = attach(ctx);
        const target = `${ctx.outside}-asked`;
        const dialog = a.waitNew((r) => r.type === "extension_ui_request" && r.method === "select" && QUESTION.test(r.title ?? ""));
        const done = a.bash(`bash touch ${target} && echo touched`);
        const req = await dialog;
        assert.match(req.title, /^Run again in the sandbox\?/, "opens in the sandbox");
        a.send({ type: "extension_ui_response", id: req.id, value: req.options.find((o) => o.startsWith("Allow once in the sandbox")) });
        const result = await done;
        assert.ok(existsSync(target), "allowed once: it ran");
        assert.match(result, /Piton asked the user/);
        assert.match(result, /touched/);
        assert.doesNotMatch(result, /exited with code/, "the rerun's exit is the result's");
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
        assert.match(result, /sandbox with everything open/, "the result says where it ran");
        await a.close();
    },

    async "a host hint opens on the host, the sandbox stays; a host run passes the floor and starts a sandbox of its own"(ctx) {
        const a = attach(ctx);
        const target = join(ctx.ws, ".pi", "host-wrote");
        // Only a host run writes a protected file, and only a host run can start a sandbox (they don't nest). The helper
        // applies the policy in its environment: the one a launch in this workspace gets.
        const policy = `${piton} sandbox show | node -e 'process.stdout.write(JSON.stringify(JSON.parse(require("fs").readFileSync(0)).policy))'`;
        const command =
            `mkdir -p .pi && touch ${target} && ` +
            `PITON_SANDBOX_POLICY="$(${policy})" ${piton} sandbox-exec -- /usr/bin/true && echo nested-ok`;
        const dialog = a.waitNew((r) => r.type === "extension_ui_request" && r.method === "select" && QUESTION.test(r.title ?? ""));
        const done = a.bash(`rerun-host host tools|${command}`);
        const host = await dialog;
        assert.match(host.title, /^Run on the host, with no sandbox\?/, "opens on the hinted side");
        const switched = a.waitNew((r) => r.type === "extension_ui_request" && r.method === "select" && r.id !== host.id);
        a.send({ type: "extension_ui_response", id: host.id, value: host.options.find((o) => o.startsWith("Switch to running in the sandbox")) });
        const sandboxed = await switched;
        assert.match(sandboxed.title, /^Run again in the sandbox\?/, "the sandbox side stays");
        const back = a.waitNew((r) => r.type === "extension_ui_request" && r.method === "select" && r.id !== sandboxed.id);
        a.send({ type: "extension_ui_response", id: sandboxed.id, value: sandboxed.options.find((o) => o.startsWith("Switch to running on the host")) });
        const again = await back;
        a.send({ type: "extension_ui_response", id: again.id, value: again.options.find((o) => o.startsWith("Run once on the host")) });
        const result = await done;
        assert.ok(existsSync(target), "on the host: no floor");
        assert.match(result, /nested-ok/, `no sandbox around it: ${result}`);
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
        assert.match(await done, /declined/);
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
        // Offered as `"<path>" = "write"` in [sandbox.filesystem]; no answer saves a denial.
        assert.ok(req.title.includes(`filesystem."${target}" = "write"`), req.title);
        assert.equal(req.options[0], "Choose an answer below");
        assert.ok(req.options.includes("Not now"));
        assert.ok(!req.options.some((o) => o.startsWith("Deny")), req.options.join(", "));
        // The run ends before an answer: the question is asked again, not lost. Both waits start now: the repeat can
        // arrive in the same chunk as `agent_settled`.
        const settled = a.waitNew((r) => r.type === "agent_settled", 30000).catch(() => {});
        const repeated = a.waitNew(
            (r) => r.type === "extension_ui_request" && r.method === "select" && r.title === req.title && r.id !== req.id,
            45000,
        );
        await settled;
        const again = await repeated;
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

    async "git's control files are reported, not asked; the user's own deny is only logged"(ctx) {
        const a = attach(ctx);
        // pi's own process writes the workspace's .git/config: no entry lifts it, so a notice, never a question.
        const notice = a.waitNew(
            (r) => r.type === "extension_ui_request" && r.method === "notify" && /Git's hooks and configuration/.test(r.message ?? ""),
            30000,
        );
        a.send({ type: "prompt", message: `write ${join(ctx.ws, ".git/config")}` });
        const shown = await notice;
        assert.ok(shown.message.includes(join(ctx.ws, ".git/config")), shown.message);
        assert.match(shown.message, /Git skipped that step/);
        await a.waitNew((r) => r.type === "agent_settled", 30000).catch(() => {});
        assert.ok(
            !a.records.some((r) => r.type === "extension_ui_request" && r.method === "select" && (r.title ?? "").includes(".git/config")),
            "no question",
        );

        await a.close();

        // A `"deny"` the user wrote: the block is only logged, never asked about or noticed.
        const target = `${ctx.outside}-silenced`;
        writeFileSync(join(ctx.config, "config.toml"), `[sandbox.filesystem]\n"${target}" = "deny"\n`);
        const b = attach(ctx);
        b.send({ type: "prompt", message: `write ${target}` });
        await b.waitNew((r) => r.type === "agent_settled", 30000).catch(() => {});
        await sleep(2000);
        assert.ok(
            !b.records.some((r) => r.type === "extension_ui_request" && (r.title ?? r.message ?? "").includes(target)),
            "a silenced path is only logged",
        );
        const log = readFileSync(join(ctx.dir, "piton.log"), "utf8");
        assert.ok(
            log.split("\n").some((l) => l.includes(target) && l.includes("your own [sandbox] entry")),
            "the daemon logged it",
        );
        await b.close();
    },
};

await runSuite("blocks.mjs", tests, {
    prepare,
    cleanup(ctx) {
        for (const suffix of ["-plain", "-asked", "-rerun", "-rerun2", "-persist", "-process", "-process2", "-silenced"]) {
            rmSync(`${ctx.outside}${suffix}`, { force: true });
        }
    },
    skip: "sandboxes don't nest and `log` refuses inside one",
});
