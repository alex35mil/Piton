/**
 * Protected files: every configuration file the user edits by hand, which an agent may not change without the user's
 * approval. In the checkout: the source's `.piton/` except `.local/`, and `.pi/` and fnox's config files at any depth.
 * Outside it: Piton's configuration levels (the global tier, `.piton/` and `.agents/` in parent folders), pi's agent
 * directory (except the runtime state pi writes itself), the files fnox loads, and what their symlinks point to.
 * piton's binary is the one definition (`piton config protected`, which in a session answers from the session's facts,
 * asked of the daemon), shared with the sandbox rules and the daemon's check.
 *
 * Every pi the daemon launches, sandboxed or on the host, writes, deletes, and renames them through the daemon (the
 * channel, `agent.ts`), which asks the user in every client and acts on approval; in a sandbox the OS also denies every
 * direct change. Only a pi running outside piton (no channel) asks the user itself before the write or edit tool writes.
 */

import { execFileSync } from "node:child_process"
import * as fs from "node:fs"
import * as path from "node:path"

import { channel, requestWrite } from "./agent.js"
import { BIN_ENV, pitonBinary, RESOLVE_FORMAT } from "./settings.js"

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

/**
 * Writes a protected file the user already approved in the conversation (project setup's own files). Under piton's
 * daemon the write goes through it, which asks the user again with the diff (the sandbox can't write it); otherwise it's
 * written here. Throws with the reason when the write didn't happen as given.
 */
export async function writeProtected(file: string, content: string, signal?: AbortSignal): Promise<void> {
    const current = channel()
    switch (current.kind) {
        case "noSession":
            await fs.promises.mkdir(path.dirname(file), { recursive: true })
            await fs.promises.writeFile(file, content, "utf-8")
            return
        case "session":
            break
    }
    const outcome = await requestWrite(current, file, content, signal)
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

/** What a shell command did to a protected file when the sandbox denied it, as its output says. */
export type DeniedChange = "write" | "delete" | "rename" | "moveIn" | "moveOut"

const PROTECTED = "This file is protected (it decides what agents may do): shell commands can't change it."

/** The message that explains a protected file to the agent, for a change the sandbox denied: what works instead. */
export function protectedHint(change: DeniedChange): string {
    switch (change) {
        case "write":
            return `${PROTECTED} Use the edit or write tool, which asks the user.`
        case "delete":
            return `${PROTECTED} Use the delete_protected_file tool, which asks the user; it deletes one file at a time (a folder goes with its last file).`
        case "rename":
            return `${PROTECTED} Use the rename_protected_file tool, which asks the user.`
        case "moveIn":
            return `${PROTECTED} To move a file into it, write the destination with the write tool (it asks the user), then remove the original with bash.`
        case "moveOut":
            return `${PROTECTED} To move it out, copy it with bash, then delete the original with the delete_protected_file tool (it asks the user).`
    }
}
