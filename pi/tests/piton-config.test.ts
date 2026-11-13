import "./host-runtime.ts"

import assert from "node:assert/strict"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { describe, it } from "node:test"

import { BIN_ENV, clearSettingsCache, RESOLVE_FORMAT, resolveSettings, section } from "../extensions/__lib/settings.ts"

/** A stub `piton` that prints `output` and counts its calls. */
function stub(output: unknown, exitCode = 0) {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "piton-stub-"))
    const bin = path.join(dir, "piton")
    const calls = path.join(dir, "calls")
    fs.writeFileSync(path.join(dir, "out.json"), JSON.stringify(output))
    fs.writeFileSync(
        bin,
        `#!/bin/sh\necho "$@" >> ${JSON.stringify(calls)}\ncat ${JSON.stringify(path.join(dir, "out.json"))}\nexit ${exitCode}\n`,
    )
    fs.chmodSync(bin, 0o755)
    const old = process.env[BIN_ENV]
    process.env[BIN_ENV] = bin
    clearSettingsCache()
    return {
        dir,
        calls: () => (fs.existsSync(calls) ? fs.readFileSync(calls, "utf-8").trim().split("\n") : []),
        cleanup() {
            if (old === undefined) delete process.env[BIN_ENV]
            else process.env[BIN_ENV] = old
            clearSettingsCache()
            fs.rmSync(dir, { recursive: true, force: true })
        },
    }
}

function resolved(watch: string[]) {
    return {
        format: RESOLVE_FORMAT,
        workspace: "/w",
        source: "/w",
        checkout: "/w",
        global: "/g",
        runtime: "host",
        permissions: { ok: { merged: { allow: ["read"], ask: [], deny: [] }, sources: [] } },
        mcp: { ok: { servers: [] } },
        mcp_global: { ok: { servers: [] } },
        web_search: { ok: {} },
        branches: { error: { message: "invalid /w/.piton/config.toml" } },
        tracker: { ok: null },
        forge: { ok: null },
        development: { ok: {} },
        policies: { ok: [] },
        instructions: { ok: { rules: [], skills: [] } },
        files: [],
        watch,
    }
}

describe("piton config resolve client", () => {
    it("runs the binary once and again only after a watched path changes", () => {
        const s = stub(resolved([]))
        try {
            const watched = path.join(s.dir, "watched.json")
            fs.writeFileSync(path.join(s.dir, "out.json"), JSON.stringify(resolved([watched])))
            const cwd = s.dir
            // A broken part fails only its readers.
            assert.deepEqual(section(resolveSettings(cwd, "host").permissions).merged.allow, ["read"])
            assert.throws(() => section(resolveSettings(cwd, "host").branches), /config\.toml/)
            resolveSettings(cwd, "host")
            assert.equal(s.calls().length, 1)
            assert.match(s.calls()[0], /config resolve --workspace .* --runtime host/)

            // A file appearing where none was is a change.
            fs.writeFileSync(watched, "{}")
            resolveSettings(cwd, "host")
            assert.equal(s.calls().length, 2)
            // Each runtime is its own result.
            resolveSettings(cwd, "sandbox")
            assert.equal(s.calls().length, 3)
        } finally {
            s.cleanup()
        }
    })

    it("reports the resolver's error and refuses an unknown format", () => {
        const invalid = stub({ format: RESOLVE_FORMAT, error: { kind: "invalid", message: "invalid here" } }, 3)
        try {
            assert.throws(() => resolveSettings(invalid.dir, "host"), /invalid here/)
        } finally {
            invalid.cleanup()
        }
        const future = stub({ ...resolved([]), format: RESOLVE_FORMAT + 1 })
        try {
            assert.throws(() => resolveSettings(future.dir, "host"), /format/)
        } finally {
            future.cleanup()
        }
    })

    it("names a missing binary", () => {
        const old = process.env[BIN_ENV]
        process.env[BIN_ENV] = path.join(os.tmpdir(), "no-such-piton-binary")
        clearSettingsCache()
        try {
            assert.throws(() => resolveSettings(os.tmpdir(), "host"), /wasn't found/)
        } finally {
            if (old === undefined) delete process.env[BIN_ENV]
            else process.env[BIN_ENV] = old
            clearSettingsCache()
        }
    })
})
