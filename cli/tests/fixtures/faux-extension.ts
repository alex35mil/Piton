// Test-only pi extension: registers a credential-free "faux" provider whose replies are scripted by the prompt text.
//
// Prompt directives (anything else gets a short text reply):
//   "slow <n>"   reply with n words, streamed slowly, so a test can attach mid-stream
//   "tool"       call the built-in bash tool with `sleep 2; echo done`, then answer
//   "bash <cmd>" call the bash tool with <cmd>; "rerun <reason>|<cmd>" with <cmd> and a sandbox rerun;
//                "rerun-host <reason>|<cmd>" with a host rerun
//   "write <path>" call the `faux_write` tool: pi's own process writes <path> (a block that isn't a bash call), once a
//                second for up to 10 s while it's refused: macOS may not report a process's first refusals for a
//                few seconds (docs/pi-findings.md)
//   "ask"        ask a select dialog through the `faux_ask` tool, then answer with the choice
//   "picture"    call the `faux_picture` tool, which returns a 2 MB image (larger than a phone's WebSocket message)
//   "status"     call the `faux_status` tool, which sets a status line and a widget, then clears them on "status clear"
//   "env"        call the `faux_env` tool, which returns the daemon's variables for this pi (token, agent socket, and
//                what the fake fnox set)
import { fauxProvider, fauxAssistantMessage, fauxText, fauxToolCall } from "@earendil-works/pi-ai";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
    const faux = fauxProvider({
        provider: "faux",
        models: [{ id: "faux-1", name: "Faux 1", contextWindow: 100000, maxTokens: 4096 }],
        tokensPerSecond: Number(process.env.PITON_FAUX_TPS ?? 200),
    });

    const reply = (context: { messages: Array<{ role: string; content: unknown }> }) => {
        const last = context.messages[context.messages.length - 1];
        if (last?.role === "toolResult") {
            return fauxAssistantMessage(fauxText(`tool finished: ${JSON.stringify(last.content).slice(0, 80)}`));
        }
        const text = textOf(last?.content);
        const slow = /^slow (\d+)/.exec(text);
        if (slow) {
            const words = Array.from({ length: Number(slow[1]) }, (_, i) => `word${i}`).join(" ");
            return fauxAssistantMessage(fauxText(words));
        }
        const bash = /^bash (.+)$/s.exec(text);
        if (bash) {
            return fauxAssistantMessage([fauxToolCall("bash", { command: bash[1] })], { stopReason: "toolUse" });
        }
        const rerun = /^rerun(-host)? ([^|]*)\|(.+)$/s.exec(text);
        if (rerun) {
            const runtime = rerun[1] ? "host" : "sandbox";
            return fauxAssistantMessage(
                [fauxToolCall("bash", { command: rerun[3], rerun: { runtime }, reason: rerun[2] })],
                { stopReason: "toolUse" },
            );
        }
        const write = /^write (.+)$/.exec(text);
        if (write) {
            return fauxAssistantMessage([fauxToolCall("faux_write", { path: write[1] })], { stopReason: "toolUse" });
        }
        if (text.startsWith("tool")) {
            return fauxAssistantMessage([fauxToolCall("bash", { command: "sleep 2; echo done" })], {
                stopReason: "toolUse",
            });
        }
        if (text.startsWith("ask")) {
            return fauxAssistantMessage([fauxToolCall("faux_ask", { question: "Pick one" })], { stopReason: "toolUse" });
        }
        if (text.startsWith("picture")) {
            return fauxAssistantMessage([fauxToolCall("faux_picture", {})], { stopReason: "toolUse" });
        }
        if (text.startsWith("env")) {
            return fauxAssistantMessage([fauxToolCall("faux_env", {})], { stopReason: "toolUse" });
        }
        if (text.startsWith("status")) {
            return fauxAssistantMessage([fauxToolCall("faux_status", { clear: text.includes("clear") })], {
                stopReason: "toolUse",
            });
        }
        return fauxAssistantMessage(fauxText(`echo: ${text}`));
    };

    // Each model call takes the next scripted response; a factory answers every call.
    const refill = () => faux.setResponses(Array.from({ length: 1000 }, () => reply));
    refill();

    pi.registerProvider(faux.provider);

    pi.registerTool({
        name: "faux_ask",
        label: "Faux ask",
        description: "Ask the user to pick an option (test tool).",
        parameters: { type: "object", properties: { question: { type: "string" } }, required: ["question"] },
        async execute(_id, params, _signal, _onUpdate, ctx) {
            const choice = await ctx.ui.select(String(params.question), ["alpha", "beta"]);
            return { content: [{ type: "text", text: `picked ${choice ?? "nothing"}` }], details: {} };
        },
    });

    pi.registerTool({
        name: "faux_status",
        label: "Faux status",
        description: "Set or clear a status line and a widget (test tool).",
        parameters: { type: "object", properties: { clear: { type: "boolean" } } },
        async execute(_id, params, _signal, _onUpdate, ctx) {
            const clear = Boolean(params.clear);
            ctx.ui.setStatus("faux", clear ? undefined : "busy");
            ctx.ui.setWidget("faux", clear ? undefined : ["line one", "line two"]);
            return { content: [{ type: "text", text: clear ? "cleared" : "set" }], details: {} };
        },
    });

    pi.registerTool({
        name: "faux_env",
        label: "Faux env",
        description: "Return the daemon's variables for this pi (test tool).",
        parameters: { type: "object", properties: {} },
        async execute() {
            const env = {
                token: process.env.PITON_SESSION_TOKEN ?? null,
                socket: process.env.PITON_AGENT_SOCKET ?? null,
                fnox: process.env.FAKE_FNOX ?? null,
                githubToken: process.env.GITHUB_TOKEN ?? null,
                fnoxProfile: process.env.FNOX_PROFILE ?? null,
                workspaceMarker: process.env.PITON_WORKSPACE_MARKER ?? null,
            };
            return { content: [{ type: "text", text: JSON.stringify(env) }], details: env };
        },
    });

    pi.registerTool({
        name: "faux_write",
        label: "Faux write",
        description: "Write a file from pi's own process (test tool).",
        parameters: { type: "object", properties: { path: { type: "string" } }, required: ["path"] },
        async execute(_id, params) {
            const fs = await import("node:fs");
            let failure: unknown;
            for (let attempt = 0; attempt < 10; attempt++) {
                if (attempt > 0) await new Promise((resolve) => setTimeout(resolve, 1000));
                try {
                    fs.writeFileSync(String(params.path), "x");
                    return { content: [{ type: "text", text: "written" }], details: {} };
                } catch (error) {
                    failure = error;
                }
            }
            return { content: [{ type: "text", text: String(failure) }], details: {} };
        },
    });

    pi.registerTool({
        name: "faux_picture",
        label: "Faux picture",
        description: "Return a large image (test tool).",
        parameters: { type: "object", properties: {} },
        async execute() {
            const data = Buffer.alloc(1536 * 1024, 7).toString("base64");
            return {
                content: [
                    { type: "text", text: "a picture" },
                    { type: "image", mimeType: "image/png", data },
                ],
                details: {},
            };
        },
    });
}

function textOf(content: unknown): string {
    if (typeof content === "string") return content;
    if (Array.isArray(content)) {
        return content
            .filter((b): b is { type: "text"; text: string } => b?.type === "text")
            .map((b) => b.text)
            .join("");
    }
    return "";
}
