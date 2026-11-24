import * as path from "node:path"

import { Type } from "typebox"

import { compile, parse } from "../../../extensions/__lib/schema.js"
import type { McpToolCatalog } from "../../../extensions/__lib/mcp.js"
import {
    clearSettingsCache,
    plannedConfigEdit,
    projectConfigPath,
    repositoryConfigFile,
    resolveSettings,
    type Sharing,
} from "../../../extensions/__lib/settings.js"
import {
    inspectMcpAllowRules,
    inspectPersistentPermissions,
    validatePermissionSettings,
} from "../../../extensions/permissions/index.js"
import { writeProtected } from "../../../extensions/__lib/protected.js"
import { ensureLocalSettingsExclusion } from "./local-settings.js"
import { setupRoot } from "./root.js"

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
        scope: "Persistent [tools.permissions] and current-runtime [tools.permissions.<runtime>] tables of every level's config.toml and config.local.toml; excludes session overrides and skill-derived allows.",
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
    // Setup writes only `[tools.permissions]` of the source checkout's config.toml (or its .local); runtime tables are
    // maintained by hand.
    const sharing: Sharing = parsed.target === "local" ? "local" : "shared"
    const settingsPath = projectConfigPath(resolved, sharing)
    // The target takes part in the inspection below, so rules added earlier in this request cover later ones. A table
    // that doesn't exist yet joins the sources as the nearest one.
    let target = sources.find((source) => source.path === settingsPath && source.scope === "shared")
    if (!target) {
        target = {
            level: "repository",
            scope: "shared",
            sharing,
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
    const lists = validatePermissionSettings({
        ...(added.allow.length > 0 ? { allow: [...(existing.allow ?? []), ...added.allow] } : {}),
        ...(added.ask.length > 0 ? { ask: [...(existing.ask ?? []), ...added.ask] } : {}),
    })
    const exclusion =
        changed && sharing === "local" ? ensureLocalSettingsExclusion(root, repositoryConfigFile("local")) : undefined
    if (changed) {
        // Only the changed lists, so the rest of the file keeps its comments and layout.
        const edit = plannedConfigEdit(
            cwd,
            Object.fromEntries(Object.entries(lists).map(([list, rules]) => [`tools.permissions.${list}`, rules])),
            sharing,
        )
        // A protected file: under the daemon, the user sees the diff and approves it.
        if (edit.content !== null) await writeProtected(edit.path, edit.content)
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
