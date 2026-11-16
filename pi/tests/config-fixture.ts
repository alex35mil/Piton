/**
 * Writes a fixture repository's configuration as piton reads it: `.piton/config.toml` (and `config.local.toml`), through
 * the binary. Takes values as the code names them (camelCase tracker and permission keys) and writes them with the
 * file's key names, so fixtures read like the objects the code uses.
 */

import { integrationsToFile } from "../workflows/extensions/integrations/config.ts"
import { writeConfig } from "./host-runtime.ts"

export interface ProjectConfigFixture {
    branchFormat?: string
    username?: string
    tracker?: unknown
    forge?: unknown
    permissions?: PermissionsFixture
    /** `[permissions.sandbox]` / `[permissions.host]`. */
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
    if (config.permissions !== undefined) shared.permissions = permissionsTable(config.permissions)
    for (const runtime of ["host", "sandbox"] as const) {
        const rules = config.runtimePermissions?.[runtime]
        if (rules !== undefined) shared[`permissions.${runtime}`] = permissionsTable(rules)
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
 * Writes `[permissions]` of `root` (`"global"`: the test's global tier) from the object the permissions extension uses.
 * Replaces the table whole. A value the file can't hold (a number where rules go) is written as is, for tests of
 * invalid settings.
 */
export function writePermissions(root: string, rules: PermissionsFixture, local = false): string {
    return writeConfig(root, { permissions: permissionsTable(rules) }, local)
}

/** Writes `root`'s configuration (`"global"`: the test's global tier). Returns the shared file. */
export function writeProjectConfig(root: string, config: ProjectConfigFixture): string {
    const { shared, local } = changes(config)
    if (Object.keys(local).length > 0) writeConfig(root, local, true)
    return writeConfig(root, shared)
}
