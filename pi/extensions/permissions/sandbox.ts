/**
 * Bash in a sandboxed session piton's daemon launched. A sandboxed process can't start one with another sandbox, so
 * the daemon, outside any, runs every bash call and `!` command, each in its own sandbox tagged with the call: the
 * session's rules, or more when the user granted them (`[[sandbox.commands]]`) or approved a rerun the agent asked for
 * after the sandbox blocked it (`rerun`, hinting sandbox or host), never past the floor; or, when the user chose it, on the
 * host with no sandbox. The tag lets the daemon tell which call the kernel refused; refusals in pi's own sandbox (MCP
 * servers, extensions) are the session's.
 *
 * The tool is pi's own bash definition, with pi's `shellPath`, `shellCommandPrefix`, rendering, truncation, and
 * session environment; only where a command runs changes. The daemon decides what a request gets: it matches grants
 * itself and asks the user for a rerun, so the agent can ask but never approve. When a call fails on a block the
 * daemon recorded and an approved rerun lifts, the daemon asks the user itself, and the rerun's result is the call's;
 * a failed call's result ends with the blocks the daemon recorded.
 */

import * as path from "node:path"

import {
    createBashToolDefinition,
    getAgentDir,
    SettingsManager,
    type BashOperations,
    type ExtensionAPI,
} from "@earendil-works/pi-coding-agent"
import { StringEnum } from "@earendil-works/pi-ai"
import { Type } from "typebox"

import {
    channel,
    run,
    sessionFacts,
    TOKEN_ENV,
    type RanBy,
    type RanRuntime,
    type Rerun,
    type RunRequest,
    type Runtime,
    type SessionChannel,
} from "../__lib/agent.js"

/** What the daemon said about a failed run's blocks, for the tool's result. */
export interface BlockReport {
    blocks: string[]
    note?: string
    /** Piton asked the user to run it again. */
    asked: boolean
    /** Why it didn't run again, when the user didn't approve. */
    notRerun?: string
}

/** What goes into the output stream when Piton asks: the rerun's output follows. */
export const BLOCKED_SEPARATOR = "\n[The sandbox blocked this run; Piton asked the user to run it again.]\n"

/** The lines the tool's result ends with for a failed run's blocks. */
export function blockLines(report: BlockReport): string[] {
    const lines: string[] = []
    if (report.blocks.length > 0) lines.push(`The sandbox blocked: ${report.blocks.join("; ")}.`)
    if (report.note) lines.push(`${report.note}.`)
    if (report.notRerun) lines.push(`Not run again: ${report.notRerun}.`)
    return lines
}

/** The system-prompt guideline the sandboxed bash tool adds. */
export const SANDBOX_GUIDELINE =
    "Commands run in Piton's sandbox. When a command fails with 'Operation not permitted', a refused socket or " +
    "keychain, or `open` error -54, the sandbox likely blocked it even when no block is listed: ask with `rerun` " +
    "instead of explaining the failure or working around it, unless the user already declined to run that command " +
    "again. When Piton saw the block it asks the user itself, and the result says so."

/** The bash tool's `rerun`: the side of the user's question it opens on. The user may choose the other or deny. */
export const RerunSchema = Type.Object(
    {
        runtime: StringEnum(["sandbox", "host"] as const, {
            description:
                "`sandbox`: everything open but protected files and configuration. `host`: no sandbox, only for what " +
                "no sandbox can run (a command that starts a sandbox of its own, `log`).",
        }),
    },
    {
        additionalProperties: false,
        description:
            "Only after this exact command failed because the sandbox blocked it (Operation not permitted, a refused " +
            "keychain or socket) and Piton didn't already ask: run it again where `runtime` says. The user is asked " +
            "first and may choose the other runtime or deny; the whole command runs again. Never after the user " +
            "declined to run it again.",
    },
)

/** Where a call ran, when the daemon said it wasn't the session's own sandbox. */
export interface Ran {
    runtime: RanRuntime
    by: RanBy
}

/** The line the tool's result gets for a run outside the session's own sandbox. */
export function ranLine(ran: Ran): string {
    const where = ((): string => {
        switch (ran.runtime) {
            case "sandbox":
                switch (ran.by) {
                    // A saved entry's grant may be narrowed: it only says the entry widened the sandbox.
                    case "grant":
                        return "Ran in a sandbox widened"
                    // An approved rerun always runs with everything but the floor.
                    case "approval":
                    case "session":
                        return "Ran in a sandbox with everything open but protected files and configuration"
                }
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
            case "session":
                return "approved by the user for this session"
        }
    })()
    return `${where}, ${why}.`
}

/** pi's shell settings for `cwd`: what it passes its built-in bash tool (the daemon reads the same files). */
export function piShellSettings(cwd: string): { shellPath?: string; commandPrefix?: string } {
    const settings = SettingsManager.create(cwd, getAgentDir())
    return { shellPath: settings.getShellPath(), commandPrefix: settings.getShellCommandPrefix() }
}

/**
 * pi's shell environment, as its local bash builds it: this process's, with pi's bin folder first on `PATH`, without
 * pi's own token (a call gets its own from the daemon).
 */
export function piShellEnv(env: NodeJS.ProcessEnv = process.env): Record<string, string> {
    const out: Record<string, string> = {}
    for (const [key, value] of Object.entries(env)) if (value !== undefined && key !== TOKEN_ENV) out[key] = value
    const bin = path.join(getAgentDir(), "bin")
    const entries = (out.PATH ?? "").split(path.delimiter).filter(Boolean)
    if (!entries.includes(bin)) out.PATH = [bin, ...entries].join(path.delimiter)
    return out
}

/**
 * Sends a run request and streams the daemon's answer: output chunks to `onData`, where it ran (when not the
 * session's own sandbox) to `onRan`, and a failed run's blocks to `report`. When Piton asks the user to run it again,
 * `onData` gets [`BLOCKED_SEPARATOR`] and the rerun's output follows. Resolves with the exit code (the rerun's, or the
 * first run's when it didn't run again). Rejects with the daemon's reason when it didn't run (denied, unanswered,
 * refused), with `timeout:<s>` when it timed out (pi's own timeout message), and `aborted` when `signal` stops it (the
 * daemon kills it).
 */
export async function requestRun(
    session: SessionChannel,
    request: RunRequest,
    onData: (data: Buffer) => void,
    signal?: AbortSignal,
    onRan?: (ran: Ran) => void,
    report?: BlockReport,
): Promise<{ exitCode: number }> {
    let end
    try {
        end = await run(
            session,
            request,
            (line) => {
                switch (line.type) {
                    case "ran":
                        return onRan?.({ runtime: line.runtime, by: line.by })
                    case "output":
                        return onData(Buffer.from(line.data, "base64"))
                    case "blocks":
                        if (report) {
                            report.blocks = line.blocks
                            if (line.note) report.note = line.note
                        }
                        return
                    case "blocked":
                        if (report) report.asked = true
                        return onData(Buffer.from(BLOCKED_SEPARATOR))
                    case "not_rerun":
                        if (report) report.notRerun = line.reason
                        return
                }
            },
            signal,
        )
    } catch (error) {
        // pi's bash reports its own stop as `aborted`.
        if (signal?.aborted) throw new Error("aborted")
        throw error
    }
    switch (end.kind) {
        case "exited":
            return { exitCode: end.code }
        case "timedOut":
            throw new Error(`timeout:${request.timeout ?? 0}`)
        case "notRun":
            throw new Error(`Not run: ${end.reason}`)
    }
}

/** Strips pi's `shellCommandPrefix` from a command pi prefixed: the daemon applies it itself. */
export function unprefixed(resolved: string, prefix: string | undefined): string {
    return prefix && resolved.startsWith(`${prefix}\n`) ? resolved.slice(prefix.length + 1) : resolved
}

/**
 * Registers the bash tool and the `!` command handler for a sandboxed session under piton's daemon (the session's
 * `facts` say so). Does nothing elsewhere: pi's own bash stays.
 */
export async function registerSandboxBash(
    pi: ExtensionAPI,
    env: NodeJS.ProcessEnv = process.env,
    runtime: (session: SessionChannel) => Promise<Runtime> = async (session) => (await sessionFacts(session)).runtime,
): Promise<boolean> {
    const session = channel(env)
    if (session.kind === "noSession") return false
    if ((await runtime(session)) !== "sandbox") return false
    const base = createBashToolDefinition(process.cwd())
    pi.registerTool({
        ...base,
        promptGuidelines: [...(base.promptGuidelines ?? []), SANDBOX_GUIDELINE],
        parameters: Type.Object({
            ...base.parameters.properties,
            rerun: Type.Optional(RerunSchema),
            reason: Type.Optional(Type.String({ description: "With rerun: why it needs to run again." })),
        }),
        async execute(toolCallId, params, signal, onUpdate, ctx) {
            const { command, timeout, reason } = params
            // pi validated it against `RerunSchema`, whose runtime `StringEnum` types as `unknown`.
            const rerun = params.rerun as Rerun | undefined
            const { commandPrefix } = piShellSettings(ctx.cwd)
            let ran: Ran | undefined
            const report: BlockReport = { blocks: [], asked: false }
            const operations: BashOperations = {
                async exec(_resolved, cwd, options) {
                    // The agent's command: the daemon applies pi's prefix and shell itself, from pi's settings files.
                    const request: RunRequest = {
                        origin: {
                            kind: "bash_call",
                            call_id: toolCallId,
                            command,
                            env: piShellEnv(options.env),
                            rerun,
                        },
                        cwd,
                        timeout,
                        reason,
                    }
                    return requestRun(
                        session,
                        request,
                        options.onData,
                        options.signal,
                        (r) => {
                            ran = r
                        },
                        report,
                    )
                },
            }
            const tool = createBashToolDefinition(ctx.cwd, { operations, commandPrefix })
            const result = await tool.execute(toolCallId, { command, timeout }, signal, onUpdate, ctx)
            const lines = blockLines(report)
            if (ran) lines.push(ranLine(ran))
            if (lines.length === 0) return result
            const structured = result.structuredContent
            const extra = {
                ...(ran ? { ran: { runtime: ran.runtime, by: ran.by } } : {}),
                ...(report.blocks.length > 0 ? { blocks: report.blocks } : {}),
            }
            return {
                ...result,
                content: [...result.content, ...lines.map((text) => ({ type: "text" as const, text }))],
                ...(structured && typeof structured === "object" && !Array.isArray(structured)
                    ? { structuredContent: { ...structured, ...extra } }
                    : {}),
            }
        },
    })
    pi.on("user_bash", (event) => {
        const { commandPrefix } = piShellSettings(event.cwd)
        const operations: BashOperations = {
            exec(resolved, cwd, options) {
                const request: RunRequest = {
                    origin: {
                        kind: "typed",
                        command: unprefixed(resolved, commandPrefix),
                        env: piShellEnv(options.env),
                    },
                    cwd,
                    timeout: options.timeout,
                }
                return requestRun(session, request, options.onData, options.signal)
            },
        }
        return { operations }
    })
    return true
}
