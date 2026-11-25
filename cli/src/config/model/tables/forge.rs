//! `[forge]`: where pull requests go. Off when missing, or with `provider = "none"`.

use serde::{Deserialize, Serialize};
use toml::{Spanned, de::DeValue};

use super::{
    ReadError,
    integration::{GitHubProvider, McpServerName, Providers, Repository, RoleTable, read_provider},
};
use crate::config::model::key::Issue;

/// `[forge]` as written.
pub type ForgeTable = RoleTable<ForgeProvider>;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ForgeProvider {
    GitHub(GitHubForge),
}

impl Providers for ForgeProvider {
    const TABLE: &'static str = "forge";
    const NAMES: &'static [&'static str] = &["github"];

    fn read(provider: &str, value: Spanned<DeValue<'_>>) -> Option<Result<Self, ReadError>> {
        match provider {
            "github" => Some(read_provider(value).map(ForgeProvider::GitHub)),
            _ => None,
        }
    }

    fn check(&self) -> Result<(), Issue> {
        match self {
            ForgeProvider::GitHub(_) => Ok(()),
        }
    }
}

/// Pull requests of one GitHub repository.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubForge {
    pub provider: GitHubProvider,
    pub mcp_server: McpServerName,
    pub repository: Repository,
}
