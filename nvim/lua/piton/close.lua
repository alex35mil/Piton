-- Closing an agent's tab, or deleting the agent. piton deletes the checkout
-- (`piton worker delete`), stopping its Pi sessions first.

local util = require("piton.util")
local config = require("piton.config")
local log = require("piton.notify")
local cli = require("piton.cli")
local tabs = require("piton.tabs")

local M = {}

---@param agent Agent
function M.close_tab(agent)
    require("snacks").picker({
        title = "Close: " .. agent.label,
        items = {
            { text = "Delete agent", action = "delete" },
            { text = "Close tab", action = "close-tab" },
        },
        format = function(item)
            return { { " ", "SnacksPickerIcon" }, { item.text } }
        end,
        confirm = function(picker, item)
            picker:close()
            if not item then
                return
            end
            if item.action == "close-tab" then
                vim.cmd("tabclose")
            elseif item.action == "delete" then
                M.delete(agent)
            end
        end,
        layout = { preset = "select" },
    })
end

--- The answer to an update of the source that changes protected files: nil keeps the agent.
---@param agent Agent
---@param refusal { kind: "protected_update", target: string, checkout: string }
---@return PitonSourceUpdate?
local function source_update(agent, refusal)
    local question = cli.describe(refusal)
        .. "\n\n"
        .. require("piton.create").PROTECTED_UPDATE_NOTE
        .. "\n\nDelete agent "
        .. agent.label
        .. " and apply these changes to "
        .. refusal.checkout
        .. "?"
    if config.confirm(question) then
        return { kind = "approve", target = refusal.target }
    end
    if config.confirm("Delete agent " .. agent.label .. " without updating " .. refusal.checkout .. "?") then
        return { kind = "skip" }
    end
    return nil
end

--- Deletes the agent after every question is answered; its tab stays open until then.
---@param agent Agent
function M.delete(agent)
    util.async(function()
        local unsaved = vim.tbl_filter(function(buffer)
            return buffer.modified
        end, M.agent_buffers(agent.path))
        if #unsaved > 0 then
            local names = vim.tbl_map(function(buffer)
                return buffer.name
            end, unsaved)
            local message = "Unsaved changes in agent "
                .. agent.label
                .. ":\n"
                .. table.concat(util.head(names, 15), "\n")
            if not config.confirm(message .. "\n\nDiscard and delete?") then
                log.info("Kept agent: " .. agent.path)
                return
            end
        end

        -- The daemon checks leftovers, then the source's update, before it changes anything: each answer retries.
        local force, update = false, nil
        local result = cli.delete(agent.path, force, update)
        if result.kind == "refused" and result.refusal.kind == "leftovers" then
            local message = "Agent " .. agent.label .. " isn't clean:\n" .. table.concat(result.refusal.lines, "\n")
            if not config.confirm(message .. "\n\nDelete anyway?") then
                log.info("Kept agent: " .. agent.path)
                return
            end
            force = true
            result = cli.delete(agent.path, force, update)
        end
        if result.kind == "refused" and result.refusal.kind == "protected_update" then
            update = source_update(agent, result.refusal --[[@as { kind: "protected_update", target: string, checkout: string }]])
            if not update then
                log.info("Kept agent: " .. agent.path)
                return
            end
            result = cli.delete(agent.path, force, update)
        end

        if result.kind == "refused" then
            util.refuse("Can't delete agent " .. agent.label .. ": " .. cli.describe(result.refusal))
        elseif result.kind == "deleted" then
            local tab = tabs.find(agent.path)
            if tab then
                vim.cmd("tabclose " .. vim.api.nvim_tabpage_get_number(tab))
            end
            -- Its files are gone; buffers left open would point at nothing.
            for _, buffer in ipairs(M.agent_buffers(agent.path)) do
                pcall(vim.api.nvim_buf_delete, buffer.buf, { force = true })
            end
            log.info("Deleted agent: " .. agent.path)
            if result.warning then
                log.warn("Source not updated: " .. result.warning)
            end
        end
    end)
end

---@class AgentBuffer
---@field buf integer
---@field name string Path relative to the agent.
---@field modified boolean

--- Buffers whose file is inside the agent.
---@param path string
---@return AgentBuffer[]
function M.agent_buffers(path)
    local prefix = path .. "/"
    local buffers = {}
    for _, buf in ipairs(vim.api.nvim_list_bufs()) do
        local name = vim.api.nvim_buf_get_name(buf)
        if vim.startswith(name, prefix) then
            table.insert(buffers, { buf = buf, name = name:sub(#prefix + 1), modified = vim.bo[buf].modified })
        end
    end
    return buffers
end

return M
