/**
 * Writes a fixture repository's configuration as piton reads it: `.piton/config.toml` (and `config.local.toml`), through
 * the binary. Takes values as the code names them (camelCase tracker and permission keys) and writes them with the
 * file's key names, so fixtures read like the objects the code uses.
 */

import * as fs from "node:fs"
import * as path from "node:path"

import { clearSettingsCache } from "../extensions/__lib/settings.ts"
import { integrationsToFile } from "../workflows/extensions/integrations/config.ts"
import { writeConfig } from "./host-runtime.ts"

export interface ProjectConfigFixture {
    branchFormat?: string
    username?: string
    tracker?: unknown
    forge?: unknown
    permissions?: PermissionsFixture
    /** `[tools.permissions.sandbox]` / `[tools.permissions.host]`. */
    runtimePermissions?: { host?: PermissionsFixture; sandbox?: PermissionsFixture }
}

export interface PermissionsFixture {
    defaultMode?: string
    skillGrants?: boolean
    allow?: string[]
    ask?: string[]
    deny?: string[]
}

/** The fixture's settings as `piton config set --changes` takes them. */
function changes(config: ProjectConfigFixture): { shared: Record<string, unknown>; local: Record<string, unknown> } {
    const shared: Record<string, unknown> = {}
    const local: Record<string, unknown> = {}
    if (config.branchFormat !== undefined) shared["branches.format"] = config.branchFormat
    if (config.username !== undefined) local["branches.username"] = config.username
    if (config.tracker !== undefined) shared.tracker = integrationsToFile(config.tracker)
    if (config.forge !== undefined) shared.forge = integrationsToFile(config.forge)
    if (config.permissions !== undefined) shared["tools.permissions"] = permissionsTable(config.permissions)
    for (const runtime of ["host", "sandbox"] as const) {
        const rules = config.runtimePermissions?.[runtime]
        if (rules !== undefined) shared[`tools.permissions.${runtime}`] = permissionsTable(rules)
    }
    return { shared, local }
}

export function permissionsTable(rules: PermissionsFixture): Record<string, unknown> {
    const { defaultMode, skillGrants, ...lists } = rules
    return {
        ...(defaultMode !== undefined ? { default_mode: defaultMode } : {}),
        ...(skillGrants !== undefined ? { skill_grants: skillGrants } : {}),
        ...lists,
    }
}

/**
 * Writes `[tools.permissions]` of `root` (`"global"`: the test's global tier) from the object the permissions extension uses.
 * Replaces the table whole. A value the file can't hold (a number where rules go) is written as is, for tests of
 * invalid settings.
 */
export function writePermissions(root: string, rules: PermissionsFixture, local = false): string {
    return writeConfig(root, { "tools.permissions": permissionsTable(rules) }, local)
}

/** Writes `root`'s configuration (`"global"`: the test's global tier). Returns the shared file. */
export function writeProjectConfig(root: string, config: ProjectConfigFixture): string {
    const { shared, local } = changes(config)
    if (Object.keys(local).length > 0) writeConfig(root, local, true)
    return writeConfig(root, shared)
}

/**
 * Replaces a repository's `[tracker]` and `[forge]` in `.piton/config.toml` with tables piton refuses, by hand:
 * `piton config set` won't write a file the resolver can't read. Other tables stay; a role left out is removed. Values as
 * the code names them; each table is one line of inline TOML.
 */
export function writeInvalidIntegrations(root: string, tables: { tracker?: unknown; forge?: unknown }): string {
    const inline = (value: unknown): string =>
        Array.isArray(value)
            ? `[${value.map(inline).join(", ")}]`
            : value !== null && typeof value === "object"
              ? `{ ${Object.entries(value)
                    .map(([key, inner]) => `${JSON.stringify(key)} = ${inline(inner)}`)
                    .join(", ")} }`
              : JSON.stringify(value)
    const lines = Object.entries(tables).flatMap(([table, value]) =>
        value === undefined ? [] : [`${table} = ${inline(integrationsToFile(value))}`],
    )
    const file = path.join(fs.realpathSync(root), ".piton", "config.toml")
    // Clear both roles through the binary, so the rest of the file is kept as written; then add the tables in front of
    // its first `[table]`, where a top-level key must go.
    writeConfig(root, { tracker: null, forge: null })
    const rest = fs.existsSync(file) ? fs.readFileSync(file, "utf8") : ""
    fs.mkdirSync(path.dirname(file), { recursive: true })
    fs.writeFileSync(file, `${lines.join("\n")}\n${rest}`)
    clearSettingsCache()
    return file
}
