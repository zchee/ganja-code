//! Where in a file something sits, and the one way this crate says so.
//!
//! Two callers wanted the same walk for different reasons and would otherwise
//! have written it twice: `migrate` numbers the lines that held a comment it
//! could not carry, and every command that parses TOML has to say where a
//! parse stopped.
//!
//! That second one is why the module exists rather than being a line inside
//! `migrate`. `toml_edit`'s error types render themselves the way a compiler
//! does — the offending line reproduced with a caret under it — which is the
//! wrong thing to print here: the line that failed to parse is a line of
//! somebody's config, and an `mcp` entry's `headers` map is where a bearer
//! token lives. This build withholds header *values* even from `ganja mcp
//! get`, so an error must not hand one back through a terminal somebody is
//! sharing or a log somebody keeps. [`located`] carries the two facts that
//! help — what went wrong, and where to look — and none of the file's own
//! bytes.

use std::ops::Range;

/// The one-based line `offset` falls on.
pub(crate) fn line_of(text: &str, offset: usize) -> usize {
    at(text, offset).0
}

/// What a parser said and where, and deliberately nothing else.
///
/// `message` and `span` are the accessor pair both `toml_edit::TomlError` and
/// `toml_edit::de::Error` carry, so a caller passes its error's own two
/// values and never its `Display`. A span-less error — serde's own `custom`,
/// which carries no position — renders as the message alone rather than
/// inventing a line 1.
///
/// The guarantee is about the *line*, which is the whole of what `Display`
/// adds and the whole of what a neighbouring key on it would give away. It is
/// not a claim that no byte of the file can appear: a serde type mismatch
/// names the value it rejected ("invalid type: integer `1`"), the way every
/// serde-backed loader does and the way this build's own JSONC reader always
/// did. That is one value, chosen because the message is useless without it,
/// rather than every value that shared a line with it.
pub(crate) fn located(message: &str, span: Option<Range<usize>>, text: &str) -> String {
    let Some(span) = span else {
        return message.to_owned();
    };

    let (line, column) = at(text, span.start);

    format!("{message} at line {line}, column {column}")
}

/// The one-based line and column `offset` falls on.
///
/// Walked rather than sliced, because a span is a byte range and slicing at
/// one that is not a character boundary would have to fall back to something —
/// and every "something" here reports a position that is not the one asked
/// about. Columns are counted in characters, so a line holding multi-byte text
/// still points at the character somebody would count to.
fn at(text: &str, offset: usize) -> (usize, usize) {
    let (mut line, mut column) = (1usize, 1usize);
    for (index, character) in text.char_indices() {
        if index >= offset {
            break;
        }
        if character == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }

    (line, column)
}

#[cfg(test)]
#[path = "position_tests.rs"]
mod tests;
