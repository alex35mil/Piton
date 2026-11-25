//! Opening the database, its schema's migrations, and the backup before migrating.
//!
//! `PRAGMA user_version` is the schema version. At open, ordered migrations bring an older database up to date in one
//! transaction, after a `VACUUM INTO` backup of the previous version; a database newer than this binary is refused.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};

use super::{StateError, io, set_owner_only, sqlite};

/// The database file in the daemon's state folder.
pub const FILE: &str = "piton.db";

/// Each migration takes the schema from its index to the next version; the schema version is their count.
pub(super) const MIGRATIONS: &[&str] = &[
    // 1: the host's identity.
    "CREATE TABLE host (id INTEGER PRIMARY KEY CHECK (id = 1), host_id TEXT NOT NULL);",
    // 2: the user's trust decisions, per source checkout: the answer, the grants it was given for (JSON), and when.
    "CREATE TABLE trust (source TEXT PRIMARY KEY, decision TEXT NOT NULL CHECK (decision IN ('trusted', 'declined')), \
     grants TEXT NOT NULL, decided_at TEXT NOT NULL);",
];

/// Tables whose rows are this host's identity rather than state an import would lose.
const IDENTITY_TABLES: &[&str] = &["host"];

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

    /// The decision recorded for a source checkout; `None` when there's none.
    pub fn trust(&self, source: &Path) -> Result<Option<TrustRecord>, StateError> {
        read_trust(&self.conn, &self.path, source)
    }

    /// Records the decision for a source checkout, replacing any earlier one.
    pub fn set_trust(&self, source: &Path, record: &TrustRecord) -> Result<(), StateError> {
        let grants = serde_json::to_string(&record.grants).map_err(|e| StateError::Encoding {
            path: self.path.clone(),
            reason: e.to_string(),
        })?;
        self.conn
            .execute(
                "INSERT INTO trust (source, decision, grants, decided_at) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (source) DO UPDATE SET decision = ?2, grants = ?3, decided_at = ?4",
                params![
                    source.to_string_lossy(),
                    record.decision.name(),
                    grants,
                    record.decided_at
                ],
            )
            .map_err(|source| sqlite(&self.path, source))?;
        Ok(())
    }

    /// Forgets the decision for a source checkout: the next launch there asks again. Whether there was one.
    pub fn reset_trust(&self, source: &Path) -> Result<bool, StateError> {
        self.conn
            .execute("DELETE FROM trust WHERE source = ?1", params![source.to_string_lossy()])
            .map(|n| n > 0)
            .map_err(|source| sqlite(&self.path, source))
    }

    /// The tables with rows besides this host's identity: state an import over this database would lose.
    pub(super) fn tables_with_state(&self) -> Result<Vec<String>, StateError> {
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

/// What the user decided about a repository's grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Trusted,
    Declined,
}

impl Decision {
    fn name(self) -> &'static str {
        match self {
            Decision::Trusted => "trusted",
            Decision::Declined => "declined",
        }
    }
}

/// One recorded decision: the answer, the grants the user was shown when giving it, and when (RFC 3339, UTC).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustRecord {
    pub decision: Decision,
    pub grants: crate::config::Grants,
    pub decided_at: String,
}

/// The decision for `source` in an open database (also used read-only, outside the daemon).
pub(super) fn read_trust(conn: &Connection, path: &Path, source: &Path) -> Result<Option<TrustRecord>, StateError> {
    use rusqlite::OptionalExtension;
    let row: Option<(String, String, String)> = conn
        .query_row(
            "SELECT decision, grants, decided_at FROM trust WHERE source = ?1",
            params![source.to_string_lossy()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|e| sqlite(path, e))?;
    let Some((decision, grants, decided_at)) = row else {
        return Ok(None);
    };
    let corrupt = |reason: String| StateError::Encoding {
        path: path.to_path_buf(),
        reason,
    };
    let decision = match decision.as_str() {
        "trusted" => Decision::Trusted,
        "declined" => Decision::Declined,
        other => return Err(corrupt(format!("trust decision {other:?}"))),
    };
    let grants = serde_json::from_str(&grants).map_err(|e| corrupt(format!("trusted grants: {e}")))?;
    Ok(Some(TrustRecord {
        decision,
        grants,
        decided_at,
    }))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn db(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("state").join(FILE)
    }

    #[test]
    fn creates_and_keeps_the_host_across_opens() {
        let dir = tempfile::tempdir().unwrap();
        let path = db(&dir);
        let state = State::open(&path).unwrap();
        let host = state.host_id().unwrap();
        assert_eq!(host.len(), 32);
        drop(state);

        // A restart: same host.
        let state = State::open(&path).unwrap();
        assert_eq!(state.host_id().unwrap(), host);
        assert_eq!(state.version().unwrap(), MIGRATIONS.len() as i64);
        assert!(state.tables_with_state().unwrap().is_empty());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        // A new database has nothing to back up.
        assert!(!backup_path(&path, 0).exists());
    }

    #[test]
    fn keeps_trust_per_source() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open(&db(&dir)).unwrap();
        let source = Path::new("/Users/a/repo");
        assert_eq!(state.trust(source).unwrap(), None);
        let record = TrustRecord {
            decision: Decision::Trusted,
            grants: [crate::config::GrantEntry::item("sandbox.presets", "rust")].into(),
            decided_at: "2026-11-24T00:00:00Z".into(),
        };
        state.set_trust(source, &record).unwrap();
        assert_eq!(state.trust(source).unwrap(), Some(record.clone()));
        assert_eq!(state.trust(Path::new("/Users/a/other")).unwrap(), None);
        let declined = TrustRecord {
            decision: Decision::Declined,
            grants: Default::default(),
            decided_at: "2026-11-25T00:00:00Z".into(),
        };
        state.set_trust(source, &declined).unwrap();
        assert_eq!(state.trust(source).unwrap(), Some(declined));
        assert_eq!(state.tables_with_state().unwrap(), ["trust"]);
        assert!(state.reset_trust(source).unwrap());
        assert!(!state.reset_trust(source).unwrap());
        assert_eq!(state.trust(source).unwrap(), None);
    }

    /// A database from before trust was kept gets the table, keeping its host.
    #[test]
    fn migrates_the_first_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = db(&dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.execute("INSERT INTO host (id, host_id) VALUES (1, 'h')", [])
            .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        drop(conn);
        let state = State::open(&path).unwrap();
        assert_eq!(state.host_id().unwrap(), "h");
        assert_eq!(state.trust(Path::new("/r")).unwrap(), None);
        assert!(backup_path(&path, 1).exists());
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
        let host = state.host_id().unwrap();
        let next = [MIGRATIONS, &["CREATE TABLE later (x INTEGER);"]].concat();
        state.migrate(&next).unwrap();
        assert_eq!(state.version().unwrap(), next.len() as i64);

        let backup = Connection::open(backup_path(&path, MIGRATIONS.len() as i64)).unwrap();
        let version: i64 = backup.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        let kept: String = backup
            .query_row("SELECT host_id FROM host WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, host);
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
}
