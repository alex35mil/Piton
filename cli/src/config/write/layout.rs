//! How Piton lays out an array it writes: inline with zero or one item, one item per line (indented, with a trailing
//! comma) with two or more. An array that replaces one a person wrote keeps the comments of the items still in it, and
//! their spelling; a single item with a comment stays on its own line, so the comment isn't lost. Only arrays Piton
//! writes are laid out; arrays nested in an array or an inline table stay inline.

use toml_edit::{Array, Item, RawString, Table, Value};

const INDENT: &str = "    ";

/// Lays out every key-level array in an item Piton writes new: a value, or each table of a table or array of tables.
pub(super) fn item(item: &mut Item) {
    match item {
        Item::None => {}
        Item::Value(value) => key_value(value, None),
        Item::Table(table) => self::table(table),
        Item::ArrayOfTables(array) => {
            for table in array.iter_mut() {
                self::table(table);
            }
        }
    }
}

/// Lays out every key-level array in `table` and in the tables below it.
pub(super) fn table(table: &mut Table) {
    for (_, child) in table.iter_mut() {
        item(child);
    }
}

/// Lays out `value`, which replaces `previous` at the same key, keeping the comments of the items it still holds.
pub(super) fn replacing(value: &mut Value, previous: &Value) {
    key_value(value, Some(previous));
}

fn key_value(value: &mut Value, previous: Option<&Value>) {
    match value {
        Value::Array(array) => lay_out(array, previous.and_then(Value::as_array)),
        Value::String(_)
        | Value::Integer(_)
        | Value::Float(_)
        | Value::Boolean(_)
        | Value::Datetime(_)
        | Value::InlineTable(_) => {}
    }
}

/// One item of a laid-out array, with the comments it brings from the array it replaces.
struct Entry {
    value: Value,
    /// Comment lines above the item.
    above: Vec<String>,
    /// The comment after the item on its line.
    after: Option<String>,
}

/// The comments of an array a person wrote.
struct Written {
    /// The comment after `[`.
    open: Option<String>,
    items: Vec<Option<Entry>>,
    /// Comment lines before `]`.
    close: Vec<String>,
}

fn lay_out(array: &mut Array, previous: Option<&Array>) {
    let mut written = previous.map(comments).unwrap_or(Written {
        open: None,
        items: Vec::new(),
        close: Vec::new(),
    });
    // Each item takes the first unused item of the old array with the same value: its spelling and its comments.
    let entries: Vec<Entry> = array
        .iter()
        .map(|value| {
            let kept = written
                .items
                .iter_mut()
                .find(|old| old.as_ref().is_some_and(|old| same(&old.value, value)))
                .and_then(Option::take);
            kept.unwrap_or_else(|| Entry {
                value: value.clone(),
                above: Vec::new(),
                after: None,
            })
        })
        .collect();
    let commented = written.open.is_some()
        || !written.close.is_empty()
        || entries.iter().any(|e| !e.above.is_empty() || e.after.is_some());

    let mut out = Array::new();
    *out.decor_mut() = array.decor().clone();
    if entries.len() < 2 && !commented {
        for entry in entries {
            out.push_formatted(entry.value);
        }
        out.fmt();
    } else {
        let lines = |comments: &[String]| -> String { comments.iter().map(|c| format!("{INDENT}{c}\n")).collect() };
        let same_line = |comment: Option<String>| comment.map(|c| format!(" {c}")).unwrap_or_default();
        // The comment on the line before an item: after `[` for the first one, after the previous item for the rest.
        let mut before = written.open;
        for entry in entries {
            let mut value = entry.value;
            let decor = value.decor_mut();
            decor.set_prefix(format!("{}\n{}{INDENT}", same_line(before), lines(&entry.above)));
            decor.set_suffix("");
            out.push_formatted(value);
            before = entry.after;
        }
        out.set_trailing_comma(true);
        out.set_trailing(format!("{}\n{}", same_line(before), lines(&written.close)));
    }
    *array = out;
}

/// The comments in an array as written, each with the item it belongs to.
fn comments(array: &Array) -> Written {
    let raw = |text: Option<&RawString>| text.and_then(RawString::as_str).unwrap_or("").to_string();
    let values: Vec<&Value> = array.iter().collect();
    // The text between two items (the comma left out), and before the first and after the last: its first line ends
    // the line of what comes before, the rest is above what comes after.
    let gaps: Vec<(Option<String>, Vec<String>)> = (0..=values.len())
        .map(|i| {
            let after_previous = match i {
                0 => String::new(),
                _ => raw(values[i - 1].decor().suffix()),
            };
            let before_next = match values.get(i) {
                Some(value) => raw(value.decor().prefix()),
                None => raw(Some(array.trailing())),
            };
            split(&(after_previous + &before_next))
        })
        .collect();
    let mut gaps = gaps.into_iter();
    let (open, mut above) = gaps.next().unwrap_or_default();
    let mut items = Vec::with_capacity(values.len());
    for (value, (after, next)) in values.into_iter().zip(gaps) {
        let mut value = value.clone();
        value.decor_mut().clear();
        items.push(Some(Entry {
            value,
            above: std::mem::replace(&mut above, next),
            after,
        }));
    }
    Written {
        open,
        items,
        close: above,
    }
}

/// A gap's comment on its first line, and the comment lines after it.
fn split(gap: &str) -> (Option<String>, Vec<String>) {
    match gap.split_once('\n') {
        Some((first, rest)) => (comment(first), rest.lines().filter_map(comment).collect()),
        None => (comment(gap), Vec::new()),
    }
}

fn comment(line: &str) -> Option<String> {
    let line = line.trim();
    line.starts_with('#').then(|| line.to_string())
}

/// Whether two values are equal, however they're written.
fn same(a: &Value, b: &Value) -> bool {
    match a {
        Value::String(a) => b.as_str() == Some(a.value().as_str()),
        Value::Integer(a) => b.as_integer() == Some(*a.value()),
        Value::Float(a) => b.as_float() == Some(*a.value()),
        Value::Boolean(a) => b.as_bool() == Some(*a.value()),
        Value::Datetime(a) => b.as_datetime() == Some(a.value()),
        Value::Array(a) => b
            .as_array()
            .is_some_and(|b| a.len() == b.len() && a.iter().zip(b.iter()).all(|(a, b)| same(a, b))),
        Value::InlineTable(a) => b
            .as_inline_table()
            .is_some_and(|b| a.len() == b.len() && a.iter().all(|(key, a)| b.get(key).is_some_and(|b| same(a, b)))),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::edit::{Change, append_table, edited, item_from_json};

    const PATH: &str = "config.toml";

    /// `text` with `key` set to the JSON `value` (removed for `None`), as `piton config set` and the apps write it.
    fn set(text: &str, key: &str, value: Option<serde_json::Value>) -> String {
        let path = Path::new(PATH);
        let parts: Vec<&str> = key.split('.').collect();
        let item = value.as_ref().map(|value| item_from_json(&parts, value, path).unwrap());
        let new = edited(
            Some(text),
            &[Change {
                key: &parts,
                value: item.as_ref(),
            }],
            path,
        )
        .unwrap()
        .unwrap_or_else(|| text.to_string());
        // Whatever the layout, the file reads back as the values written.
        let read = toml::Value::Table(toml::from_str(&new).unwrap());
        let back = parts.iter().try_fold(&read, |value, part| value.get(*part));
        assert_eq!(back.map(|v| serde_json::to_value(v).unwrap()), value, "{new}");
        new
    }

    fn json(items: &[&str]) -> Option<serde_json::Value> {
        Some(serde_json::json!(items))
    }

    #[test]
    fn one_item_inline_then_one_per_line() {
        let one = set("", "sandbox.presets", json(&["rust"]));
        assert_eq!(one, "[sandbox]\npresets = [\"rust\"]\n");
        let two = set(&one, "sandbox.presets", json(&["rust", "swift"]));
        assert_eq!(two, "[sandbox]\npresets = [\n    \"rust\",\n    \"swift\",\n]\n");
        let three = set(&two, "sandbox.presets", json(&["rust", "swift", "node"]));
        assert_eq!(
            three,
            "[sandbox]\npresets = [\n    \"rust\",\n    \"swift\",\n    \"node\",\n]\n"
        );
        // Down to one: inline again; empty stays `[]`.
        let back = set(&three, "sandbox.presets", json(&["swift"]));
        assert_eq!(back, "[sandbox]\npresets = [\"swift\"]\n");
        assert_eq!(set(&back, "sandbox.presets", json(&[])), "[sandbox]\npresets = []\n");
    }

    /// A hand-written inline array gets the layout when Piton writes it; every other array, and the comments around the
    /// changed key, stay byte for byte.
    #[test]
    fn only_the_written_array_changes() {
        let text = "\
[sandbox]
# what I read
read = [\"~/a\", \"~/b\"]
# toolchains
presets = ['rust', \"swift\"]   # mine
write = [ \"/x\",  \"/y\" ]
";
        let new = set(text, "sandbox.presets", json(&["rust", "swift", "node"]));
        assert_eq!(
            new,
            "\
[sandbox]
# what I read
read = [\"~/a\", \"~/b\"]
# toolchains
presets = [
    'rust',
    \"swift\",
    \"node\",
]   # mine
write = [ \"/x\",  \"/y\" ]
"
        );
    }

    /// Comments inside an array a person wrote stay with their items; a removed item's comments go with it.
    #[test]
    fn comments_stay_with_their_items() {
        let text = "\
write = [ # paths
    # build output
    \"/a\", # the a
    \"/b\",
    \"/c\" # the c
    # more later
]
";
        let new = set(text, "write", json(&["/c", "/d", "/a"]));
        assert_eq!(
            new,
            "\
write = [ # paths
    \"/c\", # the c
    \"/d\",
    # build output
    \"/a\", # the a
    # more later
]
"
        );
        // Down to one item with a comment: it stays on its own line, comment and all.
        let one = set(
            "write = [\n    \"/a\", # the a\n    \"/b\",\n]\n",
            "write",
            json(&["/a"]),
        );
        assert_eq!(one, "write = [\n    \"/a\", # the a\n]\n");
        // Its comment gone with the removed item: inline.
        let plain = set(
            "write = [\n    \"/a\", # the a\n    \"/b\",\n]\n",
            "write",
            json(&["/b"]),
        );
        assert_eq!(plain, "write = [\"/b\"]\n");
    }

    /// Arrays inside a table or a list of tables Piton writes whole get the layout; arrays in an array stay inline.
    #[test]
    fn tables_written_whole() {
        let forge = set(
            "",
            "sandbox",
            Some(serde_json::json!({"presets": ["rust"], "write": ["/a", "/b"], "x": {"y": [[1, 2], [3]]}})),
        );
        assert_eq!(
            forge,
            "[sandbox]\npresets = [\"rust\"]\nwrite = [\n    \"/a\",\n    \"/b\",\n]\n\n[sandbox.x]\ny = [\n    [1, 2],\n    [3],\n]\n"
        );
        let commands = set(
            "",
            "sandbox.commands",
            Some(serde_json::json!([{"match": "bash(x)", "grant": {"write": ["/a", "/b"]}}])),
        );
        assert!(
            commands.contains("write = [\n    \"/a\",\n    \"/b\",\n]\n"),
            "{commands}"
        );
    }

    #[test]
    fn an_appended_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PATH);
        std::fs::write(&path, "read = [\"~/a\", \"~/b\"]\n").unwrap();
        let entry: toml_edit::DocumentMut = "match = \"bash(x)\"\n[grant]\nwrite = [\"/a\", \"/b\"]\nread = [\"/c\"]\n"
            .parse()
            .unwrap();
        append_table(&path, &["sandbox", "commands"], entry.as_table(), &|_, _| Ok(())).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("read = [\"~/a\", \"~/b\"]\n"), "{text}");
        assert!(
            text.contains("write = [\n    \"/a\",\n    \"/b\",\n]\nread = [\"/c\"]\n"),
            "{text}"
        );
    }
}
