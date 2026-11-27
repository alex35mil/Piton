/** Native MCP composition: Pi owns connections, authentication, discovery, results, and shutdown. */
import {
    createMcpExtension,
    type ExtensionAPI,
    type McpExtensionOptions,
    type ToolDefinition,
} from "@earendil-works/pi-coding-agent"

import {
    isMcpRecord,
    MCP_TOOL_CATALOG_EVENT,
    MCP_TOOL_DECORATORS_EVENT,
    readMcpConfiguration,
    type McpServerTransport,
    type McpToolDecorator,
    type McpToolDecorators,
    type McpToolMetadata,
} from "../__lib/mcp.js"
import { LIFT_GRANTED_EVENT, type LiftGranted } from "../__lib/lift.js"

export default function (pi: ExtensionAPI) {
    return registerNativeMcp(pi)
}

export function registerNativeMcp(pi: ExtensionAPI, options: Omit<McpExtensionOptions, "loadConfig"> = {}) {
    const catalog = new Map<string, McpToolMetadata>()
    const ownedServers = new Set<string>()
    let autoEnableCodemode: boolean | undefined
    const publish = () => pi.events.emit(MCP_TOOL_CATALOG_EVENT, new Map(catalog))
    // The session's UI, for notices outside a hook (a lift granted on pi's stream).
    let notify: ((message: string) => void) | undefined

    pi.on("session_start", (_event, ctx) => {
        notify = ctx.hasUI ? (message) => ctx.ui.notify(message, "warning") : undefined
        catalog.clear()
        publish()
        for (const name of ownedServers) pi.unregisterMcpServer(name)
        ownedServers.clear()
        const configured = readMcpConfiguration(ctx.cwd, ctx.isProjectTrusted())
        autoEnableCodemode = configured.autoEnableCodemode
        for (const { name, config } of configured.servers) {
            if (!isMcpRecord(config)) throw new Error(`Invalid MCP registration ${JSON.stringify(name)}`)
            // Native registration validates the remaining fields; references stay unresolved here.
            if (typeof config.command === "string") pi.registerMcpServer(name, { ...config, command: config.command })
            else if (typeof config.url === "string") pi.registerMcpServer(name, { ...config, url: config.url })
            else throw new Error(`MCP registration ${JSON.stringify(name)} needs command or url`)
            ownedServers.add(name)
        }
    })

    // A live lift widened pi's sandbox (#408) for a block in a process pi started: a stdio server is pi's child and kept
    // the sandbox it started with, so the servers this extension registered start again to inherit the grant. Others are
    // named in a notice. A grant for pi's own process restarts nothing.
    const disposeLifts = pi.events.on(LIFT_GRANTED_EVENT, (data) => {
        if (!(data as LiftGranted).forChild) return
        const restarted: string[] = []
        const others: string[] = []
        for (const server of pi.getMcpServers()) {
            if (!isMcpRecord(server.config) || typeof server.config.command !== "string") continue
            if (!ownedServers.has(server.name)) {
                others.push(server.name)
                continue
            }
            const config = { ...server.config, command: server.config.command }
            pi.unregisterMcpServer(server.name)
            pi.registerMcpServer(server.name, config)
            restarted.push(server.name)
        }
        if (others.length > 0)
            notify?.(
                `pi's sandbox was widened. ${others.join(", ")} started before it: restart with /mcp reconnect <name> to use it.`,
            )
        return restarted
    })

    pi.on("session_shutdown", () => {
        catalog.clear()
        publish()
        disposeLifts()
    })

    const transportFor = (server: string): McpServerTransport => {
        const config = pi.getMcpServers().find((entry) => entry.name === server)?.config
        if (!isMcpRecord(config))
            throw new Error(`Missing configuration for native MCP server ${JSON.stringify(server)}`)
        if (typeof config.url === "string") return { type: "streamable-http", url: config.url }
        if (typeof config.command === "string") return { type: "stdio" }
        throw new Error(`Missing transport for native MCP server ${JSON.stringify(server)}`)
    }

    const registerTool = (native: ToolDefinition) => {
        if (!native.name.startsWith("mcp__")) {
            pi.registerTool(native)
            return
        }
        if (native.exposure === "hidden") {
            catalog.delete(native.name)
            publish()
            pi.registerTool(native)
            return
        }
        // Pi 1.5.2 supplies the original server/tool in its label. Never split its
        // model-facing name: that name may be normalized, truncated, or hashed.
        const separator = native.label.indexOf("/")
        if (separator < 1) throw new Error("Native MCP registration is missing its original server/tool label")
        const metadata: McpToolMetadata = {
            serverName: native.label.slice(0, separator),
            serverToolName: native.label.slice(separator + 1),
            inputSchema: structuredClone(native.parameters as Record<string, unknown>),
            ...(native.annotations ? { annotations: { ...native.annotations } } : {}),
        }
        const transport = transportFor(metadata.serverName)
        const decorators: McpToolDecorator[] = []
        pi.events.emit(MCP_TOOL_DECORATORS_EVENT, {
            use: (decorate) => decorators.push(decorate),
        } satisfies McpToolDecorators)
        const definition = decorators.reduce((tool, decorate) => decorate(tool, metadata, transport), native)
        if (definition.name !== native.name) throw new Error("MCP decorators must preserve tool names")
        catalog.set(native.name, metadata)
        publish()
        pi.registerTool(definition)
    }

    return createMcpExtension({
        ...options,
        // The adapter owns file discovery. Native Pi consumes extension registrations only.
        loadConfig: () => ({ servers: [], errors: [], autoEnableCodemode }),
    })(
        new Proxy(pi, {
            get(target, property, receiver) {
                return property === "registerTool" ? registerTool : Reflect.get(target, property, receiver)
            },
        }),
    )
}
