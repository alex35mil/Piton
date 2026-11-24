-- Whether a repository's committed grants apply, and pi loads its project files: the user's decision, kept by the
-- daemon per source checkout (`piton trust`). Asked before an agent's tab opens, so its pi starts with it decided.

local cli = require("piton.cli")
local config = require("piton.config")
local util = require("piton.util")

local M = {}

--- The question's text: the grants (new ones marked) and what pi would load.
---@param source string
---@param trust { grants: string[], added: string[], resources: { pi: string[], mcp_servers: string[] } }
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
    lines[#lines + 1] = ""
    lines[#lines + 1] = "Trust it? No keeps its restrictions and leaves its grants out."
    return table.concat(lines, "\n")
end

--- Asks when the repository of `path` needs a decision, and records the answer. Runs inside `util.async`.
---@param path string
function M.ensure(path)
    local status = cli.trust_status(path)
    if status.trust.state ~= "needed" then
        return
    end
    local decision = config.confirm(M.question(status.source, status.trust)) and "allow" or "deny"
    cli.trust_decide(path, decision)
end

return M
