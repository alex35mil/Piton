/**
 * `delete_protected_file` and `rename_protected_file`: the agent's way to delete or rename a protected file, which the
 * sandbox denies to shell commands (`../__lib/protected.ts`). Each asks piton's daemon (`agent.ts`), which checks the
 * paths, asks the user in every client, and acts on approval. They act only on protected files: the daemon refuses any
 * other path, which bash deletes or renames. Outside a session piton's daemon launched there's no one to ask, and
 * nothing is registered.
 */

import * as path from "node:path"

import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent"
import { Type } from "typebox"

import { requestDelete, requestRename, type ChangeOutcome, type SessionChannel } from "../__lib/agent.js"

/** The tools this module registers: the daemon asks the user, so the permissions extension doesn't. */
export const PROTECTED_TOOLS = ["delete_protected_file", "rename_protected_file"] as const

const ONLY =
    "Only for protected files (.piton/, .pi/, fnox's files, Piton's configuration levels, pi's agent directory): any other path is refused; use bash for those. piton's daemon asks the user before it acts."

type Text = { content: Array<{ type: "text"; text: string }>; details: ChangeOutcome }

/** The tool's result: done, or why not. A denial ends the turn, as rejecting a file review without notes does. */
function result(outcome: ChangeOutcome, done: string, what: string, ctx: ExtensionContext): Text {
    const text = (value: string): Text => ({ content: [{ type: "text", text: value }], details: outcome })
    switch (outcome.outcome) {
        case "done":
            return text(done)
        case "denied":
            ctx.abort()
            return text(`[rejected] The user denied ${what}. Nothing changed.`)
        case "refused":
            throw new Error(`[rejected] ${outcome.reason}`)
    }
}

export function registerProtectedTools(pi: ExtensionAPI, session: SessionChannel): void {
    pi.registerTool({
        name: "delete_protected_file",
        label: "delete protected file",
        description: `Delete one protected file or symlink (the link, not its target), and the folders it leaves empty. ${ONLY}`,
        parameters: Type.Object({
            path: Type.String({ description: "The file to delete (relative to the working folder, or absolute)" }),
        }),
        async execute(_id, params, signal, _onUpdate, ctx) {
            const file = (params as { path: string }).path
            const outcome = await requestDelete(session, path.resolve(ctx.cwd, file), signal)
            return result(outcome, `Deleted ${file}.`, `deleting ${file}`, ctx)
        },
    })

    pi.registerTool({
        name: "rename_protected_file",
        label: "rename protected file",
        description: `Rename or move one protected file or symlink to another protected path that doesn't exist yet (missing folders are created). ${ONLY}`,
        parameters: Type.Object({
            from: Type.String({ description: "The file to rename (relative to the working folder, or absolute)" }),
            to: Type.String({ description: "Its new path, which must not exist" }),
        }),
        async execute(_id, params, signal, _onUpdate, ctx) {
            const { from, to } = params as { from: string; to: string }
            const outcome = await requestRename(session, path.resolve(ctx.cwd, from), path.resolve(ctx.cwd, to), signal)
            return result(outcome, `Renamed ${from} to ${to}.`, `renaming ${from} to ${to}`, ctx)
        },
    })
}
