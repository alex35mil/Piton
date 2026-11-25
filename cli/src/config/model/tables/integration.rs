//! What `[tracker]` and `[forge]` share: a table that turns its role off (`provider = "none"`) or configures one
//! provider, chosen by `provider`, and the checked values their schemas are built from.
//!
//! The nearest table replaces a farther one whole, so a repository can turn off a role a parent folder configures.
//! Project only: a global one would become the role of every repository that has none.

use std::{fmt, num::NonZeroU64};

use serde::{Deserialize, Deserializer, Serialize};
use toml::{Spanned, de::DeValue};

use super::{ReadError, TableValue};
use crate::config::model::key::Issue;

/// The providers of one role (`[tracker]` or `[forge]`), other than `none`: each reads its own table.
pub trait Providers: Sized + Serialize {
    /// The table's name in the file.
    const TABLE: &'static str;
    /// Every provider's name, for messages.
    const NAMES: &'static [&'static str];
    /// The table of `provider`; `None` when there's no such provider.
    fn read(provider: &str, value: Spanned<DeValue<'_>>) -> Option<Result<Self, ReadError>>;
    /// Values the schema accepts but Piton can't use.
    fn check(&self) -> Result<(), Issue>;
}

/// `[tracker]` or `[forge]`: off, or one provider's configuration.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum RoleTable<P> {
    /// `provider = "none"`, and nothing else.
    Off(Disabled),
    On(P),
}

/// `provider = "none"`: the role is off, also over a farther level's table.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Disabled {
    pub provider: NoneProvider,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum NoneProvider {
    #[serde(rename = "none")]
    None,
}

/// The `provider` of a table that names it: `github` for both roles' GitHub tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum GitHubProvider {
    #[serde(rename = "github")]
    GitHub,
}

impl<P: Providers> RoleTable<P> {
    fn read_table(value: Spanned<DeValue<'_>>) -> Result<Self, ReadError> {
        let span = value.span();
        let DeValue::Table(table) = value.get_ref() else {
            return Err(ReadError::at(span, "must be a table"));
        };
        let Some(provider) = table.get("provider") else {
            return Err(ReadError::at(span, "missing field `provider`"));
        };
        let provider_span = provider.span();
        let Some(name) = provider.get_ref().as_str().map(str::to_string) else {
            return Err(ReadError::at(provider_span, "`provider` must be a string"));
        };
        match name.as_str() {
            "none" => Disabled::deserialize(toml::de::ValueDeserializer::from(value))
                .map(RoleTable::Off)
                .map_err(ReadError::from),
            _ => match P::read(&name, value) {
                Some(table) => table.map(RoleTable::On),
                None => Err(ReadError::at(
                    provider_span,
                    format!(
                        "unknown provider {name:?}, expected {}",
                        std::iter::once("none")
                            .chain(P::NAMES.iter().copied())
                            .map(|name| format!("`{name}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )),
            },
        }
    }
}

/// Only for `TableValue`'s bound: tables are read with [`TableValue::read`], which keeps each error's position.
impl<'de, P: Providers> Deserialize<'de> for RoleTable<P> {
    fn deserialize<D: Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
        Err(serde::de::Error::custom(format!(
            "[{}] is read from a TOML document",
            P::TABLE
        )))
    }
}

impl<P: Providers> TableValue for RoleTable<P> {
    fn read(value: Spanned<DeValue<'_>>) -> Result<Self, ReadError> {
        Self::read_table(value)
    }
    // Project only, as a whole: `Table::rule`, checked before the table is read.
    fn check(&self) -> Result<(), Issue> {
        match self {
            RoleTable::Off(_) => Ok(()),
            RoleTable::On(table) => table.check(),
        }
    }
}

/// Reads one provider's table, its `provider` included.
pub fn read_provider<T: serde::de::DeserializeOwned>(value: Spanned<DeValue<'_>>) -> Result<T, ReadError> {
    T::deserialize(toml::de::ValueDeserializer::from(value)).map_err(ReadError::from)
}

/// A string with at least one character.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Text(String);

impl Text {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        match text.is_empty() {
            true => Err(serde::de::Error::custom("must not be empty")),
            false => Ok(Self(text)),
        }
    }
}

impl fmt::Display for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A string with at least one character, on one line: a name shown in a provider.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Line(String);

impl Line {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Line {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        match (text.is_empty(), text.contains(['\r', '\n'])) {
            (true, _) => Err(serde::de::Error::custom("must not be empty")),
            (false, true) => Err(serde::de::Error::custom("must be one line")),
            (false, false) => Ok(Self(text)),
        }
    }
}

/// The registered MCP server a role's calls go through: letters, digits, `_`, `.`, and `-`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct McpServerName(String);

impl<'de> Deserialize<'de> for McpServerName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        let valid = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
        match valid {
            true => Ok(Self(name)),
            false => Err(serde::de::Error::custom(
                "must be an MCP server name: letters, digits, `_`, `.`, and `-`",
            )),
        }
    }
}

/// A GitHub repository.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    pub owner: Text,
    pub repo: Text,
}

/// A whole number from 1.
pub type Number = NonZeroU64;
