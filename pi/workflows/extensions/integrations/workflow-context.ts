import type { ExtensionAPI } from "@earendil-works/pi-coding-agent"

import { integrationPresence } from "./config.js"

export const WORKFLOW_CONTEXT_SECTION = "workflows"

export const WORKFLOW_CONTEXT =
    "This repository uses workflows. Every repository change happens inside a workflow entity: a Gig (`/gig`) for standalone work, or a Task (`/task`) for work planned in an Epic. Before implementing anything, start or resume one, and follow its lifecycle through planning, review, delivery, and shipping. Never edit, commit, or ship repository changes outside it unless the user explicitly approves. Questions, design discussion, and standalone tools such as `review` need no entity."

// In a workflow-configured repository (a [tracker] or [forge] anywhere in piton's configuration chain), every agent run
// gets the workflow rule in its system prompt. Checked per run, so adding or removing the configuration takes effect
// on the next run. When the configuration can't be read, the rule stays, with the reason, so a broken file never
// silently drops it.
export function registerWorkflowContext(pi: ExtensionAPI): void {
    pi.on("before_agent_start", async (event, ctx) => {
        const presence = integrationPresence(ctx.cwd)
        switch (presence.kind) {
            case "configured":
                event.systemPromptOptions.sections[WORKFLOW_CONTEXT_SECTION] = WORKFLOW_CONTEXT
                break
            case "unreadable":
                event.systemPromptOptions.sections[WORKFLOW_CONTEXT_SECTION] =
                    `${WORKFLOW_CONTEXT}\n\npiton's workflow configuration can't be read; tell the user before starting work: ${presence.error}`
                break
            case "absent":
                delete event.systemPromptOptions.sections[WORKFLOW_CONTEXT_SECTION]
                break
        }
    })
}
