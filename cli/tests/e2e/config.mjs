// Writes a test configuration folder in the layout the daemon reads (the global tier): `config.host.toml` with how the
// daemon runs and this host's preferences. Returns the folder for PITON_CONFIG_HOME.
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

/// A TOML value: strings, numbers, booleans, and arrays as JSON writes them; objects as inline tables.
const toml = (value) =>
    value && typeof value === "object" && !Array.isArray(value)
        ? `{ ${Object.entries(value).map(([k, v]) => `${JSON.stringify(k)} = ${toml(v)}`).join(", ")} }`
        : JSON.stringify(value);

/// `daemon` and `host` are flat objects of TOML values for the host file; table keys (`pi`, `env`, `stop`,
/// `integrations`, `debugging`) are objects of their own, and an object inside one is a table of its own
/// (`integrations: { mise: { enabled: false } }` writes `[integrations.mise]` with `enabled = false`), as a person
/// writes it, so the apps' writes can add keys to it. Durations
/// are strings (`"3s"`). `project` is the global `config.toml` (sandbox policy, the defaults every project inherits).
/// Keep-awake is off unless `keepAwake`, so no `caffeinate` runs. fnox is off unless `daemon.integrations.fnox` says
/// otherwise: the user's own fnox config (with secrets behind it) applies to any folder.
export function writeConfig(dir, { daemon = {}, host = {}, project = {}, keepAwake = false } = {}) {
    const home = join(dir, "config");
    mkdirSync(home, { recursive: true });
    const integrations = { fnox: { enabled: false }, ...daemon.integrations };
    writeFileSync(join(home, "config.host.toml"), render({ keep_awake: keepAwake, ...daemon, integrations, ...host }));
    if (Object.keys(project).length > 0) writeFileSync(join(home, "config.toml"), render(project));
    if (daemon.socket) seedState(join(dirname(daemon.socket), "piton.db"));
    return home;
}

/// A fixed host identity, so replies are comparable; the daemon opens the database at version 1 and migrates it. A
/// database the daemon already opened is left alone: setting its version back would run its migrations again.
export function seedState(path) {
    if (existsSync(path)) return;
    execFileSync("sqlite3", [
        path,
        `CREATE TABLE IF NOT EXISTS host (id INTEGER PRIMARY KEY CHECK (id = 1), host_id TEXT NOT NULL);
         INSERT OR IGNORE INTO host VALUES (1, 'e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0');
         PRAGMA user_version = 1;`,
    ]);
}

const isTable = (value) => value && typeof value === "object" && !Array.isArray(value);
/// A key as TOML writes it: bare when it can be, quoted otherwise (`"~/Library/Keychains"`).
const key = (name) => (/^[A-Za-z0-9_-]+$/.test(name) ? name : JSON.stringify(name));

/// `object` as a TOML file: its plain keys, then each object key as a `[table]`, nested ones as `[a.b]`.
function render(object) {
    const lines = [];
    const table = (path, entries) => {
        const plain = entries.filter(([, value]) => !isTable(value));
        if (path && plain.length > 0) lines.push(`[${path}]`);
        for (const [name, value] of plain) lines.push(`${key(name)} = ${toml(value)}`);
        for (const [name, value] of entries.filter(([, value]) => isTable(value))) {
            table(path ? `${path}.${key(name)}` : key(name), Object.entries(value));
        }
    };
    table("", Object.entries(object));
    return lines.join("\n") + "\n";
}
