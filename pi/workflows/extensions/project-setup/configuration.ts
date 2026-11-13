import * as path from "node:path"

import { Type } from "typebox"

import { writeProtected } from "../../../extensions/__lib/protected.js"
import { compile, parse } from "../../../extensions/__lib/schema.js"
import {
    CONFIG_LOCAL_FILE,
    clearSettingsCache,
    PITON_DIR,
    plannedConfigEdit,
} from "../../../extensions/__lib/settings.js"
import { BranchFormatSchema } from "../__lib/domain.js"
import { GitHubForgeSchema, integrationsToFile, TrackerConfigSchema } from "../integrations/config.js"
import { ensureLocalSettingsExclusion } from "./local-settings.js"
import { setupRoot } from "./root.js"

/**
 * The approved workflow configuration of a repository. Each part is optional: omitted parts stay as they are. The
 * tracker and forge are the exact objects the setup tools returned (`trackerConfig`, label setup's `configuration`), in
 * the tools' own key names; they're written as `[tracker]` and `[forge]` with the file's key names.
 */
export const ProjectConfigurationSchema = Type.Object(
    {
        branchFormat: Type.Optional(BranchFormatSchema),
        tracker: Type.Optional(TrackerConfigSchema),
        forge: Type.Optional(GitHubForgeSchema),
        /** Personal: written to `.piton/config.local.toml`, which stays outside Git. */
        username: Type.Optional(Type.String({ minLength: 1, pattern: "^[^\\s/]+$" })),
    },
    { additionalProperties: false },
)
export const ProjectConfigurationValidator = compile(ProjectConfigurationSchema)

/** One file's planned change: its path relative to the repository, and the content before and after. */
export interface PlannedFile {
    file: string
    content: string | null
}

/**
 * Writes the approved configuration: `[branches] format`, `[tracker]`, and `[forge]` in `.piton/config.toml`, the
 * username in `.piton/config.local.toml`. `piton config set` makes each file's new text (keeping its comments and
 * layout, refusing a result the resolver wouldn't accept); each file is then written as one protected write, which
 * under piton's daemon shows the user the diff for approval. Both files are planned before either is written.
 */
export async function configureProject(cwd: string, input: unknown) {
    const parsed = parse(ProjectConfigurationValidator, input)
    const root = setupRoot(cwd)
    const shared: Record<string, unknown> = {}
    if (parsed.branchFormat !== undefined) shared["branches.format"] = parsed.branchFormat
    if (parsed.tracker !== undefined) shared.tracker = integrationsToFile(parsed.tracker)
    if (parsed.forge !== undefined) shared.forge = integrationsToFile(parsed.forge)
    const local: Record<string, unknown> = parsed.username === undefined ? {} : { "branches.username": parsed.username }
    if (Object.keys(shared).length + Object.keys(local).length === 0) throw new Error("nothing to configure")

    const planned = [
        ...(Object.keys(shared).length > 0 ? [plannedConfigEdit(cwd, shared, false)] : []),
        ...(Object.keys(local).length > 0 ? [plannedConfigEdit(cwd, local, true)] : []),
    ]
    const written: string[] = []
    for (const edit of planned) {
        if (edit.content === null) continue
        if (path.basename(edit.path) === CONFIG_LOCAL_FILE) {
            ensureLocalSettingsExclusion(root, `${PITON_DIR}/${CONFIG_LOCAL_FILE}`)
        }
        await writeProtected(edit.path, edit.content)
        written.push(path.relative(root, edit.path))
    }
    clearSettingsCache()
    return { changed: written, unchanged: planned.length - written.length }
}
