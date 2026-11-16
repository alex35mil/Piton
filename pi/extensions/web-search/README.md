# Web Search Extension

Registers a `web_search` tool that searches the web using the [Brave Search API](https://brave.com/search/api/). Returns the top 10 results with title, URL, and snippet.

Free tier: 1,000 queries/month.

## Configuration

Requires a Brave Search API key. This can be provided via an environment variable or settings.

### Environment Variable

```sh
export BRAVE_SEARCH_API_KEY="..."
```

### Settings

`[web_search]` of `config.local.toml` (the key is a secret, so a `config.toml` that sets it is refused) in any level of Piton's `.piton/` chain: the global folder `$XDG_CONFIG_HOME/piton/` (default `~/.config/piton/`), parent folders' `.piton/`, and `<repo-root>/.piton/`. The nearest value wins; `piton config resolve` does the merging.

```toml
[web_search]
api_key = "..."
```

The environment variable takes precedence over settings.

## Behavior

- If the API key is missing, the tool returns a configuration error message and a startup widget shows the missing key
- Results are formatted as a numbered list with title, link, and snippet
