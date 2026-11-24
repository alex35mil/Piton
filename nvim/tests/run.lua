-- Local, dependency-free tests: real headless Neovim, fake pi/Snacks/daemon/tracker boundaries.
-- Run from the repository root through `mise run nvim:test`.

local plugin = vim.fs.joinpath(vim.fn.getcwd(), "nvim")
vim.opt.runtimepath:prepend(plugin)

local tests = {}
local restores = {}
local scratch = vim.fn.tempname()
vim.fn.mkdir(scratch, "p")

local function test(name, run)
    tests[#tests + 1] = { name = name, run = run }
end

local function equal(expected, actual)
    assert(vim.deep_equal(expected, actual), "expected " .. vim.inspect(expected) .. ", got " .. vim.inspect(actual))
end

local function replace(object, key, value)
    local before = object[key]
    restores[#restores + 1] = function()
        object[key] = before
    end
    object[key] = value
end

local function fails(pattern, run)
    local ok, err = pcall(run)
    assert(not ok, "expected an error")
    -- Refusals are tables carrying the message shown to the user.
    local message = type(err) == "table" and tostring(err.message) or tostring(err)
    assert(message:find(pattern, 1, true), message)
end

-- A test must explicitly fake these boundaries: never call the installed daemon or real trackers.
vim.system = function()
    error("unexpected subprocess in nvim tests")
end

local before_globals = {}
for key in pairs(_G) do
    before_globals[key] = true
end
local before_maps = {}
for _, mode in ipairs({ "n", "i", "v", "t" }) do
    before_maps[mode] = vim.api.nvim_get_keymap(mode)
end

local piton = require("piton")
local config = require("piton.config")
local tabs = require("piton.tabs")
local close = require("piton.close")
local create = require("piton.create")
local lookup = require("piton.lookup")
local cli = require("piton.cli")
local util = require("piton.util")

local function directory(name)
    local path = vim.fs.joinpath(scratch, name)
    vim.fn.mkdir(path, "p")
    return vim.uv.fs_realpath(path)
end

local function fake_pi(show)
    replace(package.loaded, "pi", { show = show })
end

local function fake_picker(open)
    replace(package.loaded, "snacks", { picker = open })
end

-- Every tab asks the daemon about trust first: a repository with nothing to trust unless a test says otherwise.
local trust_state = { state = "nothing_to_trust", grants = {}, added = {}, resources = { pi = {}, mcp_servers = {} } }
cli.trust_status = function(path)
    return { source = path, trust = trust_state }
end

test("every module loads without dependencies, globals, or mappings", function()
    local files = vim.fn.globpath(plugin .. "/lua", "**/*.lua", false, true)
    assert(#files > 0)
    for _, file in ipairs(files) do
        local module = file:sub(#plugin + #"/lua/" + 1):gsub("%.lua$", ""):gsub("/init$", ""):gsub("/", ".")
        assert(type(require(module)) == "table", module)
    end
    piton.setup()
    assert(package.loaded.pi == nil and package.loaded.snacks == nil)
    for key in pairs(_G) do
        assert(before_globals[key], "plugin created global " .. key)
    end
    for mode, maps in pairs(before_maps) do
        equal(maps, vim.api.nvim_get_keymap(mode))
    end
    local exports = vim.tbl_keys(piton)
    table.sort(exports)
    equal({ "close_tab", "get", "new", "pick", "setup" }, exports)
end)

test("configuration merges defaults, resets, and rejects invalid options atomically", function()
    local initial = config.get()
    local new_tab = function() end
    piton.setup({ tabs = { new = new_tab }, icons = { source = "S" } })
    equal(new_tab, config.get().tabs.new)
    equal("S", config.get().icons.source)
    equal(initial.icons.agent, config.get().icons.agent)
    local configured = config.get()
    for _, invalid in ipairs({
        false,
        "bad",
        { surprise = true },
        { tabs = { new = true } },
        { tabs = { unknown = function() end } },
        { icons = { agent = 3 } },
        { icons = { worker = "W" } },
        { confirm = true },
        { queue = { with_credential = true } },
        { queue = { credential = function() end } },
    }) do
        local ok = pcall(piton.setup, invalid)
        assert(not ok, vim.inspect(invalid))
        equal(configured, config.get())
    end
    piton.setup()
    equal(initial.icons, config.get().icons)
    assert(config.get().tabs.new ~= new_tab)
end)

test("default confirmation is no by default; custom confirmation must return boolean", function()
    replace(vim.fn, "confirm", function(message, choices, default)
        equal("Really?", message)
        equal("&Yes\n&No", choices)
        equal(2, default)
        return 2
    end)
    equal(false, config.confirm("Really?"))
    replace(vim.fn, "confirm", function() return 1 end)
    equal(true, config.confirm("Really?"))
    piton.setup({ confirm = function() return true end })
    equal(true, config.confirm("Really?"))
    piton.setup({ confirm = function() return 1 end })
    fails("must return a boolean", function() config.confirm("Really?") end)
end)

test("default tab opens the relative file and calls pi.show after setting cwd and label", function()
    local path = directory("default agent")
    vim.fn.writefile({ "hello" }, path .. "/file.txt")
    local shown = 0
    fake_pi(function(opts)
        shown = shown + 1
        equal({ layout = "float", draft = "/gig test " }, opts)
        equal(path, vim.fn.getcwd())
        equal(path .. "/file.txt", vim.api.nvim_buf_get_name(0))
        equal({ icon = config.get().icons.agent, name = "Fix login" }, vim.t.piton_label)
    end)
    local count = #vim.api.nvim_list_tabpages()
    tabs.open({ path = path, label = "Fix login" }, "file.txt", "/gig test ")
    equal(count + 1, #vim.api.nvim_list_tabpages())
    equal(1, shown)
    equal(vim.api.nvim_get_current_tabpage(), tabs.find(path))
end)

test("tab hooks run in order and already-loaded modules see new options", function()
    local path = directory("custom agent")
    local source = directory("custom source")
    local calls = {}
    local old_cwd = vim.fn.getcwd()
    piton.setup({
        icons = { agent = "A", source = "S" },
        tabs = {
            new = function()
                calls[#calls + 1] = "new"
                -- A caller can dismiss its welcome screen here, before creating a tab.
                equal(old_cwd, vim.fn.getcwd())
                vim.cmd("tabnew")
            end,
            set_label = function(label)
                calls[#calls + 1] = label
                equal(#calls == 2 and path or source, vim.fn.getcwd())
            end,
        },
    })
    fake_pi(function(opts)
        calls[#calls + 1] = "chat"
        equal("float", opts.layout)
        equal(nil, opts.draft)
    end)
    tabs.open({ path = path, label = "Agent" })
    equal({ "new", { icon = "A", name = "Agent" }, "chat" }, calls)
    old_cwd = path
    tabs.switch_to_source(source)
    equal({ "new", { icon = "A", name = "Agent" }, "chat", "new", { icon = "S", name = "custom source" } }, calls)
    local count = #vim.api.nvim_list_tabpages()
    tabs.switch_to({ path = path, label = "Agent" })
    equal(path, vim.fn.getcwd())
    tabs.switch_to_source(source)
    equal(source, vim.fn.getcwd())
    equal(count, #vim.api.nvim_list_tabpages())
    equal(5, #calls)
end)

test("a repository that needs trust is asked about before its agent's tab opens", function()
    local path = directory("untrusted agent")
    replace(cli, "trust_status", function(p)
        equal(path, p)
        return {
            source = "/src",
            trust = {
                state = "needed",
                grants = { 'sandbox.presets "rust"', 'sandbox.presets "swift"' },
                added = { 'sandbox.presets "swift"' },
                resources = { pi = { ".pi/extensions" }, mcp_servers = {} },
            },
        }
    end)
    local asked, decided = nil, {}
    piton.setup({ confirm = function(message)
        asked = message
        return false
    end })
    replace(cli, "trust_decide", function(p, decision)
        decided[#decided + 1] = { p, decision }
    end)
    local shown = 0
    fake_pi(function()
        shown = shown + 1
    end)
    tabs.open({ path = path, label = "Agent" })
    assert(asked:find('sandbox.presets "swift"  (new)', 1, true), asked)
    assert(not asked:find('"rust"  (new)', 1, true), asked)
    assert(asked:find(".pi/extensions", 1, true), asked)
    equal({ { path, "deny" } }, decided)
    equal(1, shown, "declining still opens the agent, without the grants")
end)

test("a decided repository opens without a question", function()
    local path = directory("trusted agent")
    replace(trust_state, "state", "trusted")
    piton.setup({ confirm = function() error("must not ask") end })
    replace(cli, "trust_decide", function() error("must not record") end)
    local shown = 0
    fake_pi(function()
        shown = shown + 1
    end)
    tabs.open({ path = path, label = "Agent" })
    equal(1, shown)
end)

test("tab labels truncate Unicode at a word and source tabs don't open chat", function()
    equal("Short", tabs.tab_label("Short"))
    local long = ("猫 "):rep(30)
    assert(vim.fn.strchars(tabs.tab_label(long)) <= 41)
    assert(vim.endswith(tabs.tab_label(long), "…"))
    fake_pi(function() error("source must not open chat") end)
    local source = directory("source only")
    tabs.switch_to_source(source, "missing.txt")
    equal({ icon = config.get().icons.source, name = "source only" }, vim.t.piton_label)
end)

test("picker uses configured icons, explicit Snacks module, and current worker activity", function()
    local source = directory("picker source")
    local agent = { path = directory("picker worker"), source = source, label = "Worker" }
    piton.setup({ icons = { agent = "A", source = "S" } })
    replace(lookup, "current_source", function() return source, source end)
    replace(lookup, "list", function()
        return { { agent = agent, sessions = { { key = 1, status = "working" } } } }
    end)
    local opened = false
    fake_picker(function(opts)
        opened = true
        equal("Agents", opts.title)
        equal("S ", opts.format(opts.items[1])[1][1])
        local parts = opts.format(opts.items[2])
        equal("A ", parts[1][1])
        equal(" · working", parts[3][1])
    end)
    piton.pick()
    assert(opened)
end)

test("new-agent picker loads mocked queue without credentials or real providers", function()
    local source = directory("queue source")
    replace(lookup, "current_source", function() return source, source end)
    replace(lookup, "list", function() return {} end)
    local queue = require("piton.queue")
    replace(queue, "load", function() return {} end)
    local picker = { closed = false, update_titles = function() end, find = function() end }
    fake_picker(function(opts)
        assert(opts.title:find("loading", 1, true))
        equal("named", opts.finder()[1].entry.kind)
        return picker
    end)
    piton.new()
    assert(picker.title:find("0 Todo", 1, true))
end)

-- Queue fakes: `piton config get` answers for the tracker, the forge, and two MCP registrations, and one GraphQL
-- endpoint per provider. Nothing reaches a real daemon or provider.
local GITHUB_TRACKER = {
    provider = "github",
    mcp_server = "github-tracker",
    repository = { owner = "o", repo = "r" },
    project = { owner = "o", owner_type = "user", number = 1 },
    fields = {
        status = { field = "Status", values = { todo = "Todo", backlog = "Backlog" } },
        priority = { field = "Priority", values = {} },
    },
    labels = { groups = { kind = { values = { epic = { name = "Kind: Epic" } } } } },
}
local LINEAR_TRACKER = {
    provider = "linear",
    mcp_server = "linear",
    team = "ENG",
    statuses = {
        projects = { todo = "Planned", backlog = "Backlog" },
        issues = { todo = "Todo", backlog = "Backlog" },
    },
}
local GITHUB_FORGE = { provider = "github", mcp_server = "github", repository = { owner = "o", repo = "r" } }

local function fake_roles(source, tracker, forge)
    local function part(value)
        if value == nil then
            return { code = 0, stdout = "null" }
        end
        return { code = 0, stdout = vim.json.encode({ path = source .. "/.piton/config.toml", value = value }) }
    end
    local responses = {
        tracker = part(tracker),
        forge = part(forge),
        mcp = {
            code = 0,
            stdout = vim.json.encode({
                servers = {
                    { name = "github", config = { headers = { Authorization = "Bearer ${FORGE_TOKEN}" } } },
                    { name = "github-tracker", config = { headers = { Authorization = "Bearer ${TRACKER_TOKEN}" } } },
                    { name = "linear", config = { headers = { Authorization = "Bearer ${LINEAR_KEY}" } } },
                },
            }),
        },
    }
    replace(util, "exec", function(cmd)
        equal({ "piton", "config", "get" }, { cmd[1], cmd[2], cmd[3] })
        return assert(responses[cmd[4]], cmd[4])
    end)
end

local function board_item(status, typename, number, title, repository)
    return {
        status = { name = status },
        content = {
            __typename = typename,
            number = number,
            title = title,
            url = "https://github.com/o/r/" .. number,
            state = "OPEN",
            repository = { nameWithOwner = repository or "o/r" },
            labels = { nodes = {} },
        },
    }
end

local function pull_request(number, title, author)
    return { number = number, title = title, url = "https://github.com/o/r/" .. number, author = author }
end

-- Answers each provider query and records which ones ran: { query, credential, detail }.
local function fake_providers(responses)
    local queue = require("piton.queue")
    local requests = {}
    replace(queue, "post", function(connection, _, _, query, variables)
        local function ok(data)
            return { status = 200, body = { data = data } }
        end
        local function page(nodes)
            return { nodes = nodes, pageInfo = { hasNextPage = false } }
        end
        if query:find("pullRequests", 1, true) then
            table.insert(requests, { "pull_requests", connection.credential, variables.owner .. "/" .. variables.repo })
            return ok({ viewer = { login = "me" }, repository = { pullRequests = page(responses.open or {}) } })
        elseif query:find("projectV2", 1, true) then
            table.insert(requests, { "board", connection.credential })
            return ok({ owner = { projectV2 = { items = page(responses.board or {}) } } })
        elseif query:find("viewer { id }", 1, true) then
            return ok({ viewer = { id = "u" } })
        elseif query:find("projects(", 1, true) then
            return ok({ projects = page({}) })
        elseif query:find("issues(", 1, true) then
            table.insert(requests, { "linear", connection.credential, variables.state })
            return ok({ issues = page(variables.state == "Todo" and responses.linear or {}) })
        end
        error("unexpected query: " .. query)
    end)
    return requests
end

local function summary(items)
    return vim.tbl_map(function(item)
        return { item.id, item.section, item.kind.kind, item.kind.board }
    end, items)
end

local OPEN = {
    pull_request(5, "Off-board PR", { login = "alice" }),
    pull_request(7, "Board PR", { login = "bob" }),
    pull_request(8, "My PR", { login = "me" }),
    pull_request(9, "My board PR", { login = "me" }),
    pull_request(10, "Ghost PR"),
}

test("GitHub tracker and forge list each incoming pull request once, board ones first with their Status", function()
    local source = directory("github queue")
    local queue = require("piton.queue")
    fake_roles(source, GITHUB_TRACKER, GITHUB_FORGE)
    local requests = fake_providers({
        board = {
            board_item("Backlog", "PullRequest", 7, "Board PR"),
            board_item("Todo", "Issue", 3, "An issue"),
            board_item("Todo", "PullRequest", 9, "My board PR"),
            -- On the board, but not an open PR of the forge's repository.
            board_item("Todo", "PullRequest", 11, "Merged PR"),
            board_item("Todo", "PullRequest", 4, "Elsewhere", "o/other"),
            board_item("Todo", "Issue", 6, "Other repository's issue", "o/other"),
        },
        open = OPEN,
    })
    local items = queue.load(source)
    equal({
        { "github:o/r#3", "todo", "gig", nil },
        { "github:o/r#7", "pull_requests", "pr", "backlog" },
        { "github:o/r#5", "pull_requests", "pr", nil },
        { "github:o/r#10", "pull_requests", "pr", nil },
    }, summary(items))
    equal("/pr #7 Board PR ", queue.command(items[2]))
    -- Each role reads with its own registration's credential.
    equal({ { "board", "TRACKER_TOKEN" }, { "pull_requests", "FORGE_TOKEN", "o/r" } }, requests)
end)

test("Linear tracker with a GitHub forge lists the forge's incoming pull requests after the issues", function()
    local source = directory("linear queue")
    local queue = require("piton.queue")
    fake_roles(source, LINEAR_TRACKER, {
        provider = "github",
        mcp_server = "github",
        repository = { owner = "acme", repo = "app" },
    })
    local requests = fake_providers({
        linear = {
            {
                id = "i1",
                identifier = "ENG-1",
                title = "A gig",
                url = "https://linear.app/i1",
                priority = 0,
                prioritySortOrder = 1,
            },
        },
        open = OPEN,
    })
    local items = queue.load(source)
    equal({
        { "linear:issue:i1", "todo", "gig", nil },
        { "github:acme/app#5", "pull_requests", "pr", nil },
        { "github:acme/app#7", "pull_requests", "pr", nil },
        { "github:acme/app#10", "pull_requests", "pr", nil },
    }, summary(items))
    equal({
        { "linear", "LINEAR_KEY", "Todo" },
        { "linear", "LINEAR_KEY", "Backlog" },
        { "pull_requests", "FORGE_TOKEN", "acme/app" },
    }, requests)
end)

test("without a GitHub forge the queue lists no pull requests, board ones included", function()
    local source = directory("no forge queue")
    local queue = require("piton.queue")
    local board = {
        board_item("Todo", "Issue", 3, "An issue"),
        board_item("Backlog", "PullRequest", 7, "Board PR"),
    }
    for _, forge in ipairs({ vim.NIL, { provider = "none" }, { provider = "gitlab", mcp_server = "github" } }) do
        fake_roles(source, GITHUB_TRACKER, forge ~= vim.NIL and forge or nil)
        local requests = fake_providers({ board = board, open = OPEN })
        equal({ { "github:o/r#3", "todo", "gig", nil } }, summary(queue.load(source)))
        equal({ { "board", "TRACKER_TOKEN" } }, requests)
    end
    fake_roles(source, LINEAR_TRACKER, { provider = "gitlab" })
    fake_providers({ linear = {}, open = OPEN })
    equal({}, queue.load(source))
end)

test("without a tracker a GitHub forge still lists its pull requests", function()
    local source = directory("forge only queue")
    local queue = require("piton.queue")
    for _, tracker in ipairs({ vim.NIL, { provider = "none" } }) do
        fake_roles(source, tracker ~= vim.NIL and tracker or nil, GITHUB_FORGE)
        local requests = fake_providers({ open = OPEN })
        equal({
            { "github:o/r#5", "pull_requests", "pr", nil },
            { "github:o/r#7", "pull_requests", "pr", nil },
            { "github:o/r#10", "pull_requests", "pr", nil },
        }, summary(queue.load(source)))
        equal({ { "pull_requests", "FORGE_TOKEN", "o/r" } }, requests)
    end
end)

test("queue reads its tracker and credential through piton config get", function()
    local source = directory("config source")
    local queue = require("piton.queue")
    local calls = {}
    local responses = {
        tracker = {
            code = 0,
            stdout = vim.json.encode({
                path = source .. "/.piton/config.toml",
                value = { provider = "other", mcp_server = "linear" },
            }),
        },
        mcp = {
            code = 0,
            stdout = vim.json.encode({
                servers = {
                    { name = "linear", config = { headers = { Authorization = "Bearer ${LINEAR_KEY}" } } },
                },
            }),
        },
    }
    replace(util, "exec", function(cmd)
        equal({ "piton", "config", "get" }, { cmd[1], cmd[2], cmd[3] })
        equal({ "--workspace", source }, { cmd[5], cmd[6] })
        calls[#calls + 1] = cmd[4]
        return responses[cmd[4]]
    end)
    equal("LINEAR_KEY", queue.credential(source, "linear"))
    fails("Unsupported tracker provider in " .. source .. "/.piton/config.toml", function() queue.load(source) end)
    equal({ "mcp", "tracker" }, calls)

    -- Neither a tracker nor a GitHub forge: the queue is unavailable.
    responses.forge = { code = 0, stdout = "null" }
    responses.tracker = { code = 0, stdout = "null" }
    fails("No tracker or GitHub forge configured: no [tracker] in " .. source, function() queue.load(source) end)
    responses.tracker = {
        code = 0,
        stdout = vim.json.encode({ path = source .. "/.piton/config.toml", value = { provider = "none" } }),
    }
    fails("the tracker is turned off", function() queue.load(source) end)
    -- A GitHub forge must name its registration and repository.
    responses.forge = {
        code = 0,
        stdout = vim.json.encode({ path = source .. "/.piton/config.toml", value = { provider = "github" } }),
    }
    fails("has no repository", function() queue.load(source) end)
    responses.forge = {
        code = 0,
        stdout = vim.json.encode({
            path = source .. "/.piton/config.toml",
            value = { provider = "github", repository = { owner = "o", repo = "r" } },
        }),
    }
    fails("The [forge] in " .. source .. "/.piton/config.toml has no mcp_server", function() queue.load(source) end)
    -- A broken file: the resolver's message, unchanged.
    responses.tracker = {
        code = 3,
        stdout = vim.json.encode({ format = 1, error = { kind = "invalid", message = "invalid config.toml" } }),
    }
    fails("invalid config.toml", function() queue.load(source) end)
end)

test("queue requests run curl in the source through the configured credential hook", function()
    local queue = require("piton.queue")
    local connection = { source = "/p/repo", config = {}, credential = "TOKEN" }
    local runs = {}
    local result = { code = 0, stdout = '{"data":{}}\n200' }
    replace(util, "exec", function(cmd, opts)
        runs[#runs + 1] = { cmd = cmd, cwd = opts.cwd, stdin = opts.stdin }
        return result
    end)
    local function post()
        return queue.post(connection, "https://api", "Bearer {{VAR}}", "query", {})
    end

    -- Default: bare curl, which imports the variable itself; the value never appears in the command.
    equal({ status = 200, body = { data = {} } }, post())
    local curl = runs[1].cmd
    equal("curl", curl[1])
    assert(vim.list_contains(curl, "%TOKEN") and vim.list_contains(curl, "Authorization: Bearer {{TOKEN}}"))
    equal("/p/repo", runs[1].cwd)
    equal({ query = "query", variables = vim.empty_dict() }, vim.json.decode(runs[1].stdin))

    -- A configured hook gets curl and the credential, and its command is what runs.
    local seen
    piton.setup({ queue = { with_credential = function(command, credential)
        seen = credential
        return vim.list_extend({ "fnox", "exec", "--only", credential.variable, "--" }, command)
    end } })
    post()
    equal({ variable = "TOKEN", source = "/p/repo" }, seen)
    equal(vim.list_extend({ "fnox", "exec", "--only", "TOKEN", "--" }, curl), runs[2].cmd)
    equal("/p/repo", runs[2].cwd)

    -- A hook that doesn't return a command refuses before running anything.
    for _, invalid in ipairs({ {}, "curl", { 1 }, { cmd = "curl" } }) do
        piton.setup({ queue = { with_credential = function() return invalid end } })
        fails("must return a command", post)
    end
    equal(2, #runs)

    -- curl can't import the variable: name it, and point at the option.
    piton.setup()
    result = { code = 2, stderr = "curl: Variable 'TOKEN' import failure" }
    fails("TOKEN isn't set for queue requests in /p/repo. Supply it through piton's queue.with_credential", post)
end)

test("close_tab dispatches close or delete through the public API", function()
    local agent = { path = directory("close worker"), label = "Close" }
    local opts
    fake_picker(function(value) opts = value end)
    piton.close_tab(agent)
    equal("Close: Close", opts.title)
    local deleted
    replace(close, "delete", function(value) deleted = value end)
    local picker = { close = function() end }
    opts.confirm(picker, { action = "delete" })
    equal(agent, deleted)
    vim.cmd("tabnew")
    local count = #vim.api.nvim_list_tabpages()
    opts.confirm(picker, { action = "close-tab" })
    equal(count - 1, #vim.api.nvim_list_tabpages())
end)

test("refused worker deletion uses configured confirmation and doesn't force on no", function()
    local agent = { path = directory("kept worker"), label = "Kept" }
    local forced = {}
    replace(cli, "delete", function(path, force)
        equal(agent.path, path)
        forced[#forced + 1] = force
        return { kind = "refused", refusal = { kind = "leftovers", path = path, lines = { "unpushed commit" } } }
    end)
    local asked = false
    piton.setup({ confirm = function(message)
        asked = true
        assert(message:find("unpushed commit", 1, true))
        return false
    end })
    close.delete(agent)
    assert(asked)
    equal({ false }, forced)
end)

test("approved worker deletion retries with force through the daemon", function()
    local agent = { path = directory("deleted worker"), label = "Delete" }
    local forced = {}
    replace(cli, "delete", function(path, force)
        equal(agent.path, path)
        forced[#forced + 1] = force
        if force then return { kind = "deleted", stopped = 1 } end
        return { kind = "refused", refusal = { kind = "leftovers", path = path, lines = { "dirty" } } }
    end)
    piton.setup({ confirm = function() return true end })
    close.delete(agent)
    equal({ false, true }, forced)
end)

local PROTECTED = {
    kind = "protected_update",
    checkout = "/p/repo",
    upstream = "origin/main",
    target = string.rep("a", 40),
    files = { ".piton/config.toml" },
}

-- Answers each confirmation in turn, recording the questions.
local function answers(list)
    local asked = {}
    piton.setup({ confirm = function(message)
        asked[#asked + 1] = message
        return table.remove(list, 1)
    end })
    return asked
end

test("a protected source update asks before deleting, and the tab closes only after", function()
    local agent = { path = directory("protected worker"), label = "Protected" }
    vim.cmd("tabnew")
    vim.cmd("tcd " .. vim.fn.fnameescape(agent.path))
    local tabs_before = #vim.api.nvim_list_tabpages()
    local calls = {}
    replace(cli, "delete", function(path, force, update)
        calls[#calls + 1] = { force, update and update.kind or "ask", update and update.target }
        if not update then return { kind = "refused", refusal = PROTECTED } end
        return { kind = "deleted", stopped = 0 }
    end)

    -- No to both: kept, the tab with it.
    local asked = answers({ false, false })
    close.delete(agent)
    equal({ { false, "ask" } }, calls)
    assert(asked[1]:find(".piton/config.toml", 1, true) and asked[1]:find("apply these changes", 1, true))
    assert(not asked[1]:find("outside the sandbox", 1, true))
    assert(asked[2]:find("without updating", 1, true))
    equal(tabs_before, #vim.api.nvim_list_tabpages())

    -- Skip the update.
    calls = {}
    answers({ false, true })
    close.delete(agent)
    equal({ { false, "ask" }, { false, "skip" } }, calls)
    equal(tabs_before - 1, #vim.api.nvim_list_tabpages())

    -- Apply the changes, after leftovers were forced.
    calls = {}
    replace(cli, "delete", function(path, force, update)
        calls[#calls + 1] = { force, update and update.kind or "ask", update and update.target }
        if not force then
            return { kind = "refused", refusal = { kind = "leftovers", path = path, lines = { "dirty" } } }
        end
        if not update then return { kind = "refused", refusal = PROTECTED } end
        return { kind = "deleted", stopped = 0 }
    end)
    answers({ true, true })
    close.delete(agent)
    equal({ { false, "ask" }, { true, "ask" }, { true, "approve", PROTECTED.target } }, calls)
end)

test("a protected source update asks before creating, and retries with the approval", function()
    local source = directory("protected source")
    replace(lookup, "current_source", function() return source, source end)
    replace(tabs, "relative_file", function() return nil end)
    local opened = {}
    replace(tabs, "open", function(agent) opened[#opened + 1] = agent.path end)
    local updates = {}
    replace(cli, "create", function(request)
        updates[#updates + 1] = request.update and request.update.target or "ask"
        if not request.update then return { kind = "refused", refusal = PROTECTED } end
        return { kind = "created", worker = { path = source .. "--x", source = source, label = "x" } }
    end)

    local asked = answers({ false })
    create.create({ kind = "named", name = "x" })
    equal({ "ask" }, updates)
    equal({}, opened)
    assert(asked[1]:find("run in the sandbox", 1, true))

    updates = {}
    answers({ true })
    create.create({ kind = "named", name = "x" })
    equal({ "ask", PROTECTED.target }, updates)
    equal({ source .. "--x" }, opened)
end)

test("cli passes the answers and describes the new refusals", function()
    local commands = {}
    replace(util, "exec", function(cmd)
        commands[#commands + 1] = table.concat(cmd, " ")
        return { code = 0, stdout = vim.json.encode({ type = "worker_deleted", stopped = 0 }) }
    end)
    cli.delete("/w", true, { kind = "approve", target = PROTECTED.target })
    cli.delete("/w", false, { kind = "skip" })
    equal({
        "piton worker delete /w --force --approve-source-update " .. PROTECTED.target .. " --json",
        "piton worker delete /w --skip-source-update --json",
    }, commands)
    assert(cli.describe(PROTECTED):find("origin/main", 1, true))
    equal("no socket", cli.describe({ kind = "source_fetch", checkout = "/p", message = "no socket" }))
end)

test("unsaved buffers require confirmation before any daemon deletion", function()
    local agent = { path = directory("unsaved worker"), label = "Unsaved" }
    local buf = vim.api.nvim_create_buf(true, false)
    vim.api.nvim_buf_set_name(buf, agent.path .. "/unsaved.txt")
    vim.api.nvim_buf_set_lines(buf, 0, -1, false, { "unsaved" })
    replace(cli, "delete", function() error("must not call daemon after no") end)
    local asked = false
    piton.setup({ confirm = function(message)
        asked = true
        assert(message:find("unsaved.txt", 1, true))
        return false
    end })
    close.delete(agent)
    assert(asked)
    assert(vim.api.nvim_buf_is_valid(buf) and vim.bo[buf].modified)
    vim.api.nvim_buf_delete(buf, { force = true })
end)

test("an agent.source claim counts only with the worker layout", function()
    local function git(results)
        replace(util, "git_root", function(path) return path end)
        replace(util, "git", function(_, args)
            local value = results[args[#args]]
            return value and { code = 0, stdout = value .. "\n" } or { code = 1, stdout = "" }
        end)
    end
    git({ ["agent.source"] = "/p/repo", ["agent.label"] = "Fix" })
    equal({ path = "/p/repo--fix", source = "/p/repo", label = "Fix" }, lookup.get("/p/repo--fix"))
    equal(nil, lookup.get("/q/repo--fix"))
    equal(nil, lookup.get("/p/elsewhere"))
    equal(nil, lookup.get("/p/repo--"))
end)

test("get delegates lookup and notifications use vim.notify", function()
    replace(lookup, "get", function(path)
        equal("/test/path", path)
        return { path = path, source = "/test", label = "Test" }
    end)
    equal("Test", piton.get("/test/path").label)
    local calls = {}
    replace(vim, "notify", function(message, level, opts)
        calls[#calls + 1] = { message, level, opts.title }
    end)
    require("piton.notify").info("info")
    util.async(function() util.refuse("no") end)
    equal({ { "info", vim.log.levels.INFO, "Piton" }, { "no", vim.log.levels.WARN, "Piton" } }, calls)
end)

local failed = 0
local original_notify = vim.notify
for _, case in ipairs(tests) do
    config.setup()
    -- Unexpected coroutine errors must fail the test, not disappear in a UI notification.
    vim.notify = function(message, level)
        if level == vim.log.levels.ERROR then error(message) end
    end
    local ok, err = xpcall(case.run, debug.traceback)
    for i = #restores, 1, -1 do restores[i]() end
    restores = {}
    vim.notify = original_notify
    vim.cmd("silent! tabonly!")
    if ok then
        print("ok   " .. case.name)
    else
        failed = failed + 1
        print("FAIL " .. case.name .. "\n" .. err)
    end
end
vim.fn.delete(scratch, "rf")
print(("%d tests, %d failed"):format(#tests, failed))
if failed > 0 then vim.cmd("cquit 1") else vim.cmd("qa!") end
