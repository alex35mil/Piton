/**
 * Workers Extension
 *
 * Lets the agent ask piton's daemon to create, list, and delete workers of this session's project: `create_worker`,
 * `list_workers`, and `delete_worker`, one tool per operation. The daemon decides from `[workers.permissions]` and asks
 * the user when it says so; the agent never approves, and the permissions extension leaves these tools to the daemon.
 * Outside a session piton's daemon launched, there's no one to ask, and nothing is registered.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent"
import { Type } from "typebox"

import {
    channel,
    createWorker,
    deleteWorker,
    listWorkers,
    type SessionChannel,
    type WorkerOutcome,
} from "../__lib/agent.js"

/** The tools this extension registers: the daemon decides them, so the permissions extension doesn't. */
export const WORKER_TOOLS = ["create_worker", "list_workers", "delete_worker"] as const

const ASKS =
    "piton's daemon asks the user unless their settings answer; if it isn't done, tell the user why and don't retry unless they ask."

type Text = { content: Array<{ type: "text"; text: string }>; details: unknown }

function text(value: string, details: unknown): Text {
    return { content: [{ type: "text", text: value }], details }
}

/** The tool's result: what the agent reads, with the typed outcome in `details`. */
function result<T>(outcome: WorkerOutcome<T>, done: (value: T) => string): Text {
    switch (outcome.kind) {
        case "done":
            return text(done(outcome.value), outcome)
        case "refused": {
            const { kind, ...rest } = outcome.refusal
            return text(`Refused (${kind}): ${JSON.stringify(rest)}`, outcome)
        }
        case "notDone":
            return text(`Not done: ${outcome.reason}`, outcome)
    }
}

export function registerWorkerTools(pi: ExtensionAPI, session: SessionChannel): void {
    pi.registerTool({
        name: "create_worker",
        label: "create worker",
        description: `Create a worker: a copy-on-write clone of this project's source checkout, next to it, where another session can work in isolation. ${ASKS}`,
        parameters: Type.Object({
            name: Type.String({ description: "The worker's name; its folder is <repo>.workers/<name>, normalized" }),
            label: Type.Optional(Type.String({ description: "The name shown for it; defaults to `name`" })),
            item_id: Type.Optional(
                Type.String({ description: "The stable ID of the work item (issue, pull request) it's for" }),
            ),
        }),
        async execute(_id, params, signal) {
            const { name, label, item_id } = params as { name: string; label?: string; item_id?: string }
            const outcome = await createWorker(session, { name, label, item_id }, signal)
            return result(outcome, (worker) => `Created ${worker.path}`)
        },
    })

    pi.registerTool({
        name: "list_workers",
        label: "list workers",
        description: "List this project's workers, with their live sessions.",
        parameters: Type.Object({}),
        async execute(_id, _params, signal) {
            const outcome = await listWorkers(session, signal)
            return result(outcome, (workers) =>
                workers.length === 0
                    ? "No workers."
                    : workers
                          .map(
                              (w) =>
                                  `${w.path}  ${w.label}  sessions=${w.sessions.length}${w.item_id ? `  item=${w.item_id}` : ""}`,
                          )
                          .join("\n"),
            )
        },
    })

    pi.registerTool({
        name: "delete_worker",
        label: "delete worker",
        description: `Delete one of this project's workers: its live sessions stop (this one too, if it runs there). Uncommitted or unpushed work is shown to the user first. ${ASKS}`,
        parameters: Type.Object({
            path: Type.String({ description: "The worker's folder, from list_workers" }),
        }),
        async execute(_id, params, signal) {
            const outcome = await deleteWorker(session, (params as { path: string }).path, signal)
            return result(outcome, ({ stopped, warning }) =>
                [`Deleted; ${stopped} session(s) stopped.`, warning].filter(Boolean).join("\n"),
            )
        },
    })
}

export default function (pi: ExtensionAPI) {
    const session = channel()
    switch (session.kind) {
        case "session":
            registerWorkerTools(pi, session)
            return
        case "noSession":
            return
    }
}
