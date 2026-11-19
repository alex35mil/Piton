-- `piton worker`: piton creates, lists, and deletes agent checkouts, and runs
-- their Pi. Its `--json` responses are validated here, at the boundary.

---@class PitonWorker
---@field path string
---@field source string
---@field label string
---@field issue string?

---@alias PitonSessionStatus "idle" | "working" | "needs_input" | "stopping"

---@class PitonSession
---@field key integer
---@field status PitonSessionStatus

---@class PitonWorkerEntry
---@field worker PitonWorker
---@field sessions PitonSession[]

---@alias PitonWorkerRefusal
---| { kind: "not_a_repository", path: string }
---| { kind: "invalid_name", name: string }
---| { kind: "exists", path: string }
---| { kind: "source_dirty", checkout: string, status: string[] }
---| { kind: "source_off_default", checkout: string, branch: string?, default: string }
---| { kind: "unknown_default", checkout: string }
---| { kind: "not_a_worker", path: string }
---| { kind: "leftovers", path: string, lines: string[] }
---| { kind: "invalid_configuration", message: string }
---| { kind: "source_fetch", checkout: string, message: string }
---| { kind: "protected_update", checkout: string, upstream: string, target: string, files: string[] }
---| { kind: "failed", message: string }

--- The answer to a `protected_update` refusal.
---@alias PitonSourceUpdate
---| { kind: "ask" }
---| { kind: "approve", target: string } Apply this commit's changes to protected files.
---| { kind: "skip" } Deletion only: leave the source as it is.

---@alias PitonCreateResult
---| { kind: "created", worker: PitonWorker }
---| { kind: "refused", refusal: PitonWorkerRefusal }

---@alias PitonDeleteResult
---| { kind: "deleted", stopped: integer, warning: string? }
---| { kind: "refused", refusal: PitonWorkerRefusal }

local util = require("piton.util")
local source = require("piton.source")

local M = {}

local REFUSAL_KINDS = {
    not_a_repository = true,
    invalid_name = true,
    exists = true,
    source_dirty = true,
    source_off_default = true,
    unknown_default = true,
    not_a_worker = true,
    leftovers = true,
    invalid_configuration = true,
    source_fetch = true,
    protected_update = true,
    failed = true,
}

local SESSION_STATUSES = { idle = true, working = true, needs_input = true, stopping = true }

-- Exit codes of `piton worker --json`.
local EXIT_OK = 0
local EXIT_REFUSED = 4

--- Run `piton worker <args> --json` and return its decoded response. Refuses
--- when piton fails or the daemon isn't running; never falls back to doing the
--- work here.
---@param args string[]
---@return table response
local function run(args)
    local cmd = vim.list_extend({ "piton", "worker" }, args)
    table.insert(cmd, "--json")
    local result = util.exec(cmd)
    local ok, response = pcall(vim.json.decode, result.stdout or "", { luanil = { object = true, array = true } })
    response = ok and type(response) == "table" and response or nil

    if result.code == EXIT_OK or result.code == EXIT_REFUSED then
        if not response then
            util.refuse("Unexpected output from `piton worker " .. args[1] .. "`: " .. vim.trim(result.stdout or ""))
        end
        return response --[[@as table]]
    end

    if response and response.type == "error" then
        util.refuse("piton worker " .. args[1] .. " failed: " .. tostring(response.message))
    end
    local stderr = vim.trim(result.stderr or "")
    if stderr:find("daemon not reachable", 1, true) then
        util.refuse("The piton daemon isn't running, so agents can't be managed:\n" .. stderr)
    end
    util.refuse("piton worker " .. args[1] .. " failed (exit " .. result.code .. "):\n" .. stderr)
    error("unreachable")
end

---@param value any
---@return PitonWorker
local function worker(value)
    if type(value) ~= "table" or type(value.path) ~= "string" or type(value.source) ~= "string" then
        util.refuse("Unexpected worker from piton: " .. vim.inspect(value))
    end
    return {
        path = value.path,
        source = value.source,
        label = type(value.label) == "string" and value.label or vim.fs.basename(value.path),
        issue = type(value.issue) == "string" and value.issue or nil,
    }
end

---@param value any
---@return PitonWorkerRefusal
local function refusal(value)
    if type(value) ~= "table" or not REFUSAL_KINDS[value.kind] then
        util.refuse("piton refused for a reason this editor doesn't know: " .. vim.inspect(value))
    end
    return value --[[@as PitonWorkerRefusal]]
end

---@param value any
---@return PitonSession[]
local function sessions(value)
    local result = {}
    for _, session in ipairs(type(value) == "table" and value or {}) do
        if type(session) == "table" and SESSION_STATUSES[session.status] then
            table.insert(result, { key = session.key, status = session.status })
        end
    end
    return result
end

---@param response table
---@param command string
local function unexpected(response, command)
    util.refuse("Unexpected response to `piton worker " .. command .. "`: " .. vim.inspect(response))
end

--- The CLI arguments for an answer to `protected_update`.
---@param update PitonSourceUpdate
---@return string[]
local function update_args(update)
    if update.kind == "ask" then
        return {}
    elseif update.kind == "approve" then
        return { "--approve-source-update", update.target }
    elseif update.kind == "skip" then
        return { "--skip-source-update" }
    end
    error("unknown source update: " .. vim.inspect(update))
end

---@param request { from: string, name: string, label: string, issue: string?, update: PitonSourceUpdate? }
---@return PitonCreateResult
function M.create(request)
    local args = { "create", request.name, "--from", request.from, "--label", request.label }
    if request.issue then
        vim.list_extend(args, { "--issue", request.issue })
    end
    local update = request.update or { kind = "ask" }
    if update.kind == "skip" then
        error("creating an agent always updates its source")
    end
    vim.list_extend(args, update_args(update))
    local response = run(args)
    if response.type == "worker" then
        return { kind = "created", worker = worker(response.worker) }
    elseif response.type == "worker_refused" then
        return { kind = "refused", refusal = refusal(response.refusal) }
    end
    unexpected(response, "create")
    error("unreachable")
end

--- The workers of `from`'s source, with their live sessions. Refuses when
--- piton can't list them.
---@param from string The source checkout or any of its workers.
---@return PitonWorkerEntry[]
function M.list(from)
    local response = run({ "ls", "--from", from })
    if response.type == "workers" then
        local entries = {}
        for _, entry in ipairs(response.workers or {}) do
            table.insert(entries, { worker = worker(entry), sessions = sessions(entry.sessions) })
        end
        return entries
    elseif response.type == "worker_refused" then
        util.refuse("Can't list agents: " .. M.describe(refusal(response.refusal)))
    end
    unexpected(response, "ls")
    error("unreachable")
end

--- Stop the worker's sessions and delete it. Every refusal comes before
--- anything changes: `leftovers` (uncommitted changes or unpushed commits)
--- unless `force`, and `protected_update` unless `update` answers it.
---@param path string
---@param force boolean
---@param update? PitonSourceUpdate
---@return PitonDeleteResult
function M.delete(path, force, update)
    local args = { "delete", path }
    if force then
        table.insert(args, "--force")
    end
    vim.list_extend(args, update_args(update or { kind = "ask" }))
    local response = run(args)
    if response.type == "worker_deleted" then
        return {
            kind = "deleted",
            stopped = type(response.stopped) == "number" and response.stopped or 0,
            warning = type(response.warning) == "string" and response.warning or nil,
        }
    elseif response.type == "worker_refused" then
        return { kind = "refused", refusal = refusal(response.refusal) }
    end
    unexpected(response, "delete")
    error("unreachable")
end

--- The folder piton will create for `name`, for previews only: mirrors
--- piton's `worker::normalize_name` (ASCII letters and digits kept and
--- lowercased; runs of spaces, `_`, `-` become one `-`; anything else dropped).
--- piton stays the authority: creation uses the path it returns. Nil when the
--- name normalizes to nothing.
---@param source_path string
---@param name string
---@return string?
function M.preview_folder(source_path, name)
    local out, dash = {}, false
    for c in name:lower():gmatch(".") do
        if c:match("[a-z0-9]") then
            table.insert(out, c)
            dash = false
        elseif c:match("[%s_%-]") and not dash and #out > 0 then
            table.insert(out, "-")
            dash = true
        end
    end
    local normalized = table.concat(out):gsub("%-+$", "")
    if normalized == "" then
        return nil
    end
    return vim.fs.basename(source_path) .. "--" .. normalized
end

-- Exit code of `piton config get` when the part's files are invalid.
local EXIT_INVALID_CONFIG = 3

--- One part of the workspace's resolved configuration (`piton config get`):
--- piton's resolver is the only reader of configuration files. Refuses with
--- the resolver's message when the part can't be used.
---@param part "tracker" | "forge" | "mcp"
---@param workspace string
---@return any value nil when the part is unset (`tracker` or `forge` with no table)
function M.config(part, workspace)
    local result = util.exec({ "piton", "config", "get", part, "--workspace", workspace })
    local ok, value = pcall(vim.json.decode, result.stdout or "", { luanil = { object = true, array = true } })
    if result.code == EXIT_OK and ok then
        return value
    end
    if result.code == EXIT_INVALID_CONFIG and ok and type(value) == "table" and type(value.error) == "table" then
        util.refuse(tostring(value.error.message))
    end
    util.refuse("piton config get " .. part .. " failed (exit " .. result.code .. "):\n" .. vim.trim(result.stderr or ""))
end

---@param refusal_value PitonWorkerRefusal
---@return string
function M.describe(refusal_value)
    local r = refusal_value
    if r.kind == "not_a_repository" then
        return r.path .. " is not a git checkout"
    elseif r.kind == "invalid_name" then
        return "Invalid agent name: " .. r.name
    elseif r.kind == "exists" then
        return "Already exists: " .. r.path
    elseif r.kind == "source_dirty" then
        return source.describe_state({ kind = "dirty", checkout = r.checkout, status = r.status })
    elseif r.kind == "source_off_default" then
        return source.describe_state({
            kind = "off-default",
            checkout = r.checkout,
            branch = r.branch,
            default = r.default,
        })
    elseif r.kind == "unknown_default" then
        return source.describe_state({ kind = "unknown-default", checkout = r.checkout })
    elseif r.kind == "not_a_worker" then
        return r.path .. " is not an agent"
    elseif r.kind == "leftovers" then
        return r.path .. " isn't clean:\n" .. table.concat(r.lines, "\n")
    elseif r.kind == "invalid_configuration" or r.kind == "source_fetch" or r.kind == "failed" then
        return r.message
    elseif r.kind == "protected_update" then
        return ("Updating %s to %s changes protected files, which the sandbox can't write:\n%s"):format(
            r.checkout,
            r.upstream,
            table.concat(util.head(r.files, 15), "\n")
        )
    end
    error("unknown piton refusal: " .. vim.inspect(r))
end

return M
