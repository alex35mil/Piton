//! `open` and `osascript` asked to run outside the sandbox: an approved rerun launches apps (`grant::all` opens
//! Launch Services), and macOS starts what they name outside any sandbox, so what's asked depends on the target,
//! decided by the daemon from the command.
//!
//! - `osascript`: refused. AppleScript drives other apps; nothing in the question could say what it will do.
//! - `open https://…`: a plain confirmation; the browser opens a page.
//! - `open` of a file or app the launch's sandbox could have written, as a path or a `file:` URL: refused. The agent
//!   could have put anything there, and macOS would run it unsandboxed.
//! - `open` of another URL scheme: the question names the app Launch Services hands it to. No app, or an app the
//!   sandbox could have written (one the agent built and registered): refused.
//! - Anything else `open` takes (an app by name, a file the sandbox can't write): the usual question, saying what opens.

mod handler;

use std::path::{Path, PathBuf};

use crate::launch::sandbox::{Access, Policy};

/// What a rerun of an `open` or `osascript` command gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenCheck {
    /// Not `open` or `osascript`: the usual question.
    Other,
    /// Ask, with this said about it.
    Ask { note: String },
    /// Never offered, with why.
    Refused { reason: String },
}

/// Checks a command's nodes (as matched) before it's offered to run outside the sandbox.
pub fn check(commands: &[String], cwd: &Path, policy: &Policy) -> OpenCheck {
    check_with(commands, cwd, policy, handler::default_handler)
}

/// [`check`], with `handler` naming the app a URL scheme opens in.
fn check_with(
    commands: &[String],
    cwd: &Path,
    policy: &Policy,
    handler: impl Fn(&str) -> Option<PathBuf>,
) -> OpenCheck {
    let mut notes = Vec::new();
    for command in commands {
        let words: Vec<&str> = command.split_whitespace().collect();
        let Some(program) = words.first().map(|p| p.rsplit('/').next().unwrap_or(p)) else {
            continue;
        };
        match program {
            "osascript" => {
                return OpenCheck::Refused {
                    reason: "osascript drives other apps outside the sandbox; run it yourself".into(),
                };
            }
            "open" => {
                for target in words[1..].iter().copied().filter(|w| !w.starts_with('-')) {
                    match classify(target, cwd, policy, &handler) {
                        Target::Web => notes.push(format!("opens {target} in your browser")),
                        Target::Scheme { app } => {
                            let name = app
                                .file_stem()
                                .map_or_else(|| app.display().to_string(), |s| s.to_string_lossy().into_owned());
                            notes.push(format!("opens {target} in {name} ({})", app.display()));
                        }
                        Target::NoHandler => {
                            return OpenCheck::Refused {
                                reason: format!("no app handles {target}"),
                            };
                        }
                        Target::BadFileUrl => {
                            return OpenCheck::Refused {
                                reason: format!("{target} isn't a file URL of a path on this Mac"),
                            };
                        }
                        Target::Writable(path) => {
                            return OpenCheck::Refused {
                                reason: format!(
                                    "{} is something the sandbox could have written, and macOS would run it outside \
                                     the sandbox; open it yourself if you trust it",
                                    path.display()
                                ),
                            };
                        }
                        Target::Other => notes.push(format!("opens {target}")),
                    }
                }
            }
            _ => {}
        }
    }
    match notes.is_empty() {
        true => OpenCheck::Other,
        false => OpenCheck::Ask { note: notes.join("; ") },
    }
}

enum Target {
    Web,
    /// Another scheme, and the app Launch Services hands it to.
    Scheme {
        app: PathBuf,
    },
    NoHandler,
    /// A `file:` URL naming no local path.
    BadFileUrl,
    /// A path, or the app of a scheme, that the sandbox could have written.
    Writable(PathBuf),
    Other,
}

fn classify(target: &str, cwd: &Path, policy: &Policy, handler: &impl Fn(&str) -> Option<PathBuf>) -> Target {
    let writable = |path: &Path| {
        let path = crate::agent::canonical_target(&cwd.join(path));
        policy.decide(&path, Access::Write).allowed.then_some(path)
    };
    let Some(scheme) = scheme(target) else {
        return match writable(Path::new(target)) {
            Some(path) => Target::Writable(path),
            None => Target::Other,
        };
    };
    match scheme.to_ascii_lowercase().as_str() {
        "https" | "http" => Target::Web,
        "file" => match file_url_path(target) {
            Some(path) => match writable(&path) {
                Some(path) => Target::Writable(path),
                None => Target::Other,
            },
            None => Target::BadFileUrl,
        },
        _ => match handler(target) {
            None => Target::NoHandler,
            Some(app) => match writable(&app) {
                Some(app) => Target::Writable(app),
                None => Target::Scheme { app },
            },
        },
    }
}

/// A URL's scheme: letters, digits, `+`, `-`, `.` before the first `:`, starting with a letter.
fn scheme(target: &str) -> Option<&str> {
    let (scheme, _) = target.split_once(':')?;
    let mut chars = scheme.chars();
    let first = chars.next()?;
    (first.is_ascii_alphabetic() && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
        .then_some(scheme)
}

/// The absolute path a `file:` URL names: `file:///p` or `file://localhost/p`, percent-decoded. Another host, a
/// relative path, or a bad escape: `None`.
fn file_url_path(url: &str) -> Option<PathBuf> {
    let rest = url.get(5..)?.strip_prefix("//")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let path = rest.split(['?', '#']).next()?;
    if !path.starts_with('/') {
        return None;
    }
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    use std::os::unix::ffi::OsStrExt;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(&out)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::sandbox::{Effect, Rule, Source, Target as RuleTarget};

    fn policy(writable: &str) -> Policy {
        Policy {
            rules: vec![Rule {
                effect: Effect::Allow,
                access: vec![Access::Write],
                target: RuleTarget::Subpath { path: writable.into() },
                source: Source::Workspace,
            }],
            keychain: false,
            services: crate::launch::sandbox::Services::Every {
                source: crate::launch::sandbox::Source::Workspace,
            },
            apps: crate::launch::sandbox::Apps::Closed,
            network: crate::launch::sandbox::Network::Open {
                source: crate::launch::sandbox::Source::Workspace,
            },
            listen: crate::launch::sandbox::Listen::Closed,
            tag: String::new(),
        }
    }

    /// Slack handles `slack:`, an app the agent built handles `built:`, and nothing handles `nope:`.
    fn handler(url: &str) -> Option<PathBuf> {
        match scheme(url)? {
            "slack" => Some("/Applications/Slack.app".into()),
            "built" => Some("/w/build/Evil.app".into()),
            _ => None,
        }
    }

    fn check_one(command: &str) -> OpenCheck {
        check_with(&[command.to_string()], Path::new("/w"), &policy("/w"), handler)
    }

    fn refused(command: &str) -> String {
        match check_one(command) {
            OpenCheck::Refused { reason } => reason,
            other @ (OpenCheck::Other | OpenCheck::Ask { .. }) => panic!("{command}: {other:?}"),
        }
    }

    #[test]
    fn open_is_decided_by_its_target() {
        assert!(matches!(check_one("osascript -e 'beep'"), OpenCheck::Refused { .. }));
        assert!(matches!(
            check_one("/usr/bin/osascript x.scpt"),
            OpenCheck::Refused { .. }
        ));
        assert_eq!(
            check_one("open https://example.com"),
            OpenCheck::Ask {
                note: "opens https://example.com in your browser".into()
            }
        );
        assert_eq!(
            check_one("open slack://channel"),
            OpenCheck::Ask {
                note: "opens slack://channel in Slack (/Applications/Slack.app)".into()
            }
        );
        assert!(refused("open nope://x").contains("no app handles"));
        assert!(refused("open built://x").contains("/w/build/Evil.app"));
        // `file:` URLs are the paths they name.
        assert!(refused("open file:///w/x.command").contains("/w/x.command"));
        assert!(refused("open file://localhost/w/x.command").contains("/w/x.command"));
        assert!(refused("open file:///w/my%20x.command").contains("/w/my x.command"));
        assert!(refused("open FILE:///w/x.command").contains("/w/x.command"));
        assert!(refused("open file://other/w/x").contains("isn't a file URL"));
        assert!(refused("open file:x.command").contains("isn't a file URL"));
        assert!(refused("open file:///w/%zz").contains("isn't a file URL"));
        assert_eq!(
            check_one("open file:///Applications/Safari.app"),
            OpenCheck::Ask {
                note: "opens file:///Applications/Safari.app".into()
            }
        );
        assert!(
            matches!(check_one("open ./build/App.app"), OpenCheck::Refused { reason } if reason.contains("/w/build/App.app"))
        );
        assert!(matches!(check_one("open x.command"), OpenCheck::Refused { .. }));
        assert_eq!(
            check_one("open /Applications/Safari.app"),
            OpenCheck::Ask {
                note: "opens /Applications/Safari.app".into()
            }
        );
        assert_eq!(check_one("git push"), OpenCheck::Other);
    }
}
