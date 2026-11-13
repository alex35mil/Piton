/**
 * Protected files: what an agent may not change without the user's approval (the source's `.piton/` except `.local/`,
 * and `.pi/` and `.agents/skills/` at any depth). piton's binary is the one definition (`piton config protected`),
 * shared with the sandbox rules and the daemon's check.
 *
 * Every pi the daemon launches, sandboxed or on the host, writes them through the daemon (`agent.sock`, with the
 * session's token), which asks the user in every client and writes on approval; in a sandbox the OS also denies every
 * direct write. Only a pi running outside piton (no agent channel) asks the user itself before the tool writes.
 */

import { execFileSync } from "node:child_process"
import * as fs from "node:fs"
import * as net from "node:net"
import * as path from "node:path"

import { BIN_ENV, pitonBinary, RESOLVE_FORMAT } from "./settings.js"

export const TOKEN_ENV = "PITON_SESSION_TOKEN"
export const SOCKET_ENV = "PITON_AGENT_SOCKET"

/**
 * Answers by workspace and path: protection depends only on where a path resolves, so asking the binary once per path
 * is enough. (Retargeting a symlink along a path could change it; the sandbox still denies such a write, and the
 * daemon checks the path again.)
 */
const answers = new Map<string, boolean>()

/** Forgets cached answers, for tests. */
export function clearProtectedCache(): void {
    answers.clear()
}

/** Whether `file` (relative to `cwd`, or absolute) is protected: every write to it needs the user's approval. */
export function isProtected(cwd: string, file: string): boolean {
    const key = `${path.resolve(cwd)}\0${path.resolve(cwd, file)}`
    const cached = answers.get(key)
    if (cached !== undefined) return cached
    const answer = askBinary(cwd, file)
    answers.set(key, answer)
    return answer
}

function askBinary(cwd: string, file: string): boolean {
    const bin = pitonBinary()
    let stdout: string
    try {
        stdout = execFileSync(bin, ["config", "protected", file, "--workspace", cwd], {
            encoding: "utf-8",
            stdio: ["ignore", "pipe", "pipe"],
        })
    } catch (error) {
        const failure = error as NodeJS.ErrnoException & { stderr?: string }
        if (failure.code === "ENOENT") {
            throw new Error(
                `can't tell whether ${file} is protected: ${JSON.stringify(bin)} wasn't found (set ${BIN_ENV})`,
            )
        }
        throw new Error(`can't tell whether ${file} is protected: ${failure.stderr?.trim() || failure.message}`)
    }
    const output = JSON.parse(stdout) as { format: number; protected: unknown }
    if (output.format !== RESOLVE_FORMAT) throw new Error(`piton config protected printed format ${output.format}`)
    if (typeof output.protected !== "boolean") throw new Error("piton config protected printed an unexpected result")
    return output.protected
}

/** What the daemon did with a protected write. */
export type ProtectedOutcome =
    | { outcome: "approved"; modified: boolean; content: string; notes: unknown[] }
    | { outcome: "rejected"; notes: unknown[] }
    | { outcome: "refused"; reason: string }

/** The daemon's socket and this session's token, when this pi runs under the daemon. */
export function agentChannel(env: NodeJS.ProcessEnv = process.env): { socket: string; token: string } | undefined {
    const socket = env[SOCKET_ENV]
    const token = env[TOKEN_ENV]
    return socket && token ? { socket, token } : undefined
}

/**
 * Asks the daemon to write `content` to `file` (absolute): it asks the user in every client and writes on approval.
 * Waits as long as the user takes; `signal` stops waiting (the request stays with the user until they answer).
 */
export function requestProtectedWrite(
    channel: { socket: string; token: string },
    file: string,
    content: string,
    signal?: AbortSignal,
): Promise<ProtectedOutcome> {
    return new Promise((resolve, reject) => {
        const sock = net.connect(channel.socket)
        let buf = ""
        const abort = () => {
            sock.destroy()
            reject(signal?.reason ?? new Error("aborted"))
        }
        signal?.addEventListener("abort", abort, { once: true })
        sock.setEncoding("utf-8")
        sock.on("data", (chunk: string) => (buf += chunk))
        sock.on("error", (error) => {
            signal?.removeEventListener("abort", abort)
            reject(new Error(`can't reach piton's daemon at ${channel.socket}: ${error.message}`))
        })
        sock.on("end", () => {
            signal?.removeEventListener("abort", abort)
            try {
                const response = JSON.parse(buf) as
                    | ({ type: "protected_write" } & ProtectedOutcome)
                    | { type: "error"; message: string }
                if (response.type === "error") reject(new Error(`piton's daemon refused: ${response.message}`))
                else resolve(response)
            } catch {
                reject(new Error("piton's daemon sent an unreadable answer"))
            }
        })
        sock.write(
            `${JSON.stringify({ type: "protected_write", token: channel.token, path: path.resolve(file), content })}\n`,
        )
    })
}

/**
 * Writes a protected file the user already approved in the conversation (project setup's own files). Under piton's
 * daemon the write goes through it, which asks the user again with the diff (the sandbox can't write it); otherwise it's
 * written here. Throws with the reason when the write didn't happen as given.
 */
export async function writeProtected(file: string, content: string, signal?: AbortSignal): Promise<void> {
    const channel = agentChannel()
    if (!channel) {
        await fs.promises.mkdir(path.dirname(file), { recursive: true })
        await fs.promises.writeFile(file, content, "utf-8")
        return
    }
    const outcome = await requestProtectedWrite(channel, file, content, signal)
    switch (outcome.outcome) {
        case "approved":
            if (outcome.modified) throw new Error(`the user changed ${file} before approving it; read it again`)
            return
        case "rejected":
            throw new Error(`the user rejected the change to ${file}`)
        case "refused":
            throw new Error(outcome.reason)
    }
}

/** The message that explains a protected file to the agent, for a write the sandbox denied. */
export const PROTECTED_HINT =
    "This file is protected (it decides what agents may do): shell commands can't write it. Use the edit or write tool, which asks the user."
