-- Notifications go through Neovim's public UI entry point, not a user-config global.

local M = {}

function M.info(message)
    vim.notify(message, vim.log.levels.INFO, { title = "Piton" })
end

function M.warn(message)
    vim.notify(message, vim.log.levels.WARN, { title = "Piton" })
end

function M.error(message)
    vim.notify(message, vim.log.levels.ERROR, { title = "Piton" })
end

return M
