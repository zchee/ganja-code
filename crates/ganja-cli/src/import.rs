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
//! Three rules are load-bearing, and none of them is a matter of taste:
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
//! * **Nothing is completed on a config's behalf.** An MCP server or a language
//!   server whose entry does not describe something ganja could start is left
//!   out and named, never given the field it is missing: a fabricated command
//!   would start a program nobody chose. The refusals `ganja_core::config`
//!   makes *after* decoding are therefore made here too — a file this wrote
//!   that the next launch will not read is the failure the round trip exists to
//!   prevent.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write as _},
    net::{Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, anyhow, bail};
use ganja_core::{config::Config, lsp::server::BUILTIN_IDS};
use ganja_permission::Project;

/// Directory opencode keeps its global config in, under the XDG config home.
const OPENCODE_DIRECTORY: &str = "opencode";

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

/// The two shapes an `mcp` entry takes. Both sides spell them the same way and
/// both discriminate on `type`, so the word travels as it stands.
const LOCAL: &str = "local";
/// See [`LOCAL`].
const REMOTE: &str = "remote";

/// upstream's `builtinServerIds` (`v1/config/lsp.ts:24-63`), verbatim.
///
/// Ganja ships two of these — [`BUILTIN_IDS`] — so an entry naming one of the
/// others is written against a definition that is not in this build. What that
/// costs depends on how much the entry was leaning on it: upstream lets such an
/// entry name only a `command` (or only `disabled`) and inherit the extensions
/// and the root strategy, and there is nothing here to inherit them from. An
/// entry that names both its `command` and its `extensions` is leaning on
/// nothing, and travels as what it already is — see [`stands_alone`].
const UPSTREAM_LSP_SERVERS: [&str; 38] = [
    "deno",
    "typescript",
    "vue",
    "eslint",
    "oxlint",
    "biome",
    "gopls",
    "ruby-lsp",
    "ty",
    "pyright",
    "elixir-ls",
    "zls",
    "csharp",
    "razor",
    "fsharp",
    "sourcekit-lsp",
    "rust",
    "clangd",
    "svelte",
    "astro",
    "jdtls",
    "kotlin-ls",
    "yaml-ls",
    "lua-ls",
    "php intelephense",
    "prisma",
    "dart",
    "ocaml-lsp",
    "bash",
    "terraform",
    "texlab",
    "dockerfile",
    "gleam",
    "clojure-lsp",
    "nixd",
    "tinymist",
    "haskell-language-server",
    "julials",
];

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
    /// Ganja would refuse the value at load, so writing it would produce a
    /// config file that does not read back.
    pub const REFUSED: &str = "refused";
}

/// Top-level keys that are carried nowhere, and the one word each is reported
/// with. Everything not here and not handled explicitly is [`reason::UNKNOWN`].
const SKIPPED: [(&str, &str); 23] = [
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
    ("plugin", reason::UNSUPPORTED),
    ("reference", reason::UNSUPPORTED),
    ("references", reason::UNSUPPORTED),
    ("server", reason::UNSUPPORTED),
    ("share", reason::UNSUPPORTED),
    ("skills", reason::UNSUPPORTED),
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
    // Per-agent effort pinning ganja does not have; an effort is a session
    // selection made through the catalog, not an agent's config key.
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
    /// Kept as it was written, digits and all, so a value that is re-emitted
    /// is the one that was read rather than one that went through a float.
    /// Two keys carry a number across — an MCP server's `timeout` and whatever
    /// sits inside an LSP entry's `initialization` — and every other number in
    /// an opencode config exists here to be *reported*.
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

    /// Takes everything `other` collected.
    fn adopt(&mut self, other: Self) {
        self.mapped.extend(other.mapped);
        self.skipped.extend(other.skipped);
        self.warnings.extend(other.warnings);
    }

    /// Takes what `other` has to *say*, and the rows that stay true whatever
    /// happened to the entry.
    ///
    /// For an entry that was refused after its fields had already been read:
    /// each `mapped` row names a key that landed somewhere, and none of them
    /// did, and the entry's own `skipped` row covers everything under it. The
    /// warnings are what explain why.
    ///
    /// A [`reason::CREDENTIAL`] row is the exception, and it is not a
    /// bookkeeping one: it reports that a secret was deliberately **not**
    /// carried, which is as true of an entry that was left out as of one that
    /// was written — and it is the row somebody has to see whatever else the
    /// import did with the entry around it.
    fn adopt_warnings(&mut self, other: Self) {
        self.skipped.extend(
            other
                .skipped
                .into_iter()
                .filter(|(_, reason)| reason == reason::CREDENTIAL),
        );
        self.warnings.extend(other.warnings);
    }
}

/// Why an entry could not be written at all: the word its row carries, and the
/// clause that says what about it was impossible.
type Refusal = (&'static str, String);

/// Settles a whole-or-not-at-all walk: a refusal skips the entry, keeps only
/// the collected rows that must survive one ([`Report::adopt_warnings`]'s
/// rule), and says what was left out and why; a clean walk adopts everything.
/// [`None`] is the refusal case, so a caller writes `settle(...)?;` and
/// builds its document on the next line.
fn settle(report: &mut Report, at: &At, refused: Option<Refusal>, collected: Report) -> Option<()> {
    if let Some((reason, explanation)) = refused {
        report.skip(&at.from, reason);
        report.adopt_warnings(collected);
        report.warn(format!("`{}` was left out: {explanation}", at.from));

        return None;
    }
    report.adopt(collected);

    Some(())
}

/// The `Some` pairs of `pairs`, in field order — the one fold every
/// `document()` below runs, so the emitted key order is always the field
/// order and "present" always means the same thing.
fn present<const N: usize>(pairs: [(&str, Option<Json>); N]) -> Vec<(String, Json)> {
    pairs
        .into_iter()
        .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value)))
        .collect()
}

/// Carries a `command` array across, refused when it names no program —
/// upstream destructures it as `[cmd, ...args]`, so an entry with nothing to
/// run is not a server — with the shared could-not-be-carried clause on a
/// value that would not read.
fn carried_command(collected: &mut Report, child: &At, field: &Json) -> Result<Json, Refusal> {
    match string_array(collected, child, field) {
        Ok(command) if command.is_empty() => Err((
            reason::MALFORMED,
            format!("`{}` names no program", child.from),
        )),
        Ok(command) => Ok(Json::Array(command)),
        Err(reason) => Err((
            reason,
            format!("`{}` could not be carried across", child.from),
        )),
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
    mcp: Vec<(String, Json)>,
    provider: Vec<(String, Json)>,
    lsp: Option<Json>,
    snapshot: Option<bool>,
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
            && self.mcp.is_empty()
            && self.provider.is_empty()
            && self.lsp.is_none()
            && self.snapshot.is_none()
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
        if !self.mcp.is_empty() {
            entries.push(("mcp".to_owned(), Json::Object(self.mcp)));
        }
        if !self.provider.is_empty() {
            entries.push(("provider".to_owned(), Json::Object(self.provider)));
        }
        if let Some(lsp) = self.lsp {
            entries.push(("lsp".to_owned(), lsp));
        }
        if let Some(snapshot) = self.snapshot {
            entries.push(("snapshot".to_owned(), Json::Bool(snapshot)));
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
            "mcp" => built.mcp = mcp(&mut report, &at, value),
            "lsp" => built.lsp = lsp(&mut report, &at, value),
            "snapshot" => built.snapshot = boolean(&mut report, &at, value),
            "provider" => built.provider = providers(&mut report, &at, value),
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

/// A key whose value has to be a positive whole number.
///
/// Guarded here rather than left to [`validate`] because ganja types the one
/// number it reads — an MCP server's `timeout` — as a `NonZeroU64`: a zero, a
/// fraction or a negative would otherwise turn one line of somebody else's
/// config into a failed import instead of a row.
fn positive_integer(report: &mut Report, at: &At, value: &Json) -> Option<Json> {
    let Json::Number(spelled) = value else {
        report.skip(&at.from, reason::MALFORMED);

        return None;
    };
    // Re-emitted as the digits that were read, so the value that lands is the
    // one the source wrote.
    if spelled
        .parse::<u64>()
        .is_ok_and(|milliseconds| milliseconds > 0)
    {
        report.map(&at.from, &at.to);

        return Some(Json::Number(spelled.clone()));
    }

    report.skip(&at.from, reason::MALFORMED);

    None
}

/// A key whose value has to be a map of strings, each value guarded.
///
/// The map itself is carried even when nothing survives inside it: these are
/// variables and headers layered over what a process already has, so an empty
/// one adds nothing, which is exactly what a map whose every value was a token
/// meant.
fn string_map(report: &mut Report, at: &At, value: &Json) -> Option<Json> {
    let Some(entries) = value.as_object() else {
        report.skip(&at.from, reason::MALFORMED);

        return None;
    };

    let mut kept: Vec<(String, Json)> = Vec::new();
    for (key, entry) in entries {
        let child = at.child(key);
        let Some(text) = entry.as_str() else {
            report.skip(&child.from, reason::MALFORMED);
            continue;
        };
        if let Some(text) = guard(report, &child.from, text) {
            insert(&mut kept, key.clone(), Json::String(text));
        }
    }
    report.map(&at.from, &at.to);

    Some(Json::Object(kept))
}

/// An array of strings where every element survives or none does, reported as
/// a [`Refusal`] word when one does not.
///
/// The strictness is the point, and it is where these part company with
/// `instructions`: a command and an extension list are not collections of
/// independent entries. A command missing one of its arguments runs a different
/// program, and an extension list emptied of everything that could be carried
/// is `[]`, which ganja reads as *every* file — the opposite of the narrowing
/// it was. Leaving the whole entry out is the only answer that does not quietly
/// mean something else.
fn string_array(report: &mut Report, at: &At, value: &Json) -> Result<Vec<Json>, &'static str> {
    let Some(elements) = value.as_array() else {
        return Err(reason::MALFORMED);
    };

    let mut carried = Vec::with_capacity(elements.len());
    for (index, element) in elements.iter().enumerate() {
        let entry = at.index(index);
        let text = element.as_str().ok_or(reason::MALFORMED)?;
        let text = guard(report, &entry.from, text).ok_or(reason::TOKEN)?;
        carried.push(Json::String(text));
    }
    report.map(&at.from, &at.to);

    Ok(carried)
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
        let entries = present([
            ("model", self.model),
            ("prompt", self.prompt),
            ("description", self.description),
            ("mode", self.mode),
            ("hidden", self.hidden),
            ("disable", self.disable),
            ("permission", self.permission),
        ]);

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
        let entries = present([
            ("template", self.template),
            ("description", self.description),
            ("agent", self.agent),
            ("model", self.model),
        ]);

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

/// Maps an `mcp` object into ganja's server entries.
fn mcp(report: &mut Report, at: &At, value: &Json) -> Vec<(String, Json)> {
    let Some(entries) = value.as_object() else {
        report.skip(&at.from, reason::MALFORMED);

        return Vec::new();
    };

    let mut servers = Vec::new();
    for (name, entry) in entries {
        if let Some(entry) = mcp_server(report, &at.child(name), entry) {
            insert(&mut servers, name.clone(), entry);
        }
    }

    servers
}

/// One MCP server's fields, in the order ganja writes them.
///
/// One struct for both shapes, because `type` already decides which fields are
/// legal and a field belonging to the other shape is reported the same way an
/// invented one is. The order below reads correctly for either: a local entry
/// leaves the remote fields empty and a remote entry leaves the local ones.
#[derive(Debug, Default)]
struct McpFields {
    command: Option<Json>,
    url: Option<Json>,
    cwd: Option<Json>,
    environment: Option<Json>,
    headers: Option<Json>,
    enabled: Option<Json>,
    timeout: Option<Json>,
}

impl McpFields {
    fn document(self, kind: &str) -> Json {
        let mut entries = vec![("type".to_owned(), Json::String(kind.to_owned()))];
        entries.extend(present([
            ("command", self.command),
            ("url", self.url),
            ("cwd", self.cwd),
            ("environment", self.environment),
            ("headers", self.headers),
            ("enabled", self.enabled),
            ("timeout", self.timeout),
        ]));

        Json::Object(entries)
    }
}

/// Maps one MCP server entry, which is written whole or not at all.
///
/// Whole or not at all is why the fields are read into a report of their own:
/// a `mapped` row under an entry that was then refused would claim a setting is
/// in force that was never written. Only what such a pass had to *say* survives
/// the refusal.
fn mcp_server(report: &mut Report, at: &At, value: &Json) -> Option<Json> {
    let Some(entries) = value.as_object() else {
        report.skip(&at.from, reason::MALFORMED);

        return None;
    };

    // `type` discriminates on both sides, and an entry without one describes no
    // server at all: ganja refuses it by name at load, so carrying it would
    // write a file that does not read back.
    let kind = value.get("type").and_then(Json::as_str).unwrap_or_default();
    if kind != LOCAL && kind != REMOTE {
        report.skip(&at.from, reason::MALFORMED);
        report.warn(format!(
            "`{}` names no `type` ganja knows, so no server was written for it{}",
            at.from,
            if value.get("enabled").and_then(Json::as_bool) == Some(false) {
                "; a stub that only switches a server off has nothing to switch off here"
            } else {
                ""
            }
        ));

        return None;
    }
    let local = kind == LOCAL;

    let mut collected = Report::default();
    let mut fields = McpFields::default();
    let mut refused: Option<Refusal> = None;
    for (key, field) in entries {
        let child = at.child(key);
        match key.as_str() {
            // Carried by the shape itself, and rowed like every other key so
            // that nothing in the source is missing from the table.
            "type" => collected.map(&child.from, &child.to),
            "command" if local => match carried_command(&mut collected, &child, field) {
                Ok(command) => fields.command = Some(command),
                Err(refusal) => refused = Some(refusal),
            },
            "url" if !local => match url(&mut collected, &child, field) {
                Ok(url) => fields.url = Some(url),
                Err(refusal) => refused = Some(refusal),
            },
            "cwd" if local => {
                fields.cwd = string(&mut collected, &child, field).map(Json::String);
            }
            "environment" if local => {
                fields.environment = string_map(&mut collected, &child, field);
            }
            "headers" if !local => fields.headers = string_map(&mut collected, &child, field),
            "enabled" => fields.enabled = boolean(&mut collected, &child, field).map(Json::Bool),
            "timeout" => fields.timeout = positive_integer(&mut collected, &child, field),
            "oauth" if !local => {
                collected.skip(&child.from, reason::UNSUPPORTED);
                collected.warn(format!(
                    "`{}` was left out: ganja does not authenticate itself to an MCP server yet, \
                     so one that wants a token has to be given it through `headers`",
                    child.from
                ));
            }
            _ => collected.skip(&child.from, reason::UNKNOWN),
        }
    }

    // The required field decides whether there is an entry at all, and an
    // absent one is checked after the loop because absence has no key to hang a
    // row on.
    if refused.is_none() {
        let missing = if local {
            fields.command.is_none().then_some("command")
        } else {
            fields.url.is_none().then_some("url")
        };
        if let Some(missing) = missing {
            refused = Some((
                reason::MALFORMED,
                format!("a {kind} server needs a `{missing}`"),
            ));
        }
    }

    settle(report, at, refused, collected)?;

    Some(fields.document(kind))
}

/// A remote server's endpoint.
///
/// Ganja refuses one that is neither `https` nor `http` to loopback, because a
/// remote entry's `headers` are where somebody puts a token — the rule it
/// applies to a provider's base URL, for that reason. Applied here so that what
/// this writes is a file the next launch loads.
///
/// Neither the row nor the warning quotes the value: a URL may carry a
/// credential in its userinfo, and echoing one back is how it reaches a log.
fn url(report: &mut Report, at: &At, value: &Json) -> Result<Json, Refusal> {
    let Some(text) = value.as_str() else {
        return Err((reason::MALFORMED, format!("`{}` is not a URL", at.from)));
    };
    let Some(text) = guard(report, &at.from, text) else {
        return Err((
            reason::TOKEN,
            format!(
                "`{}` is nothing but a token opencode would have expanded",
                at.from
            ),
        ));
    };
    if !reachable_in_the_clear(&text) {
        return Err((
            reason::REFUSED,
            format!(
                "`{}` is not one ganja will send headers to — a remote server has to be reached \
                 over https, or over http to loopback",
                at.from
            ),
        ));
    }
    report.map(&at.from, &at.to);

    Ok(Json::String(text))
}

/// Whether a remote MCP endpoint is one ganja will speak to.
///
/// The conservative half of `ganja_core::config`'s own check, which parses the
/// URL properly and is the authority — this one only decides whether to carry
/// an entry. The asymmetry is what licenses it: answering "no" to a URL that
/// would have been accepted costs a named row a user can act on, where
/// answering "yes" to one that would not costs a config file that does not
/// load. So every spelling this cannot resolve by itself — a host in an
/// encoding, an IPv4 address written short — is a no.
fn reachable_in_the_clear(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };

    match scheme.to_ascii_lowercase().as_str() {
        "https" => true,
        "http" => is_loopback(host(rest)),
        _ => false,
    }
}

/// The host of an authority, without its userinfo or its port.
fn host(rest: &str) -> &str {
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // Userinfo runs to the *last* `@`: a password may hold one.
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);

    // The colons inside a bracketed IPv6 literal are the address, not a port.
    if let Some(address) = authority
        .strip_prefix('[')
        .and_then(|inside| inside.split_once(']'))
        .map(|(address, _)| address)
    {
        return address;
    }

    authority
        .rsplit_once(':')
        .filter(|(_, port)| port.chars().all(|digit| digit.is_ascii_digit()))
        .map_or(authority, |(host, _)| host)
}

/// Whether `host` names this machine.
///
/// Parsed rather than matched as text, for the reason `ganja_core` spells out
/// where it makes the same decision: `127.0.0.1.example.invalid` is a hostname
/// somebody else can register, and every cheap spelling of this check is beaten
/// by one.
fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<Ipv4Addr>()
            .is_ok_and(|address| address.is_loopback())
        || host
            .parse::<Ipv6Addr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Maps an `lsp` value: the boolean, or the map of entries.
fn lsp(report: &mut Report, at: &At, value: &Json) -> Option<Json> {
    match value {
        Json::Bool(enabled) => {
            report.map(&at.from, &at.to);
            if *enabled {
                report.warn(format!(
                    "`lsp: true` starts the language servers this build ships, which are {}; \
                     opencode's list is longer, and anything else has to be written out as an \
                     entry with its own `command` and `extensions`",
                    BUILTIN_IDS.join(" and ")
                ));
            }

            Some(Json::Bool(*enabled))
        }
        Json::Object(entries) => {
            let mut kept = Vec::new();
            let mut absent = Vec::new();
            for (name, entry) in entries {
                let child = at.child(name);
                if !BUILTIN_IDS.contains(&name.as_str())
                    && UPSTREAM_LSP_SERVERS.contains(&name.as_str())
                    && !stands_alone(entry)
                {
                    report.skip(&child.from, reason::UNSUPPORTED);
                    absent.push(name.clone());
                    continue;
                }
                if let Some(entry) = lsp_entry(report, &child, name, entry) {
                    insert(&mut kept, name.clone(), entry);
                }
            }

            if !absent.is_empty() {
                report.warn(format!(
                    "opencode ships a language server definition for {} and ganja ships only {}, \
                     so there is nothing here to inherit a command, an extension list or a root \
                     from; a server named under `lsp` has to bring its own `command` and \
                     `extensions`",
                    absent.join(", "),
                    BUILTIN_IDS.join(" and ")
                ));
            }
            if kept.is_empty() {
                // An empty map is not "no servers": ganja merges a map *over*
                // the servers it ships, so `{}` would switch them on. An absent
                // key is the only spelling that means what a map nothing
                // survived means.
                report.warn(format!(
                    "nothing under `{}` describes a server this build can start, so the key was \
                     not written at all — an absent `lsp` is no language server, where an empty \
                     map would start the built-in ones",
                    at.from
                ));

                return None;
            }

            // No row for the key itself, for `agent`'s reason: a container is
            // covered by the rows its entries carry.
            Some(Json::Object(kept))
        }
        _ => {
            report.skip(&at.from, reason::MALFORMED);

            None
        }
    }
}

/// Whether an entry describes a server without help from a definition this
/// build does not have.
///
/// Both fields decide it, because both are what a custom server in ganja is:
/// the `command` to start, and the `extensions` it is asked about. An entry
/// naming both is not leaning on the builtin its name refers to upstream — it
/// already *is* a whole server description — so it imports under that name and
/// does here what it did there. An entry naming less is leaning, and what it
/// would lean on is not in this build.
///
/// Presence is read off the source rather than off a mapped entry on purpose:
/// this decides only whether the name is a reason to stop, and whether the
/// fields it names are usable is [`lsp_entry`]'s question, answered with a
/// reason of its own.
fn stands_alone(entry: &Json) -> bool {
    entry.get("command").is_some() && entry.get("extensions").is_some()
}

/// One language server entry's fields, in the order ganja writes them.
#[derive(Debug, Default)]
struct LspFields {
    command: Option<Json>,
    extensions: Option<Json>,
    disabled: Option<Json>,
    env: Option<Json>,
    initialization: Option<Json>,
}

impl LspFields {
    fn document(self) -> Json {
        Json::Object(present([
            ("command", self.command),
            ("extensions", self.extensions),
            ("disabled", self.disabled),
            ("env", self.env),
            ("initialization", self.initialization),
        ]))
    }
}

/// Maps one language server entry, whole or not at all — [`mcp_server`]'s rule,
/// for its reason.
///
/// Ganja's own two refusals are made here rather than left to the file: a
/// `command` is required except on a disabled entry, and a server this build
/// ships no definition for has to name its `extensions`. Neither is invented on
/// an entry's behalf — a fabricated command would be a program nobody chose.
fn lsp_entry(report: &mut Report, at: &At, name: &str, value: &Json) -> Option<Json> {
    let Some(entries) = value.as_object() else {
        report.skip(&at.from, reason::MALFORMED);

        return None;
    };

    let mut collected = Report::default();
    let mut fields = LspFields::default();
    let mut refused: Option<Refusal> = None;
    for (key, field) in entries {
        let child = at.child(key);
        match key.as_str() {
            "command" => match carried_command(&mut collected, &child, field) {
                Ok(command) => fields.command = Some(command),
                Err(refusal) => refused = Some(refusal),
            },
            // An empty list is legal and means every file, which is why this
            // one is not refused for being empty the way a command is.
            "extensions" => match string_array(&mut collected, &child, field) {
                Ok(extensions) => fields.extensions = Some(Json::Array(extensions)),
                Err(reason) => {
                    refused = Some((
                        reason,
                        format!("`{}` could not be carried across", child.from),
                    ));
                }
            },
            "disabled" => fields.disabled = boolean(&mut collected, &child, field).map(Json::Bool),
            "env" => fields.env = string_map(&mut collected, &child, field),
            "initialization" => {
                fields.initialization = initialization(&mut collected, &child, field);
            }
            _ => collected.skip(&child.from, reason::UNKNOWN),
        }
    }

    let disabled = fields.disabled == Some(Json::Bool(true));
    if refused.is_none() && !disabled {
        if fields.command.is_none() {
            refused = Some((
                reason::MALFORMED,
                "only a disabled server may leave out its `command`, and nothing here says which \
                 program to start"
                    .to_owned(),
            ));
        } else if fields.extensions.is_none() && !BUILTIN_IDS.contains(&name) {
            refused = Some((
                reason::MALFORMED,
                "a server this build ships no definition for has to name the `extensions` it is \
                 asked about"
                    .to_owned(),
            ));
        }
    }

    settle(report, at, refused, collected)?;

    Some(fields.document())
}

/// The `initializationOptions` a language server is started with, carried as it
/// stands.
///
/// Upstream types it as an object, and ganja answers a `workspace/configuration`
/// request by walking a dotted path into it, so anything that is not an object
/// describes nothing either side could use.
fn initialization(report: &mut Report, at: &At, value: &Json) -> Option<Json> {
    if value.as_object().is_none() {
        report.skip(&at.from, reason::MALFORMED);

        return None;
    }
    let carried = guarded(report, at, value)?;
    report.map(&at.from, &at.to);

    Some(carried)
}

/// Maps a `provider` map, entry by entry, into ganja's own.
///
/// **Partial by construction.** Ganja's `provider` table describes one thing —
/// an endpoint, the wire it speaks, the variable holding its key and the
/// headers it wants — where upstream's block also carries the model catalog,
/// the npm package and the SDK options for a dozen vendors it loads
/// dynamically. So an entry is carried when this build has a wire for it and
/// somewhere to point that wire, and reported by name when it does not. What
/// is never carried is `options.apiKey`, which gets a row and a warning of its
/// own because it is the one thing in an opencode config that must not travel.
///
/// Three refusals, each because writing the entry would produce something that
/// does not work:
///
/// * an id this build already ships — `ganja_core::config` refuses such an
///   entry by name, so writing it would produce a file the next launch will
///   not read;
/// * an `npm` package this build has no wire for, or none at all — the dialect
///   is not derivable, and guessing it is how an Anthropic body reaches a
///   chat-completions server;
/// * no endpoint — upstream takes the default from the SDK it loads, and there
///   is nothing here to take one from for a provider ganja does not ship.
fn providers(report: &mut Report, at: &At, value: &Json) -> Vec<(String, Json)> {
    let Some(entries) = value.as_object() else {
        report.skip(&at.from, reason::CATALOG);

        return Vec::new();
    };

    let mut carried = Vec::new();
    for (id, provider) in entries {
        if let Some(entry) = provider_entry(report, &at.child(id), id, provider) {
            insert(&mut carried, id.clone(), entry);
        }
    }

    carried
}

/// The npm packages whose wire this build has, and what each is in ganja's
/// vocabulary.
///
/// Upstream spells "which wire does this endpoint speak" as the SDK it loads
/// (`provider.<id>.npm`), so this is that spelling translated —
/// `@ai-sdk/openai` included, since that vendor's SDK drives the Responses
/// API and a config-named endpoint speaks it as `openai-responses`. Still
/// deliberately short: a package this table does not name is a wire this
/// build has not got, and a guessed row would quietly send one API's traffic
/// down another's encoder.
const DIALECTS: [(&str, &str); 3] = [
    ("@ai-sdk/openai-compatible", "openai-chat-completions"),
    ("@ai-sdk/openai", "openai-responses"),
    ("@ai-sdk/anthropic", "anthropic-messages"),
];

/// One provider entry's fields, in the order ganja writes them.
#[derive(Debug, Default)]
struct ProviderFields {
    dialect: Option<Json>,
    base_url: Option<Json>,
    headers: Option<Json>,
}

impl ProviderFields {
    fn document(self) -> Json {
        Json::Object(present([
            ("dialect", self.dialect),
            ("base_url", self.base_url),
            ("headers", self.headers),
        ]))
    }
}

/// Maps one provider entry, which is written whole or not at all.
///
/// Whole or not at all for [`mcp_server`]'s reason: a `mapped` row under an
/// entry that was then refused would claim a setting is in force that was
/// never written. Only what such a pass had to *say* survives the refusal —
/// which is what keeps the `apiKey` warning, the one row here that has to
/// reach a person whatever else happened.
///
/// `key_env` is deliberately never derived. Upstream's `options.apiKey` holds
/// the key itself, and there is no honest way to turn a value into the name of
/// a variable holding it; the row and the warning say where to put it instead.
fn provider_entry(report: &mut Report, at: &At, id: &str, value: &Json) -> Option<Json> {
    let Some(entries) = value.as_object() else {
        report.skip(&at.from, reason::MALFORMED);

        return None;
    };

    let mut collected = Report::default();
    let mut fields = ProviderFields::default();
    let mut refused: Option<Refusal> = None;

    if ganja_core::provider::PROVIDERS.contains(&id) {
        refused = Some((
            reason::REFUSED,
            format!(
                "`{}` names a provider ganja already ships, and a `provider` entry for one \
                 is refused at load; point the builtin somewhere else with its own base-URL \
                 variable instead",
                at.from
            ),
        ));
    }

    for (key, field) in entries {
        let child = at.child(key);
        match key.as_str() {
            // The one key that changes name on the way across: upstream says
            // which SDK loads the provider, ganja says which wire it speaks.
            "npm" => {
                let named = At {
                    from: child.from.clone(),
                    to: join(&at.to, "dialect"),
                };
                match dialect(&mut collected, &named, field) {
                    Ok(spelled) => fields.dialect = Some(Json::String(spelled.to_owned())),
                    Err(refusal) => refused = refused.or(Some(refusal)),
                }
            }
            "options" => {
                if let Err(refusal) = options(&mut collected, at, field, &mut fields) {
                    refused = refused.or(Some(refusal));
                }
            }
            // The catalog half of upstream's block: what a provider is called,
            // which models it serves and what they cost. Ganja sizes and
            // prices from a table it compiles in, so there is no key here to
            // map any of it to.
            _ => collected.skip(&child.from, reason::CATALOG),
        }
    }

    if refused.is_none() {
        if fields.dialect.is_none() {
            refused = Some((
                reason::UNSUPPORTED,
                format!(
                    "`{}` names no `npm` package this build has a wire for, so nothing says \
                     which API its endpoint speaks; ganja carries {}",
                    at.from,
                    {
                        // "a, b and c" rather than a bare join: the sentence is
                        // read by a person, and three names glued with two
                        // "and"s stopped reading like one.
                        let mut named: Vec<&str> =
                            DIALECTS.iter().map(|(package, _)| *package).collect();
                        let last = named.pop().expect("the dialect table is never empty");
                        format!("{} and {last}", named.join(", "))
                    }
                ),
            ));
        } else if fields.base_url.is_none() {
            refused = Some((
                reason::UNSUPPORTED,
                format!(
                    "`{}` names no endpoint; opencode takes one from the SDK it loads, and \
                     ganja has no default for a provider it does not ship",
                    at.from
                ),
            ));
        }
    }

    settle(report, at, refused, collected)?;

    Some(fields.document())
}

/// The dialect an entry's `npm` package names.
fn dialect(report: &mut Report, at: &At, value: &Json) -> Result<&'static str, Refusal> {
    let Some(package) = value.as_str() else {
        return Err((
            reason::MALFORMED,
            format!("`{}` is not a package name", at.from),
        ));
    };

    let Some((_, spelled)) = DIALECTS.iter().find(|(named, _)| *named == package) else {
        return Err((
            reason::UNSUPPORTED,
            format!(
                "`{}` loads {package}, which this build has no wire for",
                at.from
            ),
        ));
    };
    report.map(&at.from, &at.to);

    Ok(spelled)
}

/// The `options` object, whose carryable keys land **on the entry**.
///
/// Upstream nests the endpoint and the headers one level deeper than ganja
/// does, so this is a flattening rather than a copy: `options` itself maps to
/// the entry, and each child names the ganja key it becomes. `entry` is
/// therefore the *entry's* position, not the `options` key's — the child paths
/// on both sides are built from it.
///
/// `endpoint` outranks `baseURL` because upstream's own read does
/// (`provider/provider.ts:356`, `options?.endpoint ?? options?.baseURL`).
/// `apiKey` is rowed by the caller, before anything can refuse the entry out
/// from under its warning.
fn options(
    report: &mut Report,
    entry: &At,
    value: &Json,
    fields: &mut ProviderFields,
) -> Result<(), Refusal> {
    let from = join(&entry.from, "options");
    let Some(entries) = value.as_object() else {
        report.skip(&from, reason::MALFORMED);

        return Ok(());
    };
    report.map(&from, &entry.to);

    let mut endpoint: Option<Json> = None;
    let mut base_url: Option<Json> = None;
    // Collected rather than returned at the first failure: the loop still has
    // rows to write after a bad endpoint, and the `apiKey` row is one of them.
    let mut refused: Option<Refusal> = None;
    for (key, field) in entries {
        let at = |ganja: &str| At {
            from: join(&from, key),
            to: join(&entry.to, ganja),
        };
        match key.as_str() {
            // The one value in an opencode config that must never travel. Its
            // row survives the entry being refused — see
            // [`Report::adopt_warnings`] — because "this key was not carried"
            // is true either way, and it is the row somebody has to see.
            "apiKey" => {
                let from = join(&from, key);
                report.skip(&from, reason::CREDENTIAL);
                report.warn(format!(
                    "`{from}` holds an API key; a key is never written into a config file — \
                     store it with `ganja auth login` instead"
                ));
            }
            "endpoint" | "baseURL" => match url(report, &at("base_url"), field) {
                Ok(carried) if key == "endpoint" => endpoint = Some(carried),
                Ok(carried) => base_url = Some(carried),
                Err(refusal) => refused = refused.or(Some(refusal)),
            },
            "headers" => fields.headers = string_map(report, &at("headers"), field),
            _ => report.skip(&join(&from, key), reason::UNSUPPORTED),
        }
    }
    fields.base_url = endpoint.or(base_url);

    refused.map_or(Ok(()), Err)
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
///
/// Decoding alone is not the whole of what the loader will accept: it runs
/// `McpServer::check` over every `mcp` entry after decoding, and an entry
/// that decodes and fails *that* is a file this wrote that the next launch
/// refuses. The mapping above already declines a remote URL nothing may send
/// headers to — by the conservative text-level [`reachable_in_the_clear`],
/// which is what a translator holding a raw `Json` can do — so this is the
/// belt to that suspenders, and the one authority for all three refusals
/// rather than a fourth spelling of them.
fn validate(document: &str) -> Result<()> {
    let config = jsonc_parser::parse_to_serde_value::<Option<Config>>(
        document,
        &ganja_core::config::parse_options(),
    )
    .map_err(|error| anyhow!("the imported config is not one ganja can load: {error}\n{document}"))?
    .unwrap_or_default();

    for (name, server) in &config.mcp {
        server.check(name).map_err(|message| {
            anyhow!("the imported config is not one ganja can load: {message}")
        })?;
    }

    Ok(())
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

    match opencode_config_base() {
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
        &ganja_core::config::parse_options(),
    )?;

    match parsed.value.as_ref().map(Json::from_ast) {
        None | Some(Json::Null) => Ok(Json::Object(Vec::new())),
        Some(object @ Json::Object(_)) => Ok(object),
        Some(_) => bail!("a config file has to hold a JSON object"),
    }
}

/// `$XDG_CONFIG_HOME`, or `~/.config` — where **opencode** keeps its global
/// config. Deliberately not `ganja_core::config::config_home`: that seam
/// resolves where *ganja's* things live and moves with `GANJA_CONFIG_HOME`,
/// while the directory read here is another tool's home, fixed by that tool's
/// own convention.
fn opencode_config_base() -> Result<PathBuf> {
    use etcetera::base_strategy::{BaseStrategy as _, Xdg};

    Xdg::new()
        .map(|base| base.config_dir())
        .context("the home directory holding the global config could not be located")
}

/// Where the imported config is written.
///
/// The global destination is `ganja_core::config::config_home` — the same
/// resolution the next launch reads the global tier through, which is what
/// makes this write a file that build will read, wherever `GANJA_CONFIG_HOME`
/// or a `~/.ganja` has moved it.
fn destination(global: bool, cwd: &Path) -> Result<PathBuf> {
    let directory = if global {
        ganja_core::config::config_home()
            .context("the home directory holding the global config could not be located")?
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
#[path = "import_tests.rs"]
mod tests;
