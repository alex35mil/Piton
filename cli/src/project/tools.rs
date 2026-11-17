//! The tools a repository can configure for itself: version control, environment managers, agents, and editors. Each
//! declares the repository files that make it run code or instruct an agent when a shell, editor, or session enters
//! the checkout.
//!
//! Incoming PR review keeps these files at a trusted revision on disk (the pi package's held paths), so reviewing a
//! contribution never activates its configuration. `piton tools` prints this registry for the pi package.

use serde::Serialize;

/// Version of `piton tools`. Clients refuse a version they don't know.
pub const TOOLS_FORMAT: u32 = 1;

/// A tool, by category. Operations that apply to one category take its inner enum, so a tool of another category
/// can't reach them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "category", content = "tool", rename_all = "snake_case")]
pub enum ProjectTool {
    /// Chooses which configured helpers (filters, diff and merge drivers) Git runs on the repository's files.
    Vcs(VcsTool),
    /// Sets up the shell or session environment, and can run commands while doing it.
    Environment(EnvironmentTool),
    /// Instructs or extends an agent.
    Agent(AgentTool),
    /// Configures an editor for the project.
    Editor(EditorTool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VcsTool {
    Git,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentTool {
    Mise,
    Direnv,
    Nix,
    Devbox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTool {
    Agents,
    Pi,
    Piton,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EditorTool {
    Neovim,
    VsCode,
    Zed,
    JetBrains,
}

/// One configuration location, matched at any depth in the checkout, case-insensitively (APFS is case-insensitive by
/// default, so `AGENTS.MD` is the file a tool reads as `AGENTS.md`). Components are separated by `/`; `*` within a
/// component matches any run of characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigPath {
    /// A file whose trailing components match `path`.
    File { path: &'static str },
    /// Everything under a folder whose components match `path`.
    Directory { path: &'static str },
}

use ConfigPath::{Directory, File};

pub const VCS_TOOLS: [VcsTool; 1] = [VcsTool::Git];
pub const ENVIRONMENT_TOOLS: [EnvironmentTool; 4] = [
    EnvironmentTool::Mise,
    EnvironmentTool::Direnv,
    EnvironmentTool::Nix,
    EnvironmentTool::Devbox,
];
pub const AGENT_TOOLS: [AgentTool; 3] = [AgentTool::Agents, AgentTool::Pi, AgentTool::Piton];
pub const EDITOR_TOOLS: [EditorTool; 4] = [
    EditorTool::Neovim,
    EditorTool::VsCode,
    EditorTool::Zed,
    EditorTool::JetBrains,
];

/// Every tool, category by category.
pub fn all() -> impl Iterator<Item = ProjectTool> {
    VCS_TOOLS
        .into_iter()
        .map(ProjectTool::Vcs)
        .chain(ENVIRONMENT_TOOLS.into_iter().map(ProjectTool::Environment))
        .chain(AGENT_TOOLS.into_iter().map(ProjectTool::Agent))
        .chain(EDITOR_TOOLS.into_iter().map(ProjectTool::Editor))
}

impl ProjectTool {
    pub fn config_paths(self) -> &'static [ConfigPath] {
        match self {
            Self::Vcs(tool) => tool.config_paths(),
            Self::Environment(tool) => tool.config_paths(),
            Self::Agent(tool) => tool.config_paths(),
            Self::Editor(tool) => tool.config_paths(),
        }
    }
}

impl VcsTool {
    pub fn config_paths(self) -> &'static [ConfigPath] {
        match self {
            // A contribution's attributes could select filters and drivers the user configured for other repositories,
            // which then run whenever the user's own Git touches those files.
            Self::Git => &[File { path: ".gitattributes" }],
        }
    }
}

impl EnvironmentTool {
    pub fn config_paths(self) -> &'static [ConfigPath] {
        match self {
            Self::Mise => &[
                File { path: "mise.toml" },
                File { path: ".mise.toml" },
                File { path: "mise.*.toml" },
                File { path: ".mise.*.toml" },
                File {
                    path: "mise/config.toml",
                },
                File {
                    path: "mise/config.*.toml",
                },
                File {
                    path: ".mise/config.toml",
                },
                File {
                    path: ".mise/config.*.toml",
                },
                File {
                    path: ".config/mise.toml",
                },
                Directory { path: ".config/mise" },
                File { path: ".tool-versions" },
            ],
            Self::Direnv => &[File { path: ".envrc" }],
            // A trusted `.envrc` with `use flake` still evaluates these, so they're held with it.
            Self::Nix => &[
                File { path: "flake.nix" },
                File { path: "flake.lock" },
                File { path: "shell.nix" },
                File { path: "default.nix" },
            ],
            Self::Devbox => &[File { path: "devbox.json" }, File { path: "devbox.lock" }],
        }
    }
}

impl AgentTool {
    pub fn config_paths(self) -> &'static [ConfigPath] {
        match self {
            Self::Agents => &[
                File { path: "AGENTS.md" },
                File {
                    path: "AGENTS.override.md",
                },
                File { path: "CLAUDE.md" },
                Directory { path: ".agents" },
            ],
            Self::Pi => &[Directory { path: ".pi" }],
            Self::Piton => &[Directory { path: ".piton" }],
        }
    }
}

impl EditorTool {
    pub fn config_paths(self) -> &'static [ConfigPath] {
        match self {
            // `.lazy.lua` is a lazy.nvim plugin spec, loaded as code.
            Self::Neovim => &[
                File { path: ".nvim.lua" },
                File { path: ".nvimrc" },
                File { path: ".exrc" },
                File { path: ".lazy.lua" },
            ],
            Self::VsCode => &[Directory { path: ".vscode" }],
            Self::Zed => &[Directory { path: ".zed" }],
            Self::JetBrains => &[Directory { path: ".idea" }],
        }
    }
}

impl ConfigPath {
    /// The Git pathspec matching this location at any depth, case-insensitively. Git does the matching, so every
    /// client (`ls-tree`, `ls-files`, `diff`) agrees on which files are configuration.
    pub fn pathspec(self) -> String {
        match self {
            Self::File { path } => format!(":(glob,icase)**/{path}"),
            Self::Directory { path } => format!(":(glob,icase)**/{path}/**"),
        }
    }
}

/// One registry entry: the tool's `category` and `tool` names, its paths, and their pathspecs.
#[derive(Serialize)]
pub struct ToolEntry {
    #[serde(flatten)]
    pub tool: ProjectTool,
    pub paths: &'static [ConfigPath],
    pub pathspecs: Vec<String>,
}

#[derive(Serialize)]
pub struct Registry {
    pub format: u32,
    pub tools: Vec<ToolEntry>,
}

pub fn registry() -> Registry {
    Registry {
        format: TOOLS_FORMAT,
        tools: all()
            .map(|tool| ToolEntry {
                tool,
                paths: tool.config_paths(),
                pathspecs: tool.config_paths().iter().map(|p| p.pathspec()).collect(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_is_listed_once() {
        let tools: Vec<ProjectTool> = all().collect();
        assert_eq!(tools.len(), 12);
        for tool in &tools {
            assert_eq!(tools.iter().filter(|t| *t == tool).count(), 1);
            assert!(!tool.config_paths().is_empty(), "{tool:?} declares no paths");
        }
    }

    /// The repository paths Git matches with every registered pathspec.
    fn matched(paths: &[&str]) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        git(&["init", "-q"]);
        for path in paths {
            let file = dir.path().join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, "x").unwrap();
        }
        git(&["add", "-A", "-f"]);
        let specs: Vec<String> = all().flat_map(|t| t.config_paths()).map(|p| p.pathspec()).collect();
        let mut args = vec!["ls-files", "-z", "--"];
        args.extend(specs.iter().map(String::as_str));
        git(&args)
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn config_paths_at_any_depth() {
        let held = [
            ".gitattributes",
            "vendor/.gitattributes",
            "mise.toml",
            "apps/mise.toml",
            ".mise.local.toml",
            "mise.ci.toml",
            "mise/config.toml",
            ".config/mise/conf.d/x.toml",
            ".tool-versions",
            ".envrc",
            "nested/flake.nix",
            "devbox.json",
            "AGENTS.md",
            "docs/AGENTS.MD",
            "CLAUDE.md",
            ".agents/rules/a.md",
            ".pi/extensions/x.ts",
            "sub/.pi/settings.json",
            ".piton/config.toml",
            ".nvim.lua",
            ".lazy.lua",
            ".vscode/tasks.json",
            ".zed/settings.json",
            ".idea/workspace.xml",
        ];
        let ordinary = [
            "src/mise.rs",
            "mise/other.toml",
            "src/mise/mod.rs",
            "AGENTS.md.bak",
            "docs/agents/guide.md",
            "README.md",
            "flake.nix.example",
            "pi/index.ts",
        ];
        let all: Vec<&str> = held.iter().chain(&ordinary).copied().collect();
        let mut expected: Vec<String> = held.iter().map(|s| s.to_string()).collect();
        expected.sort();
        let mut found = matched(&all);
        found.sort();
        assert_eq!(found, expected);
    }

    #[test]
    fn registry_serializes_tools_and_paths() {
        let json = serde_json::to_value(registry()).unwrap();
        assert_eq!(json["format"], TOOLS_FORMAT);
        let mise = &json["tools"][1];
        assert_eq!(mise["category"], "environment");
        assert_eq!(mise["tool"], "mise");
        assert_eq!(
            mise["paths"][0],
            serde_json::json!({"kind": "file", "path": "mise.toml"})
        );
        assert_eq!(mise["pathspecs"][0], ":(glob,icase)**/mise.toml");
        assert_eq!(json["tools"][11]["category"], "editor");
        assert_eq!(json["tools"][11]["tool"], "jet_brains");
    }
}
