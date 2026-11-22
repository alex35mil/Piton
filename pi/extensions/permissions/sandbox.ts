/**
 * Bash in a sandboxed session piton's daemon launched. A sandboxed process can't start one with another sandbox, so
 * the daemon, outside any, runs every bash call and `!` command, each in its own sandbox tagged with the call: the
 * session's rules, or more when the user granted them (`[[sandbox.commands]]`) or approved a rerun the agent asked for
 * after the sandbox blocked it (`rerun`: a per-command entry's runtime and grant), never past the floor; or, when the
 * user chose it, on the host with no sandbox. The tag lets the daemon tell which call the kernel refused; refusals in pi's own sandbox (MCP
 * servers, extensions) are the session's.
 *
 * The tool is pi's own bash definition, with pi's `shellPath`, `shellCommandPrefix`, rendering, truncation, and
 * session environment; only where a command runs changes. The daemon decides what a request gets: it matches grants
 * itself and asks the user for a rerun, so the agent can ask but never approve.
 */

import * as net from "node:net"
import * as path from "node:path"

import {
    createBashToolDefinition,
    getAgentDir,
    SettingsManager,
    type BashOperations,
    type ExtensionAPI,
} from "@earendil-works/pi-coding-agent"
import { Type, type Static } from "typebox"

import { agentChannel } from "../__lib/protected.js"

/** What the daemon answers a run request with, line by line. */
export type RunLine =
    | ({ type: "ran" } & Ran)
    | { type: "output"; data: string }
    | { type: "exit"; code: number }
    | { type: "timed_out" }
    | { type: "not_run"; reason: string }
    | { type: "error"; message: string }

export interface RunRequest {
    /** pi's tool call id, or `user:<n>` for a `!` command. */
    call_id: string
    command: string
    cwd: string
    timeout?: number
    /** pi's shell environment for the command: used when it runs with the session's own rules. */
    env: Record<string, string>
    reason?: string
    /** The rerun the agent asked for, as it wrote it; absent for a plain call or a `!` command. */
    rerun?: Rerun
}

const RERUN_DESCRIPTION =
    "Only after this exact command failed because the sandbox blocked it (Operation not permitted, a refused " +
    "keychain or socket): run it again with what you name. Ask for the narrowest grant that lifts the block (the " +
    'refusal names the path), `"all"` when unsure, and the host only for what no sandbox can run. The user is ' +
    "asked first and may choose differently (another runtime, `all`, or deny); the whole command runs again."

const grantList = (description: string) => Type.Optional(Type.Array(Type.String(), { description }))

/** A narrowed grant: the `[sandbox]` lists a `[[sandbox.commands]]` entry's `grant` takes. */
const GrantListsSchema = Type.Object(
    {
        write: grantList("Folders to make writable, each with everything below it."),
        write_file: grantList("Files to make writable, exactly each path."),
        read: grantList("Paths whose credential deny to lift for reading."),
        connect: grantList("Unix sockets (a path), `localhost:PORT`, `*:PORT`, `*`, or a host to connect to."),
        bind: grantList("Paths where Unix sockets may be created."),
        services: grantList("Mach services to look up: a name, `name.*`, or `*`."),
        presets: grantList("Built-in sets of entries, by name (`git`, `apps`, `rust`, ...)."),
    },
    {
        additionalProperties: false,
        description:
            "Only what lifts the block, named by the refusal (paths absolute, or starting with `~/` or `$`), e.g. " +
            '`{ "write": ["~/.cache/tool"] }` or `{ "connect": ["$TMPDIR"] }`.',
    },
)

/**
 * The bash tool's `rerun`: the runtime and grant of a `[[sandbox.commands]]` entry. A union, so neither a host rerun with
 * a grant nor a sandbox one without fits; the daemon reads it with the same parser as the entry.
 */
export const RerunSchema = Type.Union(
    [
        Type.Object(
            {
                runtime: Type.Literal("sandbox"),
                grant: Type.Union([
                    Type.Literal("all", { description: "Everything except protected files and configuration." }),
                    GrantListsSchema,
                ]),
            },
            { additionalProperties: false },
        ),
        Type.Object(
            { runtime: Type.Literal("host") },
            {
                additionalProperties: false,
                description:
                    "No sandbox: only for what no sandbox can run (a command that starts a sandbox of its own, `log`, " +
                    "writing protected files or configuration).",
            },
        ),
    ],
    { description: RERUN_DESCRIPTION },
)

export type Rerun = Static<typeof RerunSchema>

/** What made a widened or host run run there: a saved `[[sandbox.commands]]` entry, or the user's answer. */
export type RanBy = "grant" | "approval"

/**
 * Where a call ran, when the daemon said it wasn't the session's own sandbox: a sandbox widened by `grant` (as written
 * after `grant = `: `"all"` or an inline table), or the host with no sandbox.
 */
export type Ran = { runtime: "sandbox"; grant: string; by: RanBy } | { runtime: "host"; by: RanBy }

/** The line the tool's result gets for a run outside the session's own sandbox. */
export function ranLine(ran: Ran): string {
    const where = ((): string => {
        switch (ran.runtime) {
            case "sandbox":
                return ran.grant === '"all"'
                    ? "Ran in a sandbox widened by everything but protected files and configuration"
                    : `Ran in a sandbox widened by \`grant = ${ran.grant}\``
            case "host":
                return "Ran on the host with no sandbox"
        }
    })()
    const why = ((): string => {
        switch (ran.by) {
            case "grant":
                return "by a saved per-command entry"
            case "approval":
                return "approved by the user"
        }
    })()
    return `${where}, ${why}.`
}

/** pi's shell settings for `cwd`: what it passes its built-in bash tool (the daemon reads the same files). */
export function piShellSettings(cwd: string): { shellPath?: string; commandPrefix?: string } {
    const settings = SettingsManager.create(cwd, getAgentDir())
    return { shellPath: settings.getShellPath(), commandPrefix: settings.getShellCommandPrefix() }
}

/** pi's shell environment, as its local bash builds it: this process's, with pi's bin folder first on `PATH`. */
export function piShellEnv(env: NodeJS.ProcessEnv = process.env): Record<string, string> {
    const out: Record<string, string> = {}
    for (const [key, value] of Object.entries(env)) if (value !== undefined) out[key] = value
    const bin = path.join(getAgentDir(), "bin")
    const entries = (out.PATH ?? "").split(path.delimiter).filter(Boolean)
    if (!entries.includes(bin)) out.PATH = [bin, ...entries].join(path.delimiter)
    return out
}

/**
 * Sends a run request and streams the daemon's answer: output chunks to `onData`, and where it ran (when not the
 * session's own sandbox) to `onRan`. Resolves with the exit code. Rejects with the daemon's reason when it didn't run
 * (denied, unanswered, refused), with `timeout:<s>` when it timed out (pi's own timeout message), and `aborted` when
 * `signal` stops it (the daemon kills it).
 */
export function requestRun(
    channel: { socket: string; token: string },
    request: RunRequest,
    onData: (data: Buffer) => void,
    signal?: AbortSignal,
    onRan?: (ran: Ran) => void,
): Promise<{ exitCode: number }> {
    return new Promise((resolve, reject) => {
        const sock = net.connect(channel.socket)
        let buf = ""
        let done = false
        const finish = (settle: () => void) => {
            if (done) return
            done = true
            signal?.removeEventListener("abort", abort)
            sock.destroy()
            settle()
        }
        const abort = () => finish(() => reject(new Error("aborted")))
        if (signal?.aborted) return abort()
        signal?.addEventListener("abort", abort, { once: true })
        sock.setEncoding("utf-8")
        const line = (text: string) => {
            let parsed: RunLine
            try {
                parsed = JSON.parse(text) as RunLine
            } catch {
                return finish(() => reject(new Error("piton's daemon sent an unreadable answer")))
            }
            switch (parsed.type) {
                case "ran": {
                    const { type: _, ...ran } = parsed
                    return onRan?.(ran)
                }
                case "output":
                    return onData(Buffer.from(parsed.data, "base64"))
                case "exit":
                    return finish(() => resolve({ exitCode: parsed.code }))
                case "timed_out":
                    return finish(() => reject(new Error(`timeout:${request.timeout ?? 0}`)))
                case "not_run":
                    return finish(() => reject(new Error(`Not run: ${parsed.reason}`)))
                case "error":
                    return finish(() => reject(new Error(`piton's daemon refused: ${parsed.message}`)))
            }
        }
        sock.on("data", (chunk: string) => {
            buf += chunk
            let newline: number
            while ((newline = buf.indexOf("\n")) >= 0) {
                const text = buf.slice(0, newline)
                buf = buf.slice(newline + 1)
                if (text) line(text)
            }
        })
        sock.on("error", (error) =>
            finish(() => reject(new Error(`can't reach piton's daemon at ${channel.socket}: ${error.message}`))),
        )
        sock.on("end", () => finish(() => reject(new Error("piton's daemon closed the connection without an answer"))))
        sock.write(`${JSON.stringify({ type: "run_command", token: channel.token, ...request })}\n`)
    })
}

/** Strips pi's `shellCommandPrefix` from a command pi prefixed: the daemon applies it itself. */
export function unprefixed(resolved: string, prefix: string | undefined): string {
    return prefix && resolved.startsWith(`${prefix}\n`) ? resolved.slice(prefix.length + 1) : resolved
}

/**
 * Registers the bash tool and the `!` command handler for a sandboxed session under piton's daemon. Does nothing
 * elsewhere: pi's own bash stays.
 */
export function registerSandboxBash(pi: ExtensionAPI, env: NodeJS.ProcessEnv = process.env): boolean {
    const channel = agentChannel(env)
    if (!channel || env.PITON_RUNTIME !== "sandbox") return false
    const base = createBashToolDefinition(process.cwd())
    pi.registerTool({
        ...base,
        parameters: Type.Object({
            ...base.parameters.properties,
            rerun: Type.Optional(RerunSchema),
            reason: Type.Optional(Type.String({ description: "With rerun: why it needs what it asks for." })),
        }),
        async execute(toolCallId, params, signal, onUpdate, ctx) {
            const { command, timeout, rerun, reason } = params
            const { commandPrefix } = piShellSettings(ctx.cwd)
            let ran: Ran | undefined
            const operations: BashOperations = {
                async exec(_resolved, cwd, options) {
                    // The agent's command: the daemon applies pi's prefix and shell itself, from pi's settings files.
                    const request: RunRequest = {
                        call_id: toolCallId,
                        command,
                        cwd,
                        timeout,
                        env: piShellEnv(options.env),
                        reason,
                        rerun,
                    }
                    return requestRun(channel, request, options.onData, options.signal, (r) => {
                        ran = r
                    })
                },
            }
            const tool = createBashToolDefinition(ctx.cwd, { operations, commandPrefix })
            const result = await tool.execute(toolCallId, { command, timeout }, signal, onUpdate, ctx)
            if (!ran) return result
            const structured = result.structuredContent
            return {
                ...result,
                content: [...result.content, { type: "text" as const, text: ranLine(ran) }],
                ...(structured && typeof structured === "object" && !Array.isArray(structured)
                    ? { structuredContent: { ...structured, ran } }
                    : {}),
            }
        },
    })
    let typed = 0
    pi.on("user_bash", (event) => {
        const { commandPrefix } = piShellSettings(event.cwd)
        const operations: BashOperations = {
            exec(resolved, cwd, options) {
                const request: RunRequest = {
                    call_id: `user:${++typed}`,
                    command: unprefixed(resolved, commandPrefix),
                    cwd,
                    timeout: options.timeout,
                    env: piShellEnv(options.env),
                }
                return requestRun(channel, request, options.onData, options.signal)
            },
        }
        return { operations }
    })
    return true
}
