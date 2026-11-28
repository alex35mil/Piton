import type { ExtensionAPI } from "@earendil-works/pi-coding-agent"

import { TASK_ROOT } from "../__lib/domain.js"

export default function (pi: ExtensionAPI): void {
    pi.registerCommand("task", {
        description: "Start or resume a Task: standalone, or one of an Epic's",
        handler: async (args, ctx) => {
            const input = args.trim()
            if (!input) {
                ctx.ui.notify(
                    `Usage: /task <description, tracker issue, or existing ${TASK_ROOT} or Epic Task path>`,
                    "error",
                )
                return
            }
            pi.sendUserMessage(["Task:", "", input, "", "Use the task skill."].join("\n"))
        },
    })
}
