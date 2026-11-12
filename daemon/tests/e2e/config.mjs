// Writes a test configuration folder in the layout the daemon reads (the global tier): `daemon.toml` for how the
// daemon runs and `settings.toml` [host] for the preferences. Returns the folder for PITON_CONFIG_HOME.
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const toml = (value) => JSON.stringify(value);

/// `daemon` and `host` are flat objects of TOML values; table keys (`pi`, `env`, `stop`, `tailnet`, `sandbox`) are
/// objects of their own.
export function writeConfig(dir, { daemon = {}, host = {} } = {}) {
    const home = join(dir, "config");
    mkdirSync(home, { recursive: true });
    writeFileSync(join(home, "daemon.toml"), render(daemon));
    writeFileSync(join(home, "settings.toml"), render({ host }));
    return home;
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
