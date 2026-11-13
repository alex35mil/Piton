import * as fs from "node:fs"
import * as path from "node:path"

import * as Git from "../__lib/git.js"
import { LOCAL_SETTINGS_EXCLUDE } from "../__lib/project-config.js"

/**
 * Before setup writes a `.piton/*.local.*` file: refuse one that's in Git, and make sure the shared
 * `/.piton/*.local.*` exclusion is in `.git/info/exclude`.
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

    const excludePath = path.resolve(root, Git.run(root, ["rev-parse", "--git-path", "info/exclude"]))
    const existing = fs.existsSync(excludePath) ? fs.readFileSync(excludePath, "utf-8") : ""
    if (existing.split(/\r?\n/).includes(LOCAL_SETTINGS_EXCLUDE)) return { excludePath, changed: false }

    fs.mkdirSync(path.dirname(excludePath), { recursive: true })
    fs.appendFileSync(
        excludePath,
        `${existing.length > 0 && !existing.endsWith("\n") ? "\n" : ""}${LOCAL_SETTINGS_EXCLUDE}\n`,
    )
    return { excludePath, changed: true }
}
