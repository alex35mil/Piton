/**
 * piton's resolved configuration for a workspace, from `piton config resolve`.
 *
 * The Rust resolver in the daemon's binary is the only implementation of the chains and merge rules: the global tier
 * (`$XDG_CONFIG_HOME/piton/`), parent folders, and the repository's `.piton/` and `.agents/`. This client runs it,
 * caches the result, and runs it again only when a file or folder it depends on changed, so per-tool-call loads cost
 * a few `stat` calls.
 *
 * The binary is `PITON_BIN` when set (the daemon sets it after the workspace's mise environment, so a repository can't
 * swap it), else `piton` on PATH.
 */

import { execFileSync } from "node:child_process"
import * as fs from "node:fs"
import * as path from "node:path"

import { requireRuntime, type Runtime } from "./runtime.js"

export const BIN_ENV = "PITON_BIN"
/** The `piton config resolve` output version this client understands. */
export const RESOLVE_FORMAT = 1

export type PermissionMode = "allow" | "ask" | "deny"
export type LevelKind = "global" | "parent" | "repository"

export interface ResolvedPermissions {
    defaultMode?: PermissionMode
    allow: string[]
    ask: string[]
    deny: string[]
    keybindings?: { autoAcceptEdits?: string }
}

export interface ResolvedMcpServer {
    name: string
    config: Record<string, unknown>
    /** The file the entry came from. */
    path: string
}

export interface ResolvedWorkflows {
    branches?: { format?: "identifier-title" | "username/identifier-title" | "tracker"; username?: string }
    workflowProject?: string
    sessionAnalysis?: boolean
}

export interface FileValue<T> {
    path: string
    value: T
}

export interface ResolvedSettings {
    format: typeof RESOLVE_FORMAT
    workspace: string
    /** The checkout whose `.piton/` was read: a worker's source. */
    source: string
    /** The workspace's own checkout. */
    checkout: string
    global: string
    runtime: Runtime
    permissions: ResolvedPermissions
    mcp: { servers: ResolvedMcpServer[]; autoEnableCodemode?: boolean }
    web_search: { apiKey?: string }
    workflows: ResolvedWorkflows
    integrations: FileValue<unknown> | null
    policies: FileValue<string>[]
    /** Rule folders, farthest first. */
    rules: string[]
    /** Skill folders piton adds to pi's own discovery, farthest first. */
    skills: string[]
    /** Workflow artifacts: the source's `.piton/.local`. */
    artifacts: string
    files: { path: string; level: LevelKind }[]
    watch: string[]
}

interface ResolveError {
    format: number
    error: { kind: "invalid" | "workspace"; message: string }
}

interface CacheEntry {
    settings: ResolvedSettings
    fingerprint: string
}

const cache = new Map<string, CacheEntry>()

/** The binary that resolves configuration. */
export function pitonBinary(env: NodeJS.ProcessEnv = process.env): string {
    const configured = env[BIN_ENV]
    return configured && configured.length > 0 ? configured : "piton"
}

/**
 * The resolved configuration for `cwd`, in the current runtime. Throws the resolver's message when the configuration
 * is invalid or uses the old layout, and a clear error when the binary is missing.
 */
export function resolveSettings(cwd: string, runtime: Runtime = requireRuntime()): ResolvedSettings {
    const workspace = path.resolve(cwd)
    const key = `${runtime}\0${workspace}`
    const cached = cache.get(key)
    if (cached && fingerprint(cached.settings.watch) === cached.fingerprint) return cached.settings
    const settings = runResolve(workspace, runtime)
    cache.set(key, { settings, fingerprint: fingerprint(settings.watch) })
    return settings
}

/** Forgets cached results, for tests that change files faster than the file system's timestamps. */
export function clearSettingsCache(): void {
    cache.clear()
}

function runResolve(workspace: string, runtime: Runtime): ResolvedSettings {
    const bin = pitonBinary()
    let stdout: string
    try {
        stdout = execFileSync(bin, ["config", "resolve", "--workspace", workspace, "--runtime", runtime], {
            encoding: "utf-8",
            stdio: ["ignore", "pipe", "pipe"],
            maxBuffer: 16 * 1024 * 1024,
        })
    } catch (error) {
        const failure = error as NodeJS.ErrnoException & { stdout?: string; stderr?: string; status?: number }
        if (failure.code === "ENOENT") {
            throw new Error(
                `piton's configuration can't be read: ${JSON.stringify(bin)} wasn't found (install piton, or set ${BIN_ENV})`,
            )
        }
        if (typeof failure.stdout === "string" && failure.stdout.trim().length > 0) stdout = failure.stdout
        else throw new Error(`piton config resolve failed: ${failure.stderr?.trim() || failure.message}`)
    }
    let parsed: unknown
    try {
        parsed = JSON.parse(stdout)
    } catch {
        throw new Error("piton config resolve printed invalid JSON")
    }
    const output = parsed as Partial<ResolvedSettings> & Partial<ResolveError>
    if (output.format !== RESOLVE_FORMAT) {
        throw new Error(
            `piton config resolve printed format ${JSON.stringify(output.format)}; this pi package understands ${RESOLVE_FORMAT} (update piton and the pi package together)`,
        )
    }
    if (output.error) throw new Error(output.error.message)
    return output as ResolvedSettings
}

/** Existence, size, and modification time of every watched path. */
function fingerprint(paths: readonly string[]): string {
    return paths
        .map((file) => {
            try {
                const stat = fs.statSync(file, { bigint: true })
                return `${stat.ino}:${stat.size}:${stat.mtimeNs}`
            } catch (error) {
                const code = (error as NodeJS.ErrnoException).code
                if (code === "ENOENT" || code === "ENOTDIR") return "-"
                return `!${code}`
            }
        })
        .join("|")
}
