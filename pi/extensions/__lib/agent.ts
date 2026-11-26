/**
 * The channel to piton's daemon (`agent.sock`), the one way code in pi's process asks the daemon for anything. Every
 * extension and workflow tool uses this module; none opens the socket itself.
 *
 * Each request goes on its own connection as one line, `{"token", "request": {"type", ...}}`, and the daemon answers
 * with lines of that request's own kind, or one `refused` line that says why (an unknown token, a token from the wrong
 * process, a request this caller may not send). The token is this process's own (`PITON_AGENT_TOKEN`): pi's session
 * token, which the daemon accepts only from pi's process. Bash calls get tokens of their own; pi's never reaches them.
 *
 * Outside a session the daemon launched there's no channel ([`noSession`]): a caller does the work itself, so an
 * extension is written once.
 */

import * as net from "node:net"
import * as path from "node:path"

/** The variable this process's own token is in. */
export const TOKEN_ENV = "PITON_AGENT_TOKEN"
/** The variable the socket path is in. */
export const SOCKET_ENV = "PITON_AGENT_SOCKET"

/** Whether this pi runs under piton's daemon, with the socket and its own token. */
export type Channel = { kind: "session"; socket: string; token: string } | { kind: "noSession" }

/** The channel for this process. */
export function channel(env: NodeJS.ProcessEnv = process.env): Channel {
    const socket = env[SOCKET_ENV]
    const token = env[TOKEN_ENV]
    return socket && token ? { kind: "session", socket, token } : { kind: "noSession" }
}

/** A session's channel. */
export type SessionChannel = Extract<Channel, { kind: "session" }>

/** The refusal every request kind shares: the daemon didn't handle the request. */
export interface RefusedLine {
    type: "refused"
    why: "invalid" | "unidentified" | "token" | "not_allowed" | "ended"
    message: string
}

/** The daemon refused a request before handling it. */
export class Refused extends Error {
    constructor(
        readonly why: RefusedLine["why"],
        message: string,
    ) {
        super(`piton's daemon refused: ${message}`)
    }
}

/**
 * Sends `request` and hands each answer line to `onLine` until it returns a result (`{ done }`). Rejects on a refusal,
 * an unreadable line, a closed connection, or when `signal` aborts (the connection closes, which tells the daemon the
 * caller stopped waiting).
 */
export function send<Line extends { type: string }, T>(
    session: SessionChannel,
    request: { type: string } & Record<string, unknown>,
    onLine: (line: Line) => { done: T } | undefined,
    signal?: AbortSignal,
): Promise<T> {
    return new Promise((resolve, reject) => {
        const sock = net.connect(session.socket)
        let buf = ""
        let finished = false
        const finish = (settle: () => void) => {
            if (finished) return
            finished = true
            signal?.removeEventListener("abort", abort)
            sock.destroy()
            settle()
        }
        const abort = () => finish(() => reject(signal?.reason ?? new Error("aborted")))
        if (signal?.aborted) return abort()
        signal?.addEventListener("abort", abort, { once: true })
        sock.setEncoding("utf-8")
        const handle = (text: string) => {
            let parsed: Line | RefusedLine
            try {
                parsed = JSON.parse(text) as Line | RefusedLine
            } catch {
                return finish(() => reject(new Error("piton's daemon sent an unreadable answer")))
            }
            if (parsed.type === "refused") {
                const refused = parsed as RefusedLine
                return finish(() => reject(new Refused(refused.why, refused.message)))
            }
            let result: { done: T } | undefined
            try {
                result = onLine(parsed as Line)
            } catch (error) {
                return finish(() => reject(error))
            }
            if (result) {
                const { done } = result
                finish(() => resolve(done))
            }
        }
        sock.on("data", (chunk: string) => {
            buf += chunk
            let newline: number
            while (!finished && (newline = buf.indexOf("\n")) >= 0) {
                const text = buf.slice(0, newline)
                buf = buf.slice(newline + 1)
                if (text) handle(text)
            }
        })
        sock.on("error", (error) =>
            finish(() => reject(new Error(`can't reach piton's daemon at ${session.socket}: ${error.message}`))),
        )
        sock.on("end", () => finish(() => reject(new Error("piton's daemon closed the connection without an answer"))))
        sock.write(`${JSON.stringify({ token: session.token, request })}\n`)
    })
}

/** What the daemon did with a protected write. */
export type ProtectedOutcome =
    | { outcome: "approved"; modified: boolean; content: string; notes: unknown[] }
    | { outcome: "rejected"; notes: unknown[] }
    | { outcome: "refused"; reason: string }

type WriteLine = ({ type: "protected_write" } & ProtectedOutcome) | { type: "ended" }

/**
 * Asks the daemon to write `content` to `file` (absolute): it asks the user in every client and writes on approval.
 * Waits as long as the user takes; `signal` stops waiting (the dialog closes).
 */
export function protectedWrite(
    session: SessionChannel,
    file: string,
    content: string,
    signal?: AbortSignal,
): Promise<ProtectedOutcome> {
    return send<WriteLine, ProtectedOutcome>(
        session,
        { type: "protected_write", path: path.resolve(file), content },
        (line) => {
            switch (line.type) {
                case "protected_write": {
                    const { type: _, ...outcome } = line
                    return { done: outcome as ProtectedOutcome }
                }
                case "ended":
                    throw new Error("the session ended before the user answered")
            }
        },
        signal,
    )
}

/**
 * What a lift opens in pi's running sandbox (#408): a file or folder to write (also a socket to connect to or create
 * there), a file to read, one Mach service, or TCP listening.
 */
export type LiftTarget =
    | { kind: "write"; path: string }
    | { kind: "read"; path: string }
    | { kind: "service"; name: string }
    | { kind: "listen" }

/** What the daemon answers a lift with. */
export type LiftLine =
    | { type: "granted"; tokens: string[] }
    | { type: "denied" }
    /** Not asked or not issued: a floor target, another question waiting. (`refused` is the channel's own.) */
    | { type: "not_granted"; reason: string }
    | { type: "unsupported" }
    | { type: "ended" }

/** Asks the daemon to widen pi's sandbox for `targets` (the user answers); resolves with its answer. */
export function requestLift(
    session: SessionChannel,
    targets: LiftTarget[],
    reason: string,
    signal?: AbortSignal,
): Promise<Exclude<LiftLine, { type: "ended" }>> {
    const resolved = targets.map((target) => {
        switch (target.kind) {
            case "write":
            case "read":
                return { ...target, path: path.resolve(target.path) }
            case "service":
            case "listen":
                return target
        }
    })
    return send<LiftLine, Exclude<LiftLine, { type: "ended" }>>(
        session,
        { type: "lift", targets: resolved, reason },
        (line) => {
            switch (line.type) {
                case "ended":
                    throw new Error("the session ended before the user answered")
                case "granted":
                case "denied":
                case "not_granted":
                case "unsupported":
                    return { done: line }
            }
        },
        signal,
    )
}

/** One line of the `lifts` stream: consume a token, or release one. */
export type LiftsLine =
    /** `for_child`: a process pi started (an MCP server) was blocked; it inherits the grant only once started again. */
    | { type: "grant"; id: number; token: string; lift: LiftTarget; for_child: boolean }
    | { type: "revoke"; id: number }
    | { type: "ended" }

/** pi's reply about a grant. */
export type LiftsReply = { type: "consumed"; id: number } | { type: "failed"; id: number; message: string }

/** An open `lifts` stream. */
export interface LiftsStream {
    /** Tells the daemon what became of a grant. */
    reply(reply: LiftsReply): void
    /** Closes the stream: the daemon ends every grant. */
    close(): void
    /** Resolves when the stream closes, from either side. */
    closed: Promise<void>
}

/**
 * Opens pi's `lifts` stream: the daemon sends each grant (a token bound to this process) and each revocation as the
 * user answers block questions and runs settle. Every line goes to `onLine`; the caller replies with `reply`.
 */
export function openLifts(
    session: SessionChannel,
    onLine: (line: Exclude<LiftsLine, { type: "ended" }>) => void,
): LiftsStream {
    const sock = net.connect(session.socket)
    sock.setEncoding("utf-8")
    let buf = ""
    let done: () => void = () => {}
    const closed = new Promise<void>((resolve) => (done = resolve))
    sock.on("data", (chunk: string) => {
        buf += chunk
        let newline: number
        while ((newline = buf.indexOf("\n")) >= 0) {
            const text = buf.slice(0, newline)
            buf = buf.slice(newline + 1)
            if (!text) continue
            let line: LiftsLine | RefusedLine
            try {
                line = JSON.parse(text) as LiftsLine | RefusedLine
            } catch {
                sock.destroy()
                return
            }
            switch (line.type) {
                case "grant":
                case "revoke":
                    onLine(line)
                    break
                case "ended":
                case "refused":
                    sock.destroy()
                    return
            }
        }
    })
    sock.on("close", () => done())
    sock.on("error", () => sock.destroy())
    sock.write(`${JSON.stringify({ token: session.token, request: { type: "lifts" } })}\n`)
    return {
        reply: (reply) => {
            if (!sock.destroyed) sock.write(`${JSON.stringify(reply)}\n`)
        },
        close: () => sock.destroy(),
        closed,
    }
}

/** Where a widened or host run ran: a widened sandbox, or the host with no sandbox. */
export type RanRuntime = "sandbox" | "host"
/** What made it run there: a saved `[[sandbox.commands]]` entry, the user's answer, or their answer for this session. */
export type RanBy = "grant" | "approval" | "session"

/** What the daemon answers a run request with, line by line. */
export type RunLine =
    | { type: "ran"; runtime: RanRuntime; by: RanBy }
    | { type: "output"; data: string }
    | { type: "exit"; code: number }
    | { type: "timed_out" }
    | { type: "not_run"; reason: string }
    | { type: "error"; message: string }
    /** The blocks the daemon recorded for a failed run (`operation target`, with why one isn't asked about). */
    | { type: "blocks"; blocks: string[]; note?: string }
    /** The run failed on a block and the user is asked to run it again: what follows is the rerun's, or `not_rerun`. */
    | { type: "blocked" }
    /** The user didn't approve running it again: the first run's exit follows. */
    | { type: "not_rerun"; reason: string }

/** The agent's rerun request: the side of the user's question it opens on. */
export interface Rerun {
    runtime: "sandbox" | "host"
}

export interface RunRequest {
    /** pi's tool call id, or `user:<n>` for a `!` command. */
    call_id: string
    command: string
    cwd: string
    timeout?: number
    /** pi's shell environment for the command: used when it runs with the session's own rules. */
    env: Record<string, string>
    reason?: string
    /** The agent's rerun request, with the runtime it hints; absent for a plain call or a `!` command. */
    rerun?: Rerun
}

/** How a run ended, for the caller. */
export type RunEnd = { kind: "exited"; code: number } | { kind: "timedOut" } | { kind: "notRun"; reason: string }

/**
 * Sends a run request and streams the daemon's answer: every line but the last to `onLine`. Resolves with how it ended:
 * the exit code, a timeout, or why it didn't run (denied, unanswered, refused by the session). Rejects when the daemon
 * couldn't run it, or `signal` stops it (the daemon kills it).
 */
export function run(
    session: SessionChannel,
    request: RunRequest,
    onLine: (line: Extract<RunLine, { type: "ran" | "output" | "blocks" | "blocked" | "not_rerun" }>) => void,
    signal?: AbortSignal,
): Promise<RunEnd> {
    return send<RunLine, RunEnd>(
        session,
        { type: "run_command", ...request },
        (line) => {
            switch (line.type) {
                case "exit":
                    return { done: { kind: "exited", code: line.code } }
                case "timed_out":
                    return { done: { kind: "timedOut" } }
                case "not_run":
                    return { done: { kind: "notRun", reason: line.reason } }
                case "error":
                    throw new Error(`piton's daemon couldn't run it: ${line.message}`)
                case "ran":
                case "output":
                case "blocks":
                case "blocked":
                case "not_rerun":
                    onLine(line)
                    return undefined
            }
        },
        signal,
    )
}
