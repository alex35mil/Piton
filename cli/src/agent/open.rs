//! `open` and `osascript` asked to run outside the sandbox: macOS starts what they name outside any sandbox, so what's
//! asked depends on the target, decided by the daemon from the command.
//!
//! - `osascript`: refused. AppleScript drives other apps; nothing in the question could say what it will do.
//! - `open https://…`: a plain confirmation; the browser opens a page.
//! - `open` of a file or app the launch's sandbox could have written: refused. The agent could have put anything there,
//!   and macOS would run it unsandboxed.
//! - `open` of another URL scheme: the question says an app handles it.
//! - Anything else `open` takes (an app by name, a file the sandbox can't write): the usual question, saying what opens.

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
                    match classify(target, cwd, policy) {
                        Target::Web => notes.push(format!("opens {target} in your browser")),
                        Target::Scheme => notes.push(format!("opens {target} in the app registered for it")),
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
    Scheme,
    Writable(PathBuf),
    Other,
}

fn classify(target: &str, cwd: &Path, policy: &Policy) -> Target {
    let lower = target.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        return Target::Web;
    }
    if !target.starts_with('/')
        && let Some((scheme, _)) = target.split_once(':')
        && !scheme.is_empty()
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return Target::Scheme;
    }
    let path = crate::agent::canonical_target(&cwd.join(target));
    match policy.decide(&path, Access::Write).allowed {
        true => Target::Writable(path),
        false => Target::Other,
    }
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
            network: Vec::new(),
            tag: String::new(),
        }
    }

    fn check_one(command: &str) -> OpenCheck {
        check(&[command.to_string()], Path::new("/w"), &policy("/w"))
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
                note: "opens slack://channel in the app registered for it".into()
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
