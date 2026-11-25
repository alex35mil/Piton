/**
 * piton's resolved configuration for a workspace, from `piton config resolve`, and changes to it through
 * `piton config set`.
 *
 * The Rust resolver in the daemon's binary is the only implementation of the chains and merge rules: the global tier
 * (`$XDG_CONFIG_HOME/piton/`), parent folders, and the repository's `.piton/` and `.agents/`, each level's settings in
 * one `config.toml` (and `config.local.toml`). The global tier's own files (`config.host.toml`, `config.client.toml`)
 * belong to the daemon and the apps; the pi package reads only `[debugging]` from the host file, through
 * {@link resolveGlobalSettings}. This client runs the resolver, caches the result, and runs it again only when a file
 * or folder it depends on changed, so per-tool-call loads cost a few `stat` calls.
 *
 * The file's keys are snake_case (`default_mode`, `mcp_server`); the pi package's own types are camelCase.
 * {@link camelKeys} and {@link snakeKeys} translate at this boundary, so the rest of the package keeps its names.
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

/** Permission rules as the file has them. */
export interface ResolvedPermissions {
    default_mode?: PermissionMode
    allow: string[]
    ask: string[]
    deny: string[]
}

export interface ResolvedMcpServer {
    name: string
    config: Record<string, unknown>
    /** The file the entry came from, and its level. */
    path: string
    level: LevelKind
}

export interface ResolvedMcp {
    servers: ResolvedMcpServer[]
    autoEnableCodemode?: boolean
}

export type PermissionScope = "shared" | Runtime
/** Which file of a level's pair: `config.toml` (shared) or `config.local.toml` (this machine's). */
export type Sharing = "shared" | "local"

/** One permission table that sets something: `[tools.permissions]` (`shared`) or `[tools.permissions.<runtime>]`. */
export interface ResolvedPermissionSource {
    path: string
    level: LevelKind
    scope: PermissionScope
    sharing: Sharing
    settings: ResolvedPermissions
}

/**
 * One part of the configuration: its merged value, or why its files can't be used.
 * Each consumer fails only on the parts it reads, so a broken `[tracker]` never stops permission checks.
 */
export type Section<T> = { ok: T } | { error: { message: string } }

/** The part's value; throws the resolver's message when the part's files can't be used. */
export function section<T>(part: Section<T>): T {
    if ("error" in part) throw new Error(part.error.message)
    return part.ok
}

/** The parts the pi package reads. */
const PARTS = [
    "tools",
    "mcp",
    "mcp_global",
    "web_search",
    "branches",
    "tracker",
    "forge",
    "policies",
    "instructions",
] as const satisfies readonly (keyof ResolvedSettings)[]

/** Every part whose files can't be used, with the resolver's message. */
export function settingsErrors(settings: ResolvedSettings): { part: (typeof PARTS)[number]; message: string }[] {
    return PARTS.flatMap((part) => {
        const value: Section<unknown> = settings[part]
        return "error" in value ? [{ part, message: value.error.message }] : []
    })
}

export interface ResolvedBranches {
    format?: "identifier-title" | "username/identifier-title" | "tracker"
    /** Personal: only `config.local.toml` may set it. */
    username?: string
}

export interface FileValue<T> {
    path: string
    value: T
}

/**
 * An integration role (`[tracker]` or `[forge]`) as the nearest table sets it: on, with its file and its configuration,
 * validated by piton (keys as the file has them); or off, because no level has the table, or because the nearest one
 * says `provider = "none"`.
 */
export type ResolvedRole =
    | { state: "on"; path: string; config: unknown }
    | { state: "off"; reason: "absent" }
    | { state: "off"; reason: "disabled"; path: string }

export interface ResolvedSettings {
    format: typeof RESOLVE_FORMAT
    workspace: string
    /** The checkout whose `.piton/` was read: a worker's source. */
    source: string
    /** The workspace's own checkout. */
    checkout: string
    global: string
    /** The user's home folder (from the user database), for `~` in path values. */
    home: string
    runtime: Runtime
    /** `[tools]`: the merged permission rules, and each permission table of the runtime that sets something, in merge order. */
    tools: Section<{ permissions: { merged: ResolvedPermissions; sources: ResolvedPermissionSource[] } }>
    /** Every level's servers. */
    mcp: Section<ResolvedMcp>
    /** The global tier's servers alone: what pi loads in a project it doesn't trust. */
    mcp_global: Section<ResolvedMcp>
    web_search: Section<{ api_key?: string }>
    branches: Section<ResolvedBranches>
    /** The tracker: the nearest `[tracker]`, whole. */
    tracker: Section<ResolvedRole>
    /** The forge: the nearest `[forge]`, whole. */
    forge: Section<ResolvedRole>
    /** Every `policies.md`, farthest first. */
    policies: Section<FileValue<string>[]>
    instructions: Section<{
        /** Rule folders, farthest first. */
        rules: string[]
        /** Skill folders piton adds to pi's own discovery, farthest first. */
        skills: string[]
    }>
    files: { path: string; level: LevelKind }[]
    watch: string[]
}

/** `piton config resolve --global`: the global tier's settings for the pi package, from `config.host.toml`. */
export interface GlobalSettings {
    format: typeof RESOLVE_FORMAT
    global: string
    /** `[debugging]`: switches for a development install of piton. */
    debugging: Section<{ session_analysis?: boolean }>
    watch: string[]
}

interface ResolveError {
    format: number
    error: { kind: "invalid" | "workspace"; message: string }
}

interface CacheEntry<T> {
    settings: T
    fingerprint: string
}

const cache = new Map<string, CacheEntry<ResolvedSettings>>()
let globalCache: CacheEntry<GlobalSettings> | undefined

/** The binary that resolves configuration. */
export function pitonBinary(env: NodeJS.ProcessEnv = process.env): string {
    const configured = env[BIN_ENV]
    return configured && configured.length > 0 ? configured : "piton"
}

/**
 * The resolved configuration for `cwd`, in the current runtime. Each part carries its own error (read it with
 * {@link section}); this throws only when nothing can be resolved: the binary is missing, the workspace is invalid, or
 * a level folder exists but can't be listed.
 */
export function resolveSettings(cwd: string, runtime: Runtime = requireRuntime()): ResolvedSettings {
    const workspace = path.resolve(cwd)
    const key = `${runtime}\0${workspace}`
    const cached = cache.get(key)
    if (cached && fingerprint(cached.settings.watch) === cached.fingerprint) return cached.settings
    const settings = runResolve(["config", "resolve", "--workspace", workspace, "--runtime", runtime])
    cache.set(key, { settings, fingerprint: fingerprint(settings.watch) })
    return settings
}

/** The global tier's settings for the pi package, which never depend on the current project. */
export function resolveGlobalSettings(): GlobalSettings {
    if (globalCache && fingerprint(globalCache.settings.watch) === globalCache.fingerprint) return globalCache.settings
    const settings = runPiton(["config", "resolve", "--global"], "piton config resolve") as GlobalSettings
    globalCache = { settings, fingerprint: fingerprint(settings.watch) }
    return settings
}

/** Forgets cached results, for tests that change files faster than the file system's timestamps. */
export function clearSettingsCache(): void {
    cache.clear()
    globalCache = undefined
}

/** The folder piton's tools read in a repository or parent folder; a repository's also holds workflow artifacts. */
export const PITON_DIR = ".piton"

/** A level's configuration file of one sharing: `config.toml`, or `config.local.toml` for this machine's values. */
export function configFileName(sharing: Sharing): string {
    switch (sharing) {
        case "shared":
            return "config.toml"
        case "local":
            return "config.local.toml"
    }
}

/** A repository's configuration file, relative to its root, for messages and Git paths. */
export function repositoryConfigFile(sharing: Sharing): string {
    return `${PITON_DIR}/${configFileName(sharing)}`
}

/** The source checkout's configuration file: where setup writes settings. */
export function projectConfigPath(settings: ResolvedSettings, sharing: Sharing): string {
    return path.join(settings.source, repositoryConfigFile(sharing))
}

/** The source checkout's `mcp.json`, or `mcp.local.json`: MCP servers stay JSON, the format other harnesses read. */
export function projectMcpPath(settings: ResolvedSettings, sharing: Sharing): string {
    switch (sharing) {
        case "shared":
            return path.join(settings.source, PITON_DIR, "mcp.json")
        case "local":
            return path.join(settings.source, PITON_DIR, "mcp.local.json")
    }
}

/**
 * The repository's configuration file with `changes` applied (dotted keys as in the file; `null` removes a key; an
 * object becomes a table), without writing it: `piton config set --dry-run`, which keeps the file's comments and layout
 * and refuses a result the resolver wouldn't accept. `content` is `null` when nothing changes. The caller writes the
 * file (a protected one goes through the daemon).
 */
export function plannedConfigEdit(
    cwd: string,
    changes: Record<string, unknown>,
    sharing: Sharing,
): { path: string; content: string | null } {
    const args = ["config", "set", JSON.stringify(changes), "--changes", "--dry-run", "--workspace", path.resolve(cwd)]
    if (sharing === "local") args.push("--local")
    const output = runPiton(args, "piton config set") as { path?: unknown; content?: unknown }
    if (typeof output.path !== "string" || (typeof output.content !== "string" && output.content !== null)) {
        throw new Error("piton config set printed an unexpected result")
    }
    return { path: output.path, content: output.content }
}

/**
 * `value` checked as `table` of a `config.toml` (keys as the file has them) by piton's own schema and checks, without a
 * file: `piton config validate`. Returns the value as the file would hold it; throws piton's message, which names the
 * key at fault. What setup checks before changing anything at a provider, so the rules have one implementation.
 */
export function validateTable(table: "tracker" | "forge", value: unknown): unknown {
    const output = runPiton(["config", "validate", table, JSON.stringify(value)], "piton config validate") as {
        ok?: unknown
    }
    if (!("ok" in output)) throw new Error("piton config validate printed an unexpected result")
    return output.ok
}

/** `mcp_server` -> `mcpServer`. */
function camel(name: string): string {
    return name.replace(/_([a-z0-9])/g, (_, c: string) => c.toUpperCase())
}

/** `mcpServer` -> `mcp_server`. */
function snake(name: string): string {
    return name.replace(/[A-Z]/g, (c) => `_${c.toLowerCase()}`)
}

/**
 * Whether the keys of the map at `at` (camelCase names from the root, `*` for a data key) are data, copied as they are:
 * label keys like `pi-workflows` are names, not schema.
 */
export type DataKeys = (at: readonly string[]) => boolean

/** A value from the file with its schema keys in camelCase. */
export function camelKeys(value: unknown, data: DataKeys = () => false): unknown {
    return translate(value, camel, data, [])
}

/** A value with its schema keys in snake_case, for the file. */
export function snakeKeys(value: unknown, data: DataKeys = () => false): unknown {
    return translate(value, snake, data, [])
}

function translate(value: unknown, rename: (name: string) => string, data: DataKeys, at: string[]): unknown {
    if (Array.isArray(value)) return value.map((item) => translate(item, rename, data, at))
    if (!value || typeof value !== "object") return value
    const keys = data(at)
    return Object.fromEntries(
        Object.entries(value).map(([key, inner]) => [
            keys ? key : rename(key),
            translate(inner, rename, data, [...at, keys ? "*" : camel(key)]),
        ]),
    )
}

function runResolve(args: string[]): ResolvedSettings {
    return runPiton(args, "piton config resolve") as ResolvedSettings
}

/** Runs the binary for one JSON result; throws its error (an `{error}` result, or a failure to run it). */
function runPiton(args: string[], command: string): unknown {
    const bin = pitonBinary()
    let stdout: string
    try {
        stdout = execFileSync(bin, args, {
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
        else throw new Error(`${command} failed: ${failure.stderr?.trim() || failure.message}`)
    }
    let parsed: unknown
    try {
        parsed = JSON.parse(stdout)
    } catch {
        throw new Error(`${command} printed invalid JSON`)
    }
    const output = parsed as { format?: unknown } & Partial<ResolveError>
    if (output.format !== RESOLVE_FORMAT) {
        throw new Error(
            `${command} printed format ${JSON.stringify(output.format)}; this pi package understands ${RESOLVE_FORMAT} (update piton and the pi package together)`,
        )
    }
    if (output.error) throw new Error(output.error.message)
    return output
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
