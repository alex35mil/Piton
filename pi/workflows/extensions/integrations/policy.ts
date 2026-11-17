import type { EntityStatus } from "../__lib/entity.js"
import type { GitHubForge, TrackerConfig } from "./config.js"

/** The entity's tracker item belongs to the configured tracker. */
export function assertEntityTracker(status: EntityStatus, configured: TrackerConfig): void {
    const tracker = status.tracker
    if (tracker.provider !== configured.provider)
        throw new Error(`${status.slug} is bound to ${tracker.provider}, not the configured tracker`)
    if (
        tracker.provider === "github" &&
        configured.provider === "github" &&
        !sameRepository(tracker.repository, configured.repository)
    )
        throw new Error(`${status.slug} tracker repository differs from the configured repository`)
}

/** The entity's saved PR intent, when it has one, belongs to the configured forge. */
export function assertEntityForge(status: EntityStatus, forge: GitHubForge): void {
    if (status.forge && !sameRepository(status.forge.repository, forge.repository))
        throw new Error(`${status.slug} forge repository differs from the configured repository`)
}

// A Project is identified by owner login and number; logins are unique across users and organizations,
// so `owner_type` is only a hint the server detects when absent. A passed `owner_type` must still match.
export function matchesGitHubProjectAddress(
    args: Record<string, unknown>,
    project: { owner: string; ownerType: "user" | "org"; number: number },
): boolean {
    return (
        typeof args.owner === "string" &&
        args.owner.toLowerCase() === project.owner.toLowerCase() &&
        args.project_number === project.number &&
        (args.owner_type === undefined || args.owner_type === project.ownerType)
    )
}

export function sameRepository(left: { owner: string; repo: string }, right: { owner: string; repo: string }) {
    return (
        left.owner.toLowerCase() === right.owner.toLowerCase() && left.repo.toLowerCase() === right.repo.toLowerCase()
    )
}
