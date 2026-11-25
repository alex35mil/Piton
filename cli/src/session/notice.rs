//! The warning a blocked process gets when no entry the block question saves would lift its block: what it was kept
//! from, what that cost when it's known, what would change it, and the entry that keeps it blocked without a notice.
//!
//! A block decided by an entry the user wrote (a `"deny"`, a `"read"` written to, a `false`) is only logged and never
//! reaches here. Every other notice ends with the entry that turns it into one: a `"deny"` in the block's own table for
//! a path or a host (narrowed only as far as stays safe), `allow_git_control_files = false` for Git's files, and a
//! `silence` entry only for what has no path or host (a Mach service, a TCP listen). Each case is matched exhaustively
//! on why the block can't be lifted.

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

/// The entry that keeps a block blocked without a notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Quiet {
    /// `"<path>" = "deny"` in a path table: closes it for every process, approved commands included.
    Deny { table: &'static str, key: String },
    /// `allow_git_control_files = false`.
    GitControlOff,
    /// A `silence` entry: a Mach service or TCP listening, for that process.
    Silence(Silence),
}

impl std::fmt::Display for Quiet {
    /// As the notice names it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Quiet::Deny { table, key } => write!(
                f,
                "`{} = \"deny\"` in [sandbox.{table}] (it closes that path to every process, commands you approve \
                 included)",
                toml_edit::Key::new(key.as_str()).display_repr()
            ),
            Quiet::GitControlOff => f.write_str("`allow_git_control_files = false` in [sandbox]"),
            Quiet::Silence(entry) => write!(f, "`silence = [{}]` in [sandbox]", entry.inline()),
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
            format!("The sandbox blocked {process}: {summary} (your own [sandbox] entry).")
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
    match quiet(blocked, why, context) {
        Some(entry) => {
            format!("{text} To stop this notice, add {entry} in your global config.toml or a config.local.toml.")
        }
        None => text,
    }
}

/// The entry that quiets this notice, narrowed as far as stays safe; `None` when nothing names it (the floor, what's
/// already the user's own).
pub fn quiet(blocked: &Blocked, why: &NoLift, context: &Context) -> Option<Quiet> {
    let target = Path::new(blocked.target);
    let silence = |target| {
        Some(Quiet::Silence(Silence {
            target,
            process: Some(blocked.process.to_string()),
        }))
    };
    if !target.is_absolute() {
        return match blocked.operation {
            "mach-lookup" => silence(SilenceTarget::Service(blocked.target.to_string())),
            // A connection to a host or an address and port.
            "network-outbound" => crate::launch::sandbox::Destination::parse(blocked.target).map(|to| Quiet::Deny {
                table: "network.connections",
                key: to.to_string(),
            }),
            // A TCP listen, on any port.
            op if crate::launch::sandbox::is_listen(op, blocked.target) => silence(SilenceTarget::TcpListen),
            _ => None,
        };
    }
    let table = match blocked.operation {
        "network-outbound" => "network.unix_socket_connections",
        "network-bind" => "network.unix_socket_bindings",
        _ => "filesystem",
    };
    let deny = |path: &Path| {
        Some(Quiet::Deny {
            table,
            key: shown(path, context.home),
        })
    };
    match why {
        // The folder holding it, unless that folder is too broad to close (`~/.netrc`: the file itself).
        NoLift::Decided(DeniedBy::Credential(_) | DeniedBy::Keychain(_)) => match target.parent() {
            Some(folder) if !too_broad(folder, context.home) => deny(folder),
            Some(_) | None => deny(target),
        },
        NoLift::Decided(DeniedBy::GitControl) => Some(Quiet::GitControlOff),
        NoLift::PerProcess { folder } => deny(folder),
        NoLift::Unsupported => deny(target),
        // Already the user's own, or never liftable: nothing to add.
        NoLift::Decided(DeniedBy::UserDeny | DeniedBy::FnoxConfig | DeniedBy::Floor) => None,
    }
}

/// A folder too broad to close with a notice's entry: the root, the home folder, or a folder directly in it.
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

/// A credential: what it was kept from. A read could be reopened with `"read"`, but that exposes the credential to
/// every sandboxed process, and programs nearly always carry on without it (ssh reads a private key's public half, then
/// signs through the agent), so it's named as a last resort.
fn credential(process: &str, target: &str, access: Access) -> String {
    match access {
        Access::Read => format!(
            "The sandbox kept {process} from reading {target} (a credential). Programs usually carry on without it. \
             `\"{target}\" = \"read\"` in [sandbox.filesystem] would expose it to the sandbox: add it only for a \
             program that fails without it."
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
        KeychainBlock::Readable => "If it needs it, add `\"~/Library/Keychains\" = \"read\"` to [sandbox.filesystem] \
                                    in your global config.toml or a config.local.toml."
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

/// Git's hooks and configuration, by the entry refused: what Git skipped. `allow_git_control_files` opens them, but
/// lets the agent plant what Git on the host runs, so it isn't offered.
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

    /// The home folder a test's context names.
    fn home() -> PathBuf {
        "/h".into()
    }

    fn blocked<'a>(process: &'a str, operation: &'a str, target: &'a str, summary: &'a str) -> Blocked<'a> {
        Blocked {
            process,
            operation,
            target,
            summary,
        }
    }

    fn context(home: &Path) -> Context<'_> {
        Context {
            home,
            claude: ClaudeSignIn::Keychain,
        }
    }

    /// A keychain block with fnox on suggests no `"read"`; with fnox off it does; `claude`'s names the preset; a lookup
    /// is quieted by a `silence` entry for that process.
    #[test]
    fn keychain_notices_follow_fnox() {
        let home = home();
        let context = context(&home);
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
        assert!(!closed.contains("= \"read\""), "{closed}");
        assert!(closed.contains("fnox"), "{closed}");
        let readable = notice(
            &b,
            &NoLift::Decided(DeniedBy::Keychain(KeychainBlock::Readable)),
            &context,
        );
        assert!(readable.contains("`\"~/Library/Keychains\" = \"read\"`"), "{readable}");
        assert!(
            readable.contains(r#"silence = [{ process = "security", target = "com.apple.SecurityServer" }]"#),
            "{readable}"
        );
        let claude = blocked("claude", "mach-lookup", "com.apple.SecurityServer", "x");
        let named = notice(
            &claude,
            &NoLift::Decided(DeniedBy::Keychain(KeychainBlock::ClosedForFnox)),
            &context,
        );
        assert!(named.contains("pi-claude-code preset"), "{named}");
        assert!(!named.contains("  "), "no source line's indentation: {named}");
    }

    /// A credential read names `"read"` only as a last resort; its quieting entry is a `"deny"` of the folder, or of
    /// the file when the folder is the home folder.
    #[test]
    fn credential_notices_name_a_deny() {
        let home = home();
        let context = context(&home);
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
            text.contains("`\"~/.ssh/keys\" = \"deny\"` in [sandbox.filesystem]"),
            "{text}"
        );
        assert!(text.contains("commands you approve"), "{text}");
        let netrc = blocked("curl", "file-read-data", "/h/.netrc", "x");
        assert_eq!(
            quiet(&netrc, &why, &context),
            Some(Quiet::Deny {
                table: "filesystem",
                key: "~/.netrc".into()
            })
        );
    }

    /// The git notice says what Git skipped and names `allow_git_control_files = false`.
    #[test]
    fn git_notices_say_what_was_skipped() {
        let home = home();
        let context = context(&home);
        let why = NoLift::Decided(DeniedBy::GitControl);
        let push = blocked(
            "git",
            "file-write-create",
            "/h/Projects/tooling/Piton--1-x/.git/config",
            "x",
        );
        let text = notice(&push, &why, &context);
        assert!(
            text.contains("upstream") && text.contains("allow_git_control_files = false"),
            "{text}"
        );
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

    /// A per-process path names its folder in its own table; a connection names a host deny; a listen names
    /// `silence`; the floor names nothing.
    #[test]
    fn per_process_address_and_floor_notices() {
        let home = home();
        let context = context(&home);
        let why = NoLift::PerProcess {
            folder: "/private/tmp/cc-socks".into(),
        };
        let probe = blocked("claude", "network-outbound", "/private/tmp/cc-socks/64421.sock", "x");
        assert!(notice(&probe, &why, &context).contains("never match again"));
        assert_eq!(
            quiet(&probe, &why, &context),
            Some(Quiet::Deny {
                table: "network.unix_socket_connections",
                key: "/private/tmp/cc-socks".into()
            })
        );
        let tcp = blocked(
            "node",
            "network-outbound",
            "1.2.3.4:443",
            "network-outbound 1.2.3.4:443",
        );
        assert_eq!(
            quiet(&tcp, &NoLift::Unsupported, &context),
            Some(Quiet::Deny {
                table: "network.connections",
                key: "1.2.3.4:443".into()
            })
        );
        let listen = blocked(
            "node",
            "network-inbound",
            "local:*:50958",
            "network-inbound local:*:50958",
        );
        assert!(
            notice(&listen, &NoLift::Unsupported, &context)
                .contains(r#"silence = [{ process = "node", target = "tcp-listen" }]"#)
        );
        let floor = blocked("pi", "file-write-create", "/h/.pi/agent/settings.json", "x");
        assert_eq!(quiet(&floor, &NoLift::Decided(DeniedBy::Floor), &context), None);
    }
}
