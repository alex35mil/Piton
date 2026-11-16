import * as path from "node:path"

import { Type } from "typebox"

import { compile, parse } from "../../../extensions/__lib/schema.js"
import { PITON_DIR } from "../../../extensions/__lib/settings.js"
import * as Git from "../__lib/git.js"
import {
    appendIgnoreRule,
    localExcludePath,
    PRIVATE_PROJECT_EXCLUDE,
    PROJECT_LOCAL_IGNORE,
    projectLocalIgnore,
    repositoryIgnore,
} from "../__lib/project-config.js"

/**
 * How the repository keeps Piton files out of Git, derived from Git each time:
 * - `unset`: `.piton/.local` isn't ignored, so workflows can't run yet;
 * - `shared`: `.piton/.local` is ignored and the rest of `.piton/` can be committed;
 * - `private`: all of `.piton/` is ignored, so nothing Piton writes can be committed (Git never re-includes a path
 *   below an ignored directory).
 */
export type ArtifactSharing =
    | { kind: "unset" }
    | { kind: "shared"; source: string }
    | { kind: "private"; source: string }

export function inspectArtifactSharing(root: string): ArtifactSharing {
    const project = repositoryIgnore(root, `${PITON_DIR}/`)
    switch (project.kind) {
        case "ignored":
            return { kind: "private", source: project.source }
        case "unignored":
            break
    }
    const local = projectLocalIgnore(root)
    switch (local.kind) {
        case "ignored":
            return { kind: "shared", source: local.source }
        case "unignored":
            return { kind: "unset" }
    }
}

export const ArtifactIgnoreSchema = Type.Object(
    { sharing: Type.Enum(["shared", "private"], { type: "string" }) },
    { additionalProperties: false },
)
const ArtifactIgnoreValidator = compile(ArtifactIgnoreSchema)

export type ArtifactIgnoreResult = {
    /** The repository-relative rule file appended to, or `null` when the rule was already in effect. */
    changed: { file: string; rule: string } | null
    sharing: ArtifactSharing
}

/**
 * Applies the approved sharing choice by appending one ignore rule, never changing the index:
 * - `shared`: `/.piton/.local` in the root `.gitignore`, unless `.piton/.local` is already ignored. Refused under a
 *   private setup, whose `/.piton/` rule the user removes by hand first.
 * - `private`: `/.piton/` in `.git/info/exclude`, unless `.piton/` is already ignored. Refused while any `.piton/`
 *   path is in Git or staged, since ignoring doesn't untrack it.
 * Refuses when a higher-precedence rule (such as a `.gitignore` negation) keeps the appended rule from taking effect.
 */
export function configureArtifactIgnore(root: string, input: unknown): ArtifactIgnoreResult {
    const { sharing } = parse(ArtifactIgnoreValidator, input)
    const current = inspectArtifactSharing(root)
    switch (sharing) {
        case "shared":
            return configureShared(root, current)
        case "private":
            return configurePrivate(root, current)
    }
}

function configureShared(root: string, current: ArtifactSharing): ArtifactIgnoreResult {
    switch (current.kind) {
        case "shared":
            return { changed: null, sharing: current }
        case "private":
            throw new Error(
                `${PITON_DIR}/ is ignored by ${current.source}, a private setup; remove that rule by hand before switching to a shared setup`,
            )
        case "unset":
            break
    }
    appendIgnoreRule(path.join(root, ".gitignore"), PROJECT_LOCAL_IGNORE)
    return applied(root, ".gitignore", PROJECT_LOCAL_IGNORE, "shared")
}

function configurePrivate(root: string, current: ArtifactSharing): ArtifactIgnoreResult {
    switch (current.kind) {
        case "private":
            return { changed: null, sharing: current }
        case "shared":
        case "unset":
            break
    }
    const inGit = new Set([
        ...lines(Git.run(root, ["ls-files", "--cached", "--", PITON_DIR])),
        ...lines(Git.run(root, ["diff", "--cached", "--name-only", "--", PITON_DIR])),
    ])
    if (inGit.size > 0) {
        throw new Error(
            `a private setup keeps every ${PITON_DIR}/ file outside Git, but these are in Git or staged: ${[...inGit].sort().join(", ")}`,
        )
    }
    const exclude = localExcludePath(root)
    appendIgnoreRule(exclude, PRIVATE_PROJECT_EXCLUDE, "# private piton setup")
    return applied(root, path.relative(root, exclude), PRIVATE_PROJECT_EXCLUDE, "private")
}

function applied(
    root: string,
    file: string,
    rule: string,
    expected: Exclude<ArtifactSharing["kind"], "unset">,
): ArtifactIgnoreResult {
    const sharing = inspectArtifactSharing(root)
    if (sharing.kind !== expected) {
        throw new Error(
            `appended ${rule} to ${file}, but another repository rule (such as a .gitignore negation) keeps it from taking effect; remove that rule and run setup again`,
        )
    }
    return { changed: { file, rule }, sharing }
}

function lines(value: string): string[] {
    return value ? value.split(/\r?\n/).filter(Boolean) : []
}
