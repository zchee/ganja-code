//! `ganja config migrate` — a one-way translation of a legacy
//! `ganja.jsonc`/`ganja.json` into the `ganja.toml` this build reads first.
//!
//! Spec: none. The format change is this project's own (**D536**), so there is
//! no upstream file to port — what is ported instead is `import.rs`'s
//! *posture*, key for key, because this command answers the same question
//! about a different pair of files: **every key is either mapped or
//! reported**. A setting that vanished without a row would be one its author
//! still believes is in force, which is the failure both commands exist to
//! prevent, so the table is the output and the file is a side effect of it.
//!
//! Four rules decide everything below, and none of them is a matter of taste:
//!
//! * **The source is never touched.** Not deleted, not renamed, not rewritten.
//!   A migration that removed the file it read would leave whoever ran it with
//!   nothing to compare against at the one moment they want to compare — and
//!   the loader keeps preferring the `ganja.toml` beside it either way, so
//!   nothing is gained by hurrying the old file out of the tree. The closing
//!   lines say exactly that, and name the file, so removing it stays a
//!   decision its owner makes after reading what was written.
//! * **Document order survives.** Permission rules are evaluated
//!   last-match-wins, so which of two rules covering the same call was written
//!   second is the whole answer. TOML delivers document order to the loader —
//!   `config_tests.rs` pins that, headers and dotted keys alike — but a
//!   *writer* can still invert it, because a table's key-values must all be
//!   printed before the first sub-table header that re-enters it. So a
//!   `{"bash": {…}, "edit": "ask"}` rendered as `[permission]` + `edit` +
//!   `[permission.bash]` would reach the loader as `edit, bash`. Hence
//!   [`orders`]: everything under a `permission` key is rendered **inline**,
//!   so every one of that table's keys is a key-value and the order written is
//!   the order read.
//! * **What is written is what the loader reads back.** The two decoded
//!   configs — the legacy file's and the rendered TOML's — are compared for
//!   equality *before* anything is written, and a mismatch refuses. That is
//!   the belt to [`orders`]' suspenders: if the inline rule ever stopped being
//!   enough, or a shape translated wrong, this refuses rather than writing a
//!   file that means something else. *One* of the refusals the loader makes
//!   after decoding is made here too, on the legacy side — `McpServer::check`,
//!   the only one of its seven whose authority is public. The other six (the
//!   hooks, LSP, provider, agents, teammates and openrouter checks) are
//!   private to `ganja_core::config`, so a source failing one of those still
//!   translates cleanly and is refused at the *next launch* rather than here:
//!   a legacy file whose hooks matcher does not compile is the case to know
//!   about. Mirroring the six is not the fix and none is spelled here; they
//!   arrive when this read moves onto `config::legacy`, which runs the
//!   loader's own seven in-crate.
//! * **Comments do not survive, and are counted.** A JSON value carries no
//!   comments, so the translation cannot carry them either. Every line that
//!   held one is listed by number in a warning: knowing three lines were lost
//!   and which three is the difference between re-adding them and never
//!   noticing.
//!
//! TOML has no `null`. A null **property** is dropped and reported — serde
//! reads an absent key and an explicit null as the same `None`, so nothing
//! about the loaded config changes. A null **array element** refuses the
//! migration instead: dropping it would shorten a list, and there is no
//! spelling that keeps it.
//!
//! The legacy read below is the last one outside `import.rs`; lane L1b folds
//! it into a `ganja_core::config::legacy` module that owns the old dialect.

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, anyhow, bail};
use ganja_core::config::Config;
use ganja_permission::Project;
use jsonc_parser::ast::{Object, Value as Ast};
use tempfile::NamedTempFile;
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

/// What this writes, beside whatever it read.
const DESTINATION: &str = "ganja.toml";

/// The two names this reads, in the order a directory is probed for them —
/// which is the loader's own preference between them, so a directory holding
/// both is migrated from the file that currently wins.
const LEGACY_FILES: [&str; 2] = ["ganja.jsonc", "ganja.json"];

/// Left column of both sections of the table.
const HEADER: &str = "KEY";

/// The one key whose children are rendered inline; see the module doc.
const PERMISSION: &str = "permission";

/// Why a key was left out. One word each, so the right-hand column of the
/// skipped section stays a column.
mod reason {
    /// The value was `null`, which TOML cannot spell — and which the loader
    /// read as an absent key anyway.
    pub const NULL: &str = "null";
}

/// Reads a legacy config file and writes the `ganja.toml` it maps to.
///
/// `file` migrates exactly that file and skips discovery. `global` reads the
/// config home's legacy file; without either, the nearest legacy file at or
/// above the working directory is the source. The destination is always
/// [`DESTINATION`] **beside the source**, whichever of the three chose it.
///
/// # Errors
///
/// A named file that is not there, no legacy file where one was looked for, a
/// source that is not valid in the legacy dialect or that this build would
/// refuse to load, a destination that already exists, or a translation whose
/// result does not read back as the config that was read.
pub fn migrate(file: Option<PathBuf>, global: bool, dry_run: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read the working directory")?;
    let source = source(file, global, &cwd)?;
    let destination = source
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(DESTINATION);

    // Both printed before a byte is read, and the destination refused before
    // any work is done, so a run that cannot land says so first rather than
    // after a table that looks like it worked. A dry run refuses too: one that
    // reported a write the real run would decline would be telling whoever ran
    // it the one thing they asked this mode to check. The write itself refuses
    // again, atomically, because the two moments are not the same moment.
    println!("reading {}", source.display());
    // "would write" under `--dry-run`: a preview that announced a write and
    // then said nothing was written would be contradicting itself in the one
    // mode somebody runs to be told the truth about what will happen.
    println!(
        "{} {}",
        if dry_run { "would write" } else { "writing" },
        destination.display()
    );
    if destination.exists() {
        bail!(
            "{} already exists; move it aside and run this again",
            destination.display()
        );
    }

    let text = fs::read_to_string(&source)
        .with_context(|| format!("{} could not be read", source.display()))?;
    let legacy = decode(&source, &text)?;
    let (document, report) = translate(&source, &text)?;

    print_table(&report);
    warn(&comments(&source, &text)?);

    let rendered = document.to_string();
    // The error and the path, never the document body: an `mcp` entry's
    // headers map is where a bearer token lives, and this build withholds
    // header *values* even from `ganja mcp get`. `toml_edit`'s own error
    // already names the key and the position, which is the whole of what
    // somebody needs to find it.
    let migrated = toml_edit::de::from_str::<Config>(&rendered).map_err(|error| {
        anyhow!(
            "{} would not have been one ganja can load, so nothing was written: {error}",
            destination.display()
        )
    })?;
    if migrated != legacy {
        bail!(
            "{} does not read back as {} does, so nothing was written{}",
            destination.display(),
            source.display(),
            dropped(&report)
        );
    }

    if dry_run {
        println!("dry run — nothing written");
    } else {
        write(&destination, &rendered)?;
        println!("wrote {}", destination.display());
    }

    println!(
        "{} was left exactly as it is. This build reads {DESTINATION} first, so the \
         legacy file beside it no longer decides anything — remove it once you are \
         satisfied with what was written.",
        source.display()
    );
    remaining(&source, &cwd);

    Ok(())
}

/// The legacy file this run translates.
///
/// Three ways in, in the order they outrank each other: a named file is the
/// whole answer, `--global` is the config home's, and otherwise the closest
/// one at or above the working directory — the file the loader's project walk
/// would let win.
fn source(file: Option<PathBuf>, global: bool, cwd: &Path) -> Result<PathBuf> {
    if let Some(file) = file {
        if !file.is_file() {
            bail!("{} does not exist", file.display());
        }

        return Ok(file);
    }

    if global {
        let home = ganja_core::config::config_home()
            .context("the home directory holding the global config could not be located")?;

        return legacy_in(&home).ok_or_else(|| {
            anyhow!(
                "no {} or {} in {}",
                LEGACY_FILES[0],
                LEGACY_FILES[1],
                home.display()
            )
        });
    }

    nearest(cwd).ok_or_else(|| {
        anyhow!(
            "no {} or {} in {} or any directory above it up to the project root; \
             `--global` reads the config home's instead, and `--file` names one anywhere",
            LEGACY_FILES[0],
            LEGACY_FILES[1],
            cwd.display()
        )
    })
}

/// The legacy file `directory` holds, preferring the one the loader prefers.
fn legacy_in(directory: &Path) -> Option<PathBuf> {
    LEGACY_FILES
        .iter()
        .map(|name| directory.join(name))
        .find(|path| path.is_file())
}

/// The closest legacy file at or above `cwd`, the walk stopping where the
/// loader's own stops.
///
/// Innermost first, which is the reverse of what the loader collects: it
/// merges outermost-first so the closest wins, and this wants the winner
/// directly.
fn nearest(cwd: &Path) -> Option<PathBuf> {
    // Canonicalised the way `Project::resolve` canonicalises its root, or the
    // walk would not recognise the root it is meant to stop at.
    let start = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let stop = Project::resolve(cwd).root().to_path_buf();

    for directory in start.ancestors() {
        if let Some(found) = legacy_in(directory) {
            return Some(found);
        }
        if directory == stop {
            break;
        }
    }

    None
}

/// Names every *other* legacy file this build's discovery can still see.
///
/// The countermeasure to the pre-mortem's second failure: the loader's refusal
/// will name one file, a migration fixes that one, and the next launch names
/// the next — a user who was never told the others exist concludes the command
/// is broken. One run tells the whole story instead. Silent when there is
/// nothing else to say.
fn remaining(source: &Path, cwd: &Path) {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut add = |path: PathBuf| {
        if path != *source && !found.contains(&path) {
            found.push(path);
        }
    };

    if let Some(home) = ganja_core::config::config_home() {
        for name in LEGACY_FILES {
            let path = home.join(name);
            if path.is_file() {
                add(path);
            }
        }
    }

    let start = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let stop = Project::resolve(cwd).root().to_path_buf();
    for directory in start.ancestors() {
        for name in LEGACY_FILES {
            let path = directory.join(name);
            if path.is_file() {
                add(path);
            }
        }
        if directory == stop {
            break;
        }
    }

    if let Ok(named) = std::env::var(ganja_core::config::CONFIG_ENV) {
        let path = PathBuf::from(named.trim());
        if path.is_file()
            && LEGACY_FILES
                .iter()
                .any(|name| path.file_name().is_some_and(|found| found == *name))
        {
            add(path);
        }
    }

    if found.is_empty() {
        return;
    }

    println!(
        "still legacy, and still read by this build: {}",
        found
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("migrate each with `ganja config migrate --file <path>`");
}

/// The source as this build's loader decodes it.
///
/// Through the loader's own dialect, so a file only a looser parser accepts is
/// refused here rather than translated into a TOML that will not load either.
/// The `Option` is what makes an empty file, or one holding nothing but
/// comments, an empty config rather than a type error about `null`.
///
/// One post-decode refusal travels with it, and only one: the loader runs
/// `McpServer::check` over every entry after decoding, so a source that
/// decodes and fails *that* is one this build refuses at launch —
/// `import.rs` calls the same method for the same reason, and calling it
/// beats spelling the rule a third time. The loader's six other post-decode
/// checks are private to it and are **not** applied here; see the module doc
/// for what that costs and where it is repaid.
fn decode(path: &Path, text: &str) -> Result<Config> {
    let config = jsonc_parser::parse_to_serde_value::<Option<Config>>(
        text,
        &ganja_core::config::parse_options(),
    )
    .map_err(|error| anyhow!("{}: {error}", path.display()))?
    .unwrap_or_default();

    for (name, server) in &config.mcp {
        server
            .check(name)
            .map_err(|message| anyhow!("{}: {message}", path.display()))?;
    }

    Ok(config)
}

/// The source as a TOML document, in the order the source wrote it.
///
/// Built from the syntax tree rather than from the decoded [`Config`] for two
/// reasons, and only the second is negotiable: [`Config`] has no `Serialize`
/// at all, and the tree is what still holds the order a map would have thrown
/// away.
fn translate(path: &Path, text: &str) -> Result<(DocumentMut, Report)> {
    let parsed = jsonc_parser::parse_to_ast(
        text,
        &jsonc_parser::CollectOptions {
            comments: jsonc_parser::CommentCollectionStrategy::Off,
            tokens: false,
        },
        &ganja_core::config::parse_options(),
    )
    .map_err(|error| anyhow!("{}: {error}", path.display()))?;

    let mut report = Report::default();
    let mut document = DocumentMut::new();

    match parsed.value.as_ref() {
        // A file holding nothing, or nothing but comments, is an empty config
        // rather than an error — and an empty config is an empty document.
        None | Some(Ast::NullKeyword(_)) => {}
        Some(Ast::Object(root)) => {
            for property in &root.properties {
                let key = property.name.as_str();
                let Some(entry) = item(key, key, &property.value, &mut report)? else {
                    continue;
                };
                report.map(key, &spelling(key, &entry));
                document.as_table_mut().insert(key, entry);
            }
        }
        Some(_) => bail!(
            "{}: a config file has to hold a JSON object",
            path.display()
        ),
    }

    Ok((document, report))
}

/// Whether a table spelled under `key` must have its children rendered inline.
///
/// One key, and the reason is in the module doc: `permission`'s entries are
/// evaluated last-match-wins, so their order is their meaning, and a sub-table
/// header is what would reorder them. Every other table in a config is read
/// into a struct or a `BTreeMap`, neither of which can tell.
fn orders(key: &str) -> bool {
    key == PERMISSION
}

/// Renders one value at *table* position — where a table header or an
/// array-of-tables is a legal spelling.
fn item(at: &str, key: &str, value: &Ast<'_>, report: &mut Report) -> Result<Option<Item>> {
    match value {
        Ast::NullKeyword(_) => {
            report.skip(at, reason::NULL);

            Ok(None)
        }
        Ast::Object(object) => {
            let mut table = build(at, object, orders(key), report)?;
            // A table that holds nothing but other tables needs no header of
            // its own: `[mcp]` above `[mcp.docs]` says nothing `[mcp.docs]`
            // does not. An *empty* table keeps its header, because that header
            // is the whole value — `oauth = {}` is a marker, and an implicit
            // empty table would vanish.
            if !table.is_empty() && !table.iter().any(|(_, item)| item.is_value()) {
                table.set_implicit(true);
            }

            Ok(Some(Item::Table(table)))
        }
        Ast::Array(array)
            if !array.elements.is_empty()
                && array
                    .elements
                    .iter()
                    .all(|element| matches!(element, Ast::Object(_))) =>
        {
            let mut tables = ArrayOfTables::new();
            for (index, element) in array.elements.iter().enumerate() {
                let Ast::Object(object) = element else {
                    unreachable!("the guard above admitted only objects")
                };
                // Never implicit: the header *is* the array element, so
                // suppressing it would drop the entry rather than tidy it.
                tables.push(build(&indexed(at, index), object, false, report)?);
            }

            Ok(Some(Item::ArrayOfTables(tables)))
        }
        other => Ok(inline(at, other, report)?.map(Item::Value)),
    }
}

/// Renders an object's properties into a table, in the order it wrote them.
fn build(
    at: &str,
    object: &Object<'_>,
    inline_children: bool,
    report: &mut Report,
) -> Result<Table> {
    let mut table = Table::new();
    for property in &object.properties {
        let key = property.name.as_str();
        let child = join(at, key);
        let entry = if inline_children {
            inline(&child, &property.value, report)?.map(Item::Value)
        } else {
            item(&child, key, &property.value, report)?
        };
        if let Some(entry) = entry {
            table.insert(key, entry);
        }
    }

    Ok(table)
}

/// Renders one value where only a value is legal — inside an array, inside an
/// inline table, or anywhere [`orders`] says order has to be kept.
fn inline(at: &str, value: &Ast<'_>, report: &mut Report) -> Result<Option<Value>> {
    let rendered = match value {
        Ast::NullKeyword(_) => {
            report.skip(at, reason::NULL);

            return Ok(None);
        }
        Ast::BooleanLit(literal) => Value::from(literal.value),
        Ast::NumberLit(literal) => number(at, literal.value)?,
        Ast::StringLit(literal) => Value::from(literal.value.as_ref()),
        Ast::Array(array) => {
            let mut items = Array::new();
            for (index, element) in array.elements.iter().enumerate() {
                let child = indexed(at, index);
                let Some(element) = inline(&child, element, report)? else {
                    bail!(
                        "{child} is null, and TOML has no null. Dropping it would shorten \
                         the list, which is not the same setting — remove it from the \
                         source, or give it a value, and run this again"
                    );
                };
                items.push(element);
            }

            Value::Array(items)
        }
        Ast::Object(object) => {
            let mut table = InlineTable::new();
            for property in &object.properties {
                let key = property.name.as_str();
                if let Some(entry) = inline(&join(at, key), &property.value, report)? {
                    table.insert(key, entry);
                }
            }

            Value::InlineTable(table)
        }
    };

    Ok(Some(rendered))
}

/// One JSON number as TOML holds it.
///
/// Through the literal's own text rather than an `f64`: a config may carry an
/// integer no double holds exactly, and this is a writer with no business
/// rounding one. An integer too large for TOML's signed 64 bits refuses rather
/// than becoming a float, for the same reason.
fn number(at: &str, literal: &str) -> Result<Value> {
    if literal.contains(['.', 'e', 'E']) {
        return literal.parse::<f64>().map(Value::from).map_err(|error| {
            anyhow!("{at} is {literal}, which is not a number TOML can hold: {error}")
        });
    }

    literal.parse::<i64>().map(Value::from).map_err(|error| {
        anyhow!(
            "{at} is {literal}, which is not a number TOML can hold: a whole number has to \
             fit in signed 64 bits ({error})"
        )
    })
}

/// How a top-level key was spelled in the file that was written.
///
/// The shape it took, not every header under it: a `hooks` that rendered as
/// `[[hooks.PreToolUse]]` is reported as a table, because the row answers
/// "what became of this key" and not "quote me the file".
fn spelling(key: &str, entry: &Item) -> String {
    match entry {
        Item::Table(_) => format!("[{key}]"),
        Item::ArrayOfTables(_) => format!("[[{key}]]"),
        _ => key.to_owned(),
    }
}

/// A dotted path, for a row that has to name where in the file something was.
fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        return key.to_owned();
    }

    format!("{prefix}.{key}")
}

/// The same, for an array element.
fn indexed(prefix: &str, index: usize) -> String {
    format!("{prefix}[{index}]")
}

/// Every line of the source that held a comment.
///
/// Reported rather than carried: a JSON *value* has no comments, so the
/// translation cannot keep them however it is written. Line numbers are what
/// turn that from a sentence somebody skims into three places to look. A block
/// comment names every line it covered, since all of them are gone.
fn comments(path: &Path, text: &str) -> Result<Vec<usize>> {
    let parsed = jsonc_parser::parse_to_ast(
        text,
        &jsonc_parser::CollectOptions {
            comments: jsonc_parser::CommentCollectionStrategy::Separate,
            tokens: false,
        },
        &ganja_core::config::parse_options(),
    )
    .map_err(|error| anyhow!("{}: {error}", path.display()))?;

    let mut lines: Vec<usize> = parsed
        .comments
        .iter()
        .flat_map(|map| map.values())
        .flat_map(|comments| comments.iter())
        .flat_map(|comment| {
            let range = match comment {
                jsonc_parser::ast::Comment::Line(line) => line.range,
                jsonc_parser::ast::Comment::Block(block) => block.range,
            };

            line_of(text, range.start)..=line_of(text, range.end)
        })
        .collect();
    lines.sort_unstable();
    lines.dedup();

    Ok(lines)
}

/// The one-based line `offset` falls on.
fn line_of(text: &str, offset: usize) -> usize {
    text.get(..offset.min(text.len()))
        .unwrap_or(text)
        .matches('\n')
        .count()
        + 1
}

/// The comment warning, silent when there were none to lose.
fn warn(lines: &[usize]) {
    if lines.is_empty() {
        return;
    }

    eprintln!(
        "warning: comments do not survive the translation; {} held one: {}",
        if lines.len() == 1 {
            "this line"
        } else {
            "these lines"
        },
        lines
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
}

/// What the report is allowed to add to the equality refusal.
///
/// A translation that produced a different config is a bug in this file, and
/// saying so is more useful than a guess — except in the one case with an
/// innocent explanation, where a dropped null was a value some type reads as
/// something other than absence.
fn dropped(report: &Report) -> String {
    if report.skipped.is_empty() {
        return String::new();
    }

    format!(
        ". Dropped as null, which TOML cannot spell: {}",
        report
            .skipped
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// What became of every key, and why anything was left out.
#[derive(Debug, Default)]
struct Report {
    /// One row per top-level key: what it was, and the shape it took.
    mapped: Vec<(String, String)>,
    /// One row per value that could not be carried, at whatever depth.
    skipped: Vec<(String, String)>,
}

impl Report {
    fn map(&mut self, key: &str, spelling: &str) {
        self.mapped.push((key.to_owned(), spelling.to_owned()));
    }

    fn skip(&mut self, key: &str, reason: &str) {
        self.skipped.push((key.to_owned(), reason.to_owned()));
    }
}

fn print_table(report: &Report) {
    let width = report
        .mapped
        .iter()
        .chain(&report.skipped)
        .map(|(key, _)| key.chars().count())
        .chain(std::iter::once(HEADER.chars().count()))
        .max()
        .unwrap_or_default();

    section("mapped", "TOML", &report.mapped, width);
    println!();
    section("skipped", "REASON", &report.skipped, width);
}

fn section(name: &str, right: &str, rows: &[(String, String)], width: usize) {
    println!("{name}");
    if rows.is_empty() {
        println!("  (nothing)");

        return;
    }

    println!("  {HEADER:<width$}  {right}");
    for (left, value) in rows {
        println!("  {left:<width$}  {value}");
    }
}

/// Writes `document` to `path`, staged beside it and renamed into place.
///
/// Shared with `claude_hooks.rs`, which writes into the same directory for the
/// same reason. `mcp.rs` holds a third copy of the pattern; hoisting all three
/// into one home is a tidy-up this landing deliberately does not make, because
/// that file belongs to another command and the move would touch it for no
/// behavioral reason.
///
/// Staged rather than written in place because the bytes have to reach the
/// disk before anything can read a half-file, and a rename within one
/// directory is the one step that cannot be interrupted. `persist_noclobber`
/// is what keeps the destination refusal true at the moment of writing rather
/// than only at the moment it was checked.
pub(crate) fn write(path: &Path, document: &str) -> Result<()> {
    write_with(path, document, true)
}

/// [`write()`], with the choice of whether an existing file is replaced.
///
/// `migrate` only ever creates, so it refuses a destination that appeared
/// while it worked; `import-claude-hooks` edits a file that is normally
/// already there, so it replaces.
pub(crate) fn write_with(path: &Path, document: &str, create_only: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("{} could not be created", parent.display()))?;
    }

    let staged = stage(path, document.as_bytes())?;
    let persisted = if create_only {
        staged.persist_noclobber(path)
    } else {
        staged.persist(path)
    };

    // `PersistError` hands the staged file back rather than dropping it, so
    // the temporary outlives the failed rename by exactly as long as this
    // closure holds it: letting the error go is what removes the file, and
    // leaving one behind would leave a dotted half-config in a project
    // directory forever.
    persisted.map_err(|error| {
        if create_only && error.error.kind() == std::io::ErrorKind::AlreadyExists {
            return anyhow!(
                "{} already exists; move it aside and run this again",
                path.display()
            );
        }

        anyhow!("{} could not be written: {}", path.display(), error.error)
    })?;

    Ok(())
}

/// Writes `bytes` to a fresh file beside `path`, and hands back the file
/// itself — unnamed by anything the caller has to remember to clean up.
///
/// `mcp.rs`'s twin, and for its reasons: staged *beside* so two processes in
/// one directory cannot rename each other's half-written bytes into place, and
/// tied to a value so the file is removed on every path out of every caller,
/// including the one nobody wrote.
fn stage(path: &Path, bytes: &[u8]) -> Result<NamedTempFile> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));

    // Every sentence names `path` and not the staged file: the temporary's
    // name is this function's business and never something somebody typed, so
    // a person reading the failure is told about the config file they asked
    // to write.
    let mut staged = NamedTempFile::new_in(directory)
        .with_context(|| format!("{} could not be written", path.display()))?;
    staged
        .write_all(bytes)
        .with_context(|| format!("{} could not be written", path.display()))?;
    // The rename publishes the file as complete, so its bytes must reach the
    // backing store before that atomic namespace change makes them current.
    staged
        .as_file()
        .sync_all()
        .with_context(|| format!("{} could not be written", path.display()))?;

    // A temporary is created `0600`, and a rename carries that mode onto the
    // target — so an existing config would quietly lose whatever mode its
    // owner gave it. Copying the mode across keeps an edit an edit; a file
    // this *creates* keeps the `0600`, which is the safer default for a
    // document whose remote entries carry `Authorization` headers.
    if let Ok(existing) = fs::symlink_metadata(path)
        && existing.file_type().is_file()
    {
        staged
            .as_file()
            .set_permissions(existing.permissions())
            .with_context(|| format!("{} could not be written", path.display()))?;
    }

    Ok(staged)
}

#[cfg(test)]
#[path = "migrate_tests.rs"]
mod tests;
