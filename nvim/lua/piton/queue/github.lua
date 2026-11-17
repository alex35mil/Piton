-- Todo and Backlog items of a GitHub Project: Todo first, each ordered by the
-- configured Priority, then by the Project's manual order. Then incoming pull
-- requests: the open ones of the configured repository not authored by the
-- viewer, on the board or not, listed once.

local util = require("piton.util")
local queue = require("piton.queue")

local M = {}

local URL = "https://api.github.com/graphql"

---@param owner_type string
---@return string
local function items_query(owner_type)
    return ([[
query($owner: String!, $number: Int!, $status: String!, $priority: String!, $cursor: String) {
  owner: %s(login: $owner) {
    projectV2(number: $number) {
      items(first: 100, after: $cursor, orderBy: {field: POSITION, direction: ASC}) {
        pageInfo { hasNextPage endCursor }
        nodes {
          status: fieldValueByName(name: $status) {
            ... on ProjectV2ItemFieldSingleSelectValue { name }
          }
          priority: fieldValueByName(name: $priority) {
            ... on ProjectV2ItemFieldSingleSelectValue { name }
            ... on ProjectV2ItemIssueFieldValue {
              issueFieldValue { ... on IssueFieldSingleSelectValue { name } }
            }
          }
          content {
            __typename
            ... on PullRequest {
              number title url state
              repository { nameWithOwner }
            }
            ... on Issue {
              number title url state
              repository { nameWithOwner }
              labels(first: 50) { nodes { name } }
              parent { number title labels(first: 50) { nodes { name } } }
            }
          }
        }
      }
    }
  }
}]]):format(owner_type == "org" and "organization" or "user")
end

local PULL_REQUESTS = [[
query($owner: String!, $repo: String!, $cursor: String) {
  viewer { login }
  repository(owner: $owner, name: $repo) {
    pullRequests(first: 100, after: $cursor, states: OPEN, orderBy: {field: CREATED_AT, direction: ASC}) {
      pageInfo { hasNextPage endCursor }
      nodes { number title url author { login } }
    }
  }
}]]

---@class GitHubPullRequest
---@field number integer
---@field title string
---@field url string
---@field author string? Login; nil for a deleted account.

--- Open pull requests of the repository, oldest first, and the viewer's login.
---@param tracker QueueTracker
---@return GitHubPullRequest[], string
function M.pull_requests(tracker)
    local config = tracker.config
    local viewer ---@type string?
    local nodes = queue.paginate(function(cursor)
        local response = queue.post(tracker, URL, "Bearer {{VAR}}", PULL_REQUESTS, {
            owner = config.repository.owner,
            repo = config.repository.repo,
            cursor = cursor,
        })
        local data = queue.data(URL, response)
        viewer = vim.tbl_get(data, "viewer", "login")
        local connection = vim.tbl_get(data, "repository", "pullRequests")
        if not connection then
            util.refuse(
                ("GitHub repository %s/%s not found"):format(config.repository.owner, config.repository.repo)
            )
        end
        return connection
    end)
    if type(viewer) ~= "string" then
        util.refuse("GitHub didn't return the authenticated user")
    end
    ---@cast viewer string
    local result = {} ---@type GitHubPullRequest[]
    for _, node in ipairs(nodes) do
        table.insert(result, {
            number = node.number,
            title = node.title,
            url = node.url,
            author = vim.tbl_get(node, "author", "login"),
        })
    end
    return result, viewer
end

--- Incoming pull requests: every open PR not authored by `viewer`, board PRs
--- in board order first (they carry their Status), then the rest oldest
--- first. Each PR appears once.
---@param repository string `owner/repo`
---@param board QueueItem[] Board pull requests, in board order.
---@param open GitHubPullRequest[]
---@param viewer string
---@return QueueItem[]
function M.incoming(repository, board, open, viewer)
    local authors = {} ---@type table<string, string?>
    local listed = {} ---@type table<string, true>
    for _, pr in ipairs(open) do
        authors["#" .. pr.number] = pr.author
    end
    local result = {} ---@type QueueItem[]
    local function add(item, author)
        if author ~= viewer and not listed[item.id] then
            listed[item.id] = true
            table.insert(result, item)
        end
    end
    for _, item in ipairs(board) do
        add(item, authors[item.marker])
    end
    for _, pr in ipairs(open) do
        local marker = "#" .. pr.number
        ---@type QueueItem
        local item = {
            id = "github:" .. repository .. marker,
            section = "pull_requests",
            mention = marker,
            marker = marker,
            title = pr.title,
            url = pr.url,
            kind = { kind = "pr" },
        }
        add(item, pr.author)
    end
    return result
end

---@param tracker QueueTracker
---@return QueueItem[]
function M.load(tracker)
    local config = tracker.config
    local project = config.project
    local status = config.fields.status
    local priority = config.fields.priority
    local repository = config.repository.owner .. "/" .. config.repository.repo
    local epic_label = vim.tbl_get(config, "labels", "groups", "kind", "values", "epic", "name")

    local nodes = queue.paginate(function(cursor)
        local response = queue.post(tracker, URL, "Bearer {{VAR}}", items_query(project.owner_type), {
            owner = project.owner,
            number = project.number,
            status = status.field,
            priority = priority.field,
            cursor = cursor,
        })
        local items = vim.tbl_get(queue.data(URL, response), "owner", "projectV2", "items")
        if not items then
            util.refuse(("GitHub Project %s/%d not found"):format(project.owner, project.number))
        end
        return items
    end)

    local result = {} ---@type QueueItem[]
    local orders = {} ---@type table<QueueItem, QueueOrder>
    for position, node in ipairs(nodes) do
        local content = node.content
        local queue_status = queue.status_of(vim.tbl_get(node, "status", "name"), status.values)
        if
            queue_status
            and type(content) == "table"
            and content.number
            and content.state == "OPEN"
            and vim.tbl_get(content, "repository", "nameWithOwner") == repository
        then
            -- Issues and pull requests share one number sequence per repository.
            local marker = "#" .. content.number
            local pull_request = content.__typename == "PullRequest"
            ---@type QueueItem
            local item = {
                id = "github:" .. repository .. marker,
                section = pull_request and "pull_requests" or queue_status,
                mention = marker,
                marker = marker,
                title = content.title,
                url = content.url,
                kind = pull_request and { kind = "pr", board = queue_status } or M.kind(content, epic_label),
            }
            table.insert(result, item)
            orders[item] = { rank = M.priority_rank(node.priority, priority.values), position = position }
        end
    end
    queue.sort_by_priority(result, function(item)
        return orders[item]
    end)
    local issues, board = {}, {} ---@type QueueItem[], QueueItem[]
    for _, item in ipairs(result) do
        table.insert(item.section == "pull_requests" and board or issues, item)
    end
    local open, viewer = M.pull_requests(tracker)
    return queue.group_by_section(vim.list_extend(issues, M.incoming(repository, board, open, viewer)))
end

--- Rank of an item's Priority value in the configured order, whether the
--- field is the Project's own or an issue field shown on the Project; nil
--- when unset or not a configured value.
---@param value table? The item's `priority` field value.
---@param values string[] Configured Priority options, highest first.
---@return integer?
function M.priority_rank(value, values)
    if type(value) ~= "table" then
        return nil
    end
    local name = value.name or vim.tbl_get(value, "issueFieldValue", "name")
    for rank, option in ipairs(values) do
        if option == name then
            return rank
        end
    end
    return nil
end

---@param issue table
---@param label string?
---@return boolean
local function has_label(issue, label)
    for _, node in ipairs(vim.tbl_get(issue, "labels", "nodes") or {}) do
        if node.name == label then
            return true
        end
    end
    return false
end

--- A Task is a sub-issue of an Epic; an Epic or a Gig is top-level, told
--- apart by the configured Epic Kind label.
---@param issue table
---@param epic_label string?
---@return QueueKind
function M.kind(issue, epic_label)
    local parent = issue.parent
    if type(parent) == "table" and (not epic_label or has_label(parent, epic_label)) then
        local marker = "#" .. parent.number
        return { kind = "task", epic = { mention = marker, marker = marker, title = parent.title } }
    end
    if not epic_label then
        return { kind = "unclassified" }
    end
    if has_label(issue, epic_label) then
        return { kind = "epic" }
    end
    return { kind = "gig" }
end

return M
