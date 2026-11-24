// Writes a test configuration folder in the layout the daemon reads (the global tier): `config.host.toml` with how the
// daemon runs and this host's preferences. Returns the folder for PITON_CONFIG_HOME.
import { execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

/// A TOML value: strings, numbers, booleans, and arrays as JSON writes them; objects as inline tables.
const toml = (value) =>
    value && typeof value === "object" && !Array.isArray(value)
        ? `{ ${Object.entries(value).map(([k, v]) => `${JSON.stringify(k)} = ${toml(v)}`).join(", ")} }`
        : JSON.stringify(value);

/// `daemon` and `host` are flat objects of TOML values for the host file; table keys (`pi`, `env`, `stop`, `tailnet`,
/// `tools`) are objects of their own, and an object inside one is an inline table (`tools: { mise: { enabled: false } }`
/// writes `[tools]` with `mise = { enabled = false }`). `project` is the global `config.toml` (sandbox policy, the
/// defaults every project inherits). Keep-awake is off unless `keepAwake`, so no `caffeinate` runs. fnox is off unless
/// `daemon.tools.fnox` says otherwise: the user's own fnox config (with secrets behind it) applies to any folder.
export function writeConfig(dir, { daemon = {}, host = {}, project = {}, keepAwake = false } = {}) {
    const home = join(dir, "config");
    mkdirSync(home, { recursive: true });
    const tools = { fnox: { enabled: false }, ...daemon.tools };
    writeFileSync(join(home, "config.host.toml"), render({ keep_awake: keepAwake, ...daemon, tools, ...host }));
    if (Object.keys(project).length > 0) writeFileSync(join(home, "config.toml"), render(project));
    if (daemon.socket) seedState(join(dirname(daemon.socket), "piton.db"));
    return home;
}

/// A fixed host identity, so replies are comparable; the daemon opens the database at version 1.
export function seedState(path) {
    execFileSync("sqlite3", [
        path,
        `CREATE TABLE IF NOT EXISTS host (id INTEGER PRIMARY KEY CHECK (id = 1), host_id TEXT NOT NULL);
         INSERT OR IGNORE INTO host VALUES (1, 'e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0');
         PRAGMA user_version = 1;`,
    ]);
}

function render(object) {
    const lines = [];
    const tables = [];
    for (const [key, value] of Object.entries(object)) {
        if (value && typeof value === "object" && !Array.isArray(value)) tables.push([key, value]);
        else lines.push(`${key} = ${toml(value)}`);
    }
    for (const [name, table] of tables) {
        if (Object.keys(table).length === 0) continue;
        lines.push(`[${name}]`);
        for (const [key, value] of Object.entries(table)) lines.push(`${key} = ${toml(value)}`);
    }
    return lines.join("\n") + "\n";
}
