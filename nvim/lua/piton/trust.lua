-- Whether a repository's committed grants apply, and pi loads its project files: the user's decision, kept by the
-- daemon per source checkout (`piton trust`). Asked before an agent's tab opens, so its pi starts with it decided.

local cli = require("piton.cli")
local util = require("piton.util")

local M = {}

local CHOICES = { "Trust", "Don't trust", "Cancel" }

--- The question's text: the grants (new ones marked) and what pi would load.
---@param source string
---@param trust PitonTrust
---@return string
function M.question(source, trust)
    local lines = { source .. " isn't trusted yet. Trusting it applies:" }
    if #trust.grants > 0 then
        local new = {}
        for _, grant in ipairs(trust.added) do
            new[grant] = #trust.added < #trust.grants
        end
        lines[#lines + 1] = "Committed grants:"
        for _, grant in ipairs(util.head(trust.grants, 12)) do
            lines[#lines + 1] = "  " .. grant .. (new[grant] and "  (new)" or "")
        end
    end
    if #trust.resources.pi > 0 then
        lines[#lines + 1] = "pi's project files (extensions run code in pi's process):"
        for _, path in ipairs(trust.resources.pi) do
            lines[#lines + 1] = "  " .. path
        end
    end
    if #trust.resources.mcp_servers > 0 then
        lines[#lines + 1] = "MCP servers: " .. table.concat(trust.resources.mcp_servers, ", ")
    end
    lines[#lines + 1] = "Don't trust keeps its restrictions and leaves its grants out."
    return table.concat(lines, "\n")
end

--- The user's choice, with `vim.ui.select`; yields inside `util.async`. Nil when dismissed.
---@param prompt string
---@return "Trust" | "Don't trust" | "Cancel" | nil
local function choose(prompt)
    local co = coroutine.running()
    vim.ui.select(CHOICES, { prompt = prompt }, function(choice)
        vim.schedule(function()
            util.resume(co, choice)
        end)
    end)
    return coroutine.yield()
end

--- Asks when the repository of `path` needs a decision, and records the answer for the grants it showed. Asks again
--- when they changed while the question was open. Runs inside `util.async`.
---@param path string
---@return boolean proceed false when the user cancelled or dismissed the question: open nothing, record nothing
function M.ensure(path)
    while true do
        local status = cli.trust_status(path)
        if status.trust.state ~= "needed" then
            return true
        end
        local choice = choose(M.question(status.source, status.trust))
        if choice ~= "Trust" and choice ~= "Don't trust" then
            return false
        end
        local decision = choice == "Trust" and "allow" or "deny"
        if cli.trust_decide(path, decision, status.trust.grants) == "recorded" then
            return true
        end
    end
end

return M
