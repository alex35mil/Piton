import * as fs from "node:fs"
import * as path from "node:path"

import { Type } from "typebox"

import { compile, parse } from "../../../extensions/__lib/schema.js"
import type { McpToolCatalog } from "../../../extensions/__lib/mcp.js"
import { clearSettingsCache, projectSettingsPath, resolveSettings } from "../../../extensions/__lib/settings.js"
import {
    EXTENSION as PERMISSIONS_EXTENSION,
    inspectMcpAllowRules,
    inspectPersistentPermissions,
    validatePermissionSettings,
} from "../../../extensions/permissions/index.js"
import { writeProtected } from "../../../extensions/__lib/protected.js"
import { ensureLocalSettingsExclusion } from "./local-settings.js"
import { setupRoot } from "./root.js"

const LOCAL_PERMISSIONS_SETTINGS_FILE = `.piton/${PERMISSIONS_EXTENSION}.local.json`

const SettingsTargetSchema = Type.Enum(["project", "local"], { type: "string" })
const PermissionRuleSchema = Type.String({ minLength: 1, pattern: "^[^\\r\\n]+$" })
const RulesSchema = Type.Array(PermissionRuleSchema, { uniqueItems: true })

export const PermissionInspectionSchema = Type.Object(
    { rules: Type.Array(PermissionRuleSchema, { minItems: 1, uniqueItems: true }) },
    { additionalProperties: false },
)
export const PermissionInspectionValidator = compile(PermissionInspectionSchema)

export const ProjectPermissionConfigurationSchema = Type.Object(
    { target: SettingsTargetSchema, allow: RulesSchema, ask: RulesSchema },
    { additionalProperties: false },
)
export const ProjectPermissionConfigurationValidator = compile(ProjectPermissionConfigurationSchema)

export function inspectProjectPermissions(cwd: string, input: unknown, catalog: McpToolCatalog) {
    const parsed = parse(PermissionInspectionValidator, input)
    const { sources } = inspectPersistentPermissions(cwd)
    return {
        scope: "Persistent global, project, and local settings, shared and current-runtime files; excludes session overrides and skill-derived allows.",
        rules: inspectMcpAllowRules(parsed.rules, sources, catalog),
    }
}

export async function configureProjectPermissions(cwd: string, input: unknown, catalog: McpToolCatalog) {
    const parsed = parse(ProjectPermissionConfigurationValidator, input)
    if (parsed.allow.length + parsed.ask.length === 0) throw new Error("request at least one allow or ask rule")
    validatePermissionSettings({ allow: parsed.allow, ask: parsed.ask })
    const root = setupRoot(cwd)
    const resolved = resolveSettings(cwd)
    const { sources } = inspectPersistentPermissions(cwd)
    // Setup writes only the source checkout's shared file (or its .local); runtime files are maintained by hand.
    const settingsPath = projectSettingsPath(resolved, PERMISSIONS_EXTENSION, parsed.target === "local")
    // The target takes part in the inspection below, so rules added earlier in this request cover later ones. A file
    // that doesn't exist yet joins the sources as the nearest one.
    let target = sources.find((source) => source.path === settingsPath)
    if (!target) {
        target = {
            level: "repository",
            scope: "shared",
            local: parsed.target === "local",
            path: settingsPath,
            settings: {},
        }
        sources.push(target)
    }
    const existing = target.settings
    const added = { allow: [] as string[], ask: parsed.ask.filter((rule) => !(existing.ask ?? []).includes(rule)) }
    const skippedAsk = parsed.ask.filter((rule) => (existing.ask ?? []).includes(rule))
    const inspection = [] as ReturnType<typeof inspectMcpAllowRules>
    // Validate the complete request before changing even the in-memory settings.
    inspectMcpAllowRules(parsed.allow, sources, catalog)
    target.settings = { ...existing, ask: [...(existing.ask ?? []), ...added.ask] }
    for (const rule of parsed.allow) {
        const result = inspectMcpAllowRules([rule], sources, catalog)[0]
        inspection.push(result)
        if (result.status !== "available") continue
        added.allow.push(rule)
        target.settings = { ...target.settings, allow: [...(target.settings.allow ?? []), rule] }
    }
    const changed = added.allow.length + added.ask.length > 0
    const settings = validatePermissionSettings({
        ...existing,
        ...(added.allow.length > 0 ? { allow: [...(existing.allow ?? []), ...added.allow] } : {}),
        ...(added.ask.length > 0 ? { ask: [...(existing.ask ?? []), ...added.ask] } : {}),
    })
    const exclusion =
        changed && parsed.target === "local"
            ? ensureLocalSettingsExclusion(root, LOCAL_PERMISSIONS_SETTINGS_FILE)
            : undefined
    if (changed) {
        // A protected file: under the daemon, the user sees the diff and approves it.
        await writeProtected(settingsPath, `${JSON.stringify(settings, null, 4)}\n`)
        clearSettingsCache()
    }
    return {
        target: parsed.target,
        file: path.relative(root, settingsPath),
        added,
        skipped: { allow: inspection.filter((result) => result.status !== "available"), ask: skippedAsk },
        inspection,
        changed,
        localExclusionChanged: exclusion?.changed ?? false,
    }
}
