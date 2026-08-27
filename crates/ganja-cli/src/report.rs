//! What a config-writing command did with every key it saw, and the table it
//! prints that in.
//!
//! Three commands answer that question — `config migrate`,
//! `config import-opencode` and `config import-claude-hooks` — and they
//! answered it in three copies of the same code. What actually differs between
//! them is vocabulary, not structure: the left column names whichever
//! dialect's keys the command was reading (`KEY`, `OPENCODE`, `CLAUDE`), and
//! the mapped section's right column says where a key landed (`TOML` for a
//! migration within ganja's own vocabulary, `GANJA` for an import from
//! somebody else's). Both arrive as parameters, so the rendering has one home
//! and each command keeps its own words.

/// What became of every key, and why anything was left out.
///
/// Both of the fields a caller fills — the rows and the warnings — carry text
/// out of the file being read, so both leave through this module's own
/// printers rather than through a caller's `println!`. See [`printable`].
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// One row per key that landed: what it was, and the spelling or shape it
    /// took.
    pub(crate) mapped: Vec<(String, String)>,
    /// One row per thing left out, at whatever depth, with the reason.
    pub(crate) skipped: Vec<(String, String)>,
    /// Everything that needs saying on the way, for stderr — the rows a single
    /// word cannot explain. `migrate` never has one, since a key it cannot
    /// carry is a key TOML cannot spell and that is the whole sentence.
    pub(crate) warnings: Vec<String>,
}

impl Report {
    pub(crate) fn map(&mut self, key: &str, spelling: &str) {
        self.mapped.push((key.to_owned(), spelling.to_owned()));
    }

    pub(crate) fn skip(&mut self, key: &str, reason: &str) {
        self.skipped.push((key.to_owned(), reason.to_owned()));
    }

    pub(crate) fn warn(&mut self, warning: String) {
        self.warnings.push(warning);
    }
}

/// Prints what a command did, in two sections.
///
/// One width across both, so the two read as one table rather than as two that
/// happen to be printed together. Every cell goes through [`printable`] on the
/// way out, so no row can rewrite the table it is printed in. `header` names the left column — the
/// vocabulary the command read from — and `mapped` the right column of the
/// first section; the second section's is always `REASON`, because the reason
/// a thing was left out is the same kind of answer whatever was reading.
pub(crate) fn print_table(report: &Report, header: &str, mapped: &str) {
    let width = report
        .mapped
        .iter()
        .chain(&report.skipped)
        .map(|(key, _)| key.chars().count())
        .chain(std::iter::once(header.chars().count()))
        .max()
        .unwrap_or_default();

    section("mapped", header, mapped, &report.mapped, width);
    println!();
    section("skipped", header, "REASON", &report.skipped, width);
}

/// Prints the warnings, one per line, on stderr.
///
/// Here rather than at each command's own `eprintln!` for the reason
/// [`printable`] is where it is: a warning is built from the file being
/// imported as much as a row is — the one this crate builds today quotes a
/// `settings.json` matcher back through a regex error — so a warning printed
/// past the filter would reopen exactly the hole the filter closes, on the
/// channel that prints *beside* the table it could repaint. Silent when there
/// is nothing to say, so a clean run stays clean.
pub(crate) fn print_warnings(report: &Report) {
    for warning in &report.warnings {
        eprintln!("warning: {}", printable(warning));
    }
}

fn section(name: &str, header: &str, right: &str, rows: &[(String, String)], width: usize) {
    println!("{name}");
    if rows.is_empty() {
        println!("  (nothing)");

        return;
    }

    println!("  {header:<width$}  {right}");
    for (left, value) in rows {
        println!("  {:<width$}  {}", printable(left), printable(value));
    }
}

/// `text` with every control character replaced, for a terminal.
///
/// Both columns of every row here come out of a file somebody is *deciding
/// about* — the key names of a config being migrated or imported, and the
/// command lines `import-claude-hooks` is about to install — and a control
/// sequence carried in one of those can move the cursor, erase what was
/// already printed, or repaint a row as something it is not. The table exists
/// to be read before a write is approved, so a table that can be rewritten by
/// its own contents is worse than no table.
///
/// The choke point is the renderer rather than each caller because there are
/// exactly two ways text from a read file reaches a terminal here — a row
/// through [`print_table`] and a warning through [`print_warnings`] — and
/// three commands that produce both: filtering at those two covers every one
/// of them, including the next command that adds a row and does not know this
/// paragraph exists. Callers therefore pass their text as they read it, and a
/// third door added later belongs through this function too.
///
/// A tab goes the same way as an escape. The columns are space-aligned, so a
/// tab was already going to break the alignment this function is protecting;
/// there is no reason to keep the one control character that is merely
/// annoying and drop the ones that are not.
fn printable(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                char::REPLACEMENT_CHARACTER
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;
