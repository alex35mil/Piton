import { createHash } from "node:crypto"
import * as fs from "node:fs"
import * as path from "node:path"
import { fileURLToPath } from "node:url"

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
            `invalid branch configuration: branches.username is required in ${repositoryConfigFile("local")} or the global ${configFileName("shared")} or ${configFileName("local")}`,
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

/** Where the pi package runs from, detected once: a git checkout whose root holds this package as `pi/` and the `piton` crate as `cli/`. */
export function detectInstallation(packageDir: string = PACKAGE_DIR): Installation {
    if (installation?.packageDir === packageDir) return installation.value
    let value: Installation = { kind: "app" }
    try {
        // Ask Git where the package sits instead of comparing paths: on a case-insensitive file system the package
        // path can differ in case from the checkout's real name, which `fs.realpathSync` doesn't canonicalize.
        const prefix = Git.run(packageDir, ["rev-parse", "--show-prefix"])
        const root = Git.run(packageDir, ["rev-parse", "--show-toplevel"])
        const isPiton = prefix === "pi/" && fs.existsSync(path.join(root, "cli", "Cargo.toml"))
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
 * host file's `[development] session_analysis = false` turns it off.
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
export const PITON_NEW_ISSUE_URL = "https://github.com/alex35mil/Piton/issues/new"

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
                `The workflow source is only available in a development install (the pi package running from a git checkout of Piton). Report Piton problems as a pre-filled GitHub issue for the user to open: ${PITON_NEW_ISSUE_URL}`,
            )
    }
}

/**
 * Where Project Policies rank for workflow behavior, for the session prompt and the reviewers alike. pi puts
 * repository context files such as `AGENTS.md` in the system prompt, so the order has to name them.
 */
export const PROJECT_POLICY_PRECEDENCE =
    "For workflow behavior, the user's explicit decisions come first, then Project Policies (`policies.md`), then repository context files (`AGENTS.md` and equivalents), then the reusable workflow skills and references. When a Project Policy conflicts with a context file, the policy wins: follow it, and never propose editing the context file to resolve the conflict."

/** The resolved Project Policies: their combined text, none, or why they can't be read. */
export type ProjectPolicies =
    | { kind: "present"; text: string }
    | { kind: "absent" }
    | { kind: "unreadable"; error: string }

/**
 * Project Policies: every `policies.md` in piton's configuration chain, farthest (global) first, each under a heading
 * naming its file when there are several; nearer parts take precedence where they conflict.
 */
export function loadProjectPolicies(cwd: string): ProjectPolicies {
    let policies: { path: string; value: string }[]
    try {
        policies = section(resolveSettings(cwd).policies).filter((policy) => policy.value.trim().length > 0)
    } catch (error) {
        return { kind: "unreadable", error: error instanceof Error ? error.message : String(error) }
    }
    if (policies.length === 0) return { kind: "absent" }
    if (policies.length === 1) return { kind: "present", text: policies[0]!.value }
    return {
        kind: "present",
        text: policies.map((policy) => `## From ${policy.path}\n\n${policy.value.trim()}`).join("\n\n"),
    }
}

/** Project Policies as text for a reviewer: empty when there are none; throws when they can't be read. */
export function readProjectPolicies(cwd: string): string {
    const policies = loadProjectPolicies(cwd)
    switch (policies.kind) {
        case "present":
            return policies.text
        case "absent":
            return ""
        case "unreadable":
            throw new Error(policies.error)
    }
}

/** The repository whose `.piton/` holds the workflow artifacts: the source checkout (a worker links to it). */
export function repositoryRoot(cwd: string): string {
    return resolveSettings(cwd).checkout
}

/**
 * What the workspace's checkout is, as piton's resolver decides it (`agent.source` in the local Git config plus the
 * `<repo>--<name>` layout): an original source checkout, or a disposable worker cloned from `source`. Independent of
 * the runtime: a source checkout may run sandboxed, and a worker may run on the host.
 */
export type CheckoutRelation =
    | { kind: "source"; checkout: string }
    | { kind: "worker"; checkout: string; source: string }

export function checkoutRelation(cwd: string): CheckoutRelation {
    const { checkout, source } = resolveSettings(cwd)
    return checkout === source ? { kind: "source", checkout } : { kind: "worker", checkout, source }
}

let validatedPersistenceInputs: string | undefined

export function prepareArtifactPersistence(cwd: string) {
    validatedPersistenceInputs = undefined
    const { excludePath, missing } = inspectArtifactPersistence(cwd)
    if (missing) {
        appendExcludeEntry(excludePath, missing.comment, missing.entry)
        assertArtifactPersistencePrepared(cwd)
    }
    const relation = checkoutRelation(cwd)
    switch (relation.kind) {
        case "source": {
            // A configured project always has the folder, so worker tooling can link it before the first entity.
            // Created only after the ignore check passed, so Git already ignores it.
            const localRoot = path.join(relation.checkout, PROJECT_LOCAL_ROOT)
            if (!fs.existsSync(localRoot) && !isSymbolicLink(localRoot)) fs.mkdirSync(localRoot, { recursive: true })
            break
        }
        case "worker":
            // A worker never gets a folder of its own; its artifacts live in the source's, through the link.
            assertWorkerLinked(relation)
            break
    }
    return { state: "ready" as const, excludePath }
}

export function assertArtifactPersistencePrepared(cwd: string): ProjectConfig {
    const { config, missing } = inspectArtifactPersistence(cwd)
    if (missing) throw new Error(`${missing.label} are not prepared; call prepare_artifacts before continuing`)
    return config
}

/**
 * In a worker, in any runtime, workflow artifacts must reach the source checkout through a `.piton/.local` symlink to
 * the source's folder. A missing or real directory, or a link elsewhere, would keep them only in the disposable copy,
 * so they'd be lost when the worker is deleted. A source checkout keeps its own folder, sandboxed or not.
 */
export function assertWorkerArtifactsLinked(cwd: string): void {
    const relation = checkoutRelation(cwd)
    switch (relation.kind) {
        case "source":
            return
        case "worker":
            assertWorkerLinked(relation)
            return
    }
}

function assertWorkerLinked(worker: { checkout: string; source: string }): void {
    const localRoot = path.join(worker.checkout, PROJECT_LOCAL_ROOT)
    const sourceRoot = path.join(worker.source, PROJECT_LOCAL_ROOT)
    const target = isSymbolicLink(localRoot) ? realDirectory(localRoot) : undefined
    if (target !== undefined && target === realDirectory(sourceRoot)) return
    throw new Error(
        `this worker's ${PROJECT_LOCAL_ROOT} isn't linked to its source checkout's (${sourceRoot}), so workflow artifacts would be lost when the worker is deleted; run project setup in ${worker.source}, then recreate the worker`,
    )
}

/** The directory's real path, or undefined when it's missing or not a directory. */
function realDirectory(file: string): string | undefined {
    try {
        return fs.statSync(file).isDirectory() ? fs.realpathSync(file) : undefined
    } catch {
        return undefined
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
        path.join(root, PITON_DIR, ".gitignore"),
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

// Ask Git, so the answer matches what Git will actually stage. Any repository rule counts: a committed setup's
// .gitignore or a private setup's .git/info/exclude. The global excludes file doesn't: it's outside the repository.
function assertProjectLocalIgnored(root: string): void {
    const linked = isSymbolicLink(path.join(root, PROJECT_LOCAL_ROOT))
    // A symlink is a file to Git; query a missing or real directory in directory form.
    const query = linked ? PROJECT_LOCAL_ROOT : `${PROJECT_LOCAL_ROOT}/`
    if (Git.succeeds(root, ["-c", "core.excludesFile=", "check-ignore", "--quiet", "--no-index", "--", query])) return
    throw new Error(
        `workflow artifacts require ${PROJECT_LOCAL_IGNORE} (no trailing slash) in .gitignore, or /${PITON_DIR} in .git/info/exclude for a private setup; add it through project setup before continuing`,
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
