//! The host's state database: what happened on this host, as opposed to configuration (what the user decides). SQLite,
//! owned by the daemon alone, in its state folder next to the socket (`~/.local/state/piton/piton.db` by default).
//!
//! `PRAGMA user_version` is the schema version. At open, ordered migrations bring an older database up to date in one
//! transaction, after a `VACUUM INTO` backup of the previous version; a database newer than this binary is refused.
//! Tables are added by the work that needs them: `host` (this host's identity) and `settings` (state values such as
//! keep-awake) so far.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

/// The database file in the daemon's state folder.
pub const FILE: &str = "piton.db";

/// Each migration takes the schema from its index to the next version; the schema version is their count.
const MIGRATIONS: &[&str] = &[
    // 1: the host's identity, and state values by key (JSON).
    "CREATE TABLE host (id INTEGER PRIMARY KEY CHECK (id = 1), host_id TEXT NOT NULL);
     CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
];

/// Tables whose rows are this host's identity rather than state an import would lose.
const IDENTITY_TABLES: &[&str] = &["host"];

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("{path}: {source}")]
    Sqlite { path: PathBuf, source: rusqlite::Error },
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error(
        "{path} has schema version {found}, newer than this piton understands ({supported}); update piton, or move the file aside to start over"
    )]
    Newer { path: PathBuf, found: i64, supported: i64 },
    #[error("{path} isn't a piton state database (no host identity)")]
    NotAState { path: PathBuf },
    #[error("{path} already holds state ({tables}); import over it with --force, which replaces it")]
    HasState { path: PathBuf, tables: String },
    #[error("{path} exists; choose another file")]
    Exists { path: PathBuf },
    #[error("{path} doesn't exist: this host has no state yet")]
    Missing { path: PathBuf },
}

/// A state value, by key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    /// Hold the host awake while sessions are live.
    KeepAwake,
}

impl Setting {
    fn key(self) -> &'static str {
        match self {
            Setting::KeepAwake => "keep_awake",
        }
    }
}

pub struct State {
    path: PathBuf,
    conn: Connection,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl State {
    /// Opens (creating when absent) and migrates the database at `path`.
    pub fn open(path: &Path) -> Result<Self, StateError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| io(dir, source))?;
        }
        let conn = Connection::open(path).map_err(|source| sqlite(path, source))?;
        set_owner_only(path)?;
        let mut state = Self {
            path: path.to_path_buf(),
            conn,
        };
        state
            .conn
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(|source| sqlite(path, source))?;
        state.migrate(MIGRATIONS)?;
        state.ensure_host()?;
        Ok(state)
    }

    fn version(&self) -> Result<i64, StateError> {
        self.conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|source| sqlite(&self.path, source))
    }

    fn migrate(&mut self, migrations: &[&str]) -> Result<(), StateError> {
        let found = self.version()?;
        let supported = migrations.len() as i64;
        if found > supported {
            return Err(StateError::Newer {
                path: self.path.clone(),
                found,
                supported,
            });
        }
        if found == supported {
            return Ok(());
        }
        // A database with data gets a copy of its previous version first; a new one has nothing to keep. Only the
        // latest copy is kept.
        if found > 0 {
            let backup = backup_path(&self.path, found);
            let _ = std::fs::remove_file(&backup);
            self.conn
                .execute("VACUUM INTO ?1", params![backup.to_string_lossy()])
                .map_err(|source| sqlite(&self.path, source))?;
            for older in 1..found {
                let _ = std::fs::remove_file(backup_path(&self.path, older));
            }
        }
        let path = self.path.clone();
        let tx = self.conn.transaction().map_err(|source| sqlite(&path, source))?;
        for migration in &migrations[found as usize..] {
            tx.execute_batch(migration).map_err(|source| sqlite(&path, source))?;
        }
        tx.pragma_update(None, "user_version", supported)
            .map_err(|source| sqlite(&path, source))?;
        tx.commit().map_err(|source| sqlite(&path, source))
    }

    fn ensure_host(&self) -> Result<(), StateError> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO host (id, host_id) VALUES (1, ?1)",
                params![new_host_id(&self.path)?],
            )
            .map_err(|source| sqlite(&self.path, source))?;
        Ok(())
    }

    /// This host's identity: stable for the database's life, carried over by export and import, so clients see a
    /// replacement Mac as the same host.
    pub fn host_id(&self) -> Result<String, StateError> {
        self.conn
            .query_row("SELECT host_id FROM host WHERE id = 1", [], |row| row.get(0))
            .map_err(|source| sqlite(&self.path, source))
    }

    pub fn bool(&self, setting: Setting) -> Result<Option<bool>, StateError> {
        let value: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![setting.key()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|source| sqlite(&self.path, source))?;
        // A value that doesn't decode counts as unset: the default applies and the next change overwrites it.
        Ok(value.and_then(|value| serde_json::from_str(&value).ok()))
    }

    pub fn set_bool(&self, setting: Setting, value: bool) -> Result<(), StateError> {
        self.conn
            .execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![setting.key(), serde_json::Value::Bool(value).to_string()],
            )
            .map_err(|source| sqlite(&self.path, source))?;
        Ok(())
    }

    /// The tables with rows besides this host's identity: state an import over this database would lose.
    fn tables_with_state(&self) -> Result<Vec<String>, StateError> {
        let path = &self.path;
        let mut statement = self
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .map_err(|source| sqlite(path, source))?;
        let names: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .and_then(Iterator::collect)
            .map_err(|source| sqlite(path, source))?;
        let mut with_rows = Vec::new();
        for name in names {
            if IDENTITY_TABLES.contains(&name.as_str()) {
                continue;
            }
            let any: bool = self
                .conn
                .query_row(&format!("SELECT EXISTS (SELECT 1 FROM \"{name}\")"), [], |row| {
                    row.get(0)
                })
                .map_err(|source| sqlite(path, source))?;
            if any {
                with_rows.push(name);
            }
        }
        Ok(with_rows)
    }
}

/// Writes a consistent copy of the database at `from` to `to`, which must not exist. Opens it read-only and never
/// migrates: a running daemon may hold it, at a version this binary would migrate.
pub fn export(from: &Path, to: &Path) -> Result<(), StateError> {
    if to.symlink_metadata().is_ok() {
        return Err(StateError::Exists { path: to.to_path_buf() });
    }
    let conn = open_existing(from)?;
    conn.execute("VACUUM INTO ?1", params![to.to_string_lossy()])
        .map_err(|source| sqlite(from, source))?;
    set_owner_only(to)
}

/// Whether the database at `path` is one this binary can use: it exists, is a state database, and isn't newer.
/// Read-only; never migrates.
pub fn check(path: &Path) -> Result<(), StateError> {
    open_existing(path).map(drop)
}

/// Opens an existing database read-only after checking its version.
fn open_existing(path: &Path) -> Result<Connection, StateError> {
    use rusqlite::OpenFlags;
    if !path.is_file() {
        return Err(StateError::Missing {
            path: path.to_path_buf(),
        });
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|source| sqlite(path, source))?;
    let found: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|source| sqlite(path, source))?;
    let supported = MIGRATIONS.len() as i64;
    if found > supported {
        return Err(StateError::Newer {
            path: path.to_path_buf(),
            found,
            supported,
        });
    }
    if found == 0 {
        return Err(StateError::NotAState {
            path: path.to_path_buf(),
        });
    }
    Ok(conn)
}

/// Replaces the database at `to` with the export at `from` (migrated to this version), keeping its `host_id`. Refuses
/// over a database that holds state unless `force`. The daemon must not be running: it holds the database open.
pub fn import(from: &Path, to: &Path, force: bool) -> Result<(), StateError> {
    if !force && to.exists() {
        let current = State::open(to)?;
        let tables = current.tables_with_state()?;
        if !tables.is_empty() {
            return Err(StateError::HasState {
                path: to.to_path_buf(),
                tables: tables.join(", "),
            });
        }
    }
    // Check and migrate a copy next to the target, then swap it in, so a failed import changes nothing.
    let staged = to.with_extension("db.import");
    let _ = std::fs::remove_file(&staged);
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir).map_err(|source| io(dir, source))?;
    }
    std::fs::copy(from, &staged).map_err(|source| io(from, source))?;
    let checked = (|| {
        let conn = Connection::open(&staged).map_err(|source| sqlite(from, source))?;
        let has_host: bool = conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'host')",
                [],
                |row| row.get(0),
            )
            .map_err(|source| sqlite(from, source))?;
        if !has_host {
            return Err(StateError::NotAState {
                path: from.to_path_buf(),
            });
        }
        drop(conn);
        State::open(&staged).map(drop)
    })();
    if let Err(e) = checked {
        let _ = remove_database(&staged);
        return Err(e);
    }
    remove_database(to)?;
    std::fs::rename(&staged, to).map_err(|source| io(to, source))
}

/// Removes a database file and its WAL companions.
fn remove_database(path: &Path) -> Result<(), StateError> {
    for suffix in ["", "-wal", "-shm"] {
        let file = PathBuf::from(format!("{}{suffix}", path.display()));
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(io(&file, source)),
        }
    }
    Ok(())
}

/// The copy of schema version `version` kept before migrating past it.
fn backup_path(path: &Path, version: i64) -> PathBuf {
    PathBuf::from(format!("{}.v{version}.bak", path.display()))
}

/// 128 random bits, hex.
fn new_host_id(path: &Path) -> Result<String, StateError> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|source| io(path, source))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn set_owner_only(path: &Path) -> Result<(), StateError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|source| io(path, source))
}

fn sqlite(path: &Path, source: rusqlite::Error) -> StateError {
    StateError::Sqlite {
        path: path.to_path_buf(),
        source,
    }
}

fn io(path: &Path, source: std::io::Error) -> StateError {
    StateError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("state").join(FILE)
    }

    #[test]
    fn creates_and_keeps_values_across_opens() {
        let dir = tempfile::tempdir().unwrap();
        let path = db(&dir);
        let state = State::open(&path).unwrap();
        let host = state.host_id().unwrap();
        assert_eq!(host.len(), 32);
        assert_eq!(state.bool(Setting::KeepAwake).unwrap(), None);
        state.set_bool(Setting::KeepAwake, false).unwrap();
        drop(state);

        // A restart: same host, same keep-awake.
        let state = State::open(&path).unwrap();
        assert_eq!(state.host_id().unwrap(), host);
        assert_eq!(state.bool(Setting::KeepAwake).unwrap(), Some(false));
        assert_eq!(state.version().unwrap(), MIGRATIONS.len() as i64);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        // A new database has nothing to back up.
        assert!(!backup_path(&path, 0).exists());
    }

    #[test]
    fn refuses_a_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = db(&dir);
        drop(State::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        drop(conn);
        assert!(matches!(State::open(&path), Err(StateError::Newer { found: 99, .. })));
    }

    /// A database at an older version is copied before the newer migrations run, and the copy keeps the old version.
    #[test]
    fn backs_up_before_migrating() {
        let dir = tempfile::tempdir().unwrap();
        let path = db(&dir);
        let mut state = State::open(&path).unwrap();
        state.set_bool(Setting::KeepAwake, false).unwrap();
        let next = [MIGRATIONS, &["CREATE TABLE later (x INTEGER);"]].concat();
        state.migrate(&next).unwrap();
        assert_eq!(state.version().unwrap(), next.len() as i64);

        let backup = Connection::open(backup_path(&path, MIGRATIONS.len() as i64)).unwrap();
        let version: i64 = backup.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        let value: String = backup
            .query_row("SELECT value FROM settings WHERE key = 'keep_awake'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(value, "false");
        let later: bool = backup
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = 'later')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!later, "the backup is the previous version");

        // A later migration keeps only the latest copy.
        let third = [next.as_slice(), &["CREATE TABLE third (x INTEGER);"]].concat();
        state.migrate(&third).unwrap();
        assert!(backup_path(&path, next.len() as i64).exists());
        assert!(!backup_path(&path, MIGRATIONS.len() as i64).exists());
        let next = third;

        // A failing migration leaves the database at its version.
        let broken = [next.as_slice(), &["CREATE TABLE later (x INTEGER);"]].concat();
        assert!(state.migrate(&broken).is_err());
        assert_eq!(state.version().unwrap(), next.len() as i64);
    }

    #[test]
    fn export_and_import_keep_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let path = db(&dir);
        let state = State::open(&path).unwrap();
        state.set_bool(Setting::KeepAwake, false).unwrap();
        let host = state.host_id().unwrap();
        let exported = dir.path().join("export.db");
        drop(state);
        export(&path, &exported).unwrap();
        assert!(matches!(export(&path, &exported), Err(StateError::Exists { .. })));
        // Export never creates a database.
        let none = dir.path().join("none").join(FILE);
        assert!(matches!(
            export(&none, &dir.path().join("x.db")),
            Err(StateError::Missing { .. })
        ));
        assert!(!none.exists());

        // A fresh database on the replacement Mac has only its identity: import replaces it.
        let other = dir.path().join("other").join(FILE);
        let fresh = State::open(&other).unwrap();
        assert_ne!(fresh.host_id().unwrap(), host);
        drop(fresh);
        import(&exported, &other, false).unwrap();
        let imported = State::open(&other).unwrap();
        assert_eq!(imported.host_id().unwrap(), host);
        assert_eq!(imported.bool(Setting::KeepAwake).unwrap(), Some(false));
        drop(imported);

        // Over a database that holds state, only with force.
        assert!(matches!(
            import(&exported, &other, false),
            Err(StateError::HasState { tables, .. }) if tables == "settings"
        ));
        import(&exported, &other, true).unwrap();

        // Something that isn't an export is refused and changes nothing.
        let junk = dir.path().join("junk.db");
        Connection::open(&junk)
            .unwrap()
            .execute_batch("CREATE TABLE t (x INTEGER);")
            .unwrap();
        assert!(matches!(import(&junk, &other, true), Err(StateError::NotAState { .. })));
        assert_eq!(State::open(&other).unwrap().host_id().unwrap(), host);
    }
}
