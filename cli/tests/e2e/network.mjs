// End-to-end tests of #228 on the host: a strict session's filtered network. A bash call connecting to a host no
// `connect` entry lists waits while the user is asked; allowing lets it through, denying refuses it, and "allow in this
// project" saves the entry so later calls pass without asking. A listed host passes without a question. Real pi in
// sandbox mode with Piton's pi package and the faux extension, as in blocks.mjs. It reaches public hosts (example.com,
// www.iana.org) over plain HTTP: a local server wouldn't do, since the gate refuses names that resolve to this Mac.
//
// Usage (in cli/): cargo build && node tests/e2e/network.mjs, or `mise run cli:test`. Sandboxes don't nest, so in a
// sandboxed session (PITON_RUNTIME=sandbox) every test is skipped.
import { readFileSync } from "node:fs";
import { join } from "node:path";
import assert from "node:assert/strict";

import { attach, runSuite, setup } from "./sandboxed.mjs";

const NETWORK = /wants to connect to/;

/// The destinations: public hosts the test reaches through the gate. `example.com` is listed; the others are asked about.
const LISTED = "example.com";

/// strict, with one host listed (the user's own file: loosening keys).
const prepare = () =>
    setup("network", { files: { ".piton/config.local.toml": `[sandbox]\nprofile = "strict"\nconnect = ["${LISTED}"]\n` } });

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
        // `curl -f` fails on the gate's 403, so the call fails and its result lists the refusal.
        let done = a.bash("bash curl -sf -o /dev/null --max-time 20 http://www.iana.org/");
        let q = await dialog;
        a.send({ type: "extension_ui_response", id: q.id, value: "Not now" });
        const refused = await done;
        assert.match(refused, /exit code 22|code 22/, refused);
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

await runSuite("network.mjs", tests, { prepare });
