/**
 * Web Search Extension
 *
 * Registers a `web_search` tool using Brave Search API.
 * 1,000 free queries/month.
 *
 * Auth: BRAVE_SEARCH_API_KEY from env var or settings.
 *
 * Settings: `[web_search]` of config.local.toml in piton's configuration chain (the global folder, parent folders, the
 * repository's .piton/), resolved by `piton config resolve`; the nearest api_key wins. It's a secret: a config.toml that
 * sets it is refused.
 *
 * Schema:
 *   [web_search]
 *   api_key = "..."
 */

import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent"
import { Type } from "typebox"

import { resolveSettings, section } from "../__lib/settings.js"

const EXTENSION = "web-search"

/** The configured key, or why the configuration couldn't be read. */
function configuredKey(cwd: string): { apiKey?: string } | { error: string } {
    try {
        return { apiKey: section(resolveSettings(cwd).web_search).api_key }
    } catch (error) {
        return { error: error instanceof Error ? error.message : String(error) }
    }
}

interface BraveSearchResult {
    title?: string
    url?: string
    description?: string
}

interface BraveSearchResponse {
    web?: { results?: BraveSearchResult[] }
    error?: string
}

export default async function (pi: ExtensionAPI) {
    const settings = configuredKey(process.cwd())

    pi.registerTool({
        name: "web_search",
        label: "web_search",
        description: "Search the web using Brave Search. Returns the top 10 results with title, URL, and snippet.",
        parameters: Type.Object({
            query: Type.String({ description: "The search query" }),
        }),

        async execute(_toolCallId, params, signal, _onUpdate, _ctx) {
            const apiKey = process.env.BRAVE_SEARCH_API_KEY ?? ("apiKey" in settings ? settings.apiKey : undefined)

            if (!apiKey && "error" in settings) {
                return {
                    details: undefined,
                    content: [{ type: "text" as const, text: `Web search settings can't be read: ${settings.error}` }],
                }
            }
            if (!apiKey) {
                return {
                    details: undefined,
                    content: [
                        {
                            type: "text" as const,
                            text: "Web search is not configured. Set BRAVE_SEARCH_API_KEY as an environment variable, or set api_key in [web_search] of Piton's config.local.toml.",
                        },
                    ],
                }
            }

            const query = (params as { query: string }).query
            const url = new URL("https://api.search.brave.com/res/v1/web/search")
            url.searchParams.set("q", query)
            url.searchParams.set("count", "10")

            try {
                const response = await fetch(url.toString(), {
                    headers: {
                        Accept: "application/json",
                        "Accept-Encoding": "gzip",
                        "X-Subscription-Token": apiKey,
                    },
                    signal: signal ?? undefined,
                })

                if (!response.ok) {
                    const body = await response.text()
                    return {
                        details: undefined,
                        content: [
                            {
                                type: "text" as const,
                                text: `Brave Search API error: HTTP ${response.status}\n${body}`,
                            },
                        ],
                    }
                }

                const data = (await response.json()) as BraveSearchResponse

                if (data.error) {
                    return {
                        details: undefined,
                        content: [
                            {
                                type: "text" as const,
                                text: `Brave Search API error: ${data.error}`,
                            },
                        ],
                    }
                }

                const items = data.web?.results ?? []

                if (items.length === 0) {
                    return {
                        details: undefined,
                        content: [
                            {
                                type: "text" as const,
                                text: `No results found for: ${query}`,
                            },
                        ],
                    }
                }

                const text = items
                    .map((item, i) => {
                        const title = item.title ?? "(no title)"
                        const link = item.url ?? ""
                        const snippet = item.description ?? ""
                        return `${i + 1}. ${title}\n   ${link}\n   ${snippet}`
                    })
                    .join("\n\n")

                return {
                    details: undefined,
                    content: [{ type: "text" as const, text }],
                }
            } catch (err: unknown) {
                const message = err instanceof Error ? err.message : String(err)
                return {
                    details: undefined,
                    content: [
                        {
                            type: "text" as const,
                            text: `Web search failed: ${message}`,
                        },
                    ],
                }
            }
        },
    })

    const apiKey = process.env.BRAVE_SEARCH_API_KEY ?? ("apiKey" in settings ? settings.apiKey : undefined)
    if (!apiKey) {
        const announceStartup = (ctx: ExtensionContext) => {
            ctx.ui.setWidget(`${EXTENSION}:startup`, [`Missing: BRAVE_SEARCH_API_KEY`])
        }
        pi.on("session_start", async (_event, ctx) => announceStartup(ctx))
    }
}
