//! `config.client.toml` in the global tier: the apps' preferences, served by the host to every client (iPhone, iPad,
//! the Mac app). A device keeps its own values (the Local checkbox) in its app, never in a file.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_font: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_font: Option<String>,
}
