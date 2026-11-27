/**
 * Runs from pi's process (#407): code in pi's process (an extension, a workflow tool) asks piton's daemon to run a
 * program where it needs to: the session's own sandbox, the sandbox widened by named `[sandbox]` entries, or the host.
 * The daemon decides: a saved `[[sandbox.commands]]` entry or a session approval runs it, the session's own sandbox
 * never asks, and a widened or host run asks the user.
 *
 *     const outcome = await runProgram(
 *         { kind: "exec", argv: ["git", "branch", "-D", name] },
 *         { cwd, where: { kind: "widened", grants: { allow_git_control_files: true } }, reason: "delete the branch" },
 *     )
 *
 * Outside a session piton's daemon launched, or in a host session, pi isn't sandboxed: the program runs here.
 */

import * as cp from "node:child_process"
import * as os from "node:os"

import { channel, run, sessionFacts, type Program, type RunWhere, type SessionChannel } from "./agent.js"

export type { Program, RunWhere } from "./agent.js"

/** How a run ended. */
export type RunOutcome =
    /** It ran: its exit code (128 + the signal when one ended it) and what it printed, stdout and stderr together. */
    | { kind: "exited"; code: number; output: string }
    | { kind: "timedOut" }
    /** It didn't run: the user denied it or didn't answer, or the daemon refused (a widening your entries close). */
    | { kind: "notRun"; reason: string }

export interface RunOptions {
    cwd: string
    where: RunWhere
    /** Why it needs to run where it asks, shown to the user. */
    reason: string
    /** Seconds. */
    timeout?: number
    signal?: AbortSignal
    env?: NodeJS.ProcessEnv
    /** The session's runtime (tests): the facts' by default. */
    runtime?: (session: SessionChannel) => Promise<"host" | "sandbox">
}

/** Runs `program` where `options.where` asks, through the daemon in a sandboxed session, here otherwise. */
export async function runProgram(program: Program, options: RunOptions): Promise<RunOutcome> {
    const session = channel(options.env)
    switch (session.kind) {
        case "noSession":
            return runHere(program, options)
        case "session":
            break
    }
    const runtime = await (options.runtime ?? (async (s) => (await sessionFacts(s)).runtime))(session)
    switch (runtime) {
        case "host":
            return runHere(program, options)
        case "sandbox":
            break
    }
    const chunks: Buffer[] = []
    const end = await run(
        session,
        {
            origin: { kind: "pi", program, runtime: options.where },
            cwd: options.cwd,
            timeout: options.timeout,
            reason: options.reason,
        },
        (line) => {
            switch (line.type) {
                case "output":
                    chunks.push(Buffer.from(line.data, "base64"))
                    return
                // Where it ran, and a bash call's block lines, which a pi run never gets.
                case "ran":
                case "blocks":
                case "blocked":
                case "not_rerun":
                    return
            }
        },
        options.signal,
    )
    switch (end.kind) {
        case "exited":
            return { kind: "exited", code: end.code, output: Buffer.concat(chunks).toString("utf-8") }
        case "timedOut":
            return { kind: "timedOut" }
        case "notRun":
            return { kind: "notRun", reason: end.reason }
    }
}

/** Runs `program` in this process's own environment: a program with no shell, a shell command with `/bin/sh`. */
function runHere(program: Program, options: RunOptions): RunOutcome {
    const [file, args] = ((): [string, string[]] => {
        switch (program.kind) {
            case "exec":
                return [program.argv[0]!, program.argv.slice(1)]
            case "shell":
                return ["/bin/sh", ["-c", program.command]]
        }
    })()
    const result = cp.spawnSync(file, args, {
        cwd: options.cwd,
        env: options.env ?? process.env,
        encoding: "utf-8",
        timeout: options.timeout === undefined ? undefined : options.timeout * 1000,
    })
    if (result.error && (result.error as NodeJS.ErrnoException).code === "ETIMEDOUT") return { kind: "timedOut" }
    if (result.error) return { kind: "notRun", reason: `can't start ${file}: ${result.error.message}` }
    const code = result.status ?? 128 + (result.signal ? (os.constants.signals[result.signal] ?? 1) : 1)
    return { kind: "exited", code, output: `${result.stdout}${result.stderr}` }
}
