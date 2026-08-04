//! `ganja config import-opencode` — a one-way translation of an opencode
//! config into ganja's own.
//!
//! Spec: upstream `packages/opencode/src/config/config.ts` for discovery and
//! the merge order, `packages/core/src/v1/config/*.ts` for the key set.
//!
//! This is not interop. Nothing here reads opencode's sessions, and nothing it
//! writes is read back by opencode: it takes a config that already exists and
//! answers "what would ganja make of this", once, into a file the user then
//! owns. Every key is either mapped or reported — a setting that vanished
//! without a row would be one its author still believes is in force, which is
//! the failure this command exists to prevent, so the table is the output and
//! the file is a side effect of it.
//!
//! Two rules are load-bearing, and neither is a matter of taste:
//!
//! * **A credential is never written.** `provider.<id>.options.apiKey` is
//!   skipped with a warning naming `ganja auth login`. Ganja's keys travel the
//!   environment or `auth.json`, in a `SecretString` end to end, and a config
//!   file this command produced would be the one place a key could sit in the
//!   clear.
//! * **`{env:VAR}` and `{file:path}` are never expanded.** Upstream
//!   substitutes them textually *before* parsing, which is how a secret ends up
//!   inside a config file at all. A value that is nothing but a token is left
//!   out and named; a value that merely contains one is carried verbatim,
//!   because ganja will then read it literally and its author has to know that.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write as _},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, anyhow, bail};
use ganja_core::{Project, config::Config};

/// Directory opencode keeps its global config in, under the XDG config home.
const OPENCODE_DIRECTORY: &str = "opencode";

/// Directory ganja's global config lives in, under the same home. Matches
/// `ganja_core::config`, which reads what this writes.
const GANJA_DIRECTORY: &str = "ganja";

/// opencode's global tier, in merge order — all three are read and later wins,
/// so `opencode.jsonc` has the last word (`config.ts:258-260`).
const GLOBAL_FILES: [&str; 3] = ["config.json", "opencode.json", "opencode.jsonc"];

/// opencode's project-tier names, in the order a directory is probed for them.
/// The collected list is reversed, which is what makes `opencode.jsonc` beat
/// `opencode.json` in one directory and the closest directory beat its
/// ancestors — upstream's `toReversed()`.
const PROJECT_FILES: [&str; 2] = ["opencode.jsonc", "opencode.json"];

/// What this writes. The `.jsonc` spelling is deliberately not used: the
/// generated file has no comments to justify one.
const DESTINATION: &str = "ganja.json";

/// The other name ganja will read, and the one that would *beat* what this
/// writes — so a destination directory holding it is as occupied as one
/// holding [`DESTINATION`].
const DESTINATION_ALTERNATE: &str = "ganja.jsonc";

/// The values upstream's agent `mode` field takes, which are also ganja's.
const MODES: [&str; 3] = ["primary", "subagent", "all"];

/// What a `mode.*` entry becomes: upstream folds those into `agent.*` with
/// this mode, whatever the entry said itself (`config.ts:536-543`).
const PRIMARY: &str = "primary";

/// Left column of both sections of the table.
const HEADER: &str = "OPENCODE";

/// Why a key was left out. One word each, so the right-hand column of the
/// skipped section stays a column.
mod reason {
    /// Ganja has no such feature.
    pub const UNSUPPORTED: &str = "unsupported";
    /// Providers are described by a compiled-in catalog, not by config.
    pub const CATALOG: &str = "catalog";
    /// A secret, which never belongs in a config file.
    pub const CREDENTIAL: &str = "credential";
    /// Ganja has the behavior but not the config key yet.
    pub const DEFERRED: &str = "deferred";
    /// The value is nothing but an unexpanded `{env:}`/`{file:}` token.
    pub const TOKEN: &str = "token";
    /// A key opencode does not document.
    pub const UNKNOWN: &str = "unknown";
    /// The value is not the shape the key takes.
    pub const MALFORMED: &str = "malformed";
    /// Something later in the same config already decided this.
    pub const OVERRIDDEN: &str = "overridden";
    /// Ganja publishes no schema to point an editor at.
    pub const UNPUBLISHED: &str = "unpublished";
    /// The key exists in both, but its contents mean different things.
    pub const INCOMPATIBLE: &str = "incompatible";
}

/// Top-level keys that are carried nowhere, and the one word each is reported
/// with. Everything not here and not handled explicitly is [`reason::UNKNOWN`].
const SKIPPED: [(&str, &str); 26] = [
    ("$schema", reason::UNPUBLISHED),
    ("attachment", reason::UNSUPPORTED),
    ("autoupdate", reason::UNSUPPORTED),
    ("compaction", reason::DEFERRED),
    ("disabled_providers", reason::CATALOG),
    ("enabled_providers", reason::CATALOG),
    ("enterprise", reason::UNSUPPORTED),
    ("experimental", reason::UNSUPPORTED),
    ("formatter", reason::UNSUPPORTED),
    // Ganja has `keybinds`, but its actions are a curated set of its own; an
    // upstream binding names an action that does not exist here.
    ("keybinds", reason::INCOMPATIBLE),
    ("layout", reason::UNSUPPORTED),
    ("logLevel", reason::UNSUPPORTED),
    ("lsp", reason::UNSUPPORTED),
    ("mcp", reason::UNSUPPORTED),
    ("plugin", reason::UNSUPPORTED),
    ("reference", reason::UNSUPPORTED),
    ("references", reason::UNSUPPORTED),
    ("server", reason::UNSUPPORTED),
    ("share", reason::UNSUPPORTED),
    ("skills", reason::UNSUPPORTED),
    ("snapshot", reason::UNSUPPORTED),
    ("subagent_depth", reason::UNSUPPORTED),
    ("tool_output", reason::UNSUPPORTED),
    ("tui", reason::INCOMPATIBLE),
    ("username", reason::UNSUPPORTED),
    ("watcher", reason::UNSUPPORTED),
];

/// Agent fields ganja has no use for, each reported where it was written.
const DROPPED_AGENT_FIELDS: [&str; 7] = [
    // A `ChatRequest` carries neither of these.
    "temperature",
    "top_p",
    // The agent loop has no step cap on purpose.
    "steps",
    "maxSteps",
    // Provider-variant machinery ganja does not have.
    "variant",
    "options",
    "color",
];

/// Command fields ganja has no use for.
const DROPPED_COMMAND_FIELDS: [&str; 2] = ["variant", "subtask"];

/// Reads an opencode config and writes the ganja config it maps to.
///
/// `file` imports exactly that file and skips discovery. `global` reads only
/// opencode's global tier and writes ganja's global config; without it the
/// project walk is read too and the result lands at the project root.
///
/// # Errors
///
/// A named file that is not there, a file that is not valid JSONC, a
/// destination that already exists, or a mapping that produces a config ganja
/// itself would refuse.
pub fn import_opencode(file: Option<PathBuf>, global: bool, dry_run: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read the working directory")?;

    // Resolved before anything is read so that a run which cannot land says so
    // first, rather than after a table that looks like it worked. The write
    // itself refuses again, atomically, because the two moments are not the
    // same moment.
    let destination = (!dry_run).then(|| destination(global, &cwd)).transpose()?;
    if let Some(destination) = &destination
        && let Some(occupied) = occupied(destination)
    {
        bail!(
            "{} already exists; move it aside and run this again",
            occupied.display()
        );
    }

    let sources = discover(file, global, &cwd)?;
    for path in &sources.paths {
        eprintln!("note: read {}", path.display());
    }
    if sources.paths.is_empty() {
        println!("nothing to import: no opencode config was found");
        for place in &sources.searched {
            eprintln!("note: nothing in {place}");
        }

        return Ok(());
    }

    let (built, report) = map_config(&sources.config);
    print_table(&report);
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }

    if built.is_empty() {
        println!("nothing to import: nothing in it maps to a key ganja has");

        return Ok(());
    }

    let rendered = built.document().render();
    validate(&rendered)?;

    let Some(destination) = destination else {
        println!("dry run — nothing written");

        return Ok(());
    };

    write(&destination, &rendered)?;
    println!("wrote {}", destination.display());

    Ok(())
}

/// How a config file is parsed: the JSONC dialect upstream accepts everywhere,
/// including in files named `.json`, and nothing beyond it. Matches
/// `ganja_core::config`, so a file that reads here reads there.
fn parse_options() -> jsonc_parser::ParseOptions {
    jsonc_parser::ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

/// A JSON document, with object keys in the order they were written.
///
/// Order is why an object is a `Vec` and not a map. Permission rules are
/// evaluated last-match-wins, so which of two rules covering the same call was
/// written second is the whole answer, and a map that sorted its keys would
/// silently change which rule decides. The same type is read into and written
/// out of, so nothing has to agree twice about what a document is.
#[derive(Clone, Debug, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    /// Kept as it was written. Nothing ganja's config carries is a number, so
    /// this exists to be *reported*, never re-emitted.
    Number(String),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    /// Converts a parsed document, collapsing a name spelled twice the way
    /// both JSON readers do: the later value at the earlier position.
    fn from_ast(value: &jsonc_parser::ast::Value<'_>) -> Self {
        use jsonc_parser::ast::{ObjectPropName, Value};

        match value {
            Value::StringLit(literal) => Self::String(literal.value.to_string()),
            Value::NumberLit(literal) => Self::Number(literal.value.to_owned()),
            Value::BooleanLit(literal) => Self::Bool(literal.value),
            Value::NullKeyword(_) => Self::Null,
            Value::Array(array) => Self::Array(array.elements.iter().map(Self::from_ast).collect()),
            Value::Object(object) => {
                let mut entries = Vec::with_capacity(object.properties.len());
                for property in &object.properties {
                    let name = match &property.name {
                        ObjectPropName::String(literal) => literal.value.to_string(),
                        ObjectPropName::Word(literal) => literal.value.to_owned(),
                    };
                    insert(&mut entries, name, Self::from_ast(&property.value));
                }

                Self::Object(entries)
            }
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Self::Object(entries) => Some(entries),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&[Json]> {
        match self {
            Self::Array(elements) => Some(elements),
            _ => None,
        }
    }

    fn get(&self, key: &str) -> Option<&Json> {
        self.as_object()?
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    /// The document as pretty JSON, two spaces to a level, newline-terminated.
    fn render(&self) -> String {
        let mut rendered = String::new();
        self.write(&mut rendered, 0);
        rendered.push('\n');

        rendered
    }

    fn write(&self, out: &mut String, depth: usize) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            Self::Number(value) => out.push_str(value),
            Self::String(value) => write_string(out, value),
            Self::Array(elements) if elements.is_empty() => out.push_str("[]"),
            Self::Array(elements) => {
                out.push_str("[\n");
                for (index, element) in elements.iter().enumerate() {
                    indent(out, depth + 1);
                    element.write(out, depth + 1);
                    separate(out, index + 1 < elements.len());
                }
                indent(out, depth);
                out.push(']');
            }
            Self::Object(entries) if entries.is_empty() => out.push_str("{}"),
            Self::Object(entries) => {
                out.push_str("{\n");
                for (index, (key, value)) in entries.iter().enumerate() {
                    indent(out, depth + 1);
                    write_string(out, key);
                    out.push_str(": ");
                    value.write(out, depth + 1);
                    separate(out, index + 1 < entries.len());
                }
                indent(out, depth);
                out.push('}');
            }
        }
    }
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

/// Ends one element of an object or array, with a comma when another follows.
fn separate(out: &mut String, more: bool) {
    if more {
        out.push(',');
    }
    out.push('\n');
}

/// Writes `value` as a JSON string literal.
///
/// Spelled out rather than delegated because this crate has no JSON writer of
/// its own, and the escaping is the part that has to be right: every control
/// character becomes an escape, so a value carrying a newline or a tab survives
/// the round trip [`validate`] then proves.
fn write_string(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            control if control.is_control() => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

/// Inserts `value` under `key`, keeping the position a key of that name
/// already holds.
///
/// That positional rule is upstream's `mergeDeep` (a re-specified key keeps its
/// place and takes the new value, a new key appends) and it is also what a JSON
/// reader does with an object that spells one name twice. Both matter here for
/// the same reason: `permission` is evaluated in order.
fn insert(entries: &mut Vec<(String, Json)>, key: String, value: Json) {
    match entries.iter_mut().find(|(name, _)| *name == key) {
        Some(slot) => slot.1 = value,
        None => entries.push((key, value)),
    }
}

/// Overlays `source` onto `target`: two objects merge key by key, and anything
/// else replaces wholesale. Upstream's `mergeDeep`.
fn merge(target: &mut Json, source: Json) {
    match (target, source) {
        (Json::Object(mine), Json::Object(theirs)) => {
            for (key, value) in theirs {
                match mine.iter_mut().find(|(name, _)| *name == key) {
                    Some(slot) => merge(&mut slot.1, value),
                    None => mine.push((key, value)),
                }
            }
        }
        (slot, replacement) => *slot = replacement,
    }
}

/// Overlays one config file onto the tiers below it.
///
/// `concat_instructions` is upstream's one exception to "arrays replace":
/// between tiers `instructions` is a union that keeps order and drops repeats,
/// so a project adds to the global list. Within the global tier it is a plain
/// replace, because that tier merges with `mergeConfig` and not with
/// `mergeConfigConcatArrays` (`config.ts:258-260` against `:398-410`).
fn merge_document(target: &mut Json, source: Json, concat_instructions: bool) {
    let union = concat_instructions
        .then(|| instruction_union(target, &source))
        .flatten();

    merge(target, source);

    if let (Some(union), Json::Object(entries)) = (union, target)
        && let Some(slot) = entries.iter_mut().find(|(name, _)| name == "instructions")
    {
        slot.1 = Json::Array(union);
    }
}

/// Both sides' `instructions`, in order, without repeats — or [`None`] when
/// they are not both arrays, which is when upstream's exception does not apply.
fn instruction_union(target: &Json, source: &Json) -> Option<Vec<Json>> {
    let mine = target.get("instructions")?.as_array()?;
    let theirs = source.get("instructions")?.as_array()?;

    let mut union: Vec<Json> = mine.to_vec();
    for instruction in theirs {
        if !union.contains(instruction) {
            union.push(instruction.clone());
        }
    }

    Some(union)
}

/// Where a key sits in each document: how the opencode file spells it, and how
/// the ganja file will. Carried as a pair so that a nested mapping never has to
/// rebuild either path from parts, and so a renamed branch (`mode` → `agent`)
/// renames every key under it exactly once.
#[derive(Clone, Debug)]
struct At {
    from: String,
    to: String,
}

impl At {
    /// The document itself; its children are spelled without a prefix.
    fn root() -> Self {
        Self {
            from: String::new(),
            to: String::new(),
        }
    }

    /// A branch ganja spells differently from opencode.
    fn renamed(from: &str, to: &str) -> Self {
        Self {
            from: from.to_owned(),
            to: to.to_owned(),
        }
    }

    fn child(&self, key: &str) -> Self {
        Self {
            from: join(&self.from, key),
            to: join(&self.to, key),
        }
    }

    fn index(&self, index: usize) -> Self {
        Self {
            from: format!("{}[{index}]", self.from),
            to: format!("{}[{index}]", self.to),
        }
    }
}

fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}.{key}")
    }
}

/// What the import did with every key it saw.
#[derive(Debug, Default)]
struct Report {
    /// `opencode key` → `ganja key`, in the order the document spelled them.
    mapped: Vec<(String, String)>,
    /// `opencode key` → why it was left out.
    skipped: Vec<(String, String)>,
    /// Everything that needs saying on the way, for stderr.
    warnings: Vec<String>,
}

impl Report {
    fn map(&mut self, from: &str, to: &str) {
        self.mapped.push((from.to_owned(), to.to_owned()));
    }

    fn skip(&mut self, key: &str, reason: &str) {
        self.skipped.push((key.to_owned(), reason.to_owned()));
    }

    fn warn(&mut self, warning: String) {
        self.warnings.push(warning);
    }
}

/// The ganja config being built, one slot per key it can carry.
///
/// Slots rather than a document under construction, so the emitted key order is
/// this struct's field order and not the order the source happened to use: two
/// opencode configs that say the same thing produce the same file.
#[derive(Debug, Default)]
struct Built {
    model: Option<String>,
    small_model: Option<String>,
    default_agent: Option<String>,
    theme: Option<String>,
    shell: Option<String>,
    instructions: Vec<String>,
    permission: Option<Json>,
    agent: Vec<(String, Json)>,
    command: Vec<(String, Json)>,
}

impl Built {
    /// Whether the import found nothing at all to write.
    fn is_empty(&self) -> bool {
        self.model.is_none()
            && self.small_model.is_none()
            && self.default_agent.is_none()
            && self.theme.is_none()
            && self.shell.is_none()
            && self.instructions.is_empty()
            && self.permission.is_none()
            && self.agent.is_empty()
            && self.command.is_empty()
    }

    fn document(self) -> Json {
        let mut entries = Vec::new();
        for (key, value) in [
            ("model", self.model),
            ("small_model", self.small_model),
            ("default_agent", self.default_agent),
            ("theme", self.theme),
            ("shell", self.shell),
        ] {
            if let Some(value) = value {
                entries.push((key.to_owned(), Json::String(value)));
            }
        }
        if !self.instructions.is_empty() {
            entries.push((
                "instructions".to_owned(),
                Json::Array(self.instructions.into_iter().map(Json::String).collect()),
            ));
        }
        if let Some(permission) = self.permission {
            entries.push(("permission".to_owned(), permission));
        }
        if !self.agent.is_empty() {
            entries.push(("agent".to_owned(), Json::Object(self.agent)));
        }
        if !self.command.is_empty() {
            entries.push(("command".to_owned(), Json::Object(self.command)));
        }

        Json::Object(entries)
    }
}

/// Maps a whole opencode config, in the order it spells its keys.
fn map_config(source: &Json) -> (Built, Report) {
    let mut built = Built::default();
    let mut report = Report::default();
    let root = At::root();
    let Some(entries) = source.as_object() else {
        return (built, report);
    };

    // `permission` and the legacy `tools` map produce one value between them,
    // so whichever the document spells first folds both; and `mode` is upstream
    // post-processing, applied after the file is read so that a `mode` entry
    // wins over an `agent` of the same name however they were ordered.
    let mut folded = false;
    let mut modes = None;

    for (key, value) in entries {
        let at = root.child(key);
        match key.as_str() {
            "model" => built.model = string(&mut report, &at, value),
            "small_model" => built.small_model = string(&mut report, &at, value),
            "default_agent" => built.default_agent = string(&mut report, &at, value),
            "shell" => built.shell = string(&mut report, &at, value),
            "theme" => {
                built.theme = string(&mut report, &at, value);
                if built.theme.is_some() {
                    report.warn(
                        "`theme` is opencode's legacy top-level key; a current opencode keeps \
                         the theme in tui.json, which this does not read"
                            .to_owned(),
                    );
                }
            }
            "instructions" => built.instructions = instructions(&mut report, &at, value),
            "permission" | "tools" if !folded => {
                folded = true;
                built.permission = permission(
                    &mut report,
                    &root,
                    source.get("tools"),
                    source.get("permission"),
                );
            }
            "permission" | "tools" => {}
            "agent" => {
                for (name, definition) in agents(&mut report, &at, value, false) {
                    insert(&mut built.agent, name, definition);
                }
            }
            "mode" => modes = Some(value),
            "command" => built.command = commands(&mut report, &at, value),
            "provider" => providers(&mut report, &at, value),
            "autoshare" => {
                report.skip(&at.from, reason::UNSUPPORTED);
                if value.as_bool() == Some(true) {
                    report.warn(
                        "`autoshare: true` is upstream's `share: \"auto\"`; ganja shares \
                         nothing, so neither was written"
                            .to_owned(),
                    );
                }
            }
            other => {
                let reason = SKIPPED
                    .iter()
                    .find(|(name, _)| *name == other)
                    .map_or(reason::UNKNOWN, |(_, reason)| *reason);
                report.skip(&at.from, reason);
            }
        }
    }

    if let Some(modes) = modes {
        for (name, definition) in agents(&mut report, &At::renamed("mode", "agent"), modes, true) {
            insert(&mut built.agent, name, definition);
        }
    }

    (built, report)
}

/// A key whose value has to be a string, guarded and reported.
fn string(report: &mut Report, at: &At, value: &Json) -> Option<String> {
    let Some(text) = value.as_str() else {
        report.skip(&at.from, reason::MALFORMED);

        return None;
    };
    let text = guard(report, &at.from, text)?;
    report.map(&at.from, &at.to);

    Some(text)
}

/// A key whose value has to be a boolean.
fn boolean(report: &mut Report, at: &At, value: &Json) -> Option<bool> {
    let Some(flag) = value.as_bool() else {
        report.skip(&at.from, reason::MALFORMED);

        return None;
    };
    report.map(&at.from, &at.to);

    Some(flag)
}

/// A key whose value has to be one of upstream's agent modes.
fn agent_mode(report: &mut Report, at: &At, value: &Json) -> Option<Json> {
    let Some(spelled) = value.as_str().filter(|spelled| MODES.contains(spelled)) else {
        report.skip(&at.from, reason::MALFORMED);

        return None;
    };
    report.map(&at.from, &at.to);

    Some(Json::String(spelled.to_owned()))
}

/// Copies a string, deciding what a `{env:}`/`{file:}` token in it means.
///
/// Never expands one, in either direction: a value that is nothing but a token
/// is left out, because carrying it verbatim would name a model or a path that
/// does not exist, and a value that merely contains one is carried and warned
/// about, because ganja will read it literally.
fn guard(report: &mut Report, key: &str, value: &str) -> Option<String> {
    let found = tokens(value);
    if found.is_empty() {
        return Some(value.to_owned());
    }

    let named = found.join(", ");
    if found.len() == 1 && value.trim() == found[0] {
        report.skip(key, reason::TOKEN);
        report.warn(format!(
            "`{key}` is only {named}, which opencode expands before parsing; ganja expands \
             nothing in a config file, so the key was left out"
        ));

        return None;
    }

    report.warn(format!(
        "`{key}` contains {named}; the value was carried across verbatim, and ganja will read \
         it literally"
    ));

    Some(value.to_owned())
}

/// Every `{env:…}` / `{file:…}` token in `value`, in the order they occur.
fn tokens(value: &str) -> Vec<&str> {
    /// What a token can start with. Upstream matches `\{env:([^}]+)\}` and
    /// `\{file:[^}]+\}`, both non-greedy to the first `}`.
    const OPENERS: [&str; 2] = ["{env:", "{file:"];

    let mut found = Vec::new();
    let mut start = 0;
    while start < value.len() {
        let Some((open, opener)) = OPENERS
            .into_iter()
            .filter_map(|opener| value[start..].find(opener).map(|at| (start + at, opener)))
            .min_by_key(|(at, _)| *at)
        else {
            break;
        };
        let after = open + opener.len();
        let Some(close) = value[after..].find('}') else {
            break;
        };

        found.push(&value[open..=after + close]);
        start = after + close + 1;
    }

    found
}

/// The instruction paths worth carrying.
///
/// Remote instructions are left behind: ganja reads instruction files off the
/// filesystem, and an `http(s)` entry it cannot fetch would be a line of config
/// that silently does nothing.
fn instructions(report: &mut Report, at: &At, value: &Json) -> Vec<String> {
    let Some(elements) = value.as_array() else {
        report.skip(&at.from, reason::MALFORMED);

        return Vec::new();
    };

    let mut kept = Vec::new();
    for (index, element) in elements.iter().enumerate() {
        let entry = at.index(index);
        let Some(text) = element.as_str() else {
            report.skip(&entry.from, reason::MALFORMED);
            continue;
        };
        if text.starts_with("http://") || text.starts_with("https://") {
            report.skip(&entry.from, reason::UNSUPPORTED);
            continue;
        }
        if let Some(text) = guard(report, &entry.from, text) {
            kept.push(text);
        }
    }

    if !kept.is_empty() {
        report.map(&at.from, &at.to);
    }

    kept
}

/// Folds a legacy `tools` map and an explicit `permission` value into the one
/// `permission` value ganja writes.
///
/// Upstream: `permission = mergeDeep(fromTools, permission)` — the derived
/// entries take their positions first and an explicit rule for the same tool
/// wins the value (`config.ts:553-564`, and per agent `agent.ts:69-77`).
/// `write`, `edit` and `patch` all name the edit permission.
fn permission(
    report: &mut Report,
    at: &At,
    tools: Option<&Json>,
    explicit: Option<&Json>,
) -> Option<Json> {
    let target = at.child("permission");
    // A bare action replaces the object rather than merging into it, so a
    // `tools` map beside one contributes nothing at all.
    let replaced = explicit.is_some_and(|value| value.as_str().is_some());
    let claimed: Vec<&str> = explicit
        .and_then(Json::as_object)
        .map(|entries| entries.iter().map(|(tool, _)| tool.as_str()).collect())
        .unwrap_or_default();

    let mut rules: Vec<(String, Json)> = Vec::new();
    if let Some(value) = tools {
        let source = at.child("tools");
        match value.as_object() {
            None => report.skip(&source.from, reason::MALFORMED),
            Some(entries) => {
                for (name, action) in entries {
                    let entry = source.child(name);
                    let Some(allowed) = action.as_bool() else {
                        report.skip(&entry.from, reason::MALFORMED);
                        continue;
                    };
                    let tool = match name.as_str() {
                        "write" | "edit" | "patch" => "edit",
                        other => other,
                    };
                    if replaced || claimed.contains(&tool) {
                        report.skip(&entry.from, reason::OVERRIDDEN);
                        continue;
                    }

                    report.map(&entry.from, &target.child(tool).to);
                    insert(
                        &mut rules,
                        tool.to_owned(),
                        Json::String(if allowed { "allow" } else { "deny" }.to_owned()),
                    );
                }
            }
        }
    }

    match explicit {
        None => (!rules.is_empty()).then_some(Json::Object(rules)),
        Some(Json::String(action)) => {
            let action = guard(report, &target.from, action)?;
            report.map(&target.from, &target.to);

            Some(Json::String(action))
        }
        Some(Json::Object(entries)) => {
            for (tool, rule) in entries {
                let entry = target.child(tool);
                let Some(rule) = guarded(report, &entry, rule) else {
                    continue;
                };

                report.map(&entry.from, &entry.to);
                insert(&mut rules, tool.clone(), rule);
            }

            (!rules.is_empty()).then_some(Json::Object(rules))
        }
        Some(_) => {
            report.skip(&target.from, reason::MALFORMED);

            (!rules.is_empty()).then_some(Json::Object(rules))
        }
    }
}

/// Copies a value that is carried as it stands, dropping the strings inside it
/// that are nothing but a `{env:}`/`{file:}` token.
fn guarded(report: &mut Report, at: &At, value: &Json) -> Option<Json> {
    match value {
        Json::String(text) => guard(report, &at.from, text).map(Json::String),
        Json::Object(entries) => Some(Json::Object(
            entries
                .iter()
                .filter_map(|(key, entry)| {
                    guarded(report, &at.child(key), entry).map(|entry| (key.clone(), entry))
                })
                .collect(),
        )),
        Json::Array(elements) => Some(Json::Array(
            elements
                .iter()
                .enumerate()
                .filter_map(|(index, element)| guarded(report, &at.index(index), element))
                .collect(),
        )),
        other => Some(other.clone()),
    }
}

/// Maps an `agent` (or, folded, a `mode`) object into ganja's agent
/// definitions.
fn agents(report: &mut Report, at: &At, value: &Json, primary: bool) -> Vec<(String, Json)> {
    let Some(entries) = value.as_object() else {
        report.skip(&at.from, reason::MALFORMED);

        return Vec::new();
    };

    let mut definitions = Vec::new();
    for (name, definition) in entries {
        if let Some(definition) = agent(report, &at.child(name), definition, primary) {
            definitions.push((name.clone(), definition));
        }
    }

    definitions
}

/// One agent definition's fields, in the order ganja writes them.
#[derive(Debug, Default)]
struct AgentFields {
    model: Option<Json>,
    prompt: Option<Json>,
    description: Option<Json>,
    mode: Option<Json>,
    hidden: Option<Json>,
    disable: Option<Json>,
    permission: Option<Json>,
}

impl AgentFields {
    fn document(self) -> Option<Json> {
        let entries: Vec<(String, Json)> = [
            ("model", self.model),
            ("prompt", self.prompt),
            ("description", self.description),
            ("mode", self.mode),
            ("hidden", self.hidden),
            ("disable", self.disable),
            ("permission", self.permission),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value)))
        .collect();

        (!entries.is_empty()).then_some(Json::Object(entries))
    }
}

/// Maps one agent definition. `primary` marks a `mode.*` entry, which upstream
/// folds in with `mode: "primary"` whatever the entry itself said.
fn agent(report: &mut Report, at: &At, value: &Json, primary: bool) -> Option<Json> {
    let Some(entries) = value.as_object() else {
        report.skip(&at.from, reason::MALFORMED);

        return None;
    };

    let mut fields = AgentFields::default();
    let mut folded = false;
    for (key, field) in entries {
        let child = at.child(key);
        match key.as_str() {
            "model" => fields.model = string(report, &child, field).map(Json::String),
            "prompt" => fields.prompt = string(report, &child, field).map(Json::String),
            "description" => fields.description = string(report, &child, field).map(Json::String),
            "mode" if primary => report.skip(&child.from, reason::OVERRIDDEN),
            "mode" => fields.mode = agent_mode(report, &child, field),
            "hidden" => fields.hidden = boolean(report, &child, field).map(Json::Bool),
            "disable" => fields.disable = boolean(report, &child, field).map(Json::Bool),
            "permission" | "tools" if !folded => {
                folded = true;
                fields.permission =
                    permission(report, at, value.get("tools"), value.get("permission"));
            }
            "permission" | "tools" => {}
            dropped if DROPPED_AGENT_FIELDS.contains(&dropped) => {
                report.skip(&child.from, reason::UNSUPPORTED);
            }
            _ => report.skip(&child.from, reason::UNKNOWN),
        }
    }

    if primary {
        report.map(&at.from, &at.child("mode").to);
        fields.mode = Some(Json::String(PRIMARY.to_owned()));
    }

    fields.document()
}

/// One command definition's fields, in the order ganja writes them.
#[derive(Debug, Default)]
struct CommandFields {
    template: Option<Json>,
    description: Option<Json>,
    agent: Option<Json>,
    model: Option<Json>,
}

impl CommandFields {
    fn document(self) -> Option<Json> {
        let entries: Vec<(String, Json)> = [
            ("template", self.template),
            ("description", self.description),
            ("agent", self.agent),
            ("model", self.model),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value)))
        .collect();

        (!entries.is_empty()).then_some(Json::Object(entries))
    }
}

fn commands(report: &mut Report, at: &At, value: &Json) -> Vec<(String, Json)> {
    let Some(entries) = value.as_object() else {
        report.skip(&at.from, reason::MALFORMED);

        return Vec::new();
    };

    let mut definitions = Vec::new();
    for (name, definition) in entries {
        if let Some(definition) = command(report, &at.child(name), definition) {
            definitions.push((name.clone(), definition));
        }
    }

    definitions
}

fn command(report: &mut Report, at: &At, value: &Json) -> Option<Json> {
    let Some(entries) = value.as_object() else {
        report.skip(&at.from, reason::MALFORMED);

        return None;
    };

    let mut fields = CommandFields::default();
    for (key, field) in entries {
        let child = at.child(key);
        match key.as_str() {
            "template" => fields.template = string(report, &child, field).map(Json::String),
            "description" => fields.description = string(report, &child, field).map(Json::String),
            "agent" => fields.agent = string(report, &child, field).map(Json::String),
            "model" => fields.model = string(report, &child, field).map(Json::String),
            dropped if DROPPED_COMMAND_FIELDS.contains(&dropped) => {
                report.skip(&child.from, reason::UNSUPPORTED);
            }
            _ => report.skip(&child.from, reason::UNKNOWN),
        }
    }

    if fields.template.is_none() {
        // What a command sends is the whole of it, and ganja's `CommandConfig`
        // requires the field — a command without one would not load back.
        report.skip(&at.from, reason::MALFORMED);

        return None;
    }

    fields.document()
}

/// Reports a `provider` map, which is carried nowhere.
///
/// Ganja sizes and prices models from a compiled-in catalog and takes an
/// endpoint override from the environment, so there is no key here to map to —
/// but an `apiKey` gets a row and a warning of its own, because it is the one
/// thing in an opencode config that must not travel.
fn providers(report: &mut Report, at: &At, value: &Json) {
    let Some(entries) = value.as_object() else {
        report.skip(&at.from, reason::CATALOG);

        return;
    };

    for (id, provider) in entries {
        let entry = at.child(id);
        report.skip(&entry.from, reason::CATALOG);

        if provider
            .get("options")
            .and_then(|options| options.get("apiKey"))
            .is_some()
        {
            let key = entry.child("options").child("apiKey");
            report.skip(&key.from, reason::CREDENTIAL);
            report.warn(format!(
                "`{}` holds an API key; a key is never written into a config file — store it \
                 with `ganja auth login` instead",
                key.from
            ));
        }
    }
}

/// Prints what the import did, in two sections.
///
/// One width across both, so the two read as one table rather than as two that
/// happen to be printed together.
fn print_table(report: &Report) {
    let width = report
        .mapped
        .iter()
        .chain(&report.skipped)
        .map(|(key, _)| key.chars().count())
        .chain(std::iter::once(HEADER.chars().count()))
        .max()
        .unwrap_or_default();

    section("mapped", "GANJA", &report.mapped, width);
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

/// Proves the file about to be written is one ganja can read.
///
/// The mapping builds ganja's shape by hand, so a bug in it — a key `Config`
/// does not have, a value of the wrong type — would surface at the next launch,
/// on a file its owner did not write. Decoding here turns that into an error at
/// the moment it was caused, and makes a dry run mean something.
fn validate(document: &str) -> Result<()> {
    jsonc_parser::parse_to_serde_value::<Option<Config>>(document, &parse_options())
        .map(|_| ())
        .map_err(|error| {
            anyhow!("the imported config is not one ganja can load: {error}\n{document}")
        })
}

/// The opencode config to import, and where it was read from.
struct Sources {
    /// Every file that was read, in merge order.
    paths: Vec<PathBuf>,
    /// Where the search went, for the run that finds nothing: a user whose
    /// config is somewhere else needs to be told where this looked, and the
    /// global directory is the one they cannot guess.
    searched: Vec<String>,
    /// What they merged to.
    config: Json,
}

fn discover(file: Option<PathBuf>, global: bool, cwd: &Path) -> Result<Sources> {
    let mut sources = Sources {
        paths: Vec::new(),
        searched: Vec::new(),
        config: Json::Object(Vec::new()),
    };

    // A named file is the whole import: a caller who said which file to read
    // did not ask what else is lying around.
    if let Some(file) = file {
        if !file.is_file() {
            bail!("{} does not exist", file.display());
        }
        merge_document(&mut sources.config, read(&file)?, false);
        sources.paths.push(file);

        return Ok(sources);
    }

    match config_home() {
        Ok(home) => {
            let directory = home.join(OPENCODE_DIRECTORY);
            for name in GLOBAL_FILES {
                let path = directory.join(name);
                if path.is_file() {
                    merge_document(&mut sources.config, read(&path)?, false);
                    sources.paths.push(path);
                }
            }
            sources.searched.push(directory.display().to_string());
        }
        // Not fatal: there is nowhere for a global config to have been written
        // either, and a project may still have one.
        Err(error) => eprintln!("note: opencode's global config was not looked for: {error:#}"),
    }

    if !global {
        for path in project_files(cwd) {
            merge_document(&mut sources.config, read(&path)?, true);
            sources.paths.push(path);
        }
        sources.searched.push(format!(
            "{} and every directory above it up to the project root",
            cwd.display()
        ));
    }

    Ok(sources)
}

/// Every project-tier file, outermost first so the closest directory wins.
///
/// Mirrors upstream's `ConfigPaths.files`, and `ganja_core::config`'s own walk
/// — which is private to that crate — including the reversal that makes
/// `opencode.jsonc` beat `opencode.json` in one directory.
fn project_files(cwd: &Path) -> Vec<PathBuf> {
    // Canonicalised the way `Project::resolve` canonicalises its root, or the
    // walk would not recognise the root it is meant to stop at.
    let start = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let stop = Project::resolve(cwd).root().to_path_buf();

    let mut found = Vec::new();
    for directory in start.ancestors() {
        found.extend(
            PROJECT_FILES
                .iter()
                .map(|name| directory.join(name))
                .filter(|path| path.is_file()),
        );
        if directory == stop {
            break;
        }
    }
    found.reverse();

    found
}

/// Reads one opencode config file.
fn read(path: &Path) -> Result<Json> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("{} could not be read", path.display()))?;

    parse(&text).map_err(|error| anyhow!("{}: {error}", path.display()))
}

/// Parses one opencode config file's text.
///
/// A file holding nothing, or nothing but comments, is an empty config rather
/// than an error; a file holding something that is not an object is not a
/// config at all.
fn parse(text: &str) -> Result<Json> {
    let parsed = jsonc_parser::parse_to_ast(
        text,
        &jsonc_parser::CollectOptions {
            comments: jsonc_parser::CommentCollectionStrategy::Off,
            tokens: false,
        },
        &parse_options(),
    )?;

    match parsed.value.as_ref().map(Json::from_ast) {
        None | Some(Json::Null) => Ok(Json::Object(Vec::new())),
        Some(object @ Json::Object(_)) => Ok(object),
        Some(_) => bail!("a config file has to hold a JSON object"),
    }
}

/// `$XDG_CONFIG_HOME`, or `~/.config`. The same resolution
/// `ganja_core::config` uses for ganja's own global config, which is what makes
/// the destination this writes the file that build will read.
fn config_home() -> Result<PathBuf> {
    use etcetera::base_strategy::{BaseStrategy as _, Xdg};

    Xdg::new()
        .map(|base| base.config_dir())
        .context("the home directory holding the global config could not be located")
}

/// Where the imported config is written.
fn destination(global: bool, cwd: &Path) -> Result<PathBuf> {
    let directory = if global {
        config_home()?.join(GANJA_DIRECTORY)
    } else {
        Project::resolve(cwd).root().to_path_buf()
    };

    Ok(directory.join(DESTINATION))
}

/// The config file already sitting where `destination` would land, if either
/// name is taken. Both are checked: `ganja.jsonc` would *beat* what this
/// writes, so leaving it in place would make the import look like it did
/// nothing.
fn occupied(destination: &Path) -> Option<PathBuf> {
    let directory = destination.parent()?;

    [DESTINATION_ALTERNATE, DESTINATION]
        .into_iter()
        .map(|name| directory.join(name))
        .find(|path| path.exists())
}

fn write(path: &Path, document: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("{} could not be created", parent.display()))?;
    }

    // `create_new` rather than a second look: the destination was checked
    // before the work began, and between then and now something else could have
    // written it. The refusal has to hold at the moment of writing, not before.
    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            bail!(
                "{} already exists; move it aside and run this again",
                path.display()
            )
        }
        Err(error) => {
            return Err(error).with_context(|| format!("{} could not be written", path.display()));
        }
    };

    file.write_all(document.as_bytes())
        .with_context(|| format!("{} could not be written", path.display()))
}

/// The mapping, exercised on the fixture that carries one of every shape.
#[cfg(test)]
mod tests {
    use super::{
        At, Built, Json, Report, guard, map_config, parse, permission, tokens, validate,
        write_string,
    };

    /// One opencode config holding every shape the mapping has a rule for.
    /// Shared with `tests/import_opencode.rs`, which drives the same file
    /// through the built binary.
    const FIXTURE: &str = include_str!("../tests/fixtures/opencode.jsonc");

    /// The table rows as borrowed pairs, which is the shape the assertions
    /// read in.
    fn rows(rows: &[(String, String)]) -> Vec<(&str, &str)> {
        rows.iter()
            .map(|(left, right)| (left.as_str(), right.as_str()))
            .collect()
    }

    fn imported(text: &str) -> (Built, Report) {
        map_config(&parse(text).expect("the fixture is JSONC"))
    }

    /// The accept criterion: one config in, and the table plus the file it
    /// produces, in full. Written out rather than spot-checked because the
    /// table *is* the command's output — a row that quietly changed shape is a
    /// user being told something different about their config.
    #[test]
    fn the_fixture_maps_agents_commands_permissions_and_leaves_the_rest_named() {
        let (built, report) = imported(FIXTURE);

        assert_eq!(
            rows(&report.mapped),
            vec![
                ("model", "model"),
                ("default_agent", "default_agent"),
                ("shell", "shell"),
                ("theme", "theme"),
                ("instructions", "instructions"),
                // The legacy `tools` map lands in `permission`, with `write`
                // naming the edit permission…
                ("tools.webfetch", "permission.webfetch"),
                // …and the explicit rules below winning the tools they name.
                ("permission.bash", "permission.bash"),
                ("permission.edit", "permission.edit"),
                ("permission.read", "permission.read"),
                ("agent.review.model", "agent.review.model"),
                ("agent.review.description", "agent.review.description"),
                ("agent.review.mode", "agent.review.mode"),
                ("agent.review.tools.edit", "agent.review.permission.edit"),
                (
                    "agent.review.permission.webfetch",
                    "agent.review.permission.webfetch"
                ),
                ("command.release.template", "command.release.template"),
                ("command.release.description", "command.release.description"),
                ("command.release.agent", "command.release.agent"),
                // A `mode` entry becomes an agent that only the user can pick.
                ("mode.ship.prompt", "agent.ship.prompt"),
                ("mode.ship.hidden", "agent.ship.hidden"),
                ("mode.ship", "agent.ship.mode"),
            ]
        );

        assert_eq!(
            rows(&report.skipped),
            vec![
                ("$schema", "unpublished"),
                // Nothing but a token, so carrying it would name a model that
                // does not exist.
                ("small_model", "token"),
                // Ganja has both keys; neither holds what opencode puts in
                // them, so they are refused by name rather than half-mapped.
                ("keybinds", "incompatible"),
                ("tui", "incompatible"),
                ("instructions[1]", "token"),
                ("instructions[3]", "unsupported"),
                ("tools.write", "overridden"),
                ("tools.bash", "overridden"),
                ("agent.review.temperature", "unsupported"),
                ("agent.review.top_p", "unsupported"),
                ("agent.review.steps", "unsupported"),
                ("agent.review.color", "unsupported"),
                ("agent.review.variant", "unsupported"),
                ("agent.review.options", "unsupported"),
                ("command.release.variant", "unsupported"),
                ("command.release.subtask", "unsupported"),
                ("provider.anthropic", "catalog"),
                ("provider.anthropic.options.apiKey", "credential"),
                ("mcp", "unsupported"),
                ("compaction", "deferred"),
                ("autoshare", "unsupported"),
                ("username", "unsupported"),
                ("definitely_not_an_opencode_key", "unknown"),
            ]
        );

        let rendered = built.document().render();
        assert_eq!(
            rendered,
            r#"{
  "model": "anthropic/claude-sonnet-5",
  "default_agent": "plan",
  "theme": "tokyonight",
  "shell": "/bin/zsh",
  "instructions": [
    "AGENTS.md",
    "docs/{env:TEAM}/style.md"
  ],
  "permission": {
    "webfetch": "deny",
    "bash": {
      "git status": "allow",
      "git *": "ask",
      "*": "deny"
    },
    "edit": "ask",
    "read": "allow"
  },
  "agent": {
    "review": {
      "model": "anthropic/claude-haiku-4-5",
      "description": "reads a diff and complains",
      "mode": "subagent",
      "permission": {
        "edit": "deny",
        "webfetch": "allow"
      }
    },
    "ship": {
      "prompt": "You ship what is already green.",
      "mode": "primary",
      "hidden": false
    }
  },
  "command": {
    "release": {
      "template": "cut a release for $ARGUMENTS",
      "description": "tag and push",
      "agent": "build"
    }
  }
}
"#
        );

        validate(&rendered).expect("what the importer writes has to load");
    }

    /// The one value in an opencode config that must never travel. Its own
    /// test, because the assertion is about what is *absent* — and an absence
    /// is exactly what a refactor takes away without noticing.
    #[test]
    fn an_api_key_is_never_written_and_is_pointed_at_the_credential_store() {
        let (built, report) =
            imported(r#"{"provider": {"anthropic": {"options": {"apiKey": "sk-canary-8842"}}}}"#);

        assert!(built.is_empty(), "a provider block maps to nothing");
        assert_eq!(
            rows(&report.skipped),
            vec![
                ("provider.anthropic", "catalog"),
                ("provider.anthropic.options.apiKey", "credential"),
            ]
        );
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(
            report.warnings[0].contains("ganja auth login"),
            "{}",
            report.warnings[0]
        );
        assert!(
            !report.warnings[0].contains("sk-canary-8842"),
            "the warning must not repeat the key: {}",
            report.warnings[0]
        );
    }

    /// Upstream expands these textually before parsing; ganja expands nothing,
    /// so the two cases have to be told apart — a value that *is* a token would
    /// otherwise become a literal `{env:…}` model id, and a value that merely
    /// contains one would vanish.
    #[test]
    fn a_value_that_is_only_a_token_is_left_out_and_one_that_contains_it_is_carried() {
        let mut report = Report::default();

        assert_eq!(guard(&mut report, "model", "{env:MODEL}"), None);
        assert_eq!(guard(&mut report, "shell", " {file:/etc/shell} "), None);
        assert_eq!(
            guard(&mut report, "instructions[0]", "docs/{env:TEAM}/x.md"),
            Some("docs/{env:TEAM}/x.md".to_owned())
        );
        assert_eq!(
            guard(&mut report, "model", "anthropic/claude-sonnet-5"),
            Some("anthropic/claude-sonnet-5".to_owned())
        );

        assert_eq!(
            rows(&report.skipped),
            vec![("model", "token"), ("shell", "token")]
        );
        assert_eq!(report.warnings.len(), 3, "{:?}", report.warnings);
        assert!(report.warnings[0].contains("{env:MODEL}"));
        assert!(report.warnings[2].contains("{env:TEAM}"));
    }

    #[test]
    fn every_token_in_a_value_is_found_and_none_is_invented() {
        let cases: [(&str, Vec<&str>); 6] = [
            ("plain", vec![]),
            ("{env:A}", vec!["{env:A}"]),
            ("{file:./a.md}", vec!["{file:./a.md}"]),
            ("x{env:A}y{file:b}z", vec!["{env:A}", "{file:b}"]),
            // An opener with no close is not a token, and must not eat the rest.
            ("{env:A", vec![]),
            ("${SHELL}", vec![]),
        ];

        for (value, expected) in cases {
            assert_eq!(tokens(value), expected, "scanning {value:?}");
        }
    }

    /// Order is the whole semantics of `permission`: evaluation is
    /// last-match-wins, so a rule that moved is a rule that stopped applying.
    #[test]
    fn the_legacy_tools_map_keeps_its_positions_and_loses_the_tools_named_twice() {
        let source = parse(
            r#"{
              "tools": {"webfetch": false, "patch": true, "bash": true},
              "permission": {"bash": {"git *": "allow"}, "read": "allow"}
            }"#,
        )
        .expect("the fixture is JSONC");
        let mut report = Report::default();

        let permission = permission(
            &mut report,
            &At::root(),
            source.get("tools"),
            source.get("permission"),
        )
        .expect("both halves fold into one value");

        assert_eq!(
            permission,
            Json::Object(vec![
                ("webfetch".to_owned(), Json::String("deny".to_owned())),
                // `patch` names the edit permission, the way upstream folds it.
                ("edit".to_owned(), Json::String("allow".to_owned())),
                (
                    "bash".to_owned(),
                    Json::Object(vec![("git *".to_owned(), Json::String("allow".to_owned()))])
                ),
                ("read".to_owned(), Json::String("allow".to_owned())),
            ])
        );
        assert_eq!(rows(&report.skipped), vec![("tools.bash", "overridden")]);
    }

    /// A bare action replaces everything under it rather than merging into it,
    /// which is upstream's `mergeDeep` refusing to recurse into a string.
    #[test]
    fn a_bare_permission_action_wins_over_the_whole_legacy_tools_map() {
        let (built, report) =
            imported(r#"{"tools": {"bash": true, "webfetch": false}, "permission": "ask"}"#);

        assert_eq!(
            built.permission,
            Some(Json::String("ask".to_owned())),
            "{:?}",
            built.permission
        );
        assert_eq!(
            rows(&report.skipped),
            vec![
                ("tools.bash", "overridden"),
                ("tools.webfetch", "overridden")
            ]
        );
    }

    /// Reading a config nobody wrote for this importer means every unknown key
    /// is a row, never a failure — and a value of the wrong type is a row too,
    /// because refusing the whole file over one line would make the command
    /// useless exactly when it is most wanted.
    #[test]
    fn an_unknown_key_and_a_value_of_the_wrong_type_are_reported_rather_than_fatal() {
        let (built, report) = imported(
            r#"{"model": 42, "shell": "/bin/sh", "sparkles": {"deep": [1]}, "agent": "no"}"#,
        );

        assert_eq!(built.shell.as_deref(), Some("/bin/sh"));
        assert_eq!(rows(&report.mapped), vec![("shell", "shell")]);
        assert_eq!(
            rows(&report.skipped),
            vec![
                ("model", "malformed"),
                ("sparkles", "unknown"),
                ("agent", "malformed"),
            ]
        );
    }

    /// A command with no template could not be loaded back, so it is not
    /// written at all rather than written broken.
    #[test]
    fn a_command_without_a_template_is_not_written() {
        let (built, report) = imported(
            r#"{"command": {"ship": {"description": "no template"}, "cut": {"template": "cut $1"}}}"#,
        );

        assert_eq!(
            built
                .command
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["cut"]
        );
        assert_eq!(rows(&report.skipped), vec![("command.ship", "malformed")]);
    }

    /// A config whose every key is one ganja has no home for writes nothing,
    /// and the rows say why rather than the command claiming success.
    #[test]
    fn a_config_of_nothing_but_skipped_keys_produces_no_file() {
        let (built, report) = imported(r#"{"mcp": {}, "lsp": true, "autoupdate": false}"#);

        assert!(built.is_empty());
        assert!(report.mapped.is_empty(), "{:?}", report.mapped);
        assert_eq!(report.skipped.len(), 3);
    }

    /// Comments and trailing commas are legal in every opencode config file,
    /// whatever its extension says.
    #[test]
    fn comments_and_trailing_commas_are_part_of_the_dialect() {
        let (built, _) = imported(
            r#"{
              // the model this project talks to
              "model": "anthropic/claude-sonnet-5",
              /* and nothing else */
            }"#,
        );

        assert_eq!(built.model.as_deref(), Some("anthropic/claude-sonnet-5"));
    }

    #[test]
    fn a_file_holding_nothing_is_an_empty_config_rather_than_an_error() {
        for text in ["", "   \n  ", "// nothing but a comment\n"] {
            assert_eq!(
                parse(text).expect("an empty config file is legal"),
                Json::Object(Vec::new()),
                "parsing {text:?}"
            );
        }
    }

    #[test]
    fn a_malformed_file_says_where_it_stopped() {
        let error = parse(r#"{"model": }"#).expect_err("a broken config file is fatal");

        assert!(error.to_string().contains("line 1"), "{error}");
    }

    /// The writer is the only thing between a value and a file that has to
    /// parse again, so the characters that would end the literal early get
    /// their own case.
    #[test]
    fn a_written_string_escapes_what_would_break_the_literal() {
        let cases = [
            ("plain", r#""plain""#),
            ("say \"hi\"", r#""say \"hi\"""#),
            (r"back\slash", r#""back\\slash""#),
            ("two\nlines", r#""two\nlines""#),
            ("a\tb", r#""a\tb""#),
            ("bell\u{7}", r#""bell\u0007""#),
            // Beyond the escapes, text is written as itself rather than as
            // `\u` pairs — the file is UTF-8 either way.
            ("ずっと", r#""ずっと""#),
        ];

        for (value, expected) in cases {
            let mut written = String::new();
            write_string(&mut written, value);

            assert_eq!(written, expected, "writing {value:?}");
        }
    }

    /// Everything the importer can emit has to survive the reader that will
    /// pick it up, including the values that carry escapes.
    #[test]
    fn what_the_importer_writes_is_what_ganja_reads() {
        let (built, _) = imported(
            r#"{
              "model": "anthropic/claude-sonnet-5",
              "agent": {"build": {"prompt": "line\none\t\"quoted\"", "disable": false}},
              "permission": {"bash": {"echo \"hi\"": "allow"}},
              "command": {"ship": {"template": "ship $ARGUMENTS"}}
            }"#,
        );

        let rendered = built.document().render();
        validate(&rendered).expect("the escaped values survive the round trip");
    }
}
