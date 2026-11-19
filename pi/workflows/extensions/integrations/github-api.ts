import { registeredCredentialEnvironment } from "./mcp.js"

export interface GitHubApiOptions {
    projectTrusted: boolean
    fetch?: typeof fetch
    env?: NodeJS.ProcessEnv
}

export interface GitHubApi {
    graphql<T>(query: string, variables: Record<string, unknown>): Promise<T>
    rest(method: "PUT", pathname: string): Promise<unknown>
}

/**
 * GitHub's API with the token the registered GitHub MCP server uses, for the operations the official MCP server lacks.
 * Errors carry only GitHub's messages, an HTTP status, or a fetch error name, never the request or its credential.
 */
export function githubApi(cwd: string, mcpServer: string, options: GitHubApiOptions): GitHubApi {
    const credential = registeredCredentialEnvironment(cwd, mcpServer, "github", options.projectTrusted)
    const token = (options.env ?? process.env)[credential]
    if (!token) throw new Error(`environment variable ${credential} is not set`)
    const send = async (pathname: string, init: { method: string; body?: string }) => {
        try {
            return await (options.fetch ?? fetch)(`https://api.github.com${pathname}`, {
                ...init,
                headers: {
                    Authorization: `Bearer ${token}`,
                    Accept: "application/vnd.github+json",
                    ...(init.body === undefined ? {} : { "Content-Type": "application/json" }),
                },
            })
        } catch (error) {
            throw new Error(`network request failed (${error instanceof Error ? error.name : "unknown error"})`)
        }
    }
    return {
        async graphql<T>(query: string, variables: Record<string, unknown>): Promise<T> {
            const response = await send("/graphql", { method: "POST", body: JSON.stringify({ query, variables }) })
            const body = (await response.json().catch(() => ({}))) as {
                data?: T
                errors?: Array<{ message?: string }>
            }
            if (!response.ok || body.errors?.length || !body.data)
                throw new Error(body.errors?.map((error) => error.message).join("; ") || `HTTP ${response.status}`)
            return body.data
        },
        async rest(method, pathname) {
            const response = await send(pathname, { method })
            const body = (await response.json().catch(() => undefined)) as { message?: unknown } | undefined
            if (!response.ok)
                throw new Error(
                    typeof body?.message === "string" && body.message
                        ? `HTTP ${response.status}: ${body.message}`
                        : `HTTP ${response.status}`,
                )
            return body
        },
    }
}
