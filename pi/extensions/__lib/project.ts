/**
 * The repository a working folder is in: its git root, or the folder itself outside a repository. Workflow artifacts
 * (`.piton/.local`) are addressed from here; configuration comes from `piton config resolve` (settings.ts).
 */

import * as cp from "node:child_process"
import * as path from "node:path"

let rootDirCache: { cwd: string; root: string } | undefined

export function resolveRootDir(cwd: string): string {
    const resolved = path.resolve(cwd)
    if (rootDirCache?.cwd === resolved) return rootDirCache.root

    let root: string
    try {
        const output = cp
            .execFileSync("git", ["rev-parse", "--show-toplevel"], {
                cwd,
                encoding: "utf-8",
                stdio: ["ignore", "pipe", "ignore"],
            })
            .trim()
        root = output ? path.resolve(output) : resolved
    } catch {
        root = resolved
    }

    rootDirCache = { cwd: resolved, root }
    return root
}
