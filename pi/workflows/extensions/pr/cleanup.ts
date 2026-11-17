import type { McpToolCatalog } from "../../../extensions/__lib/mcp.js"
import type { IncomingRevision } from "../integrations/forge/github/incoming-pr-context.js"
import { incomingPullRequestContext } from "../integrations/forge/github/incoming-pr-context.js"
import { incomingProjectContext } from "../integrations/forge/github/incoming-pr-project.js"
import { planGitHubMerge } from "../integrations/forge/github/merge.js"
import type { ProviderEvidence } from "../integrations/resource-id-operations.js"
import { requireAddress, type CheckoutState, type IncomingCleanupCompletion } from "./state.js"

/**
 * Checks the confirmed merge, its exact head and timestamp, and Project Done (or an unconfigured board) before cleanup
 * restores the checkout. Cleanup never deletes a checkout: the review leaves it as the maintainer started it.
 */
export function requireCompletion(
    root: string,
    current: IncomingRevision,
    state: CheckoutState | null,
    evidence: ProviderEvidence,
    done: IncomingCleanupCompletion,
    tools: McpToolCatalog,
) {
    if (state) requireAddress(state, current)
    const expected = state?.revision ?? current
    const proof = planGitHubMerge(
        {
            mcpServer: expected.mcpServer,
            repository: expected.repository,
            number: expected.number,
            base: expected.base.ref,
            head: { repository: expected.head.repository, ref: expected.head.ref },
            expectedHeadSha: state?.baselineSha ?? current.head.sha,
        },
        evidence,
        tools,
    )
    if (proof.state !== "merged")
        throw new Error("Cleanup requires exact matching confirmed merge/head/timestamp evidence")
    const context = incomingPullRequestContext(root, current.url, tools)
    switch (context.project.state) {
        case "enabled": {
            if (done.kind !== "project") throw new Error("Configured Project Done must be confirmed before cleanup")
            const planned = incomingProjectContext(
                root,
                {
                    operation: "incomingPullRequestProject",
                    reference: current.url,
                    evidence,
                    fieldEvidence: done.fieldEvidence,
                    membership: { state: "present", evidence: done.itemEvidence },
                    action: "shipping",
                },
                tools,
            )
            if (
                planned.projectAction.state !== "unchanged" ||
                planned.projectAction.status !== context.project.status.values.done
            )
                throw new Error("Project Done is unconfirmed; preserve checkout and resume the unfinished board step")
            break
        }
        case "disabled":
            if (done.kind !== "forgeOnly")
                throw new Error("Use explicit forge-only completion only with no configured GitHub Project")
            break
    }
    return proof.merge
}
