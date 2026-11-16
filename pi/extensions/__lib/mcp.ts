import * as fs from "node:fs"

import type { ExtensionAPI, ToolDefinition, ToolAnnotations } from "@earendil-works/pi-coding-agent"

import { resolveSettings, section, type LevelKind } from "./settings.js"

export const MCP_TOOL_CATALOG_EVENT = "mcp:tool-catalog"
export const MCP_TOOL_DECORATORS_EVENT = "mcp:tool-decorators"

const MCP_NAME_PATTERN = /^[A-Za-z0-9_.-]+$/

export type McpServerTransport = { type: "stdio" } | { type: "streamable-http"; url: string }

export interface McpToolMetadata {
    serverName: string
    serverToolName: string
    inputSchema: Readonly<Record<string, unknown>>
    annotations?: ToolAnnotations
}

export type McpToolCatalog = ReadonlyMap<string, McpToolMetadata>

export type McpToolDecorator = (
    tool: ToolDefinition,
    metadata: McpToolMetadata,
    transport: McpServerTransport,
) => ToolDefinition

// Listeners register synchronously; decorators are applied outside the event bus.
export interface McpToolDecorators {
    use(decorator: McpToolDecorator): void
}

export type McpSettings = Record<string, unknown> & {
    mcpServers?: Record<string, unknown>
    autoEnableCodemode?: boolean
}

export interface ConfiguredMcpServer {
    name: string
    config: unknown
    /** The file the entry came from, and its level in piton's configuration chain. */
    path: string
    level: LevelKind
}

export function validateMcpName(kind: "server" | "tool", name: string): void {
    if (!MCP_NAME_PATTERN.test(name)) {
        throw new Error(`invalid MCP ${kind} name ${JSON.stringify(name)}; expected [A-Za-z0-9_.-]+`)
    }
}

export type McpToolAddress =
    | { state: "available"; tool: string }
    | { state: "unavailable"; server: string; operation: string; reason: string }

export function resolveMcpTool(serverName: string, toolName: string, catalog: McpToolCatalog): McpToolAddress {
    for (const [registeredName, metadata] of catalog) {
        if (metadata.serverName === serverName && metadata.serverToolName === toolName)
            return { state: "available", tool: registeredName }
    }
    return {
        state: "unavailable",
        server: serverName,
        operation: toolName,
        reason: `MCP operation ${JSON.stringify(`${serverName}/${toolName}`)} is unavailable in the live tool catalog`,
    }
}

export function getMcpToolName(serverName: string, toolName: string, catalog: McpToolCatalog): string {
    const address = resolveMcpTool(serverName, toolName, catalog)
    if (address.state === "unavailable") throw new Error(address.reason)
    return address.tool
}

export function isMcpTool(
    registeredName: string,
    serverName: string,
    toolName: string,
    catalog: McpToolCatalog,
): boolean {
    const metadata = catalog.get(registeredName)
    return metadata?.serverName === serverName && metadata.serverToolName === toolName
}

// Evidence must come from one exact server operation; the error names what to send instead.
export function requireMcpTool(
    registeredName: string,
    serverName: string,
    toolName: string,
    catalog: McpToolCatalog,
    subject: string,
): void {
    if (isMcpTool(registeredName, serverName, toolName, catalog)) return
    const address = resolveMcpTool(serverName, toolName, catalog)
    switch (address.state) {
        case "available":
            throw new Error(`${subject} must come from ${address.tool}, got ${registeredName}`)
        case "unavailable":
            throw new Error(`${subject} cannot come from ${registeredName}: ${address.reason}`)
    }
}

// Each extension owns its subscription; callers pass the catalog explicitly across module boundaries.
export function observeMcpTools(pi: Pick<ExtensionAPI, "events" | "on">): McpToolCatalog {
    const catalog = new Map<string, McpToolMetadata>()
    const dispose = pi.events.on(MCP_TOOL_CATALOG_EVENT, (data) => {
        catalog.clear()
        if (data instanceof Map) for (const [name, metadata] of data as McpToolCatalog) catalog.set(name, metadata)
    })
    pi.on("session_shutdown", () => {
        dispose()
        catalog.clear()
    })
    return catalog
}

export function isMcpRecord(value: unknown): value is Record<string, unknown> {
    return value !== null && typeof value === "object" && !Array.isArray(value)
}

// Provider evidence may arrive as decoded JSON or a complete native MCP result.
export function mcpResponseValue(response: unknown): unknown {
    if (Array.isArray(response)) return response
    if (!isMcpRecord(response)) throw new Error("Provider evidence requires an object or array response")
    if (response.isError === true) throw new Error("MCP error results cannot establish provider evidence")
    if (!Array.isArray(response.content)) return response
    let payload: unknown
    if (response.structuredContent !== undefined) payload = response.structuredContent
    else {
        const [block] = response.content
        if (
            response.content.length !== 1 ||
            !isMcpRecord(block) ||
            block.type !== "text" ||
            typeof block.text !== "string"
        )
            throw new Error("MCP evidence requires structured content or one JSON text block")
        try {
            payload = JSON.parse(block.text)
        } catch {
            throw new Error("MCP evidence text is not JSON")
        }
    }
    return payload
}

export function mcpResponsePayload(response: unknown): Record<string, unknown> {
    if (!isMcpRecord(response)) throw new Error("Provider evidence requires an object response")
    const payload = mcpResponseValue(response)
    if (!isMcpRecord(payload)) throw new Error("MCP evidence payload must be an object")
    return payload
}

// Read references only. Native Pi validates registrations and resolves credentials.
export function readMcpSettings(file: string): McpSettings {
    if (!fs.existsSync(file)) return {}
    let value: unknown
    try {
        value = JSON.parse(fs.readFileSync(file, "utf-8"))
    } catch {
        throw new Error(`Invalid MCP JSON in ${file}`)
    }
    if (!isMcpRecord(value) || (value.mcpServers !== undefined && !isMcpRecord(value.mcpServers))) {
        throw new Error(`Invalid MCP settings in ${file}: expected an object with mcpServers`)
    }
    if (value.autoEnableCodemode !== undefined && typeof value.autoEnableCodemode !== "boolean") {
        throw new Error(`Invalid MCP settings in ${file}: autoEnableCodemode must be a boolean`)
    }
    return value as McpSettings
}

export function mcpServerKey(name: string): string {
    return name.replace(/-/g, "_")
}

/**
 * The MCP servers pi registers: piton's resolved chain (the nearer entry replaces the whole entry of the same server,
 * names differing only by `-` and `_` conflict). Without project trust, only the global tier's. Never connects or
 * executes credential commands.
 */
export function readMcpConfiguration(cwd: string, projectTrusted: boolean) {
    const resolved = resolveSettings(cwd)
    // Without trust only the global tier is read, so the project's files can't break it.
    const chosen = section(projectTrusted ? resolved.mcp : resolved.mcp_global)
    const servers: ConfiguredMcpServer[] = chosen.servers.map(({ name, config, path, level }) => ({
        name,
        config,
        path,
        level,
    }))
    return { servers, autoEnableCodemode: chosen.autoEnableCodemode }
}
