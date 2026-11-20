-- Editor integration belongs to the caller. Defaults use only Neovim; no dotfiles globals or load-time dependencies.

---@class PitonTabLabel
---@field icon string
---@field name string

--- The credential a queue request needs: the environment variable its MCP registration names, and the source
--- checkout the request is for (also the command's working directory).
---@class PitonCredential
---@field variable string
---@field source string

--- Wraps the queue's curl command so it runs with `credential.variable` set in its environment. Returns the command
--- to run; it never sees the credential's value.
---@alias PitonWithCredential fun(command: string[], credential: PitonCredential): string[]

---@class PitonOptions
---@field tabs? { new?: fun(), set_label?: fun(label: PitonTabLabel) }
---@field icons? { agent?: string, source?: string }
---@field confirm? fun(message: string): boolean Synchronous yes/no confirmation; true means proceed.
---@field queue? { with_credential?: PitonWithCredential }

---@class PitonConfig
---@field tabs { new: fun(), set_label: fun(label: PitonTabLabel) }
---@field icons { agent: string, source: string }
---@field confirm fun(message: string): boolean
---@field queue { with_credential: PitonWithCredential }

local M = {}

---@return PitonConfig
local function defaults()
    return {
        tabs = {
            new = function()
                vim.cmd("tabnew")
            end,
            set_label = function(label)
                -- No tabline dependency. A tabline can read this variable or supply its own hook.
                vim.t.piton_label = label
            end,
        },
        icons = { agent = "󰙅", source = "" },
        confirm = function(message)
            return vim.fn.confirm(message, "&Yes\n&No", 2) == 1
        end,
        queue = {
            -- The command as is: curl reads the credential from the environment Neovim was started with.
            with_credential = function(command)
                return command
            end,
        },
    }
end

local options = defaults()

---@param value table
---@param schema table<string, string>
---@param path string
local function validate(value, schema, path)
    if type(value) ~= "table" then
        error(path .. " must be a table", 3)
    end
    for key, entry in pairs(value) do
        local expected = schema[key]
        if not expected then
            error("unknown option: " .. path .. "." .. tostring(key), 3)
        end
        if type(entry) ~= expected then
            error(path .. "." .. key .. " must be a " .. expected, 3)
        end
    end
end

--- Replaces the previous configuration, filling omitted options from defaults. Validate before changing anything.
---@param opts? PitonOptions
function M.setup(opts)
    if opts == nil then
        opts = {}
    end
    validate(opts, { tabs = "table", icons = "table", confirm = "function", queue = "table" }, "piton")
    if opts.tabs then
        validate(opts.tabs, { new = "function", set_label = "function" }, "piton.tabs")
    end
    if opts.icons then
        validate(opts.icons, { agent = "string", source = "string" }, "piton.icons")
    end
    if opts.queue then
        validate(opts.queue, { with_credential = "function" }, "piton.queue")
    end
    options = vim.tbl_deep_extend("force", defaults(), opts)
end

---@return PitonConfig
function M.get()
    return options
end

--- Require a boolean result: a mistakenly supplied vim.fn.confirm (which returns a number) must never approve deletion.
---@param message string
---@return boolean
function M.confirm(message)
    local result = options.confirm(message)
    if type(result) ~= "boolean" then
        error("piton.confirm must return a boolean", 2)
    end
    return result
end

return M
