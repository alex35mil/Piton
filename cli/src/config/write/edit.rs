//! Writing one key of a `config.toml` / `config.local.toml` pair the way a person would: `toml_edit` keeps comments,
//! order, and formatting; the write goes to a symlink's target (a dotfiles repository keeps its link); and each write
//! rereads the file right before replacing it, so a hand edit made meanwhile is kept, not overwritten. [`edited`] is
//! the same change as text only, for a caller that writes the file another way (a protected file, through the daemon).

use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table};

use crate::config::{
    ConfigError,
    model::paths::{FilePair, Sharing},
};

/// Where a write puts the key: the file that sets it now, or one of the pair (moving it out of the other).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// The local file when it sets the key, else the shared one.
    Current,
    Move(Sharing),
}

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("{path} isn't valid TOML: {reason}")]
    Parse { path: PathBuf, reason: String },
    #[error("{path}: `{key}` is inside a value that isn't a table")]
    NotATable { path: PathBuf, key: String },
    #[error(transparent)]
    Invalid(#[from] ConfigError),
    #[error("{path} kept changing while piton wrote it; try again")]
    Busy { path: PathBuf },
    #[error("{path}: {reason}")]
    Value { path: PathBuf, reason: String },
    #[error("{path} has no local file; its values are shared by every client")]
    NoLocalFile { path: PathBuf },
}

impl FilePair {
    /// The file that sets `key`, if either does; the local file wins.
    pub fn placement(&self, key: &[&str]) -> Result<Option<Sharing>, EditError> {
        Ok(self.read()?.placement(key))
    }

    /// Both files, parsed once, to tell where many keys are set.
    pub fn read(&self) -> Result<Placements, EditError> {
        let parse = |sharing| -> Result<Option<DocumentMut>, EditError> {
            let Some(path) = self.path(sharing) else {
                return Ok(None);
            };
            read(path)?.map(|text| document(&text, path)).transpose()
        };
        Ok(Placements {
            shared: parse(Sharing::Shared)?,
            local: parse(Sharing::Local)?,
        })
    }

    /// The file of `sharing`, or why this pair has none.
    fn file(&self, sharing: Sharing) -> Result<&Path, EditError> {
        self.path(sharing).ok_or_else(|| EditError::NoLocalFile {
            path: self.shared.clone(),
        })
    }

    /// Sets `key` to `value` (removes it for `None`, back to its default) in the file `placement` names, removing it
    /// from the other one when it moves. `validate` checks each new file text before it's written, so an invalid result
    /// leaves both files as they were.
    pub fn set(
        &self,
        key: &[&str],
        value: Option<Item>,
        placement: Placement,
        validate: &dyn Fn(&str, &Path, Sharing) -> Result<(), ConfigError>,
    ) -> Result<Sharing, EditError> {
        let sharing = match placement {
            Placement::Current => self.placement(key)?.unwrap_or(Sharing::Shared),
            Placement::Move(sharing) => sharing,
        };
        let other = sharing.other();
        // The destination first, so the key is never in neither file; if removing it from the other file then fails,
        // the destination goes back to what it was, so a failed move changes nothing.
        let destination = self.file(sharing)?;
        let previous = read(&link_target(destination)?)?;
        write_key(destination, key, value.as_ref(), &|text, path| {
            validate(text, path, sharing)
        })?;
        if matches!(placement, Placement::Move(_))
            && let Some(other_path) = self.path(other)
            && let Err(e) = write_key(other_path, key, None, &|text, path| validate(text, path, other))
        {
            let target = link_target(destination)?;
            match previous {
                Some(text) => replace(&target, &text)?,
                None => {
                    let _ = std::fs::remove_file(&target);
                }
            }
            return Err(e);
        }
        Ok(sharing)
    }
}

/// A file pair's parsed documents.
#[derive(Debug)]
pub struct Placements {
    shared: Option<DocumentMut>,
    local: Option<DocumentMut>,
}

impl Placements {
    /// The file that sets a dotted `key` (`tailnet.port`), if either does; the local file wins.
    pub fn of(&self, key: &str) -> Option<Sharing> {
        self.placement(&key.split('.').collect::<Vec<_>>())
    }

    /// The file that sets `key`, if either does; the local file wins.
    pub fn placement(&self, key: &[&str]) -> Option<Sharing> {
        let sets = |doc: &Option<DocumentMut>| doc.as_ref().is_some_and(|doc| get(doc, key).is_some());
        if sets(&self.local) {
            Some(Sharing::Local)
        } else if sets(&self.shared) {
            Some(Sharing::Shared)
        } else {
            None
        }
    }
}

/// Read-modify-write of one key, retried when the file changes underneath.
pub fn write_key(
    path: &Path,
    key: &[&str],
    value: Option<&Item>,
    validate: &dyn Fn(&str, &Path) -> Result<(), ConfigError>,
) -> Result<(), EditError> {
    write_changes(path, &[Change { key, value }], validate)
}

/// One key of a file set to a value, or removed (`None`).
#[derive(Debug, Clone, Copy)]
pub struct Change<'a> {
    pub key: &'a [&'a str],
    pub value: Option<&'a Item>,
}

/// Appends `entry` to the array of tables at `key` (`[[sandbox.commands]]`), creating it; keeps the rest of the file.
pub fn append_table(
    path: &Path,
    key: &[&str],
    entry: &Table,
    validate: &dyn Fn(&str, &Path) -> Result<(), ConfigError>,
) -> Result<(), EditError> {
    let target = link_target(path)?;
    let before = read(&target)?;
    let mut doc = document(before.as_deref().unwrap_or(""), path)?;
    let Some((last, tables)) = key.split_last() else {
        return Ok(());
    };
    let not_a_table = || EditError::NotATable {
        path: path.to_path_buf(),
        key: key.join("."),
    };
    let mut table: &mut Table = doc.as_table_mut();
    for part in tables {
        let entry = table.entry(part).or_insert_with(|| {
            let mut new = Table::new();
            new.set_implicit(true);
            Item::Table(new)
        });
        table = entry.as_table_mut().ok_or_else(not_a_table)?;
    }
    let array = table
        .entry(last)
        .or_insert_with(|| Item::ArrayOfTables(toml_edit::ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .ok_or_else(not_a_table)?;
    let mut entry = entry.clone();
    entry.set_position(None);
    entry.decor_mut().set_prefix("\n");
    array.push(entry);
    let new = doc.to_string().trim_start_matches('\n').to_string();
    validate(&new, path)?;
    replace(&target, &new)
}

/// Several changes to one file, written together: one read-modify-write, checked as a whole.
pub fn write_changes(
    path: &Path,
    changes: &[Change],
    validate: &dyn Fn(&str, &Path) -> Result<(), ConfigError>,
) -> Result<(), EditError> {
    write_key_with(path, changes, validate, &mut || {})
}

/// The file's text with the changes applied in order, or `None` when that changes nothing. `text` is the file's
/// current text, `None` when it doesn't exist.
pub fn edited(text: Option<&str>, changes: &[Change], path: &Path) -> Result<Option<String>, EditError> {
    let sets = changes.iter().any(|change| change.value.is_some());
    let Some(text) = text.or(sets.then_some("")) else {
        // Only removals from a file that doesn't exist: nothing to do.
        return Ok(None);
    };
    let mut doc = document(text, path)?;
    let mut changed = false;
    for change in changes {
        changed |= match change.value {
            Some(value) => {
                insert(&mut doc, change.key, value.clone(), path)?;
                true
            }
            None => remove(&mut doc, change.key),
        };
    }
    // A table added to an empty file brings the blank line that separates tables; the file doesn't start with one.
    let new = changed.then(|| doc.to_string().trim_start_matches('\n').to_string());
    Ok(new.filter(|new| new != text))
}

/// A JSON value as the TOML item a person would write for `key`: objects as `[tables]` (lists of objects as
/// `[[tables]]`), other values inline. `null` has no TOML form and is refused.
pub fn item_from_json(key: &[&str], value: &serde_json::Value, path: &Path) -> Result<Item, EditError> {
    let Some((last, _)) = key.split_last() else {
        return Err(EditError::Value {
            path: path.to_path_buf(),
            reason: "an empty key".into(),
        });
    };
    let wrapped = serde_json::json!({ *last: value });
    let text = toml::to_string(&wrapped).map_err(|e| EditError::Value {
        path: path.to_path_buf(),
        reason: format!("{}: {e}", key.join(".")),
    })?;
    let mut doc = document(&text, path)?;
    let mut item = doc.as_table_mut().remove(last).unwrap_or(Item::None);
    // Positions from the scratch document would reorder the file the item goes into; without them, tables follow the
    // table they're inserted after.
    place(&mut item, None);
    if let Item::Table(table) = &mut item {
        table.set_implicit(false);
        // A blank line before a new table, as the file's own tables have.
        table.decor_mut().set_prefix("\n");
    }
    Ok(item)
}

/// Sets the document position of `item`'s tables (nested ones too).
fn place(item: &mut Item, position: Option<isize>) {
    match item {
        Item::Table(table) => {
            table.set_position(position);
            for (_, child) in table.iter_mut() {
                place(child, position);
            }
        }
        Item::ArrayOfTables(array) => {
            for table in array.iter_mut() {
                table.set_position(position);
                for (_, child) in table.iter_mut() {
                    place(child, position);
                }
            }
        }
        Item::None | Item::Value(_) => {}
    }
}

fn write_key_with(
    path: &Path,
    changes: &[Change],
    validate: &dyn Fn(&str, &Path) -> Result<(), ConfigError>,
    before_commit: &mut dyn FnMut(),
) -> Result<(), EditError> {
    let target = link_target(path)?;
    for _ in 0..5 {
        let before = read(&target)?;
        let Some(new) = edited(before.as_deref(), changes, path)? else {
            return Ok(());
        };
        validate(&new, path)?;
        before_commit();
        // A hand edit since the read: do it again on the new text.
        if read(&target)? != before {
            continue;
        }
        return replace(&target, &new);
    }
    Err(EditError::Busy {
        path: path.to_path_buf(),
    })
}

/// The file a write replaces: a symlink's final target, so the link stays a link.
fn link_target(path: &Path) -> Result<PathBuf, EditError> {
    match std::fs::canonicalize(path) {
        Ok(target) => Ok(target),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // A dangling link: write where it points.
            match std::fs::read_link(path) {
                Ok(target) if target.is_absolute() => Ok(target),
                Ok(target) => Ok(path.parent().unwrap_or(Path::new("/")).join(target)),
                Err(_) => Ok(path.to_path_buf()),
            }
        }
        Err(source) => Err(EditError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Replaces `target` atomically (a temporary file in its folder, then a rename), keeping its permissions.
fn replace(target: &Path, text: &str) -> Result<(), EditError> {
    let io = |path: &Path, source| EditError::Io {
        path: path.to_path_buf(),
        source,
    };
    let dir = target.parent().unwrap_or(Path::new("/"));
    std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Unique per write, so concurrent writers never share a temporary file.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = dir.join(format!(".{name}.piton-{}-{n}", std::process::id()));
    std::fs::write(&temporary, text).map_err(|e| io(&temporary, e))?;
    if let Ok(meta) = std::fs::metadata(target) {
        let _ = std::fs::set_permissions(&temporary, meta.permissions());
    }
    std::fs::rename(&temporary, target).map_err(|e| {
        let _ = std::fs::remove_file(&temporary);
        io(target, e)
    })
}

fn read(path: &Path) -> Result<Option<String>, EditError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(EditError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn document(text: &str, path: &Path) -> Result<DocumentMut, EditError> {
    text.parse().map_err(|e: toml_edit::TomlError| EditError::Parse {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })
}

fn get<'a>(doc: &'a DocumentMut, key: &[&str]) -> Option<&'a Item> {
    let mut item = doc.as_item();
    for part in key {
        item = item.get(part)?;
    }
    (!item.is_none()).then_some(item)
}

fn insert(doc: &mut DocumentMut, key: &[&str], value: Item, path: &Path) -> Result<(), EditError> {
    let Some((last, tables)) = key.split_last() else {
        return Ok(());
    };
    let mut table: &mut Table = doc.as_table_mut();
    for part in tables {
        let entry = table.entry(part).or_insert_with(|| {
            let mut new = Table::new();
            new.set_implicit(true);
            Item::Table(new)
        });
        table = entry.as_table_mut().ok_or_else(|| EditError::NotATable {
            path: path.to_path_buf(),
            key: key.join("."),
        })?;
    }
    match (table.get_mut(last), value) {
        // Keep the comment and spacing around an existing value.
        (Some(Item::Value(existing)), Item::Value(value)) => {
            let decor = existing.decor().clone();
            *existing = value;
            *existing.decor_mut() = decor;
        }
        // A replaced table keeps its place in the file and the comment above it.
        (Some(Item::Table(existing)), Item::Table(value)) => {
            let position = existing.position();
            let decor = existing.decor().clone();
            let mut value = Item::Table(value);
            place(&mut value, position);
            if let Item::Table(value) = value {
                *existing = value;
                *existing.decor_mut() = decor;
            }
        }
        (_, value) => {
            table.insert(last, value);
        }
    }
    Ok(())
}

/// Removes `key`, and the tables it leaves empty. Whether anything was removed.
fn remove(doc: &mut DocumentMut, key: &[&str]) -> bool {
    fn remove_in(table: &mut Table, key: &[&str]) -> bool {
        match key {
            [] => false,
            [last] => table.remove(last).is_some(),
            [first, rest @ ..] => {
                let Some(inner) = table.get_mut(first).and_then(Item::as_table_mut) else {
                    return false;
                };
                let removed = remove_in(inner, rest);
                if removed && inner.is_empty() {
                    table.remove(first);
                }
                removed
            }
        }
    }
    remove_in(doc.as_table_mut(), key)
}

#[cfg(test)]
mod tests {
    use toml_edit::Value;

    use super::*;
    use crate::config::model::paths::Scope;

    /// `config.host.toml` and its local file: top-level keys as well as tables.
    fn host_pair(dir: &Path) -> FilePair {
        FilePair::of(dir, Scope::Host)
    }

    fn local(files: &FilePair) -> &Path {
        files.local.as_deref().unwrap()
    }

    fn ok(_: &str, _: &Path, _: Sharing) -> Result<(), ConfigError> {
        Ok(())
    }

    fn strings(values: &[&str]) -> Item {
        Item::Value(Value::Array(values.iter().copied().collect()))
    }

    /// A JSON object becomes a `[table]` a person would write; setting it again replaces the table, keeping what's
    /// around it.
    #[test]
    fn writes_a_table_from_json() {
        let path = Path::new("config.toml");
        let value = serde_json::json!({"provider": "github", "repository": {"owner": "o", "repo": "r"}});
        let item = item_from_json(&["forge"], &value, path).unwrap();
        let text = "# mine\n[branches]\nformat = \"tracker\"\n";
        let set = |item| {
            [Change {
                key: &["forge"],
                value: Some(item),
            }]
        };
        let new = edited(Some(text), &set(&item), path).unwrap().unwrap();
        assert_eq!(
            new,
            "# mine\n[branches]\nformat = \"tracker\"\n\n[forge]\nprovider = \"github\"\n\n[forge.repository]\nowner = \"o\"\nrepo = \"r\"\n"
        );
        let again = item_from_json(&["forge"], &serde_json::json!({"provider": "github"}), path).unwrap();
        let replaced = edited(Some(&new), &set(&again), path).unwrap().unwrap();
        assert_eq!(
            replaced,
            "# mine\n[branches]\nformat = \"tracker\"\n\n[forge]\nprovider = \"github\"\n"
        );
        assert_eq!(edited(Some(&replaced), &set(&again), path).unwrap(), None);
        assert!(item_from_json(&["x"], &serde_json::Value::Null, path).is_err());

        // Several changes in one edit: a key removed, two set.
        let format = item_from_json(&["branches", "format"], &serde_json::json!("identifier-title"), path).unwrap();
        let many = edited(
            Some(&replaced),
            &[
                Change {
                    key: &["forge"],
                    value: None,
                },
                Change {
                    key: &["branches", "format"],
                    value: Some(&format),
                },
                Change {
                    key: &["tracker"],
                    value: Some(&again),
                },
            ],
            path,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            many,
            "# mine\n[branches]\nformat = \"identifier-title\"\n\n[tracker]\nprovider = \"github\"\n"
        );
    }

    #[test]
    fn keeps_comments_order_and_formatting() {
        let dir = tempfile::tempdir().unwrap();
        let files = host_pair(dir.path());
        std::fs::write(
            &files.shared,
            "# my settings\n# where I work\nroots = [\"~/a\"]   # trailing\nlogging = \"info\"\n\n[tailnet]\nport = 8\n",
        )
        .unwrap();
        files
            .set(&["roots"], Some(strings(&["~/b"])), Placement::Current, &ok)
            .unwrap();
        files
            .set(&["favorites"], Some(strings(&["~/c"])), Placement::Current, &ok)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&files.shared).unwrap(),
            "# my settings\n# where I work\nroots = [\"~/b\"]   # trailing\nlogging = \"info\"\nfavorites = [\"~/c\"]\n\n[tailnet]\nport = 8\n"
        );
        // Back to the default: the key goes, the rest stays.
        files.set(&["favorites"], None, Placement::Current, &ok).unwrap();
        files.set(&["tailnet", "port"], None, Placement::Current, &ok).unwrap();
        assert_eq!(
            std::fs::read_to_string(&files.shared).unwrap(),
            "# my settings\n# where I work\nroots = [\"~/b\"]   # trailing\nlogging = \"info\"\n"
        );
    }

    #[test]
    fn writes_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        let global = dir.path().join("global");
        std::fs::create_dir_all(&dotfiles).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(dotfiles.join("config.host.toml"), "roots = []\n").unwrap();
        std::os::unix::fs::symlink(dotfiles.join("config.host.toml"), global.join("config.host.toml")).unwrap();
        let files = host_pair(&global);
        files
            .set(&["roots"], Some(strings(&["~/p"])), Placement::Current, &ok)
            .unwrap();
        assert!(
            global
                .join("config.host.toml")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(dotfiles.join("config.host.toml")).unwrap(),
            "roots = [\"~/p\"]\n"
        );
    }

    #[test]
    fn a_hand_edit_during_a_write_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let files = host_pair(dir.path());
        std::fs::write(&files.shared, "roots = []\n").unwrap();
        let mut edited = false;
        let shared = files.shared.clone();
        let debug = Item::Value(Value::from("debug"));
        write_key_with(
            &files.shared,
            &[Change {
                key: &["logging"],
                value: Some(&debug),
            }],
            &|_, _| Ok(()),
            &mut || {
                if !edited {
                    edited = true;
                    std::fs::write(&shared, "roots = [\"~/hand\"]\n").unwrap();
                }
            },
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&files.shared).unwrap(),
            "roots = [\"~/hand\"]\nlogging = \"debug\"\n"
        );
    }

    #[test]
    fn an_invalid_result_or_file_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let files = host_pair(dir.path());
        std::fs::write(&files.shared, "roots = []\n").unwrap();
        let refuse = |_: &str, path: &Path, _: Sharing| {
            Err(ConfigError::Invalid {
                path: path.to_path_buf(),
                reason: "no".into(),
            })
        };
        assert!(matches!(
            files.set(&["x"], Some(Item::Value(Value::from(1))), Placement::Current, &refuse),
            Err(EditError::Invalid(_))
        ));
        assert_eq!(std::fs::read_to_string(&files.shared).unwrap(), "roots = []\n");

        std::fs::write(&files.shared, "[host\n").unwrap();
        assert!(matches!(
            files.set(&["roots"], Some(strings(&[])), Placement::Current, &ok),
            Err(EditError::Parse { .. })
        ));
        assert_eq!(std::fs::read_to_string(&files.shared).unwrap(), "[host\n");
    }

    /// A move whose second write fails leaves both files as they were.
    #[test]
    fn a_failed_move_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let files = host_pair(dir.path());
        std::fs::write(&files.shared, "logging = \"info\"\n").unwrap();
        std::fs::write(local(&files), "roots = [\"~/here\"]\n").unwrap();
        // Removing the key from the local file is refused (validation of the local file's new text).
        let local_path = local(&files).to_path_buf();
        let refuse_local = move |_: &str, path: &Path, _: Sharing| {
            if path == local_path {
                Err(ConfigError::Invalid {
                    path: path.to_path_buf(),
                    reason: "no".into(),
                })
            } else {
                Ok(())
            }
        };
        assert!(
            files
                .set(
                    &["roots"],
                    Some(strings(&["~/all"])),
                    Placement::Move(Sharing::Shared),
                    &refuse_local
                )
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&files.shared).unwrap(), "logging = \"info\"\n");
        assert_eq!(
            std::fs::read_to_string(local(&files)).unwrap(),
            "roots = [\"~/here\"]\n"
        );
    }

    #[test]
    fn local_moves_a_key_between_the_files() {
        let dir = tempfile::tempdir().unwrap();
        let files = host_pair(dir.path());
        std::fs::write(&files.shared, "roots = [\"~/a\"]\nlogging = \"info\"\n").unwrap();
        let roots = ["roots"];
        assert_eq!(files.placement(&roots).unwrap(), Some(Sharing::Shared));

        files
            .set(&roots, Some(strings(&["~/here"])), Placement::Move(Sharing::Local), &ok)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(local(&files)).unwrap(),
            "roots = [\"~/here\"]\n"
        );
        assert_eq!(std::fs::read_to_string(&files.shared).unwrap(), "logging = \"info\"\n");
        assert_eq!(files.placement(&roots).unwrap(), Some(Sharing::Local));
        // A later write without a placement stays where the key is.
        files
            .set(&roots, Some(strings(&["~/there"])), Placement::Current, &ok)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(local(&files)).unwrap(),
            "roots = [\"~/there\"]\n"
        );

        files
            .set(&roots, Some(strings(&["~/all"])), Placement::Move(Sharing::Shared), &ok)
            .unwrap();
        assert_eq!(std::fs::read_to_string(local(&files)).unwrap(), "");
        assert_eq!(
            std::fs::read_to_string(&files.shared).unwrap(),
            "logging = \"info\"\nroots = [\"~/all\"]\n"
        );
    }
}
