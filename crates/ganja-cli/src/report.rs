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
/// happen to be printed together. `header` names the left column — the
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

fn section(name: &str, header: &str, right: &str, rows: &[(String, String)], width: usize) {
    println!("{name}");
    if rows.is_empty() {
        println!("  (nothing)");

        return;
    }

    println!("  {header:<width$}  {right}");
    for (left, value) in rows {
        println!("  {left:<width$}  {value}");
    }
}
