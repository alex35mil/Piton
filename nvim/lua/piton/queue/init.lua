-- The source's queue: queued (Todo and Backlog) work from the nearest
-- `[tracker]` of piton's configuration chain (`config.toml`), and incoming
-- pull requests from the nearest `[forge]`, each read with the credential its
-- own MCP registration names. All of it comes from `piton config get`; the
-- plugin reads no configuration file itself. Keys are as the file has them
-- (snake_case: `mcp_server`, `owner_type`).

---@class QueueParent
---@field mention string How a workflow command names the Epic.
---@field marker string? Short reference, when the tracker has one.
---@field title string

---@alias QueueKind
---| { kind: "epic" }
---| { kind: "task", epic: QueueParent }
---| { kind: "gig" }
---| { kind: "pr", board: QueueStatus? } An incoming pull request; `board` is its Project Status when it's on the board.
---| { kind: "unclassified" } Top-level work in a tracker with no Epic Kind label configured to tell an Epic from a Gig.

---@alias QueueStatus
---| "todo"
---| "backlog"

--- Where the picker lists an item: issues by their queue status, every incoming pull request in one section after them.
---@alias QueueSection
---| "todo"
---| "backlog"
---| "pull_requests"

---@class QueueItem
---@field id string Stable identity, stored as the agent's `agent.issue`.
---@field section QueueSection
---@field mention string How a workflow command names the item.
---@field marker string? Short reference, when the tracker has one (`#12`, `ENG-42`).
---@field title string
---@field url string
---@field kind QueueKind

--- One configured role (`[tracker]` or `[forge]`) and how to reach it.
---@class QueueConnection
---@field source string
---@field config table The role's table of `config.toml`.
---@field credential string Name of the environment variable holding the token.

---@alias QueueTrackerRole
---| { kind: "off", reason: string } Why there's no tracker, for the refusal when there's no forge either.
---| { kind: "github", connection: QueueConnection }
---| { kind: "linear", connection: QueueConnection }

---@alias QueueForgeRole
---| { kind: "none", reason: string } No forge that has pull requests to list: missing, off, or not GitHub.
---| { kind: "github", connection: QueueConnection }

--- What a tracker contributes: its queued issues in listing order, and the
--- pull requests on its board (a GitHub Project), in board order.
---@class QueueTrackerWork
---@field issues QueueItem[]
---@field board QueueItem[]

local util = require("piton.util")
local cli = require("piton.cli")

local M = {}

--- Queued items, Todo before Backlog, each in tracker priority order, then
--- the forge's incoming pull requests. Refuses when a configured role can't be
--- read, or when there's neither a tracker nor a GitHub forge.
---@param source string
---@return QueueItem[]
function M.load(source)
    local tracker = M.tracker(source)
    local forge = M.forge(source)
    if tracker.kind == "off" and forge.kind == "none" then
        util.refuse(("No tracker or GitHub forge configured: %s; %s"):format(tracker.reason, forge.reason))
    end
    local work = M.tracker_work(tracker)
    return M.group_by_section(vim.list_extend(work.issues, M.pull_requests(forge, work.board)))
end

---@param tracker QueueTrackerRole
---@return QueueTrackerWork
function M.tracker_work(tracker)
    if tracker.kind == "off" then
        return { issues = {}, board = {} }
    elseif tracker.kind == "github" then
        return require("piton.queue.github").load(tracker.connection)
    elseif tracker.kind == "linear" then
        return require("piton.queue.linear").load(tracker.connection)
    end
    error("unknown tracker role: " .. vim.inspect(tracker))
end

--- The forge's incoming pull requests, merged with the tracker's board ones.
---@param forge QueueForgeRole
---@param board QueueItem[]
---@return QueueItem[]
function M.pull_requests(forge, board)
    if forge.kind == "none" then
        return {}
    elseif forge.kind == "github" then
        local github = require("piton.queue.github")
        local open, viewer = github.pull_requests(forge.connection)
        local repository = forge.connection.config.repository
        return github.incoming(repository.owner .. "/" .. repository.repo, board, open, viewer)
    end
    error("unknown forge role: " .. vim.inspect(forge))
end

--- The nearest `[tracker]`; refuses when it's unusable.
---@param source string
---@return QueueTrackerRole
function M.tracker(source)
    local found = cli.config("tracker", source)
    if type(found) ~= "table" then
        return { kind = "off", reason = "no [tracker] in " .. source .. "/.piton/config.toml or its parent folders" }
    end
    local file = tostring(found.path)
    local tracker = found.value
    if type(tracker) ~= "table" then
        util.refuse("Invalid [tracker] in " .. file)
    end
    if tracker.provider == "none" then
        return { kind = "off", reason = "the tracker is turned off (provider = \"none\" in " .. file .. ")" }
    end
    if tracker.provider ~= "github" and tracker.provider ~= "linear" then
        util.refuse("Unsupported tracker provider in " .. file .. ": " .. tostring(tracker.provider))
    end
    return { kind = tracker.provider, connection = M.connect(source, "tracker", file, tracker) }
end

--- The nearest `[forge]`. Only a GitHub forge lists pull requests; any other
--- is none. Refuses when a GitHub forge is unusable.
---@param source string
---@return QueueForgeRole
function M.forge(source)
    local found = cli.config("forge", source)
    if type(found) ~= "table" then
        return { kind = "none", reason = "no [forge] in " .. source .. "/.piton/config.toml or its parent folders" }
    end
    local file = tostring(found.path)
    local forge = found.value
    if type(forge) ~= "table" then
        util.refuse("Invalid [forge] in " .. file)
    end
    if forge.provider ~= "github" then
        return {
            kind = "none",
            reason = ("the forge isn't GitHub (provider = %s in %s)"):format(vim.inspect(forge.provider), file),
        }
    end
    local repository = forge.repository
    if
        type(repository) ~= "table"
        or type(repository.owner) ~= "string"
        or type(repository.repo) ~= "string"
    then
        util.refuse("The [forge] in " .. file .. " has no repository (owner and repo)")
    end
    return { kind = "github", connection = M.connect(source, "forge", file, forge) }
end

---@param source string
---@param role "tracker"|"forge"
---@param file string
---@param config table
---@return QueueConnection
function M.connect(source, role, file, config)
    if type(config.mcp_server) ~= "string" then
        util.refuse("The [" .. role .. "] in " .. file .. " has no mcp_server")
    end
    return { source = source, config = config, credential = M.credential(source, config.mcp_server) }
end

--- The environment variable the MCP registration authorizes with, from the
--- same resolved MCP servers piton's pi package registers.
---@param source string
---@param server string
---@return string
function M.credential(source, server)
    local mcp = cli.config("mcp", source)
    local registration
    for _, entry in ipairs(type(mcp) == "table" and type(mcp.servers) == "table" and mcp.servers or {}) do
        if type(entry) == "table" and entry.name == server then
            registration = entry.config
        end
    end
    if type(registration) ~= "table" then
        util.refuse("MCP server " .. server .. " isn't registered in " .. source .. "'s configuration (mcp.json)")
    end
    local authorization = type(registration.headers) == "table" and registration.headers.Authorization or nil
    local variable = type(authorization) == "string" and authorization:match("^Bearer %${([%a_][%w_]*)}$")
    if not variable then
        util.refuse("MCP server " .. server .. " has no `Authorization: Bearer ${VAR}` header")
    end
    return variable
end

---@class QueueResponse
---@field status integer HTTP status.
---@field body any Decoded JSON body.

--- POST a GraphQL request with curl, inside the source's mise environment so
--- the credential resolves there and never passes through Neovim.
---@param connection QueueConnection
---@param url string
---@param authorization string Authorization header, with `{{VAR}}` for the credential.
---@param query string
---@param variables table
---@return QueueResponse
function M.post(connection, url, authorization, query, variables)
    local var = connection.credential
    local result = util.exec({
        "mise",
        "exec",
        "-C",
        connection.source,
        "--",
        "curl",
        "--silent",
        "--show-error",
        "--max-time",
        "20",
        "--variable",
        "%" .. var,
        "--expand-header",
        "Authorization: " .. authorization:gsub("{{VAR}}", "{{" .. var .. "}}"),
        "--header",
        "Content-Type: application/json",
        "--data-binary",
        "@-",
        "--write-out",
        "\n%{http_code}",
        url,
    }, {
        -- An empty Lua table encodes as a JSON array; GraphQL needs an object.
        stdin = vim.json.encode({ query = query, variables = next(variables) and variables or vim.empty_dict() }),
    })

    if result.code ~= 0 then
        local stderr = vim.trim(result.stderr or "")
        if stderr:find("Variable '" .. var .. "' import fail", 1, true) then
            util.refuse(var .. " isn't set in the environment of " .. connection.source)
        end
        util.refuse("Request to " .. url .. " failed: " .. stderr)
    end

    local body, status = (result.stdout or ""):match("^(.*)\n(%d+)$")
    if not status then
        util.refuse("Unexpected response from " .. url)
    end
    local ok, decoded = pcall(vim.json.decode, body, { luanil = { object = true, array = true } })
    return {
        status = tonumber(status) --[[@as integer]],
        body = ok and decoded or body,
    }
end

--- The `data` of a successful GraphQL response; refuses otherwise.
---@param url string
---@param response QueueResponse
---@return table
function M.data(url, response)
    local body = response.body
    local errors = type(body) == "table" and body.errors or nil
    if response.status ~= 200 or type(errors) == "table" or type(body) ~= "table" or type(body.data) ~= "table" then
        util.refuse(
            ("Request to %s failed (HTTP %d):\n%s"):format(url, response.status, M.error_detail(response))
        )
    end
    return body.data
end

--- The error messages of a failed GraphQL response, or its raw body.
---@param response QueueResponse
---@return string
function M.error_detail(response)
    local body = response.body
    local errors = type(body) == "table" and body.errors or nil
    local messages = {}
    for _, err in ipairs(type(errors) == "table" and errors or {}) do
        table.insert(messages, type(err) == "table" and tostring(err.message) or tostring(err))
    end
    return #messages > 0 and table.concat(messages, "\n") or vim.inspect(body):sub(1, 300)
end

--- Fetch every page of a connection. `page` receives the cursor and returns
--- the connection's `nodes` and `pageInfo`.
---@param page fun(cursor: string?): {nodes: table[], pageInfo: {hasNextPage: boolean, endCursor: string?}}
---@return table[]
function M.paginate(page)
    local nodes, cursor = {}, nil
    for _ = 1, 50 do
        local connection = page(cursor)
        vim.list_extend(nodes, connection.nodes)
        if not connection.pageInfo.hasNextPage then
            return nodes
        end
        cursor = connection.pageInfo.endCursor
    end
    util.refuse("Too many pages of results")
    error("unreachable")
end

-- Queue statuses, in the order the picker lists them.
---@type QueueStatus[]
M.STATUSES = { "todo", "backlog" }

-- Picker sections, top to bottom.
---@type QueueSection[]
M.SECTIONS = { "todo", "backlog", "pull_requests" }

--- The queue status a tracker status name maps to; nil for any other status.
---@param name string?
---@param values {todo: string, backlog: string} Configured status names.
---@return QueueStatus?
function M.status_of(name, values)
    for _, status in ipairs(M.STATUSES) do
        if name == values[status] then
            return status
        end
    end
    return nil
end

--- `items` regrouped by section in `SECTIONS` order, keeping their order
--- within a section.
---@param items QueueItem[]
---@return QueueItem[]
function M.group_by_section(items)
    local result = {}
    for _, section in ipairs(M.SECTIONS) do
        for _, item in ipairs(items) do
            if item.section == section then
                table.insert(result, item)
            end
        end
    end
    return result
end

---@class QueueOrder
---@field rank integer? Priority rank, 1 the highest; nil when unset.
---@field position number The tracker's manual order.

--- Sort `items` the way tracker views order by priority: by rank, unset
--- last, then by manual position.
---@generic T
---@param items T[]
---@param order fun(item: T): QueueOrder
function M.sort_by_priority(items, order)
    local keys = {}
    for _, item in ipairs(items) do
        keys[item] = order(item)
    end
    table.sort(items, function(a, b)
        local ka, kb = keys[a], keys[b]
        if ka.rank ~= kb.rank then
            if ka.rank == nil or kb.rank == nil then
                return kb.rank == nil
            end
            return ka.rank < kb.rank
        end
        return ka.position < kb.position
    end)
end

--- The workflow command that starts `item`, naming it by reference and
--- title, with a trailing space so comments can follow; nil when its kind
--- is unknown.
---@param item QueueItem
---@return string?
function M.command(item)
    local subject = item.mention .. " " .. item.title
    local kind = item.kind
    if kind.kind == "epic" then
        return "/epic " .. subject .. " "
    elseif kind.kind == "task" then
        return "/task " .. subject .. " (Epic " .. kind.epic.mention .. ") "
    elseif kind.kind == "gig" then
        return "/gig " .. subject .. " "
    elseif kind.kind == "pr" then
        return "/pr " .. subject .. " "
    elseif kind.kind == "unclassified" then
        return nil
    end
    error("unknown queue kind: " .. vim.inspect(kind))
end

local NAME_MAX = 48

--- Agent directory name: `<marker>-<title slug>`, cut at a word.
---@param item QueueItem
---@return string
function M.agent_name(item)
    local name = M.slugify((item.marker or "") .. " " .. item.title)
    if #name > NAME_MAX then
        name = name:sub(1, NAME_MAX):gsub("%-[^%-]*$", "")
    end
    return name
end

--- Agent label: the marker and the full title.
---@param item QueueItem
---@return string
function M.agent_label(item)
    return item.marker and (item.marker .. " " .. item.title) or item.title
end

---@param value string
---@return string
function M.slugify(value)
    local slug = value:lower():gsub("[^a-z0-9]+", "-"):gsub("^%-+", ""):gsub("%-+$", "")
    return slug
end

return M
