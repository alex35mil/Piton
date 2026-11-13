import { resolveSettings } from "../../../extensions/__lib/settings.js"

/**
 * The checkout project setup inspects and writes: the source checkout, the one piton's resolution reads. In a worker
 * clone setup would show the clone's files while nothing reads them, so it refuses there.
 */
export function setupRoot(cwd: string): string {
    const resolved = resolveSettings(cwd)
    if (resolved.checkout !== resolved.source) {
        throw new Error(
            `project setup configures the source checkout, which piton reads for every worker; run it in ${resolved.source}`,
        )
    }
    return resolved.source
}
