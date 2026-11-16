import * as Git from "../__lib/git.js"
import {
    appendIgnoreRule,
    LOCAL_SETTINGS_EXCLUDE,
    localExcludePath,
    repositoryIgnore,
} from "../__lib/project-config.js"

/**
 * Before setup writes a `.piton/*.local.*` file: refuse one that's in Git, and make sure the repository's rules ignore
 * it, appending the shared `/.piton/*.local.*` exclusion to `.git/info/exclude` only when no rule does already (a
 * private setup's `/.piton/` covers it).
 */
export function ensureLocalSettingsExclusion(
    root: string,
    relativePath: string,
): { excludePath: string; changed: boolean } {
    const inGit = Git.run(root, ["ls-files", "--cached", "--", relativePath])
    const staged = Git.run(root, ["diff", "--cached", "--name-only", "--", relativePath])
    if (inGit || staged) {
        throw new Error(`${relativePath} is in Git or staged; local setup cannot overwrite it`)
    }

    const excludePath = localExcludePath(root)
    const match = repositoryIgnore(root, relativePath)
    switch (match.kind) {
        case "ignored":
            return { excludePath, changed: false }
        case "unignored":
            appendIgnoreRule(excludePath, LOCAL_SETTINGS_EXCLUDE)
            return { excludePath, changed: true }
    }
}
