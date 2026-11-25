---
name: gig
description: "Use for standalone Deliverable work under <repo>/.piton/.local/gigs/. Initializes or resumes a Gig, then follows the shared Deliverable lifecycle."
allowed-tools:
    - "inspect_session_analysis"
    - "preflight_gig"
    - "init_github_gig"
    - "init_linear_gig"
    - "init_local_gig"
    - "review_gig"
    - "record_review_response"
    - "prepare_artifacts"
    - "render_provider_body"
    - "prepare_tracker_publication"
    - "prepare_tracker_publication_update"
    - "record_tracker_publication"
    - "recover_tracker_publication"
    - "verify_artifact_projection"
    - "inspect_integrations"
    - "inspect_adoption"
    - "initialize_integration"
    - "resume_integration"
    - "prepare_integration_projection"
    - "prepare_integration_pull_request"
    - "prepare_integration_shipping"
    - "prepare_integration_merge"
    - "prepare_tracker_mutation"
    - "prepare_label_mutation"
    - "inspect_gate"
    - "prepare_github_phase_mutation"
    - "prepare_linear_phase_mutation"
    - "begin_resource_creation"
    - "record_resource_creation"
    - "resolve_resource_ids"
    - "record_resource_ids"
    - "invalidate_resource_ids"
    - "select_pull_request_resource"
    - "find_pull_request"
    - "cleanup_delivery_branch"
    - "prepare_merge_resolution_comment"
    - "begin_user_review"
    - "end_user_review"
    - "read(.piton/*)"
    - "write(.piton/*)"
    - "edit(.piton/*)"
---

# Gig

A Gig is a standalone Deliverable with no parent Epic.

## Read first

- [Core rules](../../references/core.md)
- [Deliverable lifecycle](../../references/deliverable.md)
- [Artifacts](../../references/artifacts.md)

The Deliverable lifecycle owns planning, root cause, review, implementation, verification, and integration. This file owns only Gig identification and initialization.

## Resume

Follow the Deliverable resume steps for a Gig under `.piton/.local/gigs/`. Verify `entity: "gig"` and that it isn't inside an Epic, then continue the lifecycle.

## Initialize

Follow the intake in [artifacts](../../references/artifacts.md#initialization-inputs), then [initialize from the tracker object](../../references/integrations/shared.md#initialize-from-the-tracker-object):

1. **New work:** call `preflight_gig`, then create the issue as the tracker reference describes.
2. **Adoption:** read the tracker reference ([GitHub](../../references/integrations/tracker/github.md#adopt-selected-queued-work) or [Linear](../../references/integrations/tracker/linear.md#queue-intake-and-explicit-adoption)) and the [discussion rules](../../references/integrations/shared.md#provider-bodies), and complete its Gig preflight; on Linear, the issue must have no Project.
3. **Initialize:** call `init_github_gig` or `init_linear_gig` with the intake values, the Request as `request`, the slug, and the issue as `object`. The Gig starts from and targets the repository default.
4. **Before planning,** finish board setup.

### Local Gig

When the tracker is off, or the user wants the Gig kept off the tracker, call `init_local_gig` with the intake values instead. Skip the tracker item, board setup, and Priority.

A local Gig follows the same lifecycle with every tracker step skipped: Phase, gates, plan publication, lifecycle, Priority, and labels. Submission and shipping still use the forge when it's on.

A Gig must never contain parent Epic fields or be converted into a Task.
