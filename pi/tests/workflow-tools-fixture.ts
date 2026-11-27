import { createEventBus, type ExtensionAPI, type ToolDefinition } from "@earendil-works/pi-coding-agent"
import context from "../extensions/context/index.ts"
import fetch from "../extensions/fetch/index.ts"
import permissions from "../extensions/permissions/index.ts"
import rules from "../extensions/rules/index.ts"
import webSearch from "../extensions/web-search/index.ts"
import initialize from "../workflows/extensions/init/index.ts"
import integrations from "../workflows/extensions/integrations/index.ts"
import setup from "../workflows/extensions/project-setup/index.ts"
import review from "../workflows/extensions/review/index.ts"
import pullRequests from "../workflows/extensions/pr/index.ts"
import delivery from "../workflows/extensions/deliverable/index.ts"

export const workflowFactories = [initialize, integrations, setup, review, pullRequests, delivery]
export const codingFactories = [context, fetch, permissions, rules, webSearch]

export function registeredWorkflowTools(): ToolDefinition[] {
    const tools: ToolDefinition[] = []
    const pi = {
        events: createEventBus(),
        on() {},
        registerCommand() {},
        registerTool: (definition: ToolDefinition) => tools.push(definition),
    } as unknown as ExtensionAPI
    for (const factory of workflowFactories) factory(pi)
    return tools
}

export const expectedWorkflowTools = [
    "preflight_epic",
    "init_github_epic",
    "init_linear_epic",
    "preflight_task",
    "init_task",
    "init_local_task",
    "route_task",
    "check_task_parent",
    "restore_task",
    "restore_epic",
    "restore_epic_plan_source",
    "begin_resource_creation",
    "record_resource_creation",
    "select_pull_request_resource",
    "resolve_resource_ids",
    "record_resource_ids",
    "invalidate_resource_ids",
    "find_pull_request",
    "prepare_tracker_publication",
    "prepare_tracker_publication_update",
    "record_tracker_publication",
    "recover_tracker_publication",
    "accept_epic_tasks",
    "plan_epic_task_issues",
    "record_epic_task_issue",
    "order_linear_epic_issues",
    "inspect_task_design",
    "record_task_design",
    "render_provider_body",
    "verify_artifact_projection",
    "inspect_session_analysis",
    "inspect_integrations",
    "inspect_queue_intake",
    "inspect_adoption",
    "initialize_integration",
    "resume_integration",
    "prepare_integration_projection",
    "prepare_integration_pull_request",
    "prepare_integration_shipping",
    "prepare_integration_merge",
    "prepare_tracker_mutation",
    "prepare_label_mutation",
    "inspect_gate",
    "prepare_github_phase_mutation",
    "prepare_linear_phase_mutation",
    "inspect_incoming_pr_context",
    "plan_incoming_pr_merge",
    "plan_incoming_pr_intake",
    "plan_incoming_pr_review",
    "plan_incoming_pr_shipping",
    "plan_incoming_pr_cancellation",
    "inspect_project_setup",
    "prepare_artifacts",
    "configure_mcp",
    "configure_project",
    "inspect_permissions",
    "configure_permissions",
    "inspect_linear_workspace",
    "preview_linear_status_setup",
    "apply_linear_status_setup",
    "render_incoming_pr_intake",
    "inspect_github_repository_setup",
    "inspect_github_project_setup",
    "inspect_github_project",
    "preview_github_project_setup",
    "apply_github_project_setup",
    "inspect_label_setup",
    "preview_label_setup",
    "apply_label_setup",
    "review_pull_request",
    "record_pull_request_review_response",
    "review",
    "review_epic",
    "review_task",
    "record_review_response",
    "prepare_incoming_pr_checkout",
    "checkout_incoming_pr",
    "inspect_incoming_pr_checkout",
    "begin_incoming_pr_fixes",
    "release_incoming_pr_configuration",
    "hold_incoming_pr_configuration",
    "observe_incoming_pr_target",
    "publish_incoming_pr_rebase",
    "publish_incoming_pr_fixes",
    "rebase_incoming_pr",
    "continue_incoming_pr_rebase",
    "commit_incoming_pr_fixes",
    "restore_incoming_pr_checkout",
    "cleanup_incoming_pr_checkout",
    "cleanup_delivery_branch",
    "prepare_merge_resolution_comment",
    "begin_user_review",
    "end_user_review",
] as const
