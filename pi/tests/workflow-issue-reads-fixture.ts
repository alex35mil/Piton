// Tracker reads for a Task issue, in the shapes real MCP responses use: GitHub `issue_read` `get` and
// `get_parent` (`{"parent": null}` or the parent summary, checked against real issues), Linear `get_issue`.
// Task initialization, routing, restore, and the resume parent check take these as `reads`.

const text = (value: unknown) => ({ content: [{ type: "text", text: JSON.stringify(value) }] })

export interface GitHubIssueFixture {
    number: number
    title?: string
    body?: string
    labels?: string[]
    /** The parent issue's number, or null for a top-level issue. */
    parent?: number | null
    /** The parent issue's labels; given only when the parent isn't a local Epic. */
    parentLabels?: string[]
    repository?: { owner: string; repo: string }
    server?: string
}

export function githubIssueRead(
    method: "get" | "get_parent",
    issueNumber: number,
    response: unknown,
    repository = { owner: "example", repo: "project" },
    server = "github",
) {
    return {
        tool: `mcp__${server}__issue_read`,
        arguments: { method, ...repository, issue_number: issueNumber },
        response: text(response),
    }
}

export function githubIssue(
    number: number,
    extra: Record<string, unknown> = {},
    repository = { owner: "example", repo: "project" },
) {
    return {
        id: 1000 + number,
        number,
        title: `Issue ${number}`,
        body: `Request ${number}.`,
        html_url: `https://github.com/${repository.owner}/${repository.repo}/issues/${number}`,
        labels: [],
        ...extra,
    }
}

/** The reads `init_task` and its neighbours take for a GitHub issue. */
export function githubReads(fixture: GitHubIssueFixture) {
    const repository = fixture.repository ?? { owner: "example", repo: "project" }
    const server = fixture.server ?? "github"
    const parent = fixture.parent ?? null
    return {
        issue: githubIssueRead(
            "get",
            fixture.number,
            githubIssue(
                fixture.number,
                {
                    ...(fixture.title ? { title: fixture.title } : {}),
                    ...(fixture.body !== undefined ? { body: fixture.body } : {}),
                    labels: fixture.labels ?? [],
                },
                repository,
            ),
            repository,
            server,
        ),
        hierarchy: githubIssueRead(
            "get_parent",
            fixture.number,
            {
                parent:
                    parent === null
                        ? null
                        : { number: parent, title: "Parent", url: "u", repository: "r", state: "OPEN" },
            },
            repository,
            server,
        ),
        ...(fixture.parentLabels !== undefined && parent !== null
            ? {
                  parent: githubIssueRead(
                      "get",
                      parent,
                      githubIssue(parent, { labels: fixture.parentLabels }, repository),
                      repository,
                      server,
                  ),
              }
            : {}),
    }
}

export interface LinearIssueFixture {
    identifier: string
    /** The issue UUID; derived from the identifier unless given. */
    uuid?: string
    title?: string
    projectId?: string
    gitBranchName?: string
    labels?: string[]
    server?: string
}

export function linearReads(fixture: LinearIssueFixture) {
    const server = fixture.server ?? "linear"
    const uuid = fixture.uuid ?? `${fixture.identifier.toLowerCase()}-uuid`
    return {
        issue: {
            tool: `mcp__${server}__get_issue`,
            arguments: { id: uuid },
            response: text({
                id: fixture.identifier,
                uuid,
                title: fixture.title ?? `Issue ${fixture.identifier}`,
                description: `Request ${fixture.identifier}.`,
                url: `https://linear.app/example/issue/${fixture.identifier}/work`,
                labels: (fixture.labels ?? []).map((name) => ({ name })),
                ...(fixture.projectId ? { projectId: fixture.projectId } : {}),
                ...(fixture.gitBranchName ? { gitBranchName: fixture.gitBranchName } : {}),
            }),
        },
    }
}
