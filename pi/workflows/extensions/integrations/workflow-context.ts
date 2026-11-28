import type { ExtensionAPI } from "@earendil-works/pi-coding-agent"

import { loadProjectPolicies, PROJECT_POLICY_PRECEDENCE } from "../__lib/project-config.js"
import { integrationPresence } from "./config.js"

export const WORKFLOW_CONTEXT_SECTION = "workflows"
export const PROJECT_POLICIES_SECTION = "project_policies"

export const WORKFLOW_CONTEXT =
    "This repository uses workflows. Every repository change happens inside a workflow entity: a Task (`/task`), standalone or planned in an Epic. Before implementing anything, start or resume one, and follow its lifecycle through planning, review, delivery, and shipping. Never edit, commit, or ship repository changes outside it unless the user explicitly approves. Questions, design discussion, and standalone tools such as `review` need no entity."

type Sections = Record<string, string>

// In a workflow-configured repository (a [tracker] or [forge] anywhere in piton's configuration chain), every agent run
// gets the workflow rule and the Project Policies in its system prompt. pi puts repository context files such as
// AGENTS.md there too; policies read only as tool output would lose to them. Checked per run, so adding or removing
// the configuration or the policies takes effect on the next run. When either can't be read, its section stays, with
// the reason, so a broken file never silently drops it.
export function registerWorkflowContext(pi: ExtensionAPI): void {
    pi.on("before_agent_start", async (event, ctx) => {
        const sections: Sections = event.systemPromptOptions.sections
        const presence = integrationPresence(ctx.cwd)
        switch (presence.kind) {
            case "configured":
                sections[WORKFLOW_CONTEXT_SECTION] = WORKFLOW_CONTEXT
                setProjectPolicies(sections, ctx.cwd)
                break
            case "unreadable":
                sections[WORKFLOW_CONTEXT_SECTION] =
                    `${WORKFLOW_CONTEXT}\n\npiton's workflow configuration can't be read; tell the user before starting work: ${presence.error}`
                setProjectPolicies(sections, ctx.cwd)
                break
            case "absent":
                delete sections[WORKFLOW_CONTEXT_SECTION]
                delete sections[PROJECT_POLICIES_SECTION]
                break
        }
    })
}

function setProjectPolicies(sections: Sections, cwd: string): void {
    const policies = loadProjectPolicies(cwd)
    switch (policies.kind) {
        case "present":
            sections[PROJECT_POLICIES_SECTION] = `${PROJECT_POLICY_PRECEDENCE}\n\n${policies.text.trim()}`
            break
        case "unreadable":
            sections[PROJECT_POLICIES_SECTION] =
                `${PROJECT_POLICY_PRECEDENCE}\n\nProject Policies can't be read; tell the user before starting work: ${policies.error}`
            break
        case "absent":
            delete sections[PROJECT_POLICIES_SECTION]
            break
    }
}
