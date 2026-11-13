import { Type } from "typebox"

import { compile } from "../../../extensions/__lib/schema.js"
import { readMcpConfiguration } from "../../../extensions/__lib/mcp.js"
import { resolveSettings, section } from "../../../extensions/__lib/settings.js"

export type WorkflowMcpService = "github" | "linear" | "other"

export function identifyMcpService(config: unknown): WorkflowMcpService {
    if (!config || typeof config !== "object" || !("url" in config)) return "other"
    if (config.url === "https://api.githubcopilot.com/mcp/") return "github"
    if (config.url === "https://mcp.linear.app/mcp") return "linear"
    return "other"
}

const ProviderRegistrationSchema = Type.Record(Type.String(), Type.Unknown())
const ProviderRegistrationValidator = compile(ProviderRegistrationSchema)

/**
 * The MCP registrations pi uses for `cwd`: each server with its provider, credential reference, and the file it came
 * from. Without project trust only the global tier's count. Never connects.
 */
export function inspectProjectMcp(cwd: string, projectTrusted: boolean) {
    try {
        const configured = readMcpConfiguration(cwd, projectTrusted)
        // Informational only: a trusted project's own files failing must not hide the global servers that load.
        const project = resolveSettings(cwd).mcp
        const all = projectTrusted || "error" in project ? [] : project.ok.servers
        return {
            effectiveServers: configured.servers.map(({ name, config, path, level }) => ({
                ...inspectServer(name, config),
                level,
                path,
            })),
            // Servers a trusted project would add: shown so setup can explain why they're not loaded.
            untrustedServers: projectTrusted
                ? []
                : all.filter((server) => server.level !== "global").map(({ name, path }) => ({ name, path })),
        }
    } catch (error) {
        return {
            effectiveServers: [],
            untrustedServers: [],
            error: error instanceof Error ? error.message : String(error),
        }
    }
}

export function registeredCredentialEnvironment(
    cwd: string,
    serverName: string,
    provider: "linear" | "github",
    projectTrusted: boolean,
): string {
    const inspection = inspectProjectMcp(cwd, projectTrusted)
    if (inspection.error) throw new Error(inspection.error)
    const server = inspection.effectiveServers.find((candidate) => candidate.name === serverName)
    if (!server) throw new Error(`MCP server ${JSON.stringify(serverName)} is not configured`)
    if (server.provider !== provider) {
        throw new Error(`MCP server ${JSON.stringify(serverName)} is not a recognized ${provider} registration`)
    }
    if (!server.credentialEnv) {
        throw new Error(
            `MCP server ${JSON.stringify(serverName)} does not expose a recognized credential environment reference`,
        )
    }
    return server.credentialEnv
}

function inspectServer(name: string, value: unknown) {
    if (!ProviderRegistrationValidator.check(value)) return { name }
    const service = identifyMcpService(value)
    const provider = service === "other" ? undefined : service
    const headers = ProviderRegistrationValidator.check(value.headers) ? value.headers : undefined
    const authorization = headers?.Authorization
    const credentialMatch =
        typeof authorization === "string" ? /^Bearer \$\{([A-Za-z_][A-Za-z0-9_]*)\}$/.exec(authorization) : undefined
    return {
        name,
        ...(provider ? { provider } : {}),
        ...(credentialMatch ? { credentialEnv: credentialMatch[1] } : {}),
    }
}
