//! `ganja config import-claude-hooks` — a Claude Code `settings.json`'s hooks
//! block, copied into a `ganja.toml` (**D537**).
//!
//! Spec: none, in the porting sense — upstream opencode has no hooks at all.
//! What this reads is Claude Code's own `settings.json`, whose `hooks` shape
//! `ganja_core::config::HookMatcher` already keeps verbatim (**D456**), and
//! that shared shape is the whole reason a copier is a few hundred lines
//! rather than a translator. It exists because the format change took the
//! paste route away: a `hooks` block used to be pasteable from one JSON file
//! into another, and TOML is not JSON. This is what replaces the paste.
//!
//! `import.rs`'s posture, key for key — **every key is either mapped or
//! reported**, and the table is the output while the file is a side effect of
//! it — plus one rule of its own:
//!
//! * **A group this build would refuse at load is refused here.** The loader
//!   checks a hooks block *after* decoding it (`config.rs`'s `check_hooks`),
//!   and a group that fails those checks makes the whole file unreadable — so
//!   writing one would hand somebody a config that stops their next launch,
//!   over a hook they may not even have wanted. A handler with no command and
//!   a matcher that is not a regular expression are both reported with the
//!   reason and left out, and the rest of the import still lands. See
//!   [`refusal`], which mirrors the rule rather than sharing it, and says so.
//! * **Only `hooks` is read.** Every other key of a settings file is a row in
//!   the skipped section: this command copies hooks, and a person who ran it
//!   expecting their permissions or their model to come across should be told
//!   in the same breath that they did not.
//! * **`timeout` is not converted.** Claude counts seconds and so does ganja
//!   (`config.rs`'s `HookCommand::timeout`), so the number travels as it
//!   stands. A conversion here would be a bug that only shows up as a hook
//!   killed early, months later.
//!
//! Groups are **appended** after whatever the target already holds for the
//! event, never merged into it and never replacing it, because appending is
//! what the config system's own hooks semantics do with two tiers naming one
//! event: every group that matches runs. Running this twice therefore lands
//! the hooks twice, which is visible in the file and in `--dry-run` before it
//! is a surprise.
//!
//! The edit goes through `toml_edit`, so every comment and every position in
//! the target survives — the same contract `ganja mcp add` keeps (**D483**),
//! for the same reason: the file being edited is one somebody wrote by hand.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, anyhow, bail};
use ganja_core::{
    config::Config,
    hook::{EVENTS, HookEvent},
};
use ganja_permission::Project;
use serde_json::Value;
use toml_edit::{ArrayOfTables, DocumentMut, InlineTable, Item, Table};

use crate::report::{Report, print_table};

/// The directory Claude keeps its settings in, under a home or a project root.
const CLAUDE_DIRECTORY: &str = ".claude";

/// Claude's settings file.
const SETTINGS: &str = "settings.json";

/// The one beside it that is not committed, and that wins where both name a
/// thing — Claude's own precedence, which is why it is read second.
const LOCAL_SETTINGS: &str = "settings.local.json";

/// What this edits, and creates when it is not there.
const TARGET: &str = "ganja.toml";

/// The table on both sides. One word, because both sides spell it the same.
const TABLE: &str = "hooks";

/// The one handler kind this build runs. Claude's `http`, `prompt` and `agent`
/// handlers are reported by name rather than accepted and ignored.
const COMMAND: &str = "command";

/// Left column of both sections of the table.
const HEADER: &str = "CLAUDE";

/// Why a group, a handler or a key was left out. One word each, so the
/// right-hand column of the skipped section stays a column.
mod reason {
    /// This command reads `hooks`, and this key is not it.
    pub const UNREAD: &str = "unread";
    /// An event name this build fires nothing for.
    pub const UNRUN: &str = "unrun";
    /// A handler kind this build cannot run.
    pub const UNSUPPORTED: &str = "unsupported";
    /// A key neither side's shape names.
    pub const UNKNOWN: &str = "unknown";
    /// The value is not the shape the key takes.
    pub const MALFORMED: &str = "malformed";
    /// Ganja would refuse the value at load, so writing it would produce a
    /// config file that does not read back.
    pub const REFUSED: &str = "refused";
    /// Everything the group would have run was left out, so there is nothing
    /// left of it to write.
    pub const EMPTY: &str = "empty";
}

/// Reads a Claude settings file and merges its hooks into a `ganja.toml`.
///
/// `file` reads exactly that file and skips discovery. `global` reads
/// `~/.claude/settings.json` and writes ganja's global config; without it the
/// project's own two settings files are read — the local one second, as Claude
/// resolves them — and the result lands at the project root.
///
/// # Errors
///
/// A named file that is not there or is not JSON, a target that cannot be
/// parsed, a target whose `hooks` is not the shape this appends to, or a
/// merged document this build could not read back.
pub fn import_claude_hooks(file: Option<PathBuf>, global: bool, dry_run: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read the working directory")?;
    let sources = sources(file, global, &cwd)?;
    let target = target(global, &cwd)?;

    for path in &sources.paths {
        println!("reading {}", path.display());
    }

    if sources.paths.is_empty() {
        println!("nothing to import: no Claude settings file was found");
        for place in &sources.searched {
            eprintln!("note: nothing in {place}");
        }

        return Ok(());
    }

    let mut report = Report::default();
    let mut collected: BTreeMap<&'static str, Vec<Group>> = BTreeMap::new();
    for path in &sources.paths {
        collect(path, &mut collected, &mut report)?;
    }

    print_table(&report, HEADER, "GANJA");
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }

    if collected.is_empty() {
        println!("nothing to import: no hooks group survived");

        return Ok(());
    }

    // Announced here rather than beside the `reading` lines above, which is
    // where it first stood: both early returns above it can discover there is
    // nothing to write at all, and a run that names a target and then writes
    // nothing to it is the self-contradiction this line exists to avoid. Below
    // them it is still before any write, which is what the pre-mortem's third
    // failure asks for — a run started in a subdirectory whose hooks land in a
    // file the session reads second, turned from a mystery into a choice. The
    // wording follows `migrate.rs`: a `--dry-run` says "would write", because a
    // preview that announced a write and then said nothing was written would be
    // contradicting itself in the one mode somebody runs to be told the truth.
    println!(
        "{} {}",
        if dry_run { "would write" } else { "writing" },
        target.display()
    );

    let existing = existing(&target)?;
    // Decoded before the merge as well as after, so a target that was already
    // unreadable is named as such rather than blamed on this import.
    toml_edit::de::from_str::<Config>(&existing).map_err(|error| {
        anyhow!(
            "{} is not a config ganja can load, and this refuses to edit one that is not: \
             {error}",
            target.display()
        )
    })?;

    let document = merge(&target, &existing, &collected)?;
    let rendered = document.to_string();
    // The error and the path, never the document body: an `mcp` entry the
    // target already carries has a headers map, and that is where a bearer
    // token lives — this build withholds header *values* even from `ganja mcp
    // get`. `toml_edit`'s own error already names the key and the position.
    toml_edit::de::from_str::<Config>(&rendered).map_err(|error| {
        anyhow!(
            "{} would not have been one ganja can load, so nothing was written: {error}",
            target.display()
        )
    })?;

    if dry_run {
        println!("dry run — nothing written");

        return Ok(());
    }

    crate::migrate::write_with(&target, &rendered, false)?;
    println!("wrote {}", target.display());
    println!("a running session keeps its hooks until its next start");

    Ok(())
}

/// The settings files to read, and where the search went.
struct Sources {
    /// Every file that will be read, in the order Claude resolves them.
    paths: Vec<PathBuf>,
    /// Where the search went, for the run that finds nothing: whoever keeps
    /// their settings somewhere else needs to be told where this looked.
    searched: Vec<String>,
}

fn sources(file: Option<PathBuf>, global: bool, cwd: &Path) -> Result<Sources> {
    // A named file is the whole import: a caller who said which file to read
    // did not ask what else is lying around.
    if let Some(file) = file {
        if !file.is_file() {
            bail!("{} does not exist", file.display());
        }

        return Ok(Sources {
            paths: vec![file],
            searched: Vec::new(),
        });
    }

    let directory = if global {
        home()?.join(CLAUDE_DIRECTORY)
    } else {
        Project::resolve(cwd).root().join(CLAUDE_DIRECTORY)
    };

    // Both, in Claude's own order: the committed file first and the local one
    // after it, so a machine-specific override is what the append sees last.
    let names: &[&str] = if global {
        &[SETTINGS]
    } else {
        &[SETTINGS, LOCAL_SETTINGS]
    };

    Ok(Sources {
        paths: names
            .iter()
            .map(|name| directory.join(name))
            .filter(|path| path.is_file())
            .collect(),
        searched: vec![directory.display().to_string()],
    })
}

/// The `ganja.toml` this edits.
///
/// The same two directories every other config-writing command here resolves —
/// the config home for `--global`, the project root otherwise — so hooks land
/// in the file the next launch reads and not in whichever directory the
/// command happened to be run from.
fn target(global: bool, cwd: &Path) -> Result<PathBuf> {
    let directory = if global {
        ganja_core::config::config_home()
            .context("the home directory holding the global config could not be located")?
    } else {
        Project::resolve(cwd).root().to_path_buf()
    };

    Ok(directory.join(TARGET))
}

/// The home directory Claude's global settings hang under.
///
/// Deliberately not `ganja_core::config::config_home`: that seam resolves
/// where *ganja's* things live and moves with `GANJA_CONFIG_HOME`, while the
/// directory read here is another tool's, fixed by that tool's own convention.
fn home() -> Result<PathBuf> {
    use etcetera::base_strategy::{BaseStrategy as _, Xdg};

    Xdg::new()
        .map(|base| base.home_dir().to_path_buf())
        .context("the home directory holding Claude's settings could not be located")
}

/// One group as this build would write it.
#[derive(Debug)]
struct Group {
    /// The subject pattern, absent when the group matches everything.
    matcher: Option<String>,
    /// What it runs, in the order the settings file listed them.
    handlers: Vec<Handler>,
}

/// One `type: "command"` handler.
#[derive(Debug)]
struct Handler {
    /// The command line.
    command: String,
    /// Its deadline in **seconds**, which is the unit on both sides.
    timeout: Option<i64>,
}

/// Reads one settings file into `into`, reporting everything it does not take.
///
/// Never fatal past the parse: an event this build does not fire, a handler it
/// cannot run and a key neither side names are rows, because the run is worth
/// having for the groups that do map and a person cannot fix what they were
/// not told about.
fn collect(
    path: &Path,
    into: &mut BTreeMap<&'static str, Vec<Group>>,
    report: &mut Report,
) -> Result<()> {
    // Every row this file produces is prefixed with its name: the project
    // tier reads two settings files, both of which have a `hooks.Stop[0]`,
    // and a row that could be either is a row nobody can act on.
    let label = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned();

    let text = fs::read_to_string(path)
        .with_context(|| format!("{} could not be read", path.display()))?;
    // Strict JSON, not the config loader's dialect: Claude's file is plain
    // JSON, and a comment in one is a file Claude itself would not read.
    let document: Value =
        serde_json::from_str(&text).map_err(|error| anyhow!("{}: {error}", path.display()))?;
    let Value::Object(settings) = document else {
        bail!(
            "{}: a settings file has to hold a JSON object",
            path.display()
        );
    };

    for key in settings.keys() {
        if key != TABLE {
            report.skip(&format!("{label}:{key}"), reason::UNREAD);
        }
    }

    let Some(hooks) = settings.get(TABLE) else {
        return Ok(());
    };
    let Some(hooks) = hooks.as_object() else {
        report.skip(&format!("{label}:{TABLE}"), reason::MALFORMED);

        return Ok(());
    };

    for (event, groups) in hooks {
        let at = format!("{label}:{TABLE}.{event}");
        let Some(known) = HookEvent::from_name(event) else {
            report.skip(&at, reason::UNRUN);
            continue;
        };
        let Some(groups) = groups.as_array() else {
            report.skip(&at, reason::MALFORMED);
            continue;
        };

        for (index, value) in groups.iter().enumerate() {
            let at = format!("{at}[{index}]");
            if let Some(group) = group(&at, value, report) {
                report.map(&at, &format!("[[{TABLE}.{event}]]"));
                into.entry(known.name()).or_default().push(group);
            }
        }
    }

    Ok(())
}

/// One group, or nothing and a row saying why.
fn group(at: &str, value: &Value, report: &mut Report) -> Option<Group> {
    let Some(object) = value.as_object() else {
        report.skip(at, reason::MALFORMED);

        return None;
    };

    let mut matcher = None;
    let mut handlers = Vec::new();
    for (key, value) in object {
        let child = format!("{at}.{key}");
        match key.as_str() {
            "matcher" => {
                // A matcher that is not a string is not a matcher, and
                // guessing which subjects it meant is the completion this
                // refuses to do anywhere.
                let Some(text) = value.as_str() else {
                    report.skip(&child, reason::MALFORMED);

                    return None;
                };
                matcher = Some(text.to_owned());
            }
            TABLE => {
                let Some(items) = value.as_array() else {
                    report.skip(&child, reason::MALFORMED);

                    return None;
                };
                for (index, item) in items.iter().enumerate() {
                    if let Some(handler) = handler(&format!("{child}[{index}]"), item, report) {
                        handlers.push(handler);
                    }
                }
            }
            other => report.skip(&format!("{at}.{other}"), reason::UNKNOWN),
        }
    }

    let group = Group { matcher, handlers };
    if group.handlers.is_empty() {
        report.skip(at, reason::EMPTY);

        return None;
    }
    if let Some(refusal) = refusal(&group) {
        report.skip(at, reason::REFUSED);
        report.warn(format!("{at} was left out — {refusal}"));

        return None;
    }

    Some(group)
}

/// One handler, or nothing and a row saying why.
fn handler(at: &str, value: &Value, report: &mut Report) -> Option<Handler> {
    let Some(object) = value.as_object() else {
        report.skip(at, reason::MALFORMED);

        return None;
    };

    // Reported before anything else is looked at: a handler of another kind
    // has fields this build has never heard of, and calling those unknown
    // would bury the one row that explains them.
    match object.get("type").and_then(Value::as_str) {
        Some(COMMAND) => {}
        Some(_) | None => {
            report.skip(at, reason::UNSUPPORTED);

            return None;
        }
    }

    let mut command = None;
    let mut timeout = None;
    for (key, value) in object {
        let child = format!("{at}.{key}");
        match key.as_str() {
            "type" => {}
            "command" => {
                let Some(text) = value.as_str() else {
                    report.skip(&child, reason::MALFORMED);

                    return None;
                };
                command = Some(text.to_owned());
            }
            "timeout" => {
                // Seconds on both sides, so the number travels as it stands —
                // and a value TOML has no whole number for is reported rather
                // than rounded into a different deadline.
                let Some(seconds) = value.as_i64().filter(|seconds| *seconds >= 0) else {
                    report.skip(&child, reason::MALFORMED);

                    return None;
                };
                timeout = Some(seconds);
            }
            other => report.skip(&format!("{at}.{other}"), reason::UNKNOWN),
        }
    }

    let Some(command) = command else {
        report.skip(at, reason::MALFORMED);

        return None;
    };

    Some(Handler { command, timeout })
}

/// The refusals `ganja_core::config`'s `check_hooks` makes, applied before
/// anything is written.
///
/// Mirrored rather than called: the checks are private to the loader, and the
/// one door into them reads a whole file off the disk and merges every tier
/// around it, so asking them about a single group would mean writing the file
/// first — which is the order this exists to invert. `import.rs` calls
/// `McpServer::check` where an authority is public and spells the rule again
/// only where it is not; this is the second case, and the cost of it is that
/// these two arms have to stay in step with `check_hooks`' by review.
///
/// Both of that function's group-level refusals are here, and both are
/// silent failures on the far side of them: a handler with nothing to run is
/// a shell invocation with nothing in it, and a matcher that does not compile
/// is a group that matches nothing forever. Neither announces itself, and
/// both make the *whole file* unreadable at the next launch — which is what
/// makes writing one worse than declining to.
///
/// The matcher is judged by the same engine the loader judges it with, which
/// is the only way the two answers cannot disagree. An empty matcher is not a
/// pattern at all — it means "everything", on both sides — so it is not put
/// to the engine, exactly as `check_hooks` does not.
fn refusal(group: &Group) -> Option<String> {
    if group
        .handlers
        .iter()
        .any(|handler| handler.command.trim().is_empty())
    {
        return Some("a command handler with no command".to_owned());
    }

    if let Some(matcher) = group
        .matcher
        .as_deref()
        .filter(|matcher| !matcher.is_empty())
        && let Err(error) = regex::Regex::new(matcher)
    {
        return Some(format!(
            "a matcher that is not a regular expression: {error}"
        ));
    }

    None
}

/// The target's own bytes, or nothing when it is not there yet.
fn existing(target: &Path) -> Result<String> {
    match fs::read_to_string(target) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("{} could not be read", target.display())),
    }
}

/// The target with the collected groups appended, everything else untouched.
///
/// Parsed as a document rather than decoded into a [`Config`]: the typed round
/// trip would drop every key this build does not know and print the file back
/// without the comments its author wrote it for. The tree holds the bytes, so
/// everything this does not touch comes out as it went in.
fn merge(
    target: &Path,
    text: &str,
    collected: &BTreeMap<&'static str, Vec<Group>>,
) -> Result<DocumentMut> {
    let mut document: DocumentMut = text
        .parse()
        .map_err(|error| anyhow!("{} could not be parsed: {error}", target.display()))?;

    let table = document.as_table_mut().entry(TABLE).or_insert_with(|| {
        let mut table = Table::new();
        // No `[hooks]` header of its own: every group is spelled
        // `[[hooks.<Event>]]`, and a bare header above them says nothing.
        table.set_implicit(true);

        Item::Table(table)
    });
    let Some(table) = table.as_table_mut() else {
        bail!("{}'s `{TABLE}` is not a table", target.display());
    };

    // Walked in the roster's own order rather than the map's, so two runs over
    // the same settings file write the same file.
    for event in EVENTS {
        let Some(groups) = collected.get(event.name()) else {
            continue;
        };

        let entry = table
            .entry(event.name())
            .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
        match entry {
            // Appended, never merged into and never replacing: two tiers
            // naming one event both fire, which is the config system's own
            // answer for what a second group means.
            Item::ArrayOfTables(tables) => {
                for group in groups {
                    tables.push(rendered(group));
                }
            }
            // The same list, spelled inline. A legal file, so it is appended
            // to in its own spelling rather than refused or rewritten.
            Item::Value(toml_edit::Value::Array(array)) => {
                for group in groups {
                    array.push(inline(group));
                }
            }
            _ => bail!(
                "{}'s `{TABLE}.{}` is not a list of groups",
                target.display(),
                event.name()
            ),
        }
    }

    Ok(document)
}

/// One group as an array-of-tables entry.
fn rendered(group: &Group) -> Table {
    let mut table = Table::new();
    if let Some(matcher) = &group.matcher {
        table.insert("matcher", toml_edit::value(matcher.clone()));
    }

    let mut handlers = ArrayOfTables::new();
    for handler in &group.handlers {
        let mut entry = Table::new();
        entry.insert("type", toml_edit::value(COMMAND));
        entry.insert("command", toml_edit::value(handler.command.clone()));
        if let Some(timeout) = handler.timeout {
            entry.insert("timeout", toml_edit::value(timeout));
        }
        handlers.push(entry);
    }
    table.insert(TABLE, Item::ArrayOfTables(handlers));

    table
}

/// The same group, for a target that spelled its list inline.
fn inline(group: &Group) -> toml_edit::Value {
    let mut table = InlineTable::new();
    if let Some(matcher) = &group.matcher {
        table.insert("matcher", matcher.clone().into());
    }

    let mut handlers = toml_edit::Array::new();
    for handler in &group.handlers {
        let mut entry = InlineTable::new();
        entry.insert("type", COMMAND.into());
        entry.insert("command", handler.command.clone().into());
        if let Some(timeout) = handler.timeout {
            entry.insert("timeout", timeout.into());
        }
        handlers.push(entry);
    }
    table.insert(TABLE, handlers.into());

    table.into()
}

#[cfg(test)]
#[path = "claude_hooks_tests.rs"]
mod tests;
