//! What pi can reach and which models it can use: one source for the network gate (which hosts pi may reach without
//! asking) and the apps' provider selector.
//!
//! Trusted, from what a sandboxed session can't write:
//! - pi's built-in providers and their sign-in hosts, read from pi's installation by running `builtin.mjs` with Node on
//!   the host, once per pi package folder (an upgrade installs a new folder);
//! - `baseUrl`s in the agent directory's `models.json`;
//! - remote MCP servers' `url`s from the user's own MCP files: the global level's `mcp.json` and `mcp.local.json`, and
//!   every level's `mcp.local.json`. A repository's committed `.piton/mcp.json` travels with the repository, so its hosts
//!   aren't trusted: they're asked about like any host.
//!
//! Discovered, untrusted: `models-store.json`, which pi rewrites at startup and the sandbox can write. Its models are
//! listed for a selector, never used for hosts: an agent could point a cached model anywhere.
//!
//! Files are read again when their modification time changes, so an edit applies to the next connection.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::SystemTime,
};

/// Where a trusted host came from: what `piton sandbox why` and `show` name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostSource {
    /// A provider built into pi.
    Provider { provider: String },
    /// A host pi signs in or refreshes a login through.
    SignIn,
    /// A `baseUrl` in pi's `models.json`.
    ModelsJson { path: PathBuf },
    /// A remote MCP server in one of the user's MCP files.
    Mcp { server: String, path: PathBuf },
}

impl std::fmt::Display for HostSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostSource::Provider { provider } => write!(f, "pi's built-in provider {provider}"),
            HostSource::SignIn => write!(f, "a host pi signs in through"),
            HostSource::ModelsJson { path } => write!(f, "a baseUrl in {}", path.display()),
            HostSource::Mcp { server, path } => write!(f, "the MCP server {server} in {}", path.display()),
        }
    }
}

/// A host pi may reach, and on which port: `None` for any (a built-in provider's or sign-in host, named without a
/// port, is reached on its scheme's).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct TrustedHost {
    pub host: String,
    pub port: Option<u16>,
    pub source: HostSource,
}

/// A model pi found in its catalog cache: listed, never trusted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DiscoveredModel {
    pub provider: String,
    pub id: String,
    pub base_url: Option<String>,
}

/// What pi's built-in catalog says, read from its installation.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct Builtin {
    pub providers: BTreeMap<String, Vec<String>>,
    #[serde(rename = "signIn")]
    pub sign_in: Vec<String>,
}

/// Why pi's built-in hosts couldn't be read: the gate then trusts only the files', and asks about the rest.
#[derive(Debug, thiserror::Error)]
pub enum BuiltinError {
    #[error("pi's package folder wasn't found from {launcher}")]
    NoPackage { launcher: String },
    #[error("can't run node: {0}")]
    Spawn(std::io::Error),
    #[error("reading pi's catalog failed: {0}")]
    Failed(String),
    #[error("pi's catalog isn't valid: {0}")]
    Invalid(String),
}

/// The script that reads pi's catalog.
const BUILTIN_SCRIPT: &str = include_str!("builtin.mjs");

/// pi's installed package folder, from the launcher `pi` resolves to: npm's link into the package's `dist/`, or a
/// shim naming the package's `cli.js` (aube's `# aube-bin-shim v2 target=...`).
pub fn package_folder(launcher: &Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(launcher).ok()?;
    let from_file = |file: &Path| {
        file.ancestors()
            .find(|dir| dir.ends_with("@earendil-works/pi-coding-agent") && dir.join("package.json").is_file())
            .map(Path::to_path_buf)
    };
    if let Some(folder) = from_file(&real) {
        return Some(folder);
    }
    let text = std::fs::read_to_string(&real).ok()?;
    let target = text.lines().take(3).find_map(|l| l.split("target=").nth(1))?.trim();
    let shim_target = real.parent()?.join(target);
    from_file(&shim_target)
}

/// pi's built-in hosts for the pi `command` runs, with `env` (pi's environment: its `PATH` finds pi and node). Read once
/// per pi package folder: an upgrade installs a new one.
pub fn builtin_for(command: &str, env: &crate::launch::Env) -> Result<Builtin, BuiltinError> {
    static READ: Mutex<Vec<(PathBuf, Builtin)>> = Mutex::new(Vec::new());
    let launcher = crate::launch::resolve_program(command, env);
    let package = package_folder(Path::new(&launcher)).ok_or_else(|| BuiltinError::NoPackage {
        launcher: launcher.clone(),
    })?;
    if let Some((_, builtin)) = crate::session::lock(&READ).iter().find(|(p, _)| *p == package) {
        return Ok(builtin.clone());
    }
    // The node next to pi's launcher first (a bundled one), else pi's PATH.
    let beside = Path::new(&launcher)
        .parent()
        .map(|d| d.join("node"))
        .filter(|n| n.is_file());
    let node = beside.unwrap_or_else(|| PathBuf::from(crate::launch::resolve_program("node", env)));
    let builtin = read_builtin(&node, &package, env)?;
    crate::session::lock(&READ).push((package, builtin.clone()));
    Ok(builtin)
}

/// Runs `builtin.mjs` with `node` against pi's package folder.
pub fn read_builtin(node: &Path, package: &Path, env: &crate::launch::Env) -> Result<Builtin, BuiltinError> {
    let out = std::process::Command::new(node)
        .args(["--input-type=module", "-e", BUILTIN_SCRIPT, "--"])
        .arg(package)
        .env_clear()
        .envs(env)
        .current_dir(package)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(BuiltinError::Spawn)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(BuiltinError::Failed(
            stderr
                .lines()
                .find(|l| l.contains("Error"))
                .unwrap_or(stderr.trim())
                .to_string(),
        ));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| BuiltinError::Invalid(e.to_string()))
}

/// The files the catalog reads for one launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Files {
    /// The launch's agent directory: `models.json` and `models-store.json`.
    pub agent_dir: PathBuf,
    /// The user's own MCP files, farthest first.
    pub mcp: Vec<PathBuf>,
}

impl Files {
    /// The MCP files a launch with these configuration levels trusts: the global level's `mcp.json` and
    /// `mcp.local.json`, and every level's `mcp.local.json`. A repository's or parent folder's committed `mcp.json` is
    /// left out.
    pub fn new(agent_dir: PathBuf, global: &Path, levels: &[PathBuf]) -> Self {
        let mut mcp = vec![global.join("mcp.json")];
        for level in std::iter::once(&global.to_path_buf()).chain(levels.iter().filter(|l| *l != global)) {
            mcp.push(level.join("mcp.local.json"));
        }
        mcp.dedup();
        Self { agent_dir, mcp }
    }
}

/// One launch's catalog: pi's built-in hosts, read once, and the files, read again when they change.
#[derive(Debug)]
pub struct Catalog {
    builtin: Builtin,
    files: Files,
    cache: Mutex<Cache>,
}

#[derive(Debug, Default)]
struct Cache {
    /// Each file's modification time when it was read (`None`: absent).
    seen: Vec<(PathBuf, Option<SystemTime>)>,
    hosts: Vec<TrustedHost>,
}

impl Catalog {
    pub fn new(builtin: Builtin, files: Files) -> Self {
        Self {
            builtin,
            files,
            cache: Mutex::new(Cache::default()),
        }
    }

    /// Every host pi may reach without asking, current as of now.
    pub fn trusted(&self) -> Vec<TrustedHost> {
        let mut paths = vec![self.files.agent_dir.join("models.json")];
        paths.extend(self.files.mcp.iter().cloned());
        let seen: Vec<(PathBuf, Option<SystemTime>)> = paths
            .into_iter()
            .map(|p| {
                let time = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
                (p, time)
            })
            .collect();
        let mut cache = crate::session::lock(&self.cache);
        if cache.seen != seen || cache.hosts.is_empty() {
            cache.hosts = self.read();
            cache.seen = seen;
        }
        cache.hosts.clone()
    }

    /// The trusted host that allows `host` on `port`, if one does.
    pub fn allows(&self, host: &str, port: u16) -> Option<TrustedHost> {
        self.trusted()
            .into_iter()
            .find(|t| t.host == host && t.port.is_none_or(|p| p == port))
    }

    fn read(&self) -> Vec<TrustedHost> {
        let mut hosts = Vec::new();
        for (provider, list) in &self.builtin.providers {
            for host in list {
                hosts.push(host_entry(
                    host,
                    HostSource::Provider {
                        provider: provider.clone(),
                    },
                ));
            }
        }
        for host in &self.builtin.sign_in {
            hosts.push(host_entry(host, HostSource::SignIn));
        }
        let models = self.files.agent_dir.join("models.json");
        for url in models_json_urls(&models) {
            if let Some(host) = url_host(&url) {
                hosts.push(TrustedHost {
                    source: HostSource::ModelsJson { path: models.clone() },
                    ..host
                });
            }
        }
        for path in &self.files.mcp {
            for (server, url) in mcp_urls(path) {
                if let Some(host) = url_host(&url) {
                    hosts.push(TrustedHost {
                        source: HostSource::Mcp {
                            server,
                            path: path.clone(),
                        },
                        ..host
                    });
                }
            }
        }
        // A host on this Mac (a local model server) is never the gate's: the sandbox decides local ports, so it needs a
        // `localhost:PORT` entry, and the gate wouldn't dial a local address anyway.
        hosts.retain(|h| !local_host(&h.host));
        hosts.sort();
        hosts.dedup_by(|a, b| a.host == b.host && a.port == b.port);
        hosts
    }

    /// The models in pi's catalog cache: for a selector, never for hosts.
    pub fn discovered(&self) -> Vec<DiscoveredModel> {
        discovered(&self.files.agent_dir.join("models-store.json"))
    }
}

/// Whether a host names this Mac: `localhost`, a name under `.localhost`, or a loopback address.
fn local_host(host: &str) -> bool {
    host == "localhost"
        || host.ends_with(".localhost")
        || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// A built-in host as pi names it (`host` or `host:port`).
fn host_entry(text: &str, source: HostSource) -> TrustedHost {
    let (host, port) = match text.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().ok()),
        None => (text, None),
    };
    TrustedHost {
        host: host.to_ascii_lowercase(),
        port,
        source,
    }
}

/// A URL's host and port (the port only when the URL names one), for `http`/`https`/`ws`/`wss` URLs.
fn url_host(url: &str) -> Option<TrustedHost> {
    let (scheme, rest) = url.split_once("://")?;
    if !matches!(scheme, "http" | "https" | "ws" | "wss") {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (host, rest) = v6.split_once(']')?;
            (host, rest.strip_prefix(':').and_then(|p| p.parse().ok()))
        }
        None => match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port.parse().ok()?)),
            None => (authority, None),
        },
    };
    match host.is_empty() || host.contains(['$', '{', ' ']) {
        true => None,
        false => Some(TrustedHost {
            host: host.to_ascii_lowercase(),
            port,
            source: HostSource::SignIn,
        }),
    }
}

/// Every `baseUrl` in pi's `models.json`: each provider's and each of its models'.
fn models_json_urls(path: &Path) -> Vec<String> {
    let Some(json) = read_json(path) else {
        return Vec::new();
    };
    let mut urls = Vec::new();
    let providers = json.get("providers").and_then(serde_json::Value::as_object);
    for provider in providers.into_iter().flat_map(|p| p.values()) {
        if let Some(url) = provider.get("baseUrl").and_then(serde_json::Value::as_str) {
            urls.push(url.to_string());
        }
        let models = provider.get("models").and_then(serde_json::Value::as_array);
        for model in models.into_iter().flatten() {
            if let Some(url) = model.get("baseUrl").and_then(serde_json::Value::as_str) {
                urls.push(url.to_string());
            }
        }
    }
    urls
}

/// Each remote MCP server's name and `url` in an `mcp[.local].json`.
fn mcp_urls(path: &Path) -> Vec<(String, String)> {
    let Some(json) = read_json(path) else {
        return Vec::new();
    };
    let servers = json.get("mcpServers").and_then(serde_json::Value::as_object);
    servers
        .into_iter()
        .flatten()
        .filter_map(|(name, server)| {
            let url = server.get("url").and_then(serde_json::Value::as_str)?;
            Some((name.clone(), url.to_string()))
        })
        .collect()
}

fn discovered(path: &Path) -> Vec<DiscoveredModel> {
    let Some(json) = read_json(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (provider, entry) in json.as_object().into_iter().flatten() {
        let models = entry.get("models").and_then(serde_json::Value::as_array);
        for model in models.into_iter().flatten() {
            let Some(id) = model.get("id").and_then(serde_json::Value::as_str) else {
                continue;
            };
            out.push(DiscoveredModel {
                provider: provider.clone(),
                id: id.to_string(),
                base_url: model
                    .get("baseUrl")
                    .and_then(serde_json::Value::as_str)
                    .map(String::from),
            });
        }
    }
    out
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin() -> Builtin {
        Builtin {
            providers: [
                ("anthropic".to_string(), vec!["api.anthropic.com".to_string()]),
                ("deepseek".to_string(), vec!["api.deepseek.com".to_string()]),
            ]
            .into(),
            sign_in: vec!["platform.claude.com".into()],
        }
    }

    /// pi's built-in and sign-in hosts, `models.json`'s `baseUrl`s, and the user's own MCP files are trusted; a
    /// repository's committed `mcp.json` and the catalog cache aren't.
    #[test]
    fn trusts_only_what_the_agent_cant_write() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (agent, global, repo) = (root.join("agent"), root.join("global"), root.join("repo/.piton"));
        for d in [&agent, &global, &repo] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(
            agent.join("models.json"),
            r#"{"providers":{"ollama":{"baseUrl":"http://localhost:11434/v1","models":[{"id":"x","baseUrl":"https://gpu.example.net:8443/v1"}]}}}"#,
        )
        .unwrap();
        std::fs::write(
            agent.join("models-store.json"),
            r#"{"deepseek":{"models":[{"id":"chat","baseUrl":"https://evil.example/"}]}}"#,
        )
        .unwrap();
        std::fs::write(
            global.join("mcp.json"),
            r#"{"mcpServers":{"gh":{"url":"https://api.githubcopilot.com/mcp/"},"local":{"command":"x"}}}"#,
        )
        .unwrap();
        std::fs::write(
            repo.join("mcp.json"),
            r#"{"mcpServers":{"c":{"url":"https://collector.example/mcp"}}}"#,
        )
        .unwrap();
        std::fs::write(
            repo.join("mcp.local.json"),
            r#"{"mcpServers":{"mine":{"url":"https://mine.example/mcp"}}}"#,
        )
        .unwrap();

        let files = Files::new(agent.clone(), &global, &[global.clone(), repo.clone()]);
        let catalog = Catalog::new(builtin(), files);
        let allowed = |host: &str, port| catalog.allows(host, port).map(|t| t.source);
        assert_eq!(
            allowed("api.anthropic.com", 443),
            Some(HostSource::Provider {
                provider: "anthropic".into()
            })
        );
        assert_eq!(allowed("platform.claude.com", 443), Some(HostSource::SignIn));
        assert!(
            allowed("localhost", 11434).is_none(),
            "a local server is the sandbox's to decide: it needs a localhost:PORT entry"
        );
        assert!(
            allowed("gpu.example.net", 8444).is_none(),
            "a URL's port is the only one"
        );
        assert!(matches!(
            allowed("gpu.example.net", 8443),
            Some(HostSource::ModelsJson { .. })
        ));
        assert!(matches!(
            allowed("api.githubcopilot.com", 443),
            Some(HostSource::Mcp { server, .. }) if server == "gh"
        ));
        assert!(matches!(allowed("mine.example", 443), Some(HostSource::Mcp { .. })));
        assert!(
            allowed("collector.example", 443).is_none(),
            "a committed mcp.json isn't the user's own"
        );
        assert!(
            allowed("evil.example", 443).is_none(),
            "the catalog cache is never trusted"
        );

        // The cache's models are listed for a selector.
        assert_eq!(
            catalog.discovered(),
            [DiscoveredModel {
                provider: "deepseek".into(),
                id: "chat".into(),
                base_url: Some("https://evil.example/".into())
            }]
        );

        // An edit applies to the next question, without a restart.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            agent.join("models.json"),
            r#"{"providers":{"p":{"baseUrl":"https://new.example"}}}"#,
        )
        .unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        std::fs::File::options()
            .write(true)
            .open(agent.join("models.json"))
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert!(catalog.allows("new.example", 443).is_some());
        assert!(catalog.allows("gpu.example.net", 8443).is_none());
    }

    #[test]
    fn urls_name_hosts_and_ports() {
        let host = |url: &str| url_host(url).map(|t| (t.host, t.port));
        assert_eq!(
            host("https://API.Example.com/v1"),
            Some(("api.example.com".into(), None))
        );
        assert_eq!(
            host("http://localhost:11434/v1"),
            Some(("localhost".into(), Some(11434)))
        );
        assert_eq!(
            host("https://u:p@h.example:8443"),
            Some(("h.example".into(), Some(8443)))
        );
        assert_eq!(host("https://[::1]:3000/"), Some(("::1".into(), Some(3000))));
        assert_eq!(host("process://claude-code"), None);
        assert_eq!(host("https://${HOST}/v1"), None);
    }

    /// The real pi on this machine, when one is installed: its catalog reads, with the providers and sign-in hosts
    /// Piton relies on. Skipped where pi or node isn't on PATH (CI without pi).
    #[test]
    fn reads_the_installed_pi_catalog() {
        let path = std::env::var("PATH").unwrap_or_default();
        let find = |name: &str| path.split(':').map(|d| Path::new(d).join(name)).find(|p| p.is_file());
        let (Some(pi), Some(node)) = (find("pi"), find("node")) else {
            return;
        };
        let Some(package) = package_folder(&pi) else {
            return;
        };
        let env: crate::launch::Env = std::env::vars().collect();
        let builtin = read_builtin(&node, &package, &env).expect("pi's catalog reads");
        assert_eq!(builtin.providers["anthropic"], ["api.anthropic.com"]);
        assert!(builtin.providers["openai-codex"].contains(&"chatgpt.com".to_string()));
        for host in ["platform.claude.com", "auth.openai.com"] {
            assert!(builtin.sign_in.contains(&host.to_string()), "{host}");
        }
    }
}
