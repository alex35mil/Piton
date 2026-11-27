-- Finding agents and the source they belong to.

---@class Agent
---@field path string Agent checkout (its git root).
---@field source string Checkout the agent was cloned from.
---@field label string Name the agent was created with.
---@field item_id string? The stable ID of the work item the agent was created for.

---@class AgentEntry
---@field agent Agent
---@field sessions PitonSession[] The agent's live piton sessions.

local util = require("piton.util")
local cli = require("piton.cli")

local M = {}

--- The agent containing `path`, or nil when it isn't inside one. piton decides
--- (`piton worker info`), from the checkout's Git config and folder layout, so
--- the editor never checks the layout itself and a stray `agent.source` value
--- isn't an agent here when piton wouldn't treat it as one.
---@param path string
---@return Agent?
function M.get(path)
    local root = util.git_root(path)
    if not root then
        return nil
    end
    local result = cli.info(root)
    if result.kind == "worker" then
        return result.worker
    elseif result.kind == "not_a_worker" then
        return nil
    end
    error("unknown info result: " .. vim.inspect(result))
end

--- The source's agents with their live sessions, from piton, sorted by path.
--- Refuses when piton can't list them.
---@param source string
---@return AgentEntry[]
function M.list(source)
    local entries = {}
    for _, entry in ipairs(cli.list(source)) do
        table.insert(entries, { agent = entry.worker, sessions = entry.sessions })
    end
    return entries
end

--- The checkout agents are created from: the current tab's repository, or
--- the source of the agent the tab is in.
---@return string? source, string? root
function M.current_source()
    local root = util.git_root(vim.fn.getcwd())
    if not root then
        return nil, nil
    end
    local agent = M.get(root)
    return agent and agent.source or root, root
end

return M
