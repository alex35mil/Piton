// Writes a test configuration folder in the layout the daemon reads (the global tier): one `config.toml` with how the
// daemon runs and the [host] preferences. Returns the folder for PITON_CONFIG_HOME.
import { execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

const toml = (value) => JSON.stringify(value);

/// `daemon` and `host` are flat objects of TOML values; table keys (`pi`, `env`, `stop`, `tailnet`, `sandbox`) are
/// objects of their own.
export function writeConfig(dir, { daemon = {}, host = {}, keepAwake = false } = {}) {
    const home = join(dir, "config");
    mkdirSync(home, { recursive: true });
    writeFileSync(join(home, "config.toml"), render({ ...daemon, host }));
    if (daemon.socket) seedState(join(dirname(daemon.socket), "piton.db"), { keepAwake });
    return home;
}

/// Keep-awake is state, kept in the daemon's database (next to its socket), not configuration. Tests start with it off
/// so no `caffeinate` runs; the daemon creates the rest of the schema at version 1.
export function seedState(path, { keepAwake }) {
    execFileSync("sqlite3", [
        path,
        `CREATE TABLE IF NOT EXISTS host (id INTEGER PRIMARY KEY CHECK (id = 1), host_id TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT OR IGNORE INTO host VALUES (1, 'e2e0e2e0e2e0e2e0e2e0e2e0e2e0e2e0');
         INSERT OR REPLACE INTO settings VALUES ('keep_awake', '${keepAwake}');
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
