import * as fs from "node:fs"

import { PROJECT_POLICY_PRECEDENCE } from "../__lib/project-config.js"

export const MAX_EMBEDDED_FILE_BYTES = 80 * 1024
export const REVIEW_AUTHORITY_INSTRUCTIONS = [
    "The shared permission extension enforces the same project and global permission settings as the parent agent.",
    PROJECT_POLICY_PRECEDENCE,
] as const

// Entity reviewers launch with the read-only `git_read` tool (see `git-read.ts`).
export const REVIEW_GIT_INSTRUCTIONS =
    "Use the git_read tool for history, diffs, blame, and merges in the repository; it runs read-only Git subcommands without a shell."

export const REVIEW_DOCUMENT_INSTRUCTIONS =
    "For plan, result, and report documents, a statement of behavior or verification that the supporting sections, the diff, or the evidence don't support is a blocking finding, and so is an ## Overview that hides a material limitation. Wording, style, and consistency between sections of one document are out of scope: do not report them, even as notes."

// The reviewer's copy of the credible-case bar owned by references/planning.md#credible-cases.
export const REVIEW_CREDIBLE_CASE_INSTRUCTIONS = [
    "Edge cases, races, and failure modes: handling is justified only by a credible case. State it in one line: the real trigger in supported use, the harm, its likelihood, and why handling costs less than that harm. Base each part on the code path or documented behavior; async code is not proof of a race, and overlapping operations are not proof of harmful ordering.",
    "A finding that asks for new handling without that line is invalid: omit it, and let it not affect signoff.",
    "Handling in the change or plan that has no credible case, or costs more than the harm it prevents, is a blocking finding; its fix is removing the handling.",
    "Security, data loss, and corrupted persisted data need only a reachable trigger; low likelihood does not dismiss them, including a case listed under ## Unhandled cases.",
].join(" ")

export async function readOptional(filePath: string): Promise<string> {
    try {
        return await fs.promises.readFile(filePath, "utf-8")
    } catch {
        return ""
    }
}

export function fence(text: string, language: string): string {
    return `\`\`\`${language}\n${text.replace(/```/g, "`\u200b``")}\n\`\`\``
}

export function truncate(text: string, maxBytes: number): string {
    if (Buffer.byteLength(text, "utf-8") <= maxBytes) return text
    let out = text.slice(0, maxBytes)
    while (Buffer.byteLength(out, "utf-8") > maxBytes) out = out.slice(0, -1)
    return `${out}\n\n[truncated]`
}
