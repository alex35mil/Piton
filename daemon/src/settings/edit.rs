//! Writing one key of a global file pair (`settings.toml` / `settings.local.toml`, `daemon.toml` /
//! `daemon.local.toml`) the way a person would: `toml_edit` keeps comments, order, and formatting; the write goes to a
//! symlink's target (a dotfiles repository keeps its link); and each write rereads the file right before replacing it,
//! so a hand edit made meanwhile is kept, not overwritten.

use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table, Value};

/// A global file and its `.local` companion (this host's values, never shared).
#[derive(Debug, Clone)]
pub struct FilePair {
    pub shared: PathBuf,
    pub local: PathBuf,
}

/// Which file of the pair a key lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Shared,
    Local,
}

/// Where a write puts the key: the file that sets it now, or one of the pair (moving it out of the other).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The local file when it sets the key, else the shared one.
    Current,
    Move(Placement),
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
    Invalid(#[from] super::SettingsError),
    #[error("{path} kept changing while piton wrote it; try again")]
    Busy { path: PathBuf },
}

impl FilePair {
    fn path(&self, placement: Placement) -> &Path {
        match placement {
            Placement::Shared => &self.shared,
            Placement::Local => &self.local,
        }
    }

    /// The file that sets `key`, if either does; the local file wins.
    pub fn placement(&self, key: &[&str]) -> Result<Option<Placement>, EditError> {
        Ok(self.read()?.placement(key))
    }

    /// Both files, parsed once, to tell where many keys are set.
    pub fn read(&self) -> Result<Placements, EditError> {
        let parse = |placement| -> Result<Option<DocumentMut>, EditError> {
            let path = self.path(placement);
            read(path)?.map(|text| document(&text, path)).transpose()
        };
        Ok(Placements {
            shared: parse(Placement::Shared)?,
            local: parse(Placement::Local)?,
        })
    }

    /// Sets `key` to `value` (removes it for `None`, back to its default) in the file `target` names, removing it from
    /// the other one when it moves. `validate` checks each new file text before it's written, so an invalid result
    /// leaves both files as they were.
    pub fn set(
        &self,
        key: &[&str],
        value: Option<Value>,
        target: Target,
        validate: &dyn Fn(&str, &Path) -> Result<(), super::SettingsError>,
    ) -> Result<Placement, EditError> {
        let placement = match target {
            Target::Current => self.placement(key)?.unwrap_or(Placement::Shared),
            Target::Move(placement) => placement,
        };
        let other = match placement {
            Placement::Shared => Placement::Local,
            Placement::Local => Placement::Shared,
        };
        // The destination first, so the key is never in neither file; if removing it from the other file then fails,
        // the destination goes back to what it was, so a failed move changes nothing.
        let destination = self.path(placement);
        let previous = read(&link_target(destination)?)?;
        write_key(destination, key, value, validate)?;
        if matches!(target, Target::Move(_))
            && let Err(e) = write_key(self.path(other), key, None, validate)
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
        Ok(placement)
    }
}

/// A file pair's parsed documents.
#[derive(Debug)]
pub struct Placements {
    shared: Option<DocumentMut>,
    local: Option<DocumentMut>,
}

impl Placements {
    /// The file that sets `key`, if either does; the local file wins.
    pub fn placement(&self, key: &[&str]) -> Option<Placement> {
        let sets = |doc: &Option<DocumentMut>| doc.as_ref().is_some_and(|doc| get(doc, key).is_some());
        if sets(&self.local) {
            Some(Placement::Local)
        } else if sets(&self.shared) {
            Some(Placement::Shared)
        } else {
            None
        }
    }
}

/// Read-modify-write of one key, retried when the file changes underneath.
fn write_key(
    path: &Path,
    key: &[&str],
    value: Option<Value>,
    validate: &dyn Fn(&str, &Path) -> Result<(), super::SettingsError>,
) -> Result<(), EditError> {
    write_key_with(path, key, value, validate, &mut || {})
}

fn write_key_with(
    path: &Path,
    key: &[&str],
    value: Option<Value>,
    validate: &dyn Fn(&str, &Path) -> Result<(), super::SettingsError>,
    before_commit: &mut dyn FnMut(),
) -> Result<(), EditError> {
    let target = link_target(path)?;
    for _ in 0..5 {
        let before = read(&target)?;
        let Some(text) = before.as_deref().or(value.as_ref().map(|_| "")) else {
            // Removing a key from a file that doesn't exist: nothing to do.
            return Ok(());
        };
        let mut doc = document(text, path)?;
        let changed = match &value {
            Some(value) => {
                insert(&mut doc, key, value.clone(), path)?;
                true
            }
            None => remove(&mut doc, key),
        };
        if !changed {
            return Ok(());
        }
        let new = doc.to_string();
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

fn insert(doc: &mut DocumentMut, key: &[&str], value: Value, path: &Path) -> Result<(), EditError> {
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
    match table.get_mut(last).and_then(Item::as_value_mut) {
        // Keep the comment and spacing around an existing value.
        Some(existing) => {
            let decor = existing.decor().clone();
            *existing = value;
            *existing.decor_mut() = decor;
        }
        None => {
            table.insert(last, Item::Value(value));
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
    use super::*;

    fn pair(dir: &Path) -> FilePair {
        FilePair {
            shared: dir.join("settings.toml"),
            local: dir.join("settings.local.toml"),
        }
    }

    fn ok(_: &str, _: &Path) -> Result<(), crate::settings::SettingsError> {
        Ok(())
    }

    fn strings(values: &[&str]) -> Value {
        Value::Array(values.iter().copied().collect())
    }

    #[test]
    fn keeps_comments_order_and_formatting() {
        let dir = tempfile::tempdir().unwrap();
        let files = pair(dir.path());
        std::fs::write(
            &files.shared,
            "# my settings\n[host]\n# where I work\nroots = [\"~/a\"]   # trailing\nlogging = \"info\"\n\n[client]\ntext_font = \"X\"\n",
        )
        .unwrap();
        files
            .set(&["host", "roots"], Some(strings(&["~/b"])), Target::Current, &ok)
            .unwrap();
        files
            .set(&["host", "favorites"], Some(strings(&["~/c"])), Target::Current, &ok)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&files.shared).unwrap(),
            "# my settings\n[host]\n# where I work\nroots = [\"~/b\"]   # trailing\nlogging = \"info\"\nfavorites = [\"~/c\"]\n\n[client]\ntext_font = \"X\"\n"
        );
        // Back to the default: the key goes, the rest stays.
        files.set(&["host", "favorites"], None, Target::Current, &ok).unwrap();
        files.set(&["client", "text_font"], None, Target::Current, &ok).unwrap();
        assert_eq!(
            std::fs::read_to_string(&files.shared).unwrap(),
            "# my settings\n[host]\n# where I work\nroots = [\"~/b\"]   # trailing\nlogging = \"info\"\n"
        );
    }

    #[test]
    fn writes_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        let global = dir.path().join("global");
        std::fs::create_dir_all(&dotfiles).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(dotfiles.join("settings.toml"), "[host]\nroots = []\n").unwrap();
        std::os::unix::fs::symlink(dotfiles.join("settings.toml"), global.join("settings.toml")).unwrap();
        let files = pair(&global);
        files
            .set(&["host", "roots"], Some(strings(&["~/p"])), Target::Current, &ok)
            .unwrap();
        assert!(
            global
                .join("settings.toml")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(dotfiles.join("settings.toml")).unwrap(),
            "[host]\nroots = [\"~/p\"]\n"
        );
    }

    #[test]
    fn a_hand_edit_during_a_write_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let files = pair(dir.path());
        std::fs::write(&files.shared, "[host]\nroots = []\n").unwrap();
        let mut edited = false;
        let shared = files.shared.clone();
        write_key_with(
            &files.shared,
            &["host", "logging"],
            Some(Value::from("debug")),
            &ok,
            &mut || {
                if !edited {
                    edited = true;
                    std::fs::write(&shared, "[host]\nroots = [\"~/hand\"]\n").unwrap();
                }
            },
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&files.shared).unwrap(),
            "[host]\nroots = [\"~/hand\"]\nlogging = \"debug\"\n"
        );
    }

    #[test]
    fn an_invalid_result_or_file_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let files = pair(dir.path());
        std::fs::write(&files.shared, "[host]\nroots = []\n").unwrap();
        let refuse = |_: &str, path: &Path| {
            Err(crate::settings::SettingsError::Invalid {
                path: path.to_path_buf(),
                reason: "no".into(),
            })
        };
        assert!(matches!(
            files.set(&["host", "x"], Some(Value::from(1)), Target::Current, &refuse),
            Err(EditError::Invalid(_))
        ));
        assert_eq!(std::fs::read_to_string(&files.shared).unwrap(), "[host]\nroots = []\n");

        std::fs::write(&files.shared, "[host\n").unwrap();
        assert!(matches!(
            files.set(&["host", "roots"], Some(strings(&[])), Target::Current, &ok),
            Err(EditError::Parse { .. })
        ));
        assert_eq!(std::fs::read_to_string(&files.shared).unwrap(), "[host\n");
    }

    /// A move whose second write fails leaves both files as they were.
    #[test]
    fn a_failed_move_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let files = pair(dir.path());
        std::fs::write(&files.shared, "[host]\nlogging = \"info\"\n").unwrap();
        std::fs::write(&files.local, "[host]\nroots = [\"~/here\"]\n").unwrap();
        // Removing the key from the local file is refused (validation of the local file's new text).
        let local = files.local.clone();
        let refuse_local = move |_: &str, path: &Path| {
            if path == local {
                Err(crate::settings::SettingsError::Invalid {
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
                    &["host", "roots"],
                    Some(strings(&["~/all"])),
                    Target::Move(Placement::Shared),
                    &refuse_local
                )
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(&files.shared).unwrap(),
            "[host]\nlogging = \"info\"\n"
        );
        assert_eq!(
            std::fs::read_to_string(&files.local).unwrap(),
            "[host]\nroots = [\"~/here\"]\n"
        );
    }

    #[test]
    fn local_moves_a_key_between_the_files() {
        let dir = tempfile::tempdir().unwrap();
        let files = pair(dir.path());
        std::fs::write(&files.shared, "[host]\nroots = [\"~/a\"]\nlogging = \"info\"\n").unwrap();
        let roots = ["host", "roots"];
        assert_eq!(files.placement(&roots).unwrap(), Some(Placement::Shared));

        files
            .set(&roots, Some(strings(&["~/here"])), Target::Move(Placement::Local), &ok)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&files.local).unwrap(),
            "[host]\nroots = [\"~/here\"]\n"
        );
        assert_eq!(
            std::fs::read_to_string(&files.shared).unwrap(),
            "[host]\nlogging = \"info\"\n"
        );
        assert_eq!(files.placement(&roots).unwrap(), Some(Placement::Local));
        // A later write without a placement stays where the key is.
        files
            .set(&roots, Some(strings(&["~/there"])), Target::Current, &ok)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&files.local).unwrap(),
            "[host]\nroots = [\"~/there\"]\n"
        );

        files
            .set(&roots, Some(strings(&["~/all"])), Target::Move(Placement::Shared), &ok)
            .unwrap();
        assert_eq!(std::fs::read_to_string(&files.local).unwrap(), "");
        assert_eq!(
            std::fs::read_to_string(&files.shared).unwrap(),
            "[host]\nlogging = \"info\"\nroots = [\"~/all\"]\n"
        );
    }
}
