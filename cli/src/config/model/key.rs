//! Where in a file a value is: its key from the top of the file (`tracker.fields.status.values.in_progress`,
//! `tracker.labels.retired[2]`), and what a check found wrong with it.

use std::fmt;

/// One step of a key: a table key, or an index into an array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyPart {
    Name(String),
    Index(usize),
}

/// A key from the top of a file. Empty only for a whole document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Key(Vec<KeyPart>);

impl Key {
    /// A key of table names, from the top of the file.
    pub fn of(names: &[&str]) -> Self {
        Self(names.iter().map(|name| KeyPart::Name((*name).to_string())).collect())
    }

    pub fn new(parts: Vec<KeyPart>) -> Self {
        Self(parts)
    }

    /// This key, then the table key `name`.
    pub fn name(&self, name: &str) -> Self {
        let mut parts = self.0.clone();
        parts.push(KeyPart::Name(name.to_string()));
        Self(parts)
    }

    /// This key, then array index `index`.
    pub fn index(&self, index: usize) -> Self {
        let mut parts = self.0.clone();
        parts.push(KeyPart::Index(index));
        Self(parts)
    }

    pub fn parts(&self) -> &[KeyPart] {
        &self.0
    }
}

impl fmt::Display for Key {
    /// As a dotted key, a part quoted when it isn't bare, and indexes in brackets: `a."b.c"[2]`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, part) in self.0.iter().enumerate() {
            match part {
                KeyPart::Name(name) => {
                    if i > 0 {
                        f.write_str(".")?;
                    }
                    f.write_str(&toml_edit::Key::new(name.as_str()).display_repr())?;
                }
                KeyPart::Index(index) => write!(f, "[{index}]")?,
            }
        }
        Ok(())
    }
}

/// Where an error is: the key, and its line when the value came from a file (a value checked on its own, such as
/// `piton config validate`'s JSON, has none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub key: Key,
    pub line: Option<usize>,
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "line {line}, {}", self.key),
            None => write!(f, "{}", self.key),
        }
    }
}

/// A value the schema accepts but Piton can't use: which key, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub key: Key,
    pub reason: String,
}

impl Issue {
    pub fn new(key: Key, reason: impl Into<String>) -> Self {
        Self {
            key,
            reason: reason.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_display_as_dotted_keys() {
        let key = Key::of(&["tracker", "labels"]).name("retired").index(2).name("name");
        assert_eq!(key.to_string(), "tracker.labels.retired[2].name");
        assert_eq!(
            Key::of(&["sandbox", "filesystem", "~/.ssh"]).to_string(),
            "sandbox.filesystem.\"~/.ssh\""
        );
    }
}
