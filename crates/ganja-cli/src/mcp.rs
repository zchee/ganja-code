//! `ganja mcp add/get/remove` — the config file's `mcp` table, edited from the
//! command line instead of by hand (**D483**).
//!
//! Spec: none. Upstream opencode has no MCP management CLI at all — its `mcp`
//! surface is a listing — so what is ported here is the *shape* of Claude
//! Code's and the Codex CLI's `mcp add`/`get`/`remove`, over ganja's own
//! `mcp` vocabulary (`ganja_core::config::McpLocal`/`McpRemote`), adding no
//! config key. `list` and `login` are untouched beside these three.
//!
//! Three rules decide everything below, and none of them is a matter of
//! taste:
//!
//! * **The file's own bytes survive the edit.** This shipped refusing a
//!   `ganja.jsonc` at the target tier by name, on the reasoning that editing
//!   one meant parsing it and printing it back and so deleting every comment
//!   somebody wrote it for. The premise was wrong, and the field found it
//!   immediately: a commented `ganja.jsonc` is what a person who configures
//!   anything *has*, so refusing it refused the feature exactly where it was
//!   wanted. What the refusal assumed ganja did not have —
//!   an editor that rewrites one property and leaves the rest of the file
//!   alone — was already in the tree, as `jsonc-parser`'s `cst` feature. The
//!   target file is parsed into a concrete syntax tree, one property is
//!   inserted, replaced or removed, and the tree is printed back: comments,
//!   key order, indentation and blank lines everywhere else are the same
//!   bytes they were. A `.jsonc` at the tier is now the *edit target* rather
//!   than a refusal, because it is also the file that **beats** a
//!   `ganja.json` beside it at load — writing the loser would look like the
//!   command did nothing.
//! * **Only the one entry is touched.** The document is never decoded into a
//!   typed `Config`: this build's key set is not the one whoever wrote the
//!   file was working from, and a typed round trip would silently drop every
//!   key that arrived from a newer one. The CST goes further than the
//!   [`serde_json::Value`] this used to read — an untouched key keeps not
//!   just its meaning but its spelling. A file that does not parse refuses
//!   with the parse error and is never overwritten.
//! * **What is written is what the loader would accept.** The entry is
//!   constructed as JSON and then *deserialized into the real
//!   [`McpServer`]* before anything is written, so a shape this build could
//!   not read back is refused at the moment it was asked for rather than at
//!   the next launch. The three refusals the loader makes after decoding — an
//!   empty command, a URL nothing may send headers to, an `output_limit` of
//!   zero — are made here too, by *calling* the one authority for them
//!   ([`McpServer::check`]) rather than by spelling them again: a file this
//!   wrote that the next launch will not read is the failure a writer exists
//!   to prevent, and two spellings of the rule are two things to keep in
//!   step. `import.rs` calls the same method for the same reason. The
//!   document is read
//!   through the loader's own dialect too, so a file only a looser parser
//!   accepts is refused here rather than rewritten into something that still
//!   will not load.
//!
//! Nothing here connects to anything. An entry lands in a file, and the note
//! printed on success says exactly when it takes effect — `/mcp` Reconnect,
//! or the next start.

use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, anyhow, bail};
use clap::Args;
use ganja_core::config::{Config, McpServer};
use ganja_permission::Project;
use jsonc_parser::cst::{CstInputValue, CstObject, CstRootNode};
use serde_json::{Map, Value};
use tempfile::NamedTempFile;

/// What this *creates*. The `.jsonc` spelling is deliberately never created:
/// a file this wrote has no comment in it to justify the name.
const CONFIG_FILE: &str = "ganja.json";

/// The other name ganja reads, and the one that *beats* [`CONFIG_FILE`] in the
/// same directory — so a tier holding it is the file this edits, and the
/// `ganja.json` beside it is the one that would have been ignored.
const CONFIG_ALTERNATE: &str = "ganja.jsonc";

/// The table an entry lives under, in every tier.
const TABLE: &str = "mcp";

/// What `ganja mcp add` was asked to write.
///
/// `--url` and a trailing `-- <cmd>` are the two kinds of server, and clap
/// decides between them: naming both conflicts, naming neither is a missing
/// required argument. Which one was named also decides which of the other
/// flags may appear — `--header` belongs to a remote, `--env` and `--cwd` to
/// a local — so a flag that could never have applied is a typed refusal and
/// not a silently ignored word.
#[derive(Debug, Args)]
pub(crate) struct AddArgs {
    /// What to call it. This is the name `/mcp`, `ganja mcp list` and every
    /// tool it lends (`mcp__<name>__<tool>`) will use.
    pub(crate) name: String,
    /// Write the config home's file instead of this project's.
    #[arg(long)]
    pub(crate) global: bool,
    /// Replace an entry of this name that the target file already holds.
    #[arg(long)]
    pub(crate) force: bool,
    /// A remote server's endpoint. Must be `https`, or `http` to loopback.
    #[arg(
        long,
        value_name = "URL",
        required_unless_present = "command",
        conflicts_with = "command"
    )]
    pub(crate) url: Option<String>,
    /// A header sent with every request to a remote server, `Key=Value`.
    #[arg(long, value_name = "KEY=VALUE", conflicts_with = "command")]
    pub(crate) header: Vec<String>,
    /// Mark a remote server for OAuth: `ganja mcp login <name>` discovers its
    /// authorization server and runs the browser login (the config's
    /// `oauth: {}`). Without this, login refuses the server by name.
    #[arg(long, conflicts_with = "command")]
    pub(crate) oauth: bool,
    /// A variable layered over the ones a local server inherits, `KEY=VALUE`.
    #[arg(long, value_name = "KEY=VALUE", conflicts_with = "url")]
    pub(crate) env: Vec<String>,
    /// Directory a local server runs in; relative to the project root.
    #[arg(long, value_name = "DIR", conflicts_with = "url")]
    pub(crate) cwd: Option<String>,
    /// Milliseconds one request may take. Governs requests, never the connect.
    #[arg(long, value_name = "MS")]
    pub(crate) timeout: Option<u64>,
    /// Bytes one tool result may carry before it is clamped.
    #[arg(long, value_name = "BYTES")]
    pub(crate) output_limit: Option<u64>,
    /// Write it configured but not connected.
    #[arg(long)]
    pub(crate) disabled: bool,
    /// A local server's program and its arguments, after `--`.
    #[arg(last = true, value_name = "CMD")]
    pub(crate) command: Vec<String>,
}

/// What `ganja mcp remove` was asked to delete, and from where.
#[derive(Debug, Args)]
pub(crate) struct RemoveArgs {
    /// The entry's name, as `ganja mcp list` shows it.
    pub(crate) name: String,
    /// Edit the config home's file instead of this project's.
    #[arg(long)]
    pub(crate) global: bool,
}

/// Which file a write lands in — the two tiers this command can edit.
///
/// Not every tier a load reads: `$GANJA_CONFIG` names a file that is a
/// request rather than a place, and the project walk reads ancestors. Those
/// are tiers this *reports* about (see [`origin`]) and never writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    /// `<project root>/ganja.json`.
    Project,
    /// `<config home>/ganja.json`.
    Global,
}

impl Tier {
    /// The tier a `--global` flag asks for.
    fn of(global: bool) -> Self {
        if global { Self::Global } else { Self::Project }
    }

    /// The other one, which a write reports about but never touches.
    fn other(self) -> Self {
        match self {
            Self::Project => Self::Global,
            Self::Global => Self::Project,
        }
    }

    /// What a message calls it.
    fn label(self) -> &'static str {
        match self {
            Self::Project => "this project",
            Self::Global => "the config home",
        }
    }

    /// The directory this tier's files sit in.
    ///
    /// The global directory is [`ganja_core::config::config_home`] — the same
    /// resolution the next launch reads the global tier through, so a write
    /// follows `GANJA_CONFIG_HOME` and a `~/.ganja` wherever they moved it.
    fn directory(self, cwd: &Path) -> Result<PathBuf> {
        match self {
            Self::Project => Ok(Project::resolve(cwd).root().to_path_buf()),
            Self::Global => ganja_core::config::config_home()
                .context("no config home could be resolved, so there is nowhere global to write"),
        }
    }
}

/// Adds one entry to a config file, or replaces one with `--force`.
pub(crate) fn add(args: &AddArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read the working directory")?;
    check_name(&args.name)?;

    let entry = entry(args)?;
    validate(&args.name, &entry)?;

    let tier = Tier::of(args.global);
    let path = writable(tier, &cwd)?;
    let document = document(&path)?;
    let table = table(&document, &path)?;

    let existing = table.get(&args.name);
    if existing.is_some() && !args.force {
        bail!(
            "mcp server \"{}\" is already in {}; pass --force to replace it",
            args.name,
            path.display()
        );
    }
    // Replacing sets the existing property's value rather than removing and
    // appending it, so an entry keeps the position — and the comment above it —
    // that whoever wrote the file gave it.
    let replaced = match existing {
        Some(property) => {
            property.set_value(input(&entry));
            true
        }
        None => {
            table.append(&args.name, input(&entry));
            false
        }
    };
    write(&path, &document)?;

    println!(
        "{} mcp server \"{}\" in {}",
        if replaced { "replaced" } else { "added" },
        args.name,
        path.display()
    );
    shadow(tier, &path, &args.name, &cwd);
    println!("a running session picks this up with `/mcp` → Reconnect, or at its next start");

    Ok(())
}

/// Prints one entry as it resolved, and which file it came from.
pub(crate) fn get(name: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read the working directory")?;
    let config = Config::load(&cwd).context("failed to read the config")?;

    let Some(server) = config.mcp.get(name) else {
        bail!("{}", unknown(name, &config));
    };

    println!("mcp server \"{name}\"");
    for (field, value) in describe(server) {
        println!("  {field:<13}{value}");
    }
    println!("  {:<13}{}", "origin", origin(name, &cwd));

    Ok(())
}

/// Deletes one entry from a config file.
pub(crate) fn remove(args: &RemoveArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read the working directory")?;
    let tier = Tier::of(args.global);
    let path = writable(tier, &cwd)?;
    let document = document(&path)?;
    let table = table(&document, &path)?;

    let Some(property) = table.get(&args.name) else {
        bail!("mcp server \"{}\" is not in {}", args.name, path.display());
    };
    property.remove();
    write(&path, &document)?;

    println!(
        "removed mcp server \"{}\" from {}",
        args.name,
        path.display()
    );
    // Both tiers, and both names within each — a `ganja.json` beside the
    // `ganja.jsonc` this just edited is a file whose entry was already being
    // ignored, and saying nothing about it would read as "it is gone now".
    for file in tier_files(tier, &cwd)
        .into_iter()
        .chain(tier_files(tier.other(), &cwd))
    {
        if file != path && holds(&file, &args.name) {
            println!("still configured in {}", file.display());
        }
    }
    println!(
        "a running session keeps it until its next start; `/mcp` → Reconnect \
         does not remove a connected server"
    );

    Ok(())
}

/// The file a write to `tier` lands in.
///
/// A `ganja.jsonc` at the tier is the answer whenever one is there, because
/// that is the file the *loader* prefers where both names sit in one
/// directory: writing the `ganja.json` beside it would land the entry in the
/// file that loses. Only when there is no `.jsonc` is the `.json` the target,
/// and only then is a file ever created.
fn writable(tier: Tier, cwd: &Path) -> Result<PathBuf> {
    let directory = tier.directory(cwd)?;
    let commented = directory.join(CONFIG_ALTERNATE);
    if commented.is_file() {
        return Ok(commented);
    }

    Ok(directory.join(CONFIG_FILE))
}

/// Reads the target file as a syntax tree, or an empty document when it is
/// absent.
///
/// A concrete syntax tree and deliberately not a `Config`, nor even a
/// [`serde_json::Value`]: this build's key set is not necessarily the one the
/// file was written against, and both of those round trips print the document
/// back from what they understood of it — the typed one dropping unknown keys
/// outright, the value one dropping every comment and every choice of
/// formatting. The tree holds the bytes, so everything this does not touch is
/// returned unchanged rather than re-rendered. A file that does not parse is
/// an error and never an empty document, because the alternative is a write
/// that deletes whatever was in it.
fn document(path: &Path) -> Result<CstRootNode> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("{} could not be read", path.display()));
        }
    };

    CstRootNode::parse(&text, &ganja_core::config::parse_options())
        .map_err(|error| anyhow!("{} could not be parsed: {error}", path.display()))
}

/// The document's `mcp` table, created empty when the file has none.
///
/// Either an `mcp` key or a root holding something that is not an object is
/// refused rather than replaced: whatever it is, it is not this command's to
/// throw away. That is why both steps take the `_or_create` spelling and not
/// the `_or_set` one — the latter overwrites what it finds.
fn table(document: &CstRootNode, path: &Path) -> Result<CstObject> {
    let root = document
        .object_value_or_create()
        .ok_or_else(|| anyhow!("{} has to hold a JSON object", path.display()))?;

    root.object_value_or_create(TABLE)
        .ok_or_else(|| anyhow!("{}'s `{TABLE}` is not an object", path.display()))
}

/// The value as this tree's editor takes it — the one translation between the
/// JSON [`entry`] built and validated and the document it is inserted into.
fn input(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(yes) => CstInputValue::Bool(*yes),
        // Through the number's own text rather than an `f64`: a config may
        // carry an integer no double holds exactly, and this is a writer with
        // no business rounding one.
        Value::Number(number) => CstInputValue::Number(number.to_string()),
        Value::String(text) => CstInputValue::String(text.clone()),
        Value::Array(items) => CstInputValue::Array(items.iter().map(input).collect()),
        Value::Object(map) => CstInputValue::Object(
            map.iter()
                .map(|(key, it)| (key.clone(), input(it)))
                .collect(),
        ),
    }
}

/// Writes `document` to `path`, staged beside it and renamed into place.
///
/// Staged rather than written in place because this rewrites a file that
/// already exists and that the next launch has to be able to read: a write
/// interrupted halfway leaves a truncated config, and a rename within one
/// directory is the one step that cannot. `import.rs` writes with
/// `create_new` for the same reason inverted — it only ever creates.
fn write(path: &Path, document: &CstRootNode) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("{} could not be created", parent.display()))?;
    }

    // The tree prints back exactly what it read, so a file that ended in a
    // newline still does. One is added only where there was none to keep —
    // a document this created, or one somebody saved without a final newline.
    let mut text = document.to_string();
    if !text.ends_with('\n') {
        text.push('\n');
    }

    let staged = stage(path, text.as_bytes())?;
    staged.persist(path).map_err(|error| {
        // `PersistError` hands the staged file back rather than dropping it,
        // so the temporary outlives the failed rename by exactly as long as
        // this closure holds it: letting the error go is what removes the
        // file, and leaving one behind would leave a dotted half-config in a
        // project directory forever.
        anyhow!("{} could not be written: {}", path.display(), error.error)
    })?;

    Ok(())
}

/// Writes `bytes` to a fresh file beside `path`, and hands back the file
/// itself — unnamed by anything the caller has to remember to clean up.
///
/// Staged beside rather than at a fixed name because two `ganja mcp add`
/// processes in one directory would otherwise write the same staging file and
/// one would rename the other's half-written bytes into place. The uniqueness
/// used to be a `create_new` loop over a pid-stamped name, which was correct
/// about collisions and silent about failure: a write that failed returned
/// early and left the staged file where it fell, since only the *rename*
/// arm above ever cleaned up. Tying the temporary's life to a value fixes
/// both halves at once — the file is removed when this is dropped, on every
/// path out of every caller, including the one nobody wrote.
fn stage(path: &Path, bytes: &[u8]) -> Result<NamedTempFile> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));

    // Both sentences name `path` and not the staged file: the temporary's
    // name is this function's business and never something somebody typed,
    // so a person reading the failure is told about the config file they
    // asked to edit.
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
    // owner gave it. Copying the mode across keeps the edit an edit. A file
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

/// Refuses a name that could not be an entry key.
///
/// A separator is the one that matters: nothing here joins a name onto a
/// path, but a name carrying one reads as a path to whoever typed it, and an
/// entry called `a/b` is one they will look for in a file called `b`.
fn check_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        bail!("an mcp server needs a name");
    }
    if name.contains(['/', '\\']) {
        bail!("mcp server \"{name}\" carries a path separator; a name is not a path");
    }

    Ok(())
}

/// Builds the entry's JSON, exactly as it will sit in the file.
///
/// Only what was asked for is written. A key left out is a key the loader
/// fills with the default it documents, and writing that default back would
/// pin today's value into somebody's config file forever.
fn entry(args: &AddArgs) -> Result<Value> {
    let mut object = Map::new();

    if let Some(url) = &args.url {
        object.insert("type".to_owned(), Value::from("remote"));
        object.insert("url".to_owned(), Value::from(url.clone()));
        if !args.header.is_empty() {
            object.insert("headers".to_owned(), pairs(&args.header, "--header")?);
        }
        if args.oauth {
            // The empty object is the whole vocabulary: `oauth: {}` is the
            // config's opt-in marker for discovery + PKCE (D466), and writing
            // any richer shape here would invent keys the loader refuses.
            object.insert("oauth".to_owned(), Value::Object(Map::new()));
        }
    } else {
        object.insert("type".to_owned(), Value::from("local"));
        object.insert(
            "command".to_owned(),
            Value::from(args.command.iter().map(String::as_str).collect::<Vec<_>>()),
        );
        if let Some(cwd) = &args.cwd {
            object.insert("cwd".to_owned(), Value::from(cwd.clone()));
        }
        if !args.env.is_empty() {
            object.insert("environment".to_owned(), pairs(&args.env, "--env")?);
        }
    }
    if args.disabled {
        object.insert("enabled".to_owned(), Value::from(false));
    }
    if let Some(timeout) = args.timeout {
        object.insert("timeout".to_owned(), Value::from(timeout));
    }
    if let Some(limit) = args.output_limit {
        object.insert("output_limit".to_owned(), Value::from(limit));
    }

    Ok(Value::Object(object))
}

/// Turns `KEY=VALUE` words into the object they describe.
///
/// The value keeps every `=` after the first: a header value and an
/// environment value are both allowed to hold one, and splitting on the last
/// would quietly truncate a base64 token.
fn pairs(words: &[String], flag: &str) -> Result<Value> {
    let mut map = Map::new();
    for word in words {
        let Some((key, value)) = word.split_once('=') else {
            bail!("{flag} takes KEY=VALUE; got \"{word}\"");
        };
        if key.trim().is_empty() {
            bail!("{flag} was given a value with no key");
        }
        map.insert(key.to_owned(), Value::from(value));
    }

    Ok(Value::Object(map))
}

/// Refuses an entry the next launch would not read, before anything is
/// written.
///
/// The decoding half is the real config type, so its `deny_unknown_fields`
/// and its `NonZeroU64`s answer for the shape. What follows the decode is
/// [`McpServer::check`] — the loader's own three refusals, *called* rather
/// than repeated: they are what stands between a written entry and a config
/// file that does not load, so a second spelling of them here would be a
/// second thing to keep in step with the loader.
fn validate(name: &str, entry: &Value) -> Result<()> {
    let server: McpServer = serde_json::from_value(entry.clone()).map_err(|error| {
        anyhow!("mcp server \"{name}\" is not one this build could read: {error}")
    })?;

    server.check(name).map_err(|message| anyhow!(message))
}

/// Says so when another config file holds this name too, naming which one
/// wins.
///
/// A warning and not a refusal: two tiers naming one server is how somebody
/// overrides a global entry for one project, and the failure worth preventing
/// is only the silent one — an `add` that appears to do nothing because the
/// file that wins was not the file written.
///
/// Two shapes of that, and they read differently. Across tiers the answer is a
/// *tier*, which is what somebody thinks in when they type `--global`. Within
/// one tier it is a file: `ganja.jsonc` beats `ganja.json` in a directory, and
/// naming the tier would say nothing at all.
fn shadow(written: Tier, path: &Path, name: &str, cwd: &Path) {
    for file in tier_files(written, cwd) {
        if file == path || !holds(&file, name) {
            continue;
        }
        // Only ever the losing name: `writable` targets the `.jsonc` wherever
        // there is one, and that is the one the loader merges last.
        eprintln!(
            "warning: mcp server \"{name}\" is also in {}; {} wins at load",
            file.display(),
            path.display()
        );
    }

    let other = written.other();
    for file in tier_files(other, cwd) {
        if !holds(&file, name) {
            continue;
        }
        // The project tier is merged last, so it wins.
        let winner = if written == Tier::Project {
            written
        } else {
            other
        };
        eprintln!(
            "warning: mcp server \"{name}\" is also in {}; {}'s entry wins at load",
            file.display(),
            winner.label()
        );
    }
}

/// Which of the two files this command writes holds `name`, spelled for a
/// person.
///
/// Derived by reading the files rather than asked of the loader, which merges
/// and does not report where a value came from. Honest about its own reach:
/// a name that resolved from somewhere else — an ancestor directory's config,
/// `$GANJA_CONFIG`, an installed plugin — is reported as exactly that rather
/// than attributed to a file it is not in.
fn origin(name: &str, cwd: &Path) -> String {
    let holders: Vec<PathBuf> = [Tier::Global, Tier::Project]
        .into_iter()
        .flat_map(|tier| tier_files(tier, cwd))
        .filter(|file| holds(file, name))
        .collect();

    match holders.as_slice() {
        [] => "not in either file `ganja mcp add` writes — an ancestor directory's \
                config, $GANJA_CONFIG, or an installed plugin"
            .to_owned(),
        [only] => only.display().to_string(),
        [earlier @ .., last] => format!(
            "{} (overridden by {})",
            earlier
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            last.display()
        ),
    }
}

/// The files at `tier` that exist, in the order a load merges them — so the
/// last one named is the one that wins within the tier.
fn tier_files(tier: Tier, cwd: &Path) -> Vec<PathBuf> {
    let Ok(directory) = tier.directory(cwd) else {
        return Vec::new();
    };

    [CONFIG_FILE, CONFIG_ALTERNATE]
        .into_iter()
        .map(|name| directory.join(name))
        .filter(|path| path.is_file())
        .collect()
}

/// Whether `path` declares an `mcp` entry called `name`.
///
/// Read through the JSONC parser, which reads plain JSON too — the same reason
/// the loader reads both names with one reader. A question about content and
/// nothing more, so it goes through the value parser rather than the tree the
/// write path builds.
fn holds(path: &Path, name: &str) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    let parsed = jsonc_parser::parse_to_serde_value(&text, &jsonc_parser::ParseOptions::default());

    matches!(
        parsed,
        Ok(Some(Value::Object(ref document)))
            if document.get(TABLE).and_then(Value::as_object).is_some_and(|table| table.contains_key(name))
    )
}

/// One entry's fields, in the order somebody reads them.
///
/// A header's *value* never appears. A configured header is where a token
/// goes, and `get` is a command whose output lands in terminal scrollback and
/// in pasted bug reports; the names are what somebody is checking, and they
/// are enough to see that the entry is the one they wrote.
fn describe(server: &McpServer) -> Vec<(&'static str, String)> {
    let mut fields = Vec::new();
    match server {
        McpServer::Local(local) => {
            fields.push(("type", "local".to_owned()));
            fields.push(("command", local.command.join(" ")));
            if let Some(cwd) = &local.cwd {
                fields.push(("cwd", cwd.clone()));
            }
            if !local.environment.is_empty() {
                fields.push(("environment", names(&local.environment)));
            }
            fields.push(("enabled", local.enabled.to_string()));
            fields.push(("timeout", budget(local.timeout.map(|ms| ms.get()))));
            fields.push(("output_limit", budget(local.output_limit)));
        }
        McpServer::Remote(remote) => {
            fields.push(("type", "remote".to_owned()));
            fields.push(("url", remote.url.clone()));
            if !remote.headers.is_empty() {
                fields.push(("headers", names(&remote.headers)));
            }
            if remote.oauth.is_some() {
                fields.push(("oauth", "configured".to_owned()));
            }
            fields.push(("enabled", remote.enabled.to_string()));
            fields.push(("timeout", budget(remote.timeout.map(|ms| ms.get()))));
            fields.push(("output_limit", budget(remote.output_limit)));
        }
    }

    fields
}

/// The keys of a header or environment map, values withheld.
fn names(map: &BTreeMap<String, String>) -> String {
    map.keys().cloned().collect::<Vec<_>>().join(", ")
}

/// A budget the entry set, or the fact that it did not.
///
/// "(default)" rather than the number this build would use: the default is
/// this build's and printing it here would read as something the file says.
fn budget(asked: Option<u64>) -> String {
    asked.map_or_else(|| "(default)".to_owned(), |value| value.to_string())
}

/// The refusal for a name nothing configures, listing what is configured.
fn unknown(name: &str, config: &Config) -> String {
    if config.mcp.is_empty() {
        return format!("mcp server \"{name}\" is not configured, and neither is any other");
    }

    format!(
        "mcp server \"{name}\" is not configured; configured: {}",
        config.mcp.keys().cloned().collect::<Vec<_>>().join(", ")
    )
}

#[cfg(test)]
#[path = "mcp_tests.rs"]
mod tests;
