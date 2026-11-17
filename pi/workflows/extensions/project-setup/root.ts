import { checkoutRelation } from "../__lib/project-config.js"

/**
 * The checkout project setup inspects and writes: the source checkout, the one piton's resolution reads. In a worker
 * clone setup would show the clone's files while nothing reads them, so it refuses there.
 */
export function setupRoot(cwd: string): string {
    const relation = checkoutRelation(cwd)
    switch (relation.kind) {
        case "source":
            return relation.checkout
        case "worker":
            throw new Error(
                `project setup configures the source checkout, which piton reads for every worker; run it in ${relation.source}`,
            )
    }
}
