import * as fs from "node:fs"
import * as path from "node:path"
import { isDeepStrictEqual } from "node:util"

import { Type } from "typebox"

import { compile, parse } from "../../../extensions/__lib/schema.js"
import { mcpServerKey, readMcpSettings, validateMcpName } from "../../../extensions/__lib/mcp.js"
import {
    clearSettingsCache,
    projectSettingsPath,
    resolveSettings,
    section,
} from "../../../extensions/__lib/settings.js"
import { writeProtected } from "../../../extensions/__lib/protected.js"
import { ensureLocalSettingsExclusion } from "./local-settings.js"
import { setupRoot } from "./root.js"

export const LOCAL_MCP_SETTINGS_FILE = ".piton/mcp.local.json"

const EnvironmentNameSchema = Type.String({ pattern: "^[A-Za-z_][A-Za-z0-9_]*$" })
const ServerNameSchema = Type.String({ minLength: 1, pattern: "^[A-Za-z0-9_-]+$" })
const SettingsTargetSchema = Type.Enum(["project", "local"], { type: "string" })

/**
 * Every GitHub registration gets every workflow toolset, whatever role it serves here: the same
 * server also files workflow reports to a GitHub-tracked workflow source. `context` provides
 * get_me, which resolves the login that starting work assigns.
 */
export const GITHUB_MCP_TOOLSETS = ["context", "issues", "projects", "pull_requests"] as const

const LinearRegistrationSchema = Type.Object(
    {
        provider: Type.Enum(["linear"], { type: "string" }),
        name: ServerNameSchema,
        credentialEnv: EnvironmentNameSchema,
    },
    { additionalProperties: false },
)

const GitHubRegistrationSchema = Type.Object(
    {
        provider: Type.Enum(["github"], { type: "string" }),
        name: ServerNameSchema,
        credentialEnv: EnvironmentNameSchema,
    },
    { additionalProperties: false },
)

export const ProjectMcpConfigurationSchema = Type.Object(
    {
        target: SettingsTargetSchema,
        registrations: Type.Array(Type.Union([LinearRegistrationSchema, GitHubRegistrationSchema]), {
            minItems: 1,
        }),
    },
    { additionalProperties: false },
)
export const ProjectMcpConfigurationValidator = compile(ProjectMcpConfigurationSchema)

export type ProjectMcpConfiguration = Type.Static<typeof ProjectMcpConfigurationSchema>
type Registration = ProjectMcpConfiguration["registrations"][number]
type SettingsTarget = ProjectMcpConfiguration["target"]

/**
 * Registers provider MCP servers in the source checkout's `.piton/mcp.json` (`project`) or `.piton/mcp.local.json`
 * (`local`). A server already registered anywhere in the configuration chain under the same name (or one differing
 * only by `-`/`_`) is refused unless it's this exact entry in the target file.
 */
export async function configureProjectMcp(cwd: string, input: unknown) {
    const parsed = parse(ProjectMcpConfigurationValidator, input)
    const root = setupRoot(cwd)
    const resolved = resolveSettings(cwd)
    const targetPath = projectSettingsPath(resolved, "mcp", parsed.target === "local")
    const target = readMcpSettings(targetPath)
    const names = new Set<string>()
    const additions: Array<{ registration: Registration; config: Record<string, unknown> }> = []

    for (const registration of parsed.registrations) {
        validateMcpName("server", registration.name)
        if (names.has(mcpServerKey(registration.name))) {
            throw new Error(`duplicate requested MCP server ${JSON.stringify(registration.name)}`)
        }
        names.add(mcpServerKey(registration.name))

        const config = registrationConfig(registration)
        const inTarget = Object.entries(target.mcpServers ?? {}).find(
            ([name]) => mcpServerKey(name) === mcpServerKey(registration.name),
        )
        if (inTarget && inTarget[0] === registration.name && isDeepStrictEqual(inTarget[1], config)) continue
        const existing = section(resolved.mcp).servers.find(
            (server) => mcpServerKey(server.name) === mcpServerKey(registration.name),
        )
        if (existing !== undefined || inTarget !== undefined) {
            throw new Error(
                `MCP server ${JSON.stringify(registration.name)} is already configured in ${existing?.path ?? targetPath}; reuse it or choose another name`,
            )
        }
        additions.push({ registration, config })
    }

    let changed = false
    for (const { registration, config } of additions) {
        target.mcpServers ??= {}
        if (target.mcpServers[registration.name] === undefined) {
            target.mcpServers[registration.name] = config
            changed = true
        }
    }

    if (parsed.target === "local") {
        ensureLocalSettingsExclusion(root, LOCAL_MCP_SETTINGS_FILE)
    }
    if (changed) {
        // A protected file: under the daemon, the user sees the diff and approves it.
        await writeProtected(targetPath, `${JSON.stringify(target, null, 4)}\n`)
        clearSettingsCache()
    }
    const changedTargets: SettingsTarget[] = changed ? [parsed.target] : []

    return {
        changed: changedTargets,
        files: changed ? [path.relative(root, targetPath)] : [],
        servers: parsed.registrations.map(({ name, provider, credentialEnv }) => ({
            name,
            provider,
            target: parsed.target,
            credentialEnv,
        })),
        restartRequired: changed,
    }
}

function registrationConfig(registration: Registration): Record<string, unknown> {
    const authorization = `Bearer \${${registration.credentialEnv}}`
    if (registration.provider === "linear") {
        return {
            url: "https://mcp.linear.app/mcp",
            headers: { Authorization: authorization },
        }
    }

    return {
        url: "https://api.githubcopilot.com/mcp/",
        headers: {
            Authorization: authorization,
            "X-MCP-Toolsets": GITHUB_MCP_TOOLSETS.join(","),
        },
    }
}
