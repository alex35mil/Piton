/**
 * Where this pi runs: directly on the host, or inside the sandbox of a session piton's daemon launched.
 *
 * A fact of the session, asked of the daemon over the channel (`facts`); outside a session the daemon launched, the
 * host.
 */

import { channel, sessionFacts, type Runtime } from "./agent.js"

export type { Runtime } from "./agent.js"

/** The current runtime: the session's, or the host outside one. Rejects when the daemon can't be asked. */
export async function currentRuntime(env: NodeJS.ProcessEnv = process.env): Promise<Runtime> {
    const current = channel(env)
    switch (current.kind) {
        case "noSession":
            return "host"
        case "session":
            return (await sessionFacts(current)).runtime
    }
}
