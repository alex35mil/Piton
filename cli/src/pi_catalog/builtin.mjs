// Prints the hosts pi itself is built to reach, as JSON: `{ "providers": { "<provider>": ["host", ...] }, "signIn":
// ["host", ...] }`. Run by the daemon on the host, outside any sandbox, with pi's installed package folder as the last
// argument: it reads pi's own model catalog and sign-in code from that installation, so the hosts are exactly the ones
// that pi version uses. Hosts with a placeholder (`{location}-aiplatform.googleapis.com`) are left out.
import { readdirSync, readFileSync } from "node:fs";
import { findPackageJSON } from "node:module";
import { pathToFileURL } from "node:url";

const pkg = process.argv.at(-1);
const piAi = findPackageJSON("@earendil-works/pi-ai", pathToFileURL(`${pkg}/dist/index.js`)).replace(/package\.json$/, "");
const all = await import(pathToFileURL(`${piAi}dist/providers/all.js`).href);
const plain = (host) => /^[a-z0-9.-]+(:[0-9]+)?$/.test(host) && host.includes(".");

const providers = {};
for (const provider of all.getBuiltinProviders()) {
    for (const model of all.getAllBuiltinModels(provider)) {
        let url;
        try {
            url = new URL(model.baseUrl);
        } catch {
            continue;
        }
        const host = url.host.toLowerCase();
        if ((url.protocol === "https:" || url.protocol === "http:") && plain(host)) {
            (providers[provider] ??= new Set()).add(host);
        }
    }
}

// Sign-in and token refresh: the hosts pi's OAuth flows name in their code.
const signIn = new Set();
const oauth = `${piAi}dist/auth/oauth`;
for (const file of readdirSync(oauth).filter((f) => f.endsWith(".js"))) {
    for (const [, host] of readFileSync(`${oauth}/${file}`, "utf8").matchAll(/https:\/\/([a-z0-9.-]+\.[a-z]{2,})(?![a-z0-9.-]*\$\{)/g)) {
        if (plain(host)) signIn.add(host);
    }
}

process.stdout.write(
    JSON.stringify({
        providers: Object.fromEntries(Object.entries(providers).map(([k, v]) => [k, [...v].sort()])),
        signIn: [...signIn].sort(),
    }),
);
