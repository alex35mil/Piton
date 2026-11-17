import { execFileSync } from "node:child_process"

import { BIN_ENV, pitonBinary } from "../../../extensions/__lib/settings.js"

/** The `piton tools --json` format this package understands. */
const TOOLS_FORMAT = 1

interface Registry {
    format: number
    tools: { tool: string; category: string; pathspecs: string[] }[]
}

let cached: { bin: string; pathspecs: string[] } | undefined

/**
 * Pathspecs of every repository file a known tool (environment manager, agent, editor) reads when a shell, editor, or
 * session enters the checkout. The CLI's registry owns the list (`cli/src/project/tools.rs`); Git matches it.
 */
export function heldPathspecs(): string[] {
    const bin = pitonBinary()
    if (cached?.bin === bin) return cached.pathspecs
    let output: string
    try {
        output = execFileSync(bin, ["tools"], { encoding: "utf-8", stdio: ["ignore", "pipe", "pipe"] })
    } catch (error) {
        const failure = error as NodeJS.ErrnoException & { stderr?: string }
        if (failure.code === "ENOENT")
            throw new Error(`piton's tool registry can't be read: ${JSON.stringify(bin)} wasn't found (set ${BIN_ENV})`)
        throw new Error(`piton tools failed: ${failure.stderr?.trim() || failure.message}`)
    }
    const registry = JSON.parse(output) as Registry
    if (registry.format !== TOOLS_FORMAT)
        throw new Error(
            `piton tools printed format ${JSON.stringify(registry.format)}; this pi package understands ${TOOLS_FORMAT} (update piton and the pi package together)`,
        )
    const pathspecs = registry.tools.flatMap((tool) => tool.pathspecs)
    if (!pathspecs.length || pathspecs.some((spec) => !spec.startsWith(":(")))
        throw new Error("piton tools printed an invalid registry")
    cached = { bin, pathspecs }
    return pathspecs
}
