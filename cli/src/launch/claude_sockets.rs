//! The `pi-claude-code` preset's socket folder of a session's own: where pi's `claude` puts its messaging socket
//! (`<folder>/cc-socks/<pid>.sock`) when the user sets neither `XDG_RUNTIME_DIR` nor `CLAUDE_CODE_TMPDIR`.
//!
//! `claude` probes a socket file with its own pid before taking the name, and the pi-claude-code extension ends every
//! `claude` with SIGKILL, so the shared `/tmp/cc-socks` fills with stale sockets and the probe meets the closed
//! sandbox. A fresh folder per session holds only that session's own. The daemon picks it when it prepares a session,
//! creates it (0700) when the session launches, sets it as `CLAUDE_CODE_TMPDIR` in pi's environment, and removes it
//! when the session ends; at start, it removes what earlier runs of the same daemon left (one per state folder, so
//! other daemons sharing temp keep theirs).

use std::path::{Path, PathBuf};

/// The variable `claude` reads its temp root from.
pub const VAR: &str = "CLAUDE_CODE_TMPDIR";

/// The longest socket path `claude` uses there before falling back to the shared `/tmp/cc-socks-<uid>`, with its
/// longest name.
const SOCKET_MAX: usize = 103;
const SOCKET_TAIL: &str = "cc-socks/99999.sock";

/// The places a folder may be in: the user's temp folder, else (when that's too deep for the socket path) the shared
/// temp, which is sticky (`claude` accepts a sticky folder on the way).
fn roots(tmp: Option<&Path>) -> Vec<PathBuf> {
    tmp.map(crate::agent::canonical_target)
        .into_iter()
        .chain([PathBuf::from("/private/tmp")])
        .collect()
}

/// The prefix of this daemon's folders: `piton-cc-<8 hex of the state folder's path>-`.
fn prefix(state_dir: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    state_dir.hash(&mut hasher);
    format!("piton-cc-{:08x}-", hasher.finish() as u32)
}

/// A new folder's path for a session (not created): in `tmp` when `claude`'s socket path fits there, else in
/// `/private/tmp`. `None` when neither fits.
pub fn pick(tmp: Option<&Path>, state_dir: &Path) -> Option<PathBuf> {
    let name = format!("{}{}", prefix(state_dir), crate::agent::random_hex(4));
    roots(tmp)
        .into_iter()
        .map(|root| root.join(&name))
        .find(|folder| folder.join(SOCKET_TAIL).as_os_str().len() <= SOCKET_MAX)
}

/// Creates the folder, private to the user.
pub fn create(folder: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(folder)
}

/// Removes a session's folder and what's in it. A link in its place is removed, not followed.
pub fn remove(folder: &Path) {
    let link = folder.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink());
    let removed = match link {
        true => std::fs::remove_file(folder),
        false => std::fs::remove_dir_all(folder),
    };
    match removed {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => crate::log!("claude socket folder {}: can't remove it: {e}", folder.display()),
    }
}

/// Removes this daemon's leftover folders (sessions that ended without their hook running), in both places.
pub fn sweep(tmp: Option<&Path>, state_dir: &Path) {
    let prefix = prefix(state_dir);
    for root in roots(tmp) {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                remove(&entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder fits `claude`'s socket path; a temp folder too deep falls back to `/private/tmp`.
    #[test]
    fn picks_a_folder_the_socket_fits_in() {
        let state = Path::new("/h/.local/state/piton");
        let short = pick(Some(Path::new("/nonexistent/T")), state).unwrap();
        assert!(short.starts_with("/nonexistent/T") && short.join(SOCKET_TAIL).as_os_str().len() <= SOCKET_MAX);
        let deep = format!("/nonexistent/{}", "d".repeat(90));
        let fallback = pick(Some(Path::new(&deep)), state).unwrap();
        assert!(fallback.starts_with("/private/tmp"), "{}", fallback.display());
        assert_ne!(pick(None, state), pick(None, state), "a new folder each time");
    }

    /// The sweep removes this daemon's folders only, and removes a link in place of one without following it.
    #[test]
    fn sweeps_only_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("T");
        std::fs::create_dir(&tmp).unwrap();
        let state = Path::new("/h/.local/state/piton");
        let own = pick(Some(&tmp), state).unwrap();
        create(&own).unwrap();
        std::fs::create_dir_all(own.join("cc-socks")).unwrap();
        std::fs::write(own.join("cc-socks/1.sock"), "").unwrap();
        let other = pick(Some(&tmp), Path::new("/other/state")).unwrap();
        create(&other).unwrap();
        let target = dir.path().join("kept");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("x"), "").unwrap();
        let linked = tmp.join(format!("{}link", prefix(state)));
        std::os::unix::fs::symlink(&target, &linked).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&own).unwrap().permissions().mode() & 0o777, 0o700);

        sweep(Some(&tmp), state);
        assert!(!own.exists());
        assert!(!linked.symlink_metadata().is_ok());
        assert!(other.exists(), "another daemon's");
        assert!(target.join("x").exists(), "a link's target is left alone");
    }
}
