import { createHash } from "node:crypto"
import * as fs from "node:fs"
import * as path from "node:path"
import { fileURLToPath } from "node:url"

import { requireRuntime } from "../../../extensions/__lib/runtime.js"
import {
    configFileName,
    PITON_DIR,
    repositoryConfigFile,
    resolveGlobalSettings,
    resolveSettings,
    section,
} from "../../../extensions/__lib/settings.js"
import { type BranchFormat, BranchFormatSchema, PROJECT_LOCAL_ROOT } from "./domain.js"
import * as Git from "./git.js"

export { BranchFormatSchema }
export type { BranchFormat }

/** Every `.local` file in `.piton/` stays out of Git; setup adds this to `.git/info/exclude`. */
export const LOCAL_SETTINGS_EXCLUDE = `/${PITON_DIR}/*.local.*`
// The single repository rule that keeps every workflow entity and private evidence out of Git.
// No trailing slash, so it also matches a worker's symlink to its source checkout's directory.
export const PROJECT_LOCAL_IGNORE = `/${PROJECT_LOCAL_ROOT}`

export interface ProjectConfig {
    branches: { format: BranchFormat; username?: string }
}

/** Whether a fully shipped Deliverable ends with an in-chat analysis of the session for workflow problems. */
export type SessionAnalysis = "enabled" | "disabled"

/**
 * The workflow configuration for `cwd`, from piton's resolved chain (`[branches]` of `config.toml` in the global tier,
 * parent folders, and the source checkout's `.piton/`). Workflows need a branch format; formats starting with
 * `username` need `branches.username`, which is personal: only a `config.local.toml` sets it.
 */
export function loadProjectConfig(cwd: string): ProjectConfig {
    const branches = section(resolveSettings(cwd).branches)
    const format = branches.format
    if (!format) {
        throw new Error(
            `missing branches.format: set it in ${repositoryConfigFile("shared")} (project setup writes it) or a parent folder's`,
        )
    }
    if (!formatUsesUsername(format)) return { branches: { format } }
    const username = branches.username
    if (!username) {
        throw new Error(
            `invalid branch configuration: branches.username is required in ${repositoryConfigFile("local")} or the global ${configFileName("local")}`,
        )
    }
    return { branches: { format, username } }
}

/** The global tier's files that can hold a username, for setup's snapshot. */
export function globalConfigPaths(cwd: string): { shared: string; local: string } {
    const global = resolveSettings(cwd).global
    return { shared: path.join(global, configFileName("shared")), local: path.join(global, configFileName("local")) }
}

/**
 * How the pi package is installed: from a git checkout of piton (a development install, whose workflow problems are
 * filed in that checkout's tracker and analyzed after shipping), or as part of the app (problems go to piton's GitHub
 * issues through the user).
 */
export type Installation = { kind: "development"; checkout: string } | { kind: "app" }

/** The pi package's own folder: `pi/` of the checkout or the app bundle. */
const PACKAGE_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..")

let installation: { packageDir: string; value: Installation } | undefined

/** Where the pi package runs from, detected once: a git checkout whose root holds this package as `pi/` and the daemon. */
export function detectInstallation(packageDir: string = PACKAGE_DIR): Installation {
    if (installation?.packageDir === packageDir) return installation.value
    let value: Installation = { kind: "app" }
    try {
        const root = fs.realpathSync(Git.run(packageDir, ["rev-parse", "--show-toplevel"]))
        const isPiton =
            fs.realpathSync(packageDir) === path.join(root, "pi") &&
            fs.existsSync(path.join(root, "daemon", "Cargo.toml"))
        if (isPiton) value = { kind: "development", checkout: root }
    } catch {
        // Not in a git checkout: an app install.
    }
    installation = { packageDir, value }
    return value
}

let installationOverride: Installation | undefined

/** For tests: run as `value` (`undefined`: detect it). */
export function setInstallationForTests(value: Installation | undefined): void {
    installationOverride = value
}

function currentInstallation(): Installation {
    return installationOverride ?? detectInstallation()
}

/**
 * Whether a finished workflow ends with session analysis: only in a development install, where it's on unless the
 * global `[development] session_analysis = false` turns it off.
 */
export function loadSessionAnalysis(): SessionAnalysis {
    const install = currentInstallation()
    switch (install.kind) {
        case "app":
            return "disabled"
        case "development":
            return section(resolveGlobalSettings().development).session_analysis === false ? "disabled" : "enabled"
    }
}

/** Where users report piton problems in an app install: a new issue on GitHub, opened by the user. */
export const PITON_NEW_ISSUE_URL = "https://github.com/alex35mil/piton/issues/new"

/**
 * The development checkout: in a development install, the checkout the pi package runs from, whose tracker takes
 * workflow reports. An app install has none; reports go through the user ({@link PITON_NEW_ISSUE_URL}).
 */
export function developmentCheckout(): string {
    const install = currentInstallation()
    switch (install.kind) {
        case "development":
            return install.checkout
        case "app":
            throw new Error(
                `The workflow source is only available in a development install (the pi package running from a git checkout of piton). Report piton problems as a pre-filled GitHub issue for the user to open: ${PITON_NEW_ISSUE_URL}`,
            )
    }
}

/**
 * Project Policies: every `policies.md` in piton's configuration chain, farthest (global) first, each under a heading
 * naming its file; nearer parts take precedence where they conflict. Empty when there are none.
 */
export function readProjectPolicies(cwd: string): string {
    const policies = section(resolveSettings(cwd).policies).filter((policy) => policy.value.trim().length > 0)
    if (policies.length === 1) return policies[0]!.value
    return policies.map((policy) => `## From ${policy.path}\n\n${policy.value.trim()}`).join("\n\n")
}

/** The repository whose `.piton/` holds the workflow artifacts: the source checkout (a worker links to it). */
export function repositoryRoot(cwd: string): string {
    return resolveSettings(cwd).checkout
}

let validatedPersistenceInputs: string | undefined

export function prepareArtifactPersistence(cwd: string) {
    validatedPersistenceInputs = undefined
    const { excludePath, missing } = inspectArtifactPersistence(cwd)
    if (missing) {
        appendExcludeEntry(excludePath, missing.comment, missing.entry)
        assertArtifactPersistencePrepared(cwd)
    }
    // A configured project always has the folder, so worker tooling can link it before the first entity.
    // Created only after the ignore check passed, so Git already ignores it.
    const localRoot = path.join(repositoryRoot(cwd), PROJECT_LOCAL_ROOT)
    if (!fs.existsSync(localRoot) && !isSymbolicLink(localRoot)) fs.mkdirSync(localRoot, { recursive: true })
    return { state: "ready" as const, excludePath }
}

export function assertArtifactPersistencePrepared(cwd: string): ProjectConfig {
    const { config, missing } = inspectArtifactPersistence(cwd)
    if (missing) throw new Error(`${missing.label} are not prepared; call prepare_artifacts before continuing`)
    return config
}

/**
 * In a sandboxed worker, workflow artifacts must reach the source checkout through the
 * `.piton/.local` symlink. A missing or real directory would keep them only in the
 * disposable copy, so they'd be lost when the worker is deleted.
 */
export function assertSandboxArtifactsLinked(cwd: string): void {
    if (requireRuntime() !== "sandbox") return
    const localRoot = path.join(repositoryRoot(cwd), PROJECT_LOCAL_ROOT)
    if (isSymbolicLink(localRoot) && isDirectory(localRoot)) return
    throw new Error(
        `this worker's ${PROJECT_LOCAL_ROOT} isn't linked to its source checkout, so workflow artifacts would be lost when the worker is deleted; run project setup in the source checkout, then recreate the worker`,
    )
}

function isDirectory(file: string): boolean {
    try {
        return fs.statSync(file).isDirectory()
    } catch {
        return false
    }
}

function inspectArtifactPersistence(cwd: string) {
    const root = repositoryRoot(cwd)
    const config = loadProjectConfig(cwd)
    const before = persistenceInputs(root, config)
    const excludePath = before.excludePath
    if (before.key === validatedPersistenceInputs) return { config, excludePath, missing: undefined }
    validatedPersistenceInputs = undefined

    assertProjectLocalIgnored(root)
    assertLocalSettingsOutsideGit(root)
    assertProjectLocalOutsideGit(root)
    if (hasLocalSettings(root) && !hasExcludeEntry(excludePath, LOCAL_SETTINGS_EXCLUDE)) {
        return {
            config,
            excludePath,
            missing: {
                entry: LOCAL_SETTINGS_EXCLUDE,
                comment: "# local piton settings",
                label: "local settings",
            },
        }
    }
    // Cache only the inputs actually inspected, never inputs changed during the scan.
    if (persistenceInputs(root, config).key === before.key) validatedPersistenceInputs = before.key
    return { config, excludePath, missing: undefined }
}

/** `.piton/*.local.*` files present in the checkout. */
function localSettingsFiles(root: string): string[] {
    const dir = path.join(root, PITON_DIR)
    if (!fs.existsSync(dir)) return []
    return fs
        .readdirSync(dir)
        .filter((name) => /\.local\.[^.]+$/.test(name))
        .map((name) => `${PITON_DIR}/${name}`)
}

function hasLocalSettings(root: string): boolean {
    return localSettingsFiles(root).length > 0
}

function persistenceInputs(root: string, config: ProjectConfig) {
    const paths = Git.run(root, [
        "rev-parse",
        "--path-format=absolute",
        "--show-toplevel",
        "--absolute-git-dir",
        "--git-path",
        "index",
        "--git-path",
        "info/exclude",
    ]).split("\n")
    const [repository, gitDirectory, indexPath, excludePath] = paths
    if (paths.length !== 4) throw new Error("could not resolve artifact persistence Git paths")
    const commit = Git.tryRun(root, ["rev-parse", "--verify", "HEAD"])
    const head = commit
        ? { kind: "commit", commit }
        : { kind: "unborn", reference: Git.run(root, ["symbolic-ref", "--quiet", "HEAD"]) }
    const files = [
        indexPath,
        excludePath,
        path.join(root, ".gitignore"),
        ...localSettingsFiles(root).map((file) => path.join(root, file)),
    ]
    return {
        excludePath,
        key: JSON.stringify({
            repository,
            gitDirectory,
            indexPath,
            excludePath,
            head,
            config,
            files: files.map(fileFingerprint),
            // The ignore check depends on whether the local root is a symlink.
            linkedLocalRoot: isSymbolicLink(path.join(root, PROJECT_LOCAL_ROOT)),
        }),
    }
}

function fileFingerprint(file: string): string | null {
    try {
        return createHash("sha256").update(fs.readFileSync(file)).digest("hex")
    } catch (error) {
        if ((error as NodeJS.ErrnoException).code === "ENOENT") return null
        throw error
    }
}

function formatUsesUsername(format: BranchFormat): boolean {
    return format.startsWith("username")
}

// Ask Git, so the answer matches what Git will actually stage. Only the committed root .gitignore
// counts: .git/info/exclude and global excludes are local to one clone.
function assertProjectLocalIgnored(root: string): void {
    const linked = isSymbolicLink(path.join(root, PROJECT_LOCAL_ROOT))
    // A symlink is a file to Git; query a missing or real directory in directory form.
    const query = linked ? PROJECT_LOCAL_ROOT : `${PROJECT_LOCAL_ROOT}/`
    const match = Git.tryRun(root, ["check-ignore", "--verbose", "--no-index", "--", query])
    const source = match.split("\t")[0]?.split(":")[0]
    if (source === ".gitignore") return
    throw new Error(
        `workflow artifacts require ${PROJECT_LOCAL_IGNORE} (no trailing slash) in .gitignore; add it through project setup before continuing`,
    )
}

function isSymbolicLink(file: string): boolean {
    try {
        return fs.lstatSync(file).isSymbolicLink()
    } catch (error) {
        if ((error as NodeJS.ErrnoException).code === "ENOENT") return false
        throw error
    }
}

function assertLocalSettingsOutsideGit(root: string): void {
    const pattern = `${PITON_DIR}/*.local.*`
    const inGit = Git.run(root, ["ls-files", "--cached", "--", pattern])
    const staged = Git.run(root, ["diff", "--cached", "--name-only", "--", pattern])
    if (inGit || staged) {
        throw new Error(
            `${(inGit || staged).split("\n")[0]} must remain outside Git; remove it from the index before continuing`,
        )
    }
}

function assertProjectLocalOutsideGit(root: string): void {
    const inGit = Git.run(root, ["ls-files", "--cached", "--", PROJECT_LOCAL_ROOT])
    const staged = Git.run(root, ["diff", "--cached", "--name-only", "--", PROJECT_LOCAL_ROOT])
    if (inGit || staged) {
        throw new Error(
            `${PROJECT_LOCAL_ROOT}/ must remain outside Git; remove it from the index with git rm --cached before continuing`,
        )
    }
}

function hasExcludeEntry(excludePath: string, entry: string): boolean {
    if (!fs.existsSync(excludePath)) return false
    return fs
        .readFileSync(excludePath, "utf-8")
        .split(/\r?\n/)
        .some((line) => line === entry)
}

function appendExcludeEntry(excludePath: string, comment: string, entry: string): void {
    fs.mkdirSync(path.dirname(excludePath), { recursive: true })
    const existing = fs.existsSync(excludePath) ? fs.readFileSync(excludePath, "utf-8") : ""
    const separator = existing.length > 0 && !existing.endsWith("\n") ? "\n" : ""
    fs.appendFileSync(excludePath, `${separator}${comment}\n${entry}\n`)
}
