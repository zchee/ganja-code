//! The minimal-YAML frontmatter reader every markdown file this build reads
//! shares.
//!
//! Spec: what upstream gets from `gray-matter` and what Claude's own
//! `SKILL.md`/agent files are written in — not a YAML implementation, and
//! deliberately not one. Four kinds of file open with a `---` fence here: a
//! skill ([`crate::skill`]), an agent definition, a command file
//! (`ganja-core`'s `agent.rs` and `command.rs`) and a plugin's own markdown
//! components (`ganja-core`'s `plugin.rs`, since it retired the tree's one
//! hand-rolled copy of this parser). The first two read exactly
//! the same shapes, and read them out of files a person may have written for
//! another agent entirely, so they read them through one parser: two copies
//! were two parsers waiting to disagree about what somebody's `description:`
//! meant.
//!
//! Everything asked of YAML is here: a `---` fence at the top of the file,
//! `key: value` at the top level with quotes stripped, and the block scalars
//! (`|`, `|-`, `>`, `>-`) a long description is usually written as. Nested
//! maps and lists are skipped rather than guessed at — no field any caller
//! reads is one — and a value that runs on into an unquoted colon is kept
//! whole, because everything after the first colon is the value.

use std::collections::BTreeMap;

/// The frontmatter and the body of a markdown file that opens with one.
///
/// [`None`] for a file that does not open with `---`, which is a file with no
/// frontmatter at all — upstream reaches the same answer through
/// `gray-matter`, which returns empty data for one. A caller that wants such a
/// file's text anyway treats the whole of it as the body.
#[must_use]
pub fn split(text: &str) -> Option<(&str, &str)> {
    // A byte-order mark ahead of the fence is what an editor on another
    // platform leaves behind, and it must not cost somebody their file.
    let text = text.trim_start_matches('\u{feff}');
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;

    for (index, _) in rest.match_indices("\n---") {
        let after = &rest[index + 4..];
        // The closing fence owns its whole line: `---` inside a value is not
        // the end of the block.
        if after.is_empty() || after.starts_with('\n') || after.starts_with("\r\n") {
            let frontmatter = &rest[..index];
            let body = after
                .strip_prefix("\r\n")
                .or_else(|| after.strip_prefix('\n'));

            return Some((frontmatter, body.unwrap_or(after)));
        }
    }

    None
}

/// The scalar fields a frontmatter block names.
///
/// A key whose value is a nested map or a list arrives as the empty string the
/// line itself carries — which is what a `tools:` written as a block list
/// leaves — rather than as a guess about what the block underneath it meant.
#[must_use]
pub fn fields(frontmatter: &str) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    let mut lines = frontmatter.lines().peekable();

    while let Some(line) = lines.next() {
        let trimmed = line.trim_end_matches('\r');
        if trimmed.trim().is_empty() || trimmed.trim_start().starts_with('#') {
            continue;
        }
        // An indented line belongs to whatever came before it, and what came
        // before it was either a block scalar this already consumed or a
        // structure this does not read.
        if trimmed.starts_with([' ', '\t']) {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim().to_owned(), value.trim());

        if matches!(value, "|" | "|-" | "|+" | ">" | ">-" | ">+") {
            let folded = value.starts_with('>');
            let mut block: Vec<String> = Vec::new();
            while let Some(next) = lines.peek() {
                let next = next.trim_end_matches('\r');
                if !next.trim().is_empty() && !next.starts_with([' ', '\t']) {
                    break;
                }
                block.push(next.trim().to_owned());
                lines.next();
            }
            while block.last().is_some_and(String::is_empty) {
                block.pop();
            }
            fields.insert(key, block.join(if folded { " " } else { "\n" }));
            continue;
        }

        fields.insert(key, unquote(value).to_owned());
    }

    fields
}

/// `value` without the quotes it may be wrapped in.
fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }

    value
}

#[cfg(test)]
#[path = "frontmatter_tests.rs"]
mod tests;
