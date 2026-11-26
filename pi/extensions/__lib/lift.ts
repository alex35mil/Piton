/**
 * Lifts for pi's live sandbox (#408): code in pi's process asks piton's daemon to widen its sandbox for one action, the
 * user approves, and this module consumes the tokens, runs the action, and releases them, whatever happens.
 *
 * The tokens are Seatbelt sandbox extensions the daemon issues bound to pi's process, so they're useless to anyone else.
 * Node has no built-in way to consume one: piton's daemon writes a small Node-API addon next to its socket
 * (`<state>/native/lift.node`, the build matching the daemon), which this module loads.
 *
 *     const outcome = await lift([{ kind: "write", path: ".git/config" }], "delete the merged branch", async () => {
 *         await git(["branch", "-D", name])
 *     })
 *
 * While a lift is open, every part of pi's process can use it, the agent's write and edit tools included (bash calls
 * run in sandboxes of their own and can't); keep the action short.
 * A file opened during the action stays writable after release (the sandbox checks at `open`).
 */

import * as path from "node:path"

import { channel, openLifts, requestLift, SOCKET_ENV, type LiftTarget, type LiftsStream } from "./agent.js"

/** The addon's two calls. */
export interface Addon {
    consume(token: string): number
    release(handle: number): void
}

/** The addon, or why it isn't there. */
export type AddonState = { kind: "loaded"; addon: Addon } | { kind: "unavailable"; reason: string }

let cached: AddonState | undefined

/** The addon's path for this session: next to the daemon's socket. */
export function addonPath(env: NodeJS.ProcessEnv = process.env): string | undefined {
    const socket = env[SOCKET_ENV]
    return socket ? path.join(path.dirname(socket), "native", "lift.node") : undefined
}

/** Loads the addon once. `load` is the loader (tests pass a fake). */
export function loadAddon(env: NodeJS.ProcessEnv = process.env, load: (file: string) => Addon = dlopen): AddonState {
    if (cached) return cached
    if (process.platform !== "darwin") return (cached = { kind: "unavailable", reason: "lifts need macOS's sandbox" })
    const file = addonPath(env)
    if (!file) return (cached = { kind: "unavailable", reason: "not in a session piton's daemon launched" })
    try {
        cached = { kind: "loaded", addon: load(file) }
    } catch (error) {
        cached = { kind: "unavailable", reason: `can't load ${file}: ${(error as Error).message}` }
    }
    return cached
}

/** Forgets the loaded addon (tests). */
export function resetAddon(): void {
    cached = undefined
}

function dlopen(file: string): Addon {
    const module = { exports: {} as Addon }
    process.dlopen(module, file)
    return module.exports
}

/** What became of a lift. */
export type LiftOutcome<T> =
    | { kind: "done"; value: T }
    /** The user said no: the action didn't run. */
    | { kind: "denied" }
    /** The daemon refused without asking (a floor path, another question waiting): the action didn't run. */
    | { kind: "refused"; reason: string }
    /** Not macOS: use the daemon's run or write requests instead. */
    | { kind: "unsupported" }
    /** The addon couldn't be loaded: the action didn't run. */
    | { kind: "unavailable"; reason: string }

/**
 * Widens pi's sandbox for `targets`, runs `action`, and releases. Outside a session piton's daemon launched, `action`
 * runs directly (nothing to widen).
 */
export async function lift<T>(
    targets: LiftTarget[],
    reason: string,
    action: () => Promise<T>,
    options: { signal?: AbortSignal; env?: NodeJS.ProcessEnv } = {},
): Promise<LiftOutcome<T>> {
    const ch = channel(options.env)
    switch (ch.kind) {
        case "noSession":
            return { kind: "done", value: await action() }
        case "session":
            break
    }
    const addon = loadAddon(options.env)
    switch (addon.kind) {
        case "unavailable":
            return { kind: "unavailable", reason: addon.reason }
        case "loaded":
            break
    }
    const answer = await requestLift(ch, targets, reason, options.signal)
    switch (answer.type) {
        case "denied":
            return { kind: "denied" }
        case "not_granted":
            return { kind: "refused", reason: answer.reason }
        case "unsupported":
            return { kind: "unsupported" }
        case "granted":
            break
    }
    const handles: number[] = []
    try {
        for (const token of answer.tokens) handles.push(addon.addon.consume(token))
        return { kind: "done", value: await action() }
    } finally {
        for (const handle of handles) {
            try {
                addon.addon.release(handle)
            } catch {
                // Released already, or never consumed: nothing stays open.
            }
        }
    }
}

/** What the stream did with a grant, for listeners (the MCP extension restarts its servers so they inherit it). */
export interface LiftGranted {
    lift: LiftTarget
    /** A process pi started (an MCP server) was blocked: it inherits the grant only once started again. */
    forChild: boolean
}

/** The event the permissions extension emits on pi's bus when a grant was consumed. */
export const LIFT_GRANTED_EVENT = "piton:lift-granted"

/**
 * Holds pi's `lifts` stream: consumes each grant with the addon, releases it on revoke, and releases everything when the
 * stream ends. `granted` hears every consumed grant.
 */
export function holdLifts(granted: (event: LiftGranted) => void, env: NodeJS.ProcessEnv = process.env) {
    const ch = channel(env)
    if (ch.kind === "noSession") return undefined
    const addon = loadAddon(env)
    if (addon.kind === "unavailable") return undefined
    const handles = new Map<number, number>()
    const stream: LiftsStream = openLifts(ch, (line) => {
        switch (line.type) {
            case "grant":
                try {
                    handles.set(line.id, addon.addon.consume(line.token))
                    stream.reply({ type: "consumed", id: line.id })
                    granted({ lift: line.lift, forChild: line.for_child })
                } catch (error) {
                    stream.reply({ type: "failed", id: line.id, message: (error as Error).message })
                }
                return
            case "revoke": {
                const handle = handles.get(line.id)
                handles.delete(line.id)
                if (handle !== undefined) {
                    try {
                        addon.addon.release(handle)
                    } catch {
                        // Nothing open under it.
                    }
                }
                return
            }
        }
    })
    void stream.closed.then(() => {
        for (const handle of handles.values()) {
            try {
                addon.addon.release(handle)
            } catch {
                // Nothing open under it.
            }
        }
        handles.clear()
    })
    return stream
}
