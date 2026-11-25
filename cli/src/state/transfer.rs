//! Moving the database to a replacement Mac (`piton state export|import`), and the read-only check `piton setup` uses.
//! Neither ever migrates a database it only reads: a running daemon may hold it.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};

use super::{
    StateError,
    db::{MIGRATIONS, State},
    io, set_owner_only, sqlite,
};

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

/// The decision recorded for `source`, read-only, for `piton config` and `piton sandbox` outside the daemon: `None`
/// when there's none, or no database yet.
pub fn read_trust(path: &Path, source: &Path) -> Result<Option<super::TrustRecord>, StateError> {
    let conn = match open_existing(path) {
        Ok(conn) => conn,
        Err(StateError::Missing { .. }) => return Ok(None),
        Err(e) => return Err(e),
    };
    // A database from before trust was kept has no table yet: nothing decided.
    let has_table: bool = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'trust')",
            [],
            |row| row.get(0),
        )
        .map_err(|source| sqlite(path, source))?;
    match has_table {
        true => super::db::read_trust(&conn, path, source),
        false => Ok(None),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::FILE;

    #[test]
    fn export_and_import_keep_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join(FILE);
        let state = State::open(&path).unwrap();
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
        assert_eq!(State::open(&other).unwrap().host_id().unwrap(), host);

        // Over a database that holds state, only with force.
        Connection::open(&other)
            .unwrap()
            .execute_batch("CREATE TABLE sessions (x INTEGER); INSERT INTO sessions VALUES (1);")
            .unwrap();
        assert!(matches!(
            import(&exported, &other, false),
            Err(StateError::HasState { tables, .. }) if tables == "sessions"
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
