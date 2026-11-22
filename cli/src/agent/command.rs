//! Which per-command grant (`[[sandbox.commands]]`) a bash call gets, decided by the daemon from the command text, never
//! from the agent's word.
//!
//! The analysis is the permissions extension's (`pi/extensions/permissions/bash.ts`), ported node for node: every
//! syntax-visible command node (nested substitutions included) is matched, a leading `cd` to the working folder is
//! ignored, and a command that doesn't parse cleanly matches nothing. Matching is the extension's glob: `*` is any text,
//! and a trailing ` *` also matches the bare command. One fixture file (`pi/tests/fixtures/bash-matching.json`) is run by
//! both test suites, so the two can't drift.
//!
//! A call gets a grant only when every command node matches the same entry, and it writes no file through a redirect
//! (`git push > ~/.zshrc` would write with the grant): such a call runs in the sandbox as usual.

use std::path::Path;

use tree_sitter::{Node, Parser};

use crate::config::{CommandGrant, Listed};

/// A bash command's syntax-visible command nodes, and whether it writes a file through a redirect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashAnalysis {
    pub commands: Vec<String>,
    pub writes_file: bool,
}

/// Why a command can't be matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("the Bash grammar couldn't be loaded")]
    Grammar,
    #[error("the command couldn't be parsed")]
    Unparsed,
    #[error("the command is malformed")]
    Malformed,
}

const WRITABLE_REDIRECTS: [&str; 6] = [">", ">|", ">>", "&>", "&>>", ">&"];

/// The command nodes of `command`, as the permissions extension sees them. `cwd` is the folder the command runs in, for
/// the leading `cd` rule.
pub fn analyze(command: &str, cwd: Option<&Path>) -> Result<BashAnalysis, ParseError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .map_err(|_| ParseError::Grammar)?;
    let tree = parser.parse(command, None).ok_or(ParseError::Unparsed)?;
    let root = tree.root_node();
    if root.has_error() || malformed(root) {
        return Err(ParseError::Malformed);
    }
    let ignored = cwd.and_then(|cwd| ignored_leading_cd(root, command, cwd));
    let mut commands = Vec::new();
    let mut writes_file = false;
    descendants(root, &mut |node| match node.kind() {
        "command" if Some(node.id()) != ignored => commands.push(command_text(node, command)),
        "file_redirect" => writes_file |= writes(node, command),
        _ => {}
    });
    Ok(BashAnalysis { commands, writes_file })
}

/// Whether `value` matches the glob `pattern`: `*` is any text (newlines too), and a trailing ` *` also matches the bare
/// command (`git push *` matches `git push`).
pub fn matches(pattern: &str, value: &str) -> bool {
    let mut regex = String::from("(?s)^");
    let (body, optional_tail) = match pattern.strip_suffix(" *") {
        Some(body) => (body, true),
        None => (pattern, false),
    };
    for part in body.split('*').enumerate() {
        if part.0 > 0 {
            regex.push_str(".*");
        }
        regex.push_str(&regex_lite::escape(part.1));
    }
    if optional_tail {
        regex.push_str("( .*)?");
    }
    regex.push('$');
    regex_lite::Regex::new(&regex).is_ok_and(|r| r.is_match(value))
}

/// What the daemon decides for a bash call: the first entry whose pattern every command node matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Matched<'a> {
    Grant(&'a Listed<CommandGrant>),
    /// No entry covers every command node.
    None,
    /// The command writes a file through a redirect: it gets no grant.
    Redirect,
}

/// The per-command grant `command` gets in `cwd`, from `grants` in their order.
pub fn grant_for<'a>(grants: &'a [Listed<CommandGrant>], command: &str, cwd: &Path) -> Result<Matched<'a>, ParseError> {
    if grants.is_empty() {
        return Ok(Matched::None);
    }
    let analysis = analyze(command, Some(cwd))?;
    if analysis.commands.is_empty() {
        return Ok(Matched::None);
    }
    if analysis.writes_file {
        return Ok(Matched::Redirect);
    }
    Ok(grants
        .iter()
        .find(|entry| {
            analysis
                .commands
                .iter()
                .all(|node| matches(entry.value.pattern.pattern(), node))
        })
        .map_or(Matched::None, Matched::Grant))
}

/// Every node below `node` (itself included), in document order.
fn descendants<'t>(node: Node<'t>, visit: &mut impl FnMut(Node<'t>)) {
    visit(node);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        descendants(child, visit);
    }
}

fn malformed(node: Node) -> bool {
    if node.is_error() || node.is_missing() {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor).any(malformed)
}

fn text<'s>(node: Node, source: &'s str) -> &'s str {
    &source[node.start_byte()..node.end_byte()]
}

/// A word's value when it has no expansion: a plain word, a single-quoted string, or a double-quoted one without
/// substitutions. Trailing carriage returns after it are kept, as the extension keeps them.
fn static_word(node: Node, source: &str) -> Option<String> {
    let raw = text(node, source);
    let value = match node.kind() {
        "word" => unescape(raw, |_| true),
        "raw_string" => raw[1..raw.len() - 1].to_string(),
        "string" => {
            let mut cursor = node.walk();
            if !node
                .named_children(&mut cursor)
                .all(|child| child.kind() == "string_content")
            {
                return None;
            }
            unescape(&raw[1..raw.len() - 1], |c| matches!(c, '\\' | '"' | '$' | '`'))
        }
        _ => return None,
    };
    let rest = &source[node.end_byte()..];
    let returns: String = rest.chars().take_while(|c| *c == '\r').collect();
    Some(value + &returns)
}

/// Drops the backslash before each character `escaped` accepts.
fn unescape(text: &str, escaped: impl Fn(char) -> bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('\\', Some(&next)) if escaped(next) => {
                out.push(next);
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

/// The id of a leading `cd <dir> &&` whose folder is `cwd`, which the extension ignores.
fn ignored_leading_cd(root: Node, source: &str, cwd: &Path) -> Option<usize> {
    let mut first = None;
    descendants(root, &mut |node| {
        if first.is_none() && node.kind() == "command" {
            first = Some(node);
        }
    });
    let command = first?;
    if !source[..command.start_byte()].chars().all(|c| c == ' ' || c == '\t') {
        return None;
    }
    if command.next_sibling()?.kind() != "&&" {
        return None;
    }
    if text(command.child_by_field_name("name")?, source) != "cd" {
        return None;
    }
    let mut cursor = command.walk();
    let args: Vec<Node> = command.children_by_field_name("argument", &mut cursor).collect();
    let [arg] = args.as_slice() else {
        return None;
    };
    let target = static_word(*arg, source)?;
    let resolved = crate::agent::lexical(&cwd.join(target));
    (resolved == crate::agent::lexical(cwd)).then(|| command.id())
}

fn redirect_operator<'t>(node: Node<'t>, source: &'t str) -> Option<&'t str> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| !child.is_named())
        .map(|child| text(child, source))
}

/// The arguments a redirect carries after its destination (`cmd > out extra`), which belong to the command.
fn trailing_redirect_arguments<'t>(node: Node<'t>, source: &str) -> Vec<Node<'t>> {
    if node.kind() != "file_redirect" {
        return Vec::new();
    }
    let mut cursor = node.walk();
    let destinations: Vec<Node> = node.children_by_field_name("destination", &mut cursor).collect();
    let skip = match redirect_operator(node, source) {
        Some("<&-" | ">&-") => 0,
        Some(_) | None => 1,
    };
    destinations.into_iter().skip(skip).collect()
}

fn command_text(node: Node, source: &str) -> String {
    let mut parts: Vec<Node> = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "file_redirect" => parts.extend(trailing_redirect_arguments(child, source)),
            "herestring_redirect" => {}
            _ if child.is_extra() => {}
            _ => parts.push(child),
        }
    }
    let mut statement = node;
    while let Some(parent) = statement.parent() {
        if parent.kind() == "negated_command" {
            statement = parent;
            continue;
        }
        if parent.kind() != "redirected_statement"
            || parent.child_by_field_name("body").map(|b| b.id()) != Some(statement.id())
        {
            break;
        }
        let mut cursor = parent.walk();
        for redirect in parent.children_by_field_name("redirect", &mut cursor) {
            parts.extend(trailing_redirect_arguments(redirect, source));
        }
        statement = parent;
    }
    let joined = parts.iter().map(|p| text(*p, source)).collect::<Vec<_>>().join(" ");
    let Some(last) = parts.last() else {
        return joined;
    };
    let padding: String = source[last.end_byte()..]
        .chars()
        .take_while(|c| matches!(c, ' ' | '\t' | '\r'))
        .collect();
    match padding.contains('\r') {
        false => joined,
        true => joined + padding.trim_end_matches([' ', '\t']),
    }
}

fn writes(node: Node, source: &str) -> bool {
    let Some(operator) = redirect_operator(node, source) else {
        return false;
    };
    if !WRITABLE_REDIRECTS.contains(&operator) {
        return false;
    }
    let mut cursor = node.walk();
    let Some(destination) = node.children_by_field_name("destination", &mut cursor).next() else {
        return false;
    };
    if destination.kind() == "process_substitution" || static_word(destination, source).as_deref() == Some("/dev/null")
    {
        return false;
    }
    if operator == ">&" {
        let target = static_word(destination, source);
        if node.child_by_field_name("descriptor").is_some() || destination.kind() == "number" {
            return false;
        }
        if let Some(target) = target
            && (target == "-" || (!target.is_empty() && target.chars().all(|c| c.is_ascii_digit())))
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::{AllGrant, CommandPattern, CommandRuntime, Grant};

    /// The fixture shared with the pi package's tests.
    #[derive(serde::Deserialize)]
    struct Fixture {
        analysis: Vec<AnalysisCase>,
        matching: Vec<MatchCase>,
    }

    #[derive(serde::Deserialize)]
    struct AnalysisCase {
        command: String,
        /// Relative to the test's cwd: `"."` is the cwd itself.
        #[serde(default)]
        cwd: Option<String>,
        /// `null`: the command doesn't parse.
        expected: Option<Expected>,
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Expected {
        commands: Vec<String>,
        writes_file: bool,
    }

    #[derive(serde::Deserialize)]
    struct MatchCase {
        pattern: String,
        value: String,
        matches: bool,
    }

    fn fixture() -> Fixture {
        serde_json::from_str(include_str!("../../../pi/tests/fixtures/bash-matching.json")).unwrap()
    }

    #[test]
    fn analysis_matches_the_extension() {
        let cwd = PathBuf::from("/work/repo");
        for case in fixture().analysis {
            let dir = case.cwd.as_deref().map(|c| lexical_join(&cwd, c));
            let got = analyze(&case.command, dir.as_deref());
            match (&case.expected, got) {
                (Some(expected), Ok(analysis)) => {
                    assert_eq!(analysis.commands, expected.commands, "{:?}", case.command);
                    assert_eq!(analysis.writes_file, expected.writes_file, "{:?}", case.command);
                }
                (None, Err(_)) => {}
                (Some(_), Err(e)) => panic!("{:?}: {e}", case.command),
                (None, Ok(a)) => panic!("{:?} parsed: {a:?}", case.command),
            }
        }
    }

    fn lexical_join(cwd: &Path, rel: &str) -> PathBuf {
        crate::agent::lexical(&cwd.join(rel))
    }

    #[test]
    fn matching_matches_the_extension() {
        for case in fixture().matching {
            assert_eq!(
                matches(&case.pattern, &case.value),
                case.matches,
                "{case:?}",
                case = (&case.pattern, &case.value)
            );
        }
    }

    fn grant(pattern: &str) -> Listed<CommandGrant> {
        Listed {
            value: CommandGrant {
                pattern: CommandPattern::new(pattern).unwrap(),
                runtime: CommandRuntime::Sandbox(Grant::All(AllGrant::All)),
            },
            file: PathBuf::from("/g/config.toml"),
        }
    }

    /// Every command node must match the same entry; the first entry that covers them all decides.
    #[test]
    fn a_grant_covers_every_part() {
        let cwd = Path::new("/work/repo");
        let grants = [grant("husky install *"), grant("git *")];
        let decide = |command: &str| grant_for(&grants, command, cwd).unwrap();
        assert_eq!(decide("husky install"), Matched::Grant(&grants[0]));
        assert_eq!(decide("git push && git fetch"), Matched::Grant(&grants[1]));
        assert_eq!(
            decide("husky install && git push"),
            Matched::None,
            "no one entry covers both"
        );
        assert_eq!(
            decide("git push $(rm -rf ~)"),
            Matched::None,
            "the substitution is a command too"
        );
        assert_eq!(
            decide("FOO=1 git push"),
            Matched::None,
            "an assignment changes what runs"
        );
        assert_eq!(decide("cd . && git push"), Matched::Grant(&grants[1]));
        assert_eq!(decide("cd .. && git push"), Matched::None);
        assert_eq!(decide("git push > ~/.zshrc"), Matched::Redirect);
        assert_eq!(decide("git push 2>/dev/null"), Matched::Grant(&grants[1]));
        assert_eq!(decide("FOO=bar"), Matched::None, "nothing to run");
        assert!(grant_for(&grants, "git push (", cwd).is_err());
        assert_eq!(grant_for(&[], "git push", cwd), Ok(Matched::None));
    }
}
