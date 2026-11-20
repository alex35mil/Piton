import type { ExtensionAPI } from "@earendil-works/pi-coding-agent"

import { registerIncomingCheckout } from "./checkout.js"
import { observeMcpTools } from "../../../extensions/__lib/mcp.js"

/**
 * `/pr <reference> [context]`: the first word is the PR (`#NN` or its URL); anything after it, such as the title the
 * Neovim picker pre-fills, is maintainer-supplied context, never part of the reference.
 */
export function parsePrArguments(args: string): { reference: string; context: string } | null {
    const trimmed = args.trim()
    if (!trimmed) return null
    const [reference, ...rest] = trimmed.split(/\s+/)
    return { reference, context: rest.join(" ") }
}

export default function (pi: ExtensionAPI): void {
    registerIncomingCheckout(pi, observeMcpTools(pi))
    pi.registerCommand("pr", {
        description: "Select an incoming GitHub PR for Project intake and private review",
        handler: async (args, ctx) => {
            const parsed = parsePrArguments(args)
            if (!parsed) {
                ctx.ui.notify("Usage: /pr <GitHub PR URL or #NN> [title or notes]", "error")
                return
            }
            pi.sendUserMessage(
                [
                    "Incoming pull request:",
                    "",
                    parsed.reference,
                    ...(parsed.context ? ["", `Maintainer context: ${parsed.context}`] : []),
                    "",
                    "Use the pr skill. Select the PR. When its context has a Project, inspect membership and ask for a missing intake choice; when the Project is disabled, skip intake without asking about a Project. A reference alone is not review-start or shipping approval; this does not authorize publication, fixes, pushing, closing, or merging.",
                ].join("\n"),
                { deliverAs: "followUp" },
            )
        },
    })
}
