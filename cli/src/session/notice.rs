//! The warning a blocked process gets when no entry the block question saves would lift its block: what it was kept
//! from, what that cost when it's known, what would change it, and the `silence` entry that quiets it.
//!
//! Every notice that names a path or a Mach service ends with an entry that silences exactly it (for that process),
//! narrowed only as far as stays safe, so noise is quieted one notice at a time. Each case is matched exhaustively on
//! why the block can't be lifted.

use std::path::Path;

use crate::{
    agent::GitControlEntry,
    config::{Silence, SilenceTarget},
    launch::sandbox::{
        Access,
        grant::{DeniedBy, KeychainBlock, NoLift},
    },
};

/// One blocked process, as the notice names it.
pub struct Blocked<'a> {
    pub process: &'a str,
    pub operation: &'a str,
    pub target: &'a str,
    /// `operation target`, as the kernel names it.
    pub summary: &'a str,
}

/// What a notice is built from, beyond the block: where the user's files are, and how pi's `claude` signs in.
pub struct Context<'a> {
    pub home: &'a Path,
    /// The launch's checkout: its parent holds the source and its workers.
    pub checkout: &'a Path,
    pub claude: ClaudeSignIn,
}

/// How the launch's pi `claude` signs in, as far as a keychain block's notice needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeSignIn {
    /// The `pi-claude-code` preset with the token login: the keychain isn't needed.
    PresetToken,
    /// No preset, or its keychain login: pi's claude-code models need the keychain in the sandbox.
    Keychain,
}

impl ClaudeSignIn {
    /// From the `pi-claude-code` preset's login, when the preset is on.
    pub fn of(login: Option<crate::config::ClaudeLogin>) -> Self {
        use crate::config::ClaudeLogin;
        match login {
            Some(ClaudeLogin::Token) => Self::PresetToken,
            Some(ClaudeLogin::Keychain) | None => Self::Keychain,
        }
    }
}

/// The notice for a block no entry lifts.
pub fn notice(blocked: &Blocked, why: &NoLift, context: &Context) -> String {
    let Blocked {
        process,
        target,
        summary,
        ..
    } = blocked;
    let text = match why {
        NoLift::Decided(DeniedBy::Credential(access)) => credential(process, target, *access),
        NoLift::Decided(DeniedBy::Keychain(block)) => keychain(process, target, *block, context.claude),
        NoLift::Decided(DeniedBy::GitControl) => git(process, target),
        NoLift::Decided(DeniedBy::UserDeny) => {
            format!("The sandbox blocked {process}: {summary} (your [sandbox] deny).")
        }
        NoLift::Decided(DeniedBy::FnoxConfig) => format!(
            "The sandbox blocked {process}: {summary} (a file fnox loads; changed only through a write you approve)."
        ),
        NoLift::Decided(DeniedBy::Floor) => format!("The sandbox blocked {process}: {summary} (never lifted)."),
        NoLift::Unsupported => format!("The sandbox blocked {process}: {summary} (no [sandbox] entry lifts it)."),
        NoLift::PerProcess { folder } => format!(
            "The sandbox blocked {process}: {summary}. The name is {process}'s own (its pid), so an entry for it would \
             never match again; a `[sandbox]` entry for {} written by hand covers every process there.",
            folder.display()
        ),
    };
    match silence_entry(blocked, why, context) {
        Some(entry) => format!(
            "{text} To stop this notice, add `silence = [{}]` to [sandbox] in your global config.toml or a \
             config.local.toml.",
            entry.inline()
        ),
        None => text,
    }
}

/// The entry that silences this notice, narrowed as far as stays safe; `None` when the target is neither a path nor a
/// Mach service (a TCP address).
pub fn silence_entry(blocked: &Blocked, why: &NoLift, context: &Context) -> Option<Silence> {
    let target = Path::new(blocked.target);
    let process = Some(blocked.process.to_string());
    let path = |path: &Path| {
        Some(Silence {
            target: SilenceTarget::Path(shown(path, context.home)),
            process: process.clone(),
        })
    };
    if !target.is_absolute() {
        return match blocked.operation {
            "mach-lookup" => Some(Silence {
                target: SilenceTarget::Service(blocked.target.to_string()),
                process,
            }),
            // A connection to a host or an address and port.
            "network-outbound" => crate::launch::sandbox::Destination::parse(blocked.target).map(|to| Silence {
                target: SilenceTarget::Host(to.to_string()),
                process,
            }),
            // A TCP listen, on any port.
            op if crate::launch::sandbox::is_listen(op, blocked.target) => Some(Silence {
                target: SilenceTarget::TcpListen,
                process,
            }),
            _ => None,
        };
    }
    match why {
        // The folder holding it, unless that folder is too broad to quiet (`~/.netrc`: the file itself).
        NoLift::Decided(DeniedBy::Credential(_) | DeniedBy::Keychain(_)) => match target.parent() {
            Some(folder) if !too_broad(folder, context.home) => path(folder),
            Some(_) | None => path(target),
        },
        // That entry in every checkout beside this one (the source and its workers).
        NoLift::Decided(DeniedBy::GitControl) => {
            let entry = GitControlEntry::of(target);
            let parent = context.checkout.parent().filter(|p| !too_broad(p, context.home));
            match (entry, parent) {
                (Some(GitControlEntry::GitDir), Some(parent)) => path(&parent.join("*/.git")),
                (Some(entry), Some(parent)) => path(&parent.join("**/.git").join(entry.name())),
                (Some(_) | None, None) | (None, Some(_)) => path(target),
            }
        }
        NoLift::PerProcess { folder } => path(folder),
        NoLift::Decided(DeniedBy::UserDeny | DeniedBy::FnoxConfig | DeniedBy::Floor) | NoLift::Unsupported => {
            path(target)
        }
    }
}

/// A folder too broad to quiet a notice with: the root, the home folder, or a folder directly in it.
fn too_broad(folder: &Path, home: &Path) -> bool {
    folder.parent().is_none() || folder == home || folder.parent() == Some(home)
}

/// A path as the user writes it: `~/...` below the home folder.
fn shown(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// A credential: what it was kept from. A read could be lifted with `read`, but that exposes the credential to every
/// sandboxed process, and programs nearly always carry on without it (ssh reads a private key's public half, then signs
/// through the agent), so it's named as a last resort.
fn credential(process: &str, target: &str, access: Access) -> String {
    match access {
        Access::Read => format!(
            "The sandbox kept {process} from reading {target} (a credential). Programs usually carry on without it. \
             `read = [\"{target}\"]` in [sandbox] would expose it to the sandbox: add it only for a program that \
             fails without it."
        ),
        Access::Write => format!("The sandbox kept {process} from writing {target} (a credential; never writable)."),
        Access::Connect => format!("The sandbox kept {process} from connecting to {target} (a credential)."),
        Access::Bind => {
            format!("The sandbox kept {process} from creating a socket at {target} (a credential; never writable).")
        }
    }
}

/// The keychain: with fnox on it stays closed whatever is listed, since the launch would be refused otherwise.
fn keychain(process: &str, target: &str, block: KeychainBlock, claude: ClaudeSignIn) -> String {
    let base = format!("The sandbox kept {process} from the keychain ({target}).");
    let fix = match block {
        KeychainBlock::Readable => "If it needs it, add `read = [\"~/Library/Keychains\"]` to [sandbox] in your \
                                    global config.toml or a config.local.toml."
            .to_string(),
        KeychainBlock::ClosedForFnox => "It stays closed while fnox runs: the credentials of fnox's secret sources \
                                         live there, so a sandbox that can read it isn't started."
            .to_string(),
    };
    // pi's claude-code models sign in through the keychain unless the preset gives `claude` a token.
    let claude = match (process, claude) {
        ("claude", ClaudeSignIn::Keychain) => {
            " For pi's claude-code models, turn on the pi-claude-code preset and sign claude in with a setup token \
             (`claude setup-token`) instead."
        }
        ("claude", ClaudeSignIn::PresetToken) | (_, ClaudeSignIn::Keychain | ClaudeSignIn::PresetToken) => "",
    };
    format!("{base} {fix}{claude}")
}

/// Git's hooks and configuration, by the entry refused: what Git skipped. The `git` preset opens them, but lets the
/// agent plant what Git on the host runs, so it isn't offered.
fn git(process: &str, target: &str) -> String {
    let cost = match GitControlEntry::of(Path::new(target)) {
        Some(GitControlEntry::Config) => {
            "Git skipped that step (an upstream setting, a deleted branch's section) and carried on"
        }
        Some(GitControlEntry::Hooks) => "the hook wasn't installed",
        Some(GitControlEntry::ConfigWorktree | GitControlEntry::Commondir | GitControlEntry::GitDir) | None => {
            "the change wasn't made"
        }
    };
    format!(
        "The sandbox kept {process} from writing {target} (Git's hooks and configuration, which Git on the host \
         runs): {cost}."
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// The home folder and checkout a test's context names.
    fn context_for_tests() -> (PathBuf, PathBuf) {
        ("/h".into(), "/h/Projects/tooling/Piton--1-x".into())
    }
    use crate::launch::sandbox::{SilenceSource, Silenced, silenced};

    fn blocked<'a>(process: &'a str, operation: &'a str, target: &'a str, summary: &'a str) -> Blocked<'a> {
        Blocked {
            process,
            operation,
            target,
            summary,
        }
    }

    /// The entry a notice names, resolved as the configuration resolves it, silences that refusal.
    fn silences(entry: &Silence, home: &Path, process: &str, operation: &str, target: &str) -> bool {
        let target_path = match &entry.target {
            SilenceTarget::Path(p) => crate::launch::sandbox::Silent::Path(match p.strip_prefix('~') {
                Some(rest) => PathBuf::from(format!("{}{rest}", home.display())),
                None => PathBuf::from(p),
            }),
            SilenceTarget::Service(s) => crate::launch::sandbox::Silent::Service(s.clone()),
            SilenceTarget::Host(h) => match crate::config::Connect::try_from(h.clone()) {
                Ok(crate::config::Connect::Network(t)) => crate::launch::sandbox::Silent::Host(t),
                Ok(crate::config::Connect::Socket(_)) | Err(_) => panic!("{h}"),
            },
            SilenceTarget::TcpListen => crate::launch::sandbox::Silent::TcpListen,
        };
        let resolved = [Silenced {
            target: target_path,
            process: entry.process.clone(),
            source: SilenceSource::File("/h/.config/piton/config.toml".into()),
        }];
        silenced(&resolved, process, operation, target).is_some()
    }

    /// A keychain block with fnox on suggests no `read`; with fnox off it does; `claude`'s names the preset.
    #[test]
    fn keychain_notices_follow_fnox() {
        let (home, checkout) = context_for_tests();
        let context = Context {
            home: &home,
            checkout: &checkout,
            claude: ClaudeSignIn::Keychain,
        };
        let b = blocked(
            "security",
            "mach-lookup",
            "com.apple.SecurityServer",
            "mach-lookup com.apple.SecurityServer",
        );
        let closed = notice(
            &b,
            &NoLift::Decided(DeniedBy::Keychain(KeychainBlock::ClosedForFnox)),
            &context,
        );
        assert!(!closed.contains("read ="), "{closed}");
        assert!(closed.contains("fnox"), "{closed}");
        let readable = notice(
            &b,
            &NoLift::Decided(DeniedBy::Keychain(KeychainBlock::Readable)),
            &context,
        );
        assert!(readable.contains("read = [\"~/Library/Keychains\"]"), "{readable}");
        let claude = blocked("claude", "mach-lookup", "com.apple.SecurityServer", "x");
        let named = notice(
            &claude,
            &NoLift::Decided(DeniedBy::Keychain(KeychainBlock::ClosedForFnox)),
            &context,
        );
        assert!(named.contains("pi-claude-code preset"), "{named}");
        // Already on with its token: not named again.
        let on = Context {
            claude: ClaudeSignIn::PresetToken,
            ..context
        };
        let quiet = notice(
            &claude,
            &NoLift::Decided(DeniedBy::Keychain(KeychainBlock::ClosedForFnox)),
            &on,
        );
        assert!(!quiet.contains("turn on"), "{quiet}");
        // No message carries a source line's indentation.
        assert!(!named.contains("  "), "{named}");
        // The service entry it names silences exactly that lookup, by that process.
        let entry = silence_entry(
            &b,
            &NoLift::Decided(DeniedBy::Keychain(KeychainBlock::Readable)),
            &context,
        )
        .unwrap();
        assert!(silences(
            &entry,
            &home,
            "security",
            "mach-lookup",
            "com.apple.SecurityServer"
        ));
        assert!(!silences(
            &entry,
            &home,
            "node",
            "mach-lookup",
            "com.apple.SecurityServer"
        ));
    }

    /// A credential read doesn't recommend `read` as the fix; its entry is the folder, or the file when the folder is
    /// the home folder.
    #[test]
    fn credential_notices_name_read_as_a_last_resort() {
        let (home, checkout) = context_for_tests();
        let context = Context {
            home: &home,
            checkout: &checkout,
            claude: ClaudeSignIn::Keychain,
        };
        let why = NoLift::Decided(DeniedBy::Credential(Access::Read));
        let ssh = blocked(
            "ssh",
            "file-read-data",
            "/h/.ssh/keys/id_ed25519",
            "file-read-data /h/.ssh/keys/id_ed25519",
        );
        let text = notice(&ssh, &why, &context);
        assert!(text.contains("usually carry on") && text.contains("expose"), "{text}");
        assert!(
            text.contains(r#"{ process = "ssh", target = "~/.ssh/keys" }"#),
            "{text}"
        );
        let entry = silence_entry(&ssh, &why, &context).unwrap();
        assert!(silences(
            &entry,
            &home,
            "ssh",
            "file-read-data",
            "/h/.ssh/keys/id_ed25519"
        ));
        let netrc = blocked("curl", "file-read-data", "/h/.netrc", "x");
        assert_eq!(
            silence_entry(&netrc, &why, &context).unwrap().target,
            SilenceTarget::Path("~/.netrc".into())
        );
        let write = notice(
            &blocked("ssh", "file-write-data", "/h/.ssh/known_hosts", "x"),
            &NoLift::Decided(DeniedBy::Credential(Access::Write)),
            &context,
        );
        assert!(!write.contains("read ="), "{write}");
    }

    /// The git notice says what Git skipped, offers no preset, and its entry quiets every checkout beside this one.
    #[test]
    fn git_notices_say_what_was_skipped() {
        let (home, checkout) = context_for_tests();
        let context = Context {
            home: &home,
            checkout: &checkout,
            claude: ClaudeSignIn::Keychain,
        };
        let why = NoLift::Decided(DeniedBy::GitControl);
        let config = "/h/Projects/tooling/Piton--1-x/.git/config";
        let push = blocked("git", "file-write-create", config, "x");
        let text = notice(&push, &why, &context);
        assert!(text.contains("upstream") && !text.contains("preset"), "{text}");
        let entry = silence_entry(&push, &why, &context).unwrap();
        assert_eq!(
            entry.target,
            SilenceTarget::Path("~/Projects/tooling/**/.git/config".into())
        );
        for worker in [
            config,
            "/h/Projects/tooling/Piton--2-y/.git/config",
            "/h/Projects/tooling/Piton/.git/config",
        ] {
            assert!(silences(&entry, &home, "git", "file-write-create", worker), "{worker}");
        }
        assert!(!silences(
            &entry,
            &home,
            "git",
            "file-write-create",
            "/h/Projects/tooling/Piton/.git/config.bak"
        ));
        let hook = notice(
            &blocked(
                "node",
                "file-write-create",
                "/h/Projects/tooling/Piton/.git/hooks/pre-commit",
                "x",
            ),
            &why,
            &context,
        );
        assert!(hook.contains("hook wasn't installed"), "{hook}");
    }

    /// A per-process path names its folder; a block whose target isn't a path or a service has no entry.
    #[test]
    fn per_process_and_address_notices() {
        let (home, checkout) = context_for_tests();
        let context = Context {
            home: &home,
            checkout: &checkout,
            claude: ClaudeSignIn::Keychain,
        };
        let why = NoLift::PerProcess {
            folder: "/private/tmp/cc-socks".into(),
        };
        let probe = blocked("claude", "network-outbound", "/private/tmp/cc-socks/64421.sock", "x");
        let text = notice(&probe, &why, &context);
        assert!(text.contains("never match again"), "{text}");
        let entry = silence_entry(&probe, &why, &context).unwrap();
        assert!(silences(
            &entry,
            &home,
            "claude",
            "network-outbound",
            "/private/tmp/cc-socks/1234.sock"
        ));
        let tcp = blocked(
            "node",
            "network-outbound",
            "1.2.3.4:443",
            "network-outbound 1.2.3.4:443",
        );
        // A connection's notice names a host entry, which silences that destination only.
        let entry = silence_entry(&tcp, &NoLift::Unsupported, &context).unwrap();
        assert_eq!(entry.target, SilenceTarget::Host("1.2.3.4:443".into()));
        assert!(notice(&tcp, &NoLift::Unsupported, &context).contains("silence"));
        assert!(silences(&entry, &home, "node", "network-outbound", "1.2.3.4:443"));
        assert!(!silences(&entry, &home, "node", "network-outbound", "1.2.3.4:80"));
        // A listen's notice names `tcp-listen`, which silences that process's listens on any port.
        let listen = blocked(
            "node",
            "network-inbound",
            "local:*:50958",
            "network-inbound local:*:50958",
        );
        let entry = silence_entry(&listen, &NoLift::Unsupported, &context).unwrap();
        assert_eq!(entry.inline(), r#"{ process = "node", target = "tcp-listen" }"#);
        assert!(silences(&entry, &home, "node", "network-inbound", "local:*:61000"));
        assert!(!silences(&entry, &home, "python3", "network-inbound", "local:*:61000"));
        assert!(!silences(&entry, &home, "node", "network-outbound", "1.2.3.4:443"));
    }
}
