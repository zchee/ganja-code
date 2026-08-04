//! What a project asks of ganja, read from its config files.
//!
//! Spec: upstream `packages/opencode/src/config/config.ts` and
//! `packages/opencode/src/config/paths.ts`.
//!
//! Two files are read, `ganja.jsonc` and `ganja.json`, both in the dialect
//! upstream accepts everywhere — comments and trailing commas, whatever the
//! extension says. They are found in three places, and later beats earlier:
//!
//! 1. the global directory, `<XDG config>/ganja/`;
//! 2. the one file [`CONFIG_ENV`] names, or the one a flag named;
//! 3. every directory from the working directory up to the project root.
//!
//! Above all of those sit the environment ([`crate::provider::PROVIDER_ENV`],
//! [`crate::provider::MODEL_ENV`], applied in [`crate::provider::select`]) and
//! then the flags a caller puts in [`Overrides`], which is why those two live
//! on the loaded [`Config`] rather than being folded into it: the tier between
//! them is read somewhere else, and a precedence that is spelled out in one
//! place cannot disagree with itself.
//!
//! # What is not here
//!
//! The key set is curated. Every top-level key this build understands is a
//! field of [`Config`]; anything else is a hard error naming the key, because a
//! silently ignored setting is a setting that does not work and says nothing
//! about it. Nested maps — `agent.*`, `permission.*` — stay open, so a config
//! written for a later build still loads.
//!
//! There is deliberately **no** `{env:VAR}` or `{file:path}` substitution. That
//! is upstream's route for putting an API key in a config file, and ganja's
//! credentials travel one path only: the environment, or `auth.json` through
//! [`crate::auth`], held in a `SecretString` end to end. The absence is also
//! what makes a parse error safe to render verbatim — no curated key carries a
//! credential, so quoting the file back at the user cannot leak one.

use std::{
    collections::BTreeMap,
    env, fmt, fs, io,
    num::NonZeroU64,
    path::{Path, PathBuf},
};

use etcetera::base_strategy::{BaseStrategy as _, Xdg};
use serde::{
    Deserialize,
    de::{self, MapAccess, Visitor},
};
use url::Url;

use crate::{
    // The `permission` block parses straight into the types the permission
    // layer evaluates: a config file describes rules, and what a rule *is* is
    // not this module's to say.
    permission::PermissionConfig,
    project::Project,
};

/// Environment variable naming one extra config file to read.
pub const CONFIG_ENV: &str = "GANJA_CONFIG";

/// Directory ganja's global config lives in, under the XDG *config* home.
/// Every other store this crate keeps is state rather than configuration and
/// hangs off the data home instead.
const DIRECTORY: &str = "ganja";

/// The config file names, in the order a directory is probed for them.
///
/// Both are read where both exist. The list is reversed before merging, which
/// is what makes `ganja.jsonc` win over `ganja.json` in the same directory —
/// upstream's `toReversed()`, whose second effect is that the outermost
/// ancestor merges first so the closest directory wins.
const FILES: [&str; 2] = ["ganja.jsonc", "ganja.json"];

/// How a config file is parsed.
///
/// Comments and trailing commas are the JSONC dialect upstream accepts, and are
/// why `.json` is parsed by the same reader. Everything else the crate would
/// tolerate by default — single quotes, hexadecimal numbers, missing commas,
/// unquoted keys — is refused, because a file that loads here and nowhere else
/// is a file that has stopped being JSON.
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

/// A config file could not be used.
///
/// There is no variant for "absent": a config file that is not there is not an
/// error, it is the common case. A file that *is* there and cannot be read or
/// understood is fatal, deliberately — upstream degrades a broken global config
/// to `{}` and this port does not, because a setting that silently stopped
/// applying is worse than a startup that says why.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file exists and is not valid JSONC, or does not describe a config.
    /// The message carries the position the parser stopped at.
    #[error("{}: {message}", path.display())]
    Parse {
        /// File that did not parse.
        path: PathBuf,
        /// What the parser said, including line and column.
        message: String,
    },
    /// The file exists and could not be read.
    #[error("{} could not be read: {source}", path.display())]
    Read {
        /// File that could not be read.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: io::Error,
    },
    /// A config file was named explicitly and is not there (**D26**).
    /// Discovery treats an absent file as nothing to merge; an explicit one is
    /// a request, and answering a request with the defaults would look like it
    /// had been read.
    #[error("{} does not exist", path.display())]
    Missing {
        /// The file that was asked for by name.
        path: PathBuf,
    },
}

/// Which agents an agent definition may be used as.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    /// Only the user may switch to it.
    Primary,
    /// Only the task tool may spawn it.
    Subagent,
    /// Both.
    All,
}

/// Whether a theme renders for a dark or a light terminal.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    /// The dark variant of the selected theme.
    Dark,
    /// The light variant.
    Light,
}

/// One agent, as a config file describes it.
///
/// Plain data: nothing here is resolved, validated against the registry or
/// turned into a runnable agent — that belongs to whoever owns the agent
/// definitions, and this type is what it reads.
///
/// The struct is deliberately **not** `deny_unknown_fields`. Upstream's agent
/// definitions carry keys this port has no use for — `temperature` and `top_p`
/// (a [`ChatRequest`](crate::provider::ChatRequest) has no such fields),
/// `steps` (the agent loop has no step cap on purpose), `variant`, `options`
/// and `color` — and a config that mentions them should load, not fail.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct AgentConfig {
    /// Model this agent asks for, `"provider/model"`.
    pub model: Option<String>,
    /// System prompt, which *replaces* the base prompt rather than adding to
    /// it.
    pub prompt: Option<String>,
    /// One line describing the agent, which the task tool shows the model.
    pub description: Option<String>,
    /// Who may use it.
    pub mode: Option<AgentMode>,
    /// Hidden agents exist but are not offered in a picker.
    pub hidden: Option<bool>,
    /// A disabled agent is removed entirely.
    pub disable: Option<bool>,
    /// Rules layered over the built-in ones for calls this agent makes.
    #[serde(default)]
    pub permission: PermissionConfig,
}

impl AgentConfig {
    /// Overlays `other` onto this definition, field by field.
    fn merge(&mut self, other: Self) {
        overlay(&mut self.model, other.model);
        overlay(&mut self.prompt, other.prompt);
        overlay(&mut self.description, other.description);
        overlay(&mut self.mode, other.mode);
        overlay(&mut self.hidden, other.hidden);
        overlay(&mut self.disable, other.disable);
        self.permission.merge(&other.permission);
    }
}

/// How long a request to an MCP server may take when the entry says nothing:
/// a tool call, in milliseconds.
///
/// Upstream leaves this to the SDK's own default rather than naming it
/// (`mcp/index.ts:661-664` resolves to `undefined`), and the SDK's is 60
/// seconds. Named here because a timeout that only exists inside somebody
/// else's library is a timeout nobody can read.
pub const MCP_CALL_TIMEOUT: u64 = 60_000;

/// The same budget for the `tools/list` a connect makes (`mcp/catalog.ts:39`).
pub const MCP_LIST_TIMEOUT: u64 = 30_000;

/// How long a connect may take, in milliseconds — **fixed**, never the entry's
/// `timeout`.
///
/// Upstream's schema documents `timeout` as "Defaults to 5000" and its code
/// then uses a hard 30 000 for the connect and never consults the config value
/// there (`mcp/index.ts:38`, used at `:286` and `:359`). This is that code, and
/// the doc comment describes what it does.
pub const MCP_CONNECT_TIMEOUT: u64 = 30_000;

/// One MCP server a session may connect to.
///
/// Tagged by `type`, and an entry that carries no `type` is a parse error —
/// upstream skips such an entry with a log line (`mcp/index.ts:510`), which
/// leaves a server silently absent. A config that names a server means to have
/// one.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServer {
    /// A child process spoken to over its stdio.
    Local(McpLocal),
    /// An HTTP endpoint spoken to over streamable HTTP.
    Remote(McpRemote),
}

impl McpServer {
    /// Whether this session should connect to it.
    #[must_use]
    pub fn enabled(&self) -> bool {
        match self {
            Self::Local(local) => local.enabled,
            Self::Remote(remote) => remote.enabled,
        }
    }

    /// Milliseconds one request to this server may take, which the entry may
    /// set and which governs **requests only** — never the connect.
    ///
    /// `fallback` is the default for the kind of request being made:
    /// [`MCP_CALL_TIMEOUT`] for a tool call, [`MCP_LIST_TIMEOUT`] for a
    /// listing.
    #[must_use]
    pub fn timeout(&self, fallback: u64) -> u64 {
        let asked = match self {
            Self::Local(local) => local.timeout,
            Self::Remote(remote) => remote.timeout,
        };

        asked.map_or(fallback, NonZeroU64::get)
    }
}

/// A local MCP server: a command this session runs and talks to over pipes.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct McpLocal {
    /// The program and its arguments, `[cmd, args...]`. Required, and refused
    /// empty: upstream destructures it as `[cmd, ...args]`, and an entry with
    /// nothing to run is not a server.
    pub command: Vec<String>,
    /// Directory the child runs in. A relative path resolves against the
    /// project root.
    pub cwd: Option<String>,
    /// Variables layered over the ones this process already has.
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    /// A disabled server is configured and not connected.
    #[serde(default = "connect_by_default")]
    pub enabled: bool,
    /// Request budget in milliseconds; see [`McpServer::timeout`].
    pub timeout: Option<NonZeroU64>,
}

/// A remote MCP server: an endpoint this session posts to.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct McpRemote {
    /// Where it lives. Refused unless it is `https`, or `http` to loopback —
    /// the rule [`crate::provider`] applies to a base URL, for the same reason:
    /// the `headers` below are where somebody puts a token.
    pub url: String,
    /// Headers sent with every request.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// A disabled server is configured and not connected.
    #[serde(default = "connect_by_default")]
    pub enabled: bool,
    /// Request budget in milliseconds; see [`McpServer::timeout`].
    pub timeout: Option<NonZeroU64>,
}

/// What `enabled` means when an entry does not say: upstream connects unless
/// told not to (`mcp/index.ts:514-517`).
fn connect_by_default() -> bool {
    true
}

/// What the `lsp` key asked for.
///
/// **Absent means no language server at all**, which is why the field holding
/// this is an [`Option`] and why `false` and absent are the same answer:
/// upstream treats a falsy `lsp` as "all LSPs are disabled" (`lsp/lsp.ts:151`),
/// and an agent that starts a language server nobody asked for has taken over
/// somebody's machine to be helpful.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LspConfig {
    /// `true` for the builtins, `false` for none.
    Enabled(bool),
    /// Entries merged over the builtins by name.
    Servers(BTreeMap<String, LspEntry>),
}

impl<'de> Deserialize<'de> for LspConfig {
    /// Hand-written rather than `#[serde(untagged)]`, and for one reason:
    /// `untagged` discards the error every variant produced and reports only
    /// that nothing matched. A config misspelling a key inside an entry would
    /// then fail with "data did not match any variant" and never name the key
    /// — which is exactly the failure this crate refuses everywhere else.
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// Accepts either spelling the `lsp` key may take.
        struct Shape;

        impl<'de> Visitor<'de> for Shape {
            type Value = LspConfig;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("true, false, or a map of language server names to entries")
            }

            fn visit_bool<E: de::Error>(self, enabled: bool) -> Result<Self::Value, E> {
                Ok(LspConfig::Enabled(enabled))
            }

            fn visit_map<M: MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
                BTreeMap::deserialize(de::value::MapAccessDeserializer::new(map))
                    .map(LspConfig::Servers)
            }
        }

        deserializer.deserialize_any(Shape)
    }
}

/// One language server, as a config file describes it.
///
/// `command` is required except on a disabled entry, and that is enforced at
/// load rather than in the type: the two shapes upstream spells as a union
/// (`config/lsp.ts:5-17`) would otherwise need a hand-written `Deserialize`
/// whose only job is to produce a worse error message than the check does.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LspEntry {
    /// The program and its arguments. Replaces a builtin's spawn entirely —
    /// it is not extra arguments for the builtin's binary.
    pub command: Option<Vec<String>>,
    /// Extensions this server is asked about, each with its leading dot. An
    /// empty list matches every file. Absent inherits the builtin's, and a
    /// server with no builtin to inherit from must say.
    pub extensions: Option<Vec<String>>,
    /// Switches a server off. The one legal shape with no `command`.
    #[serde(default)]
    pub disabled: bool,
    /// Variables layered over the ones this process already has.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// `initializationOptions`, and what a `workspace/configuration` request
    /// is answered out of.
    pub initialization: Option<serde_json::Value>,
}

/// One custom command, as a config file describes it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct CommandConfig {
    /// The prompt the command sends, with `$1`..`$N` and `$ARGUMENTS`
    /// placeholders. Required: a command with nothing to send is not a
    /// command.
    pub template: String,
    /// One line describing it, shown in the palette.
    pub description: Option<String>,
    /// Agent the command runs as, when it should not run as the current one.
    pub agent: Option<String>,
    /// Model the command asks for, `"provider/model"`.
    pub model: Option<String>,
}

impl CommandConfig {
    /// Overlays `other` onto this definition, field by field.
    fn merge(&mut self, other: Self) {
        self.template = other.template;
        overlay(&mut self.description, other.description);
        overlay(&mut self.agent, other.agent);
        overlay(&mut self.model, other.model);
    }
}

/// What a caller decided before any config file was read.
///
/// These are the flags a command line carries, and they outrank everything —
/// the files below them and the environment between. Kept as their own type so
/// the tier is visible rather than folded into the merged config, where a later
/// reader could not tell a flag from a file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Overrides {
    /// `--model`, spelled `"provider/model"` like the config key.
    pub model: Option<String>,
    /// `--agent`, naming an agent that must exist.
    pub agent: Option<String>,
    /// An explicit config file, which stands in for [`CONFIG_ENV`]. Its
    /// *contents* merge where the named file merges — between the global tier
    /// and the project tier — because a flag saying which file to read is not
    /// a flag saying what is in it.
    pub config_file: Option<PathBuf>,
}

/// Everything the config files asked for, merged.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Editor schema reference. Read so that writing one is not an error;
    /// nothing consults it.
    #[serde(rename = "$schema")]
    pub schema: Option<String>,
    /// Default model, `"provider/model"`, split on the **first** slash so that
    /// `openrouter/anthropic/claude-3` names the model `anthropic/claude-3`.
    pub model: Option<String>,
    /// Cheaper model for the requests a session makes about itself — titles
    /// and summaries.
    pub small_model: Option<String>,
    /// Agent a session starts on.
    pub default_agent: Option<String>,
    /// Agent definitions, by name.
    #[serde(default)]
    pub agent: BTreeMap<String, AgentConfig>,
    /// Permission rules layered over the built-in ones.
    #[serde(default)]
    pub permission: PermissionConfig,
    /// Extra instruction files, as paths or globs. The one array that
    /// concatenates across tiers rather than being replaced by the closest one.
    #[serde(default)]
    pub instructions: Vec<String>,
    /// Theme by name.
    pub theme: Option<String>,
    /// Which variant of it to render.
    pub theme_mode: Option<ThemeMode>,
    /// Key bindings, by action name. Kept raw: which action names exist is the
    /// frontend's question, and core has no way to answer it.
    #[serde(default)]
    pub keybinds: BTreeMap<String, String>,
    /// Shell the `bash` tool runs commands in.
    pub shell: Option<String>,
    /// Custom commands, by name.
    #[serde(default)]
    pub command: BTreeMap<String, CommandConfig>,
    /// MCP servers this session may connect to, by name. The name is half of
    /// every tool those servers contribute, so it is what a permission rule is
    /// written against.
    #[serde(default)]
    pub mcp: BTreeMap<String, McpServer>,
    /// Language servers this session may run. **Absent is none of them**; see
    /// [`LspConfig`].
    pub lsp: Option<LspConfig>,
    /// Whether this session snapshots the working tree, which is what `/undo`
    /// restores from.
    ///
    /// **Absent is on.** Upstream's check is `snapshot !== false`, so only
    /// writing `false` switches it off — which is also what makes the option
    /// mergeable: a tier that says nothing leaves the tier below it alone,
    /// where a plain `bool` would have every tier assert a default.
    /// [`Config::snapshots_enabled`] is what reads it.
    pub snapshot: Option<bool>,
    /// What the caller decided before any of this was read. Not a config key —
    /// `deny_unknown_fields` would reject one — and above every tier here.
    #[serde(skip)]
    pub overrides: Overrides,
}

impl Config {
    /// Whether this session snapshots the working tree; see
    /// [`Config::snapshot`].
    #[must_use]
    pub fn snapshots_enabled(&self) -> bool {
        self.snapshot != Some(false)
    }

    /// Loads the config for a session working in `cwd`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] for a file that exists and cannot be read or
    /// understood, including one naming a key this build does not have. A file
    /// that is simply absent is not an error.
    pub fn load(cwd: &Path) -> Result<Self, ConfigError> {
        Self::load_with(cwd, &Overrides::default())
    }

    /// Loads the config for a session working in `cwd`, with `overrides`
    /// carried on the result.
    ///
    /// # Errors
    ///
    /// As [`Config::load`].
    pub fn load_with(cwd: &Path, overrides: &Overrides) -> Result<Self, ConfigError> {
        let explicit = explicit_file(overrides);
        if let Some(path) = &explicit {
            // Discovery may find nothing; a file asked for by name is a
            // request, and loading the defaults instead would look like it had
            // been read. Checked before anything else so the complaint is about
            // the file that is missing and not about one that is merely broken.
            if !path.is_file() {
                return Err(ConfigError::Missing { path: path.clone() });
            }
        }

        let mut tiers = global_files();
        tiers.extend(explicit);
        tiers.extend(project_files(cwd));

        let mut config = merge_files(&tiers)?;
        config.overrides = overrides.clone();

        Ok(config)
    }

    /// Overlays `other` onto this config.
    ///
    /// Objects merge, scalars replace, and arrays replace — with one exception
    /// that is upstream's and not a generalisation: `instructions` concatenates
    /// with an order-preserving deduplication, so a project adds to the global
    /// list rather than replacing it.
    fn merge(&mut self, other: Self) {
        overlay(&mut self.schema, other.schema);
        overlay(&mut self.model, other.model);
        overlay(&mut self.small_model, other.small_model);
        overlay(&mut self.default_agent, other.default_agent);
        overlay(&mut self.theme, other.theme);
        overlay(&mut self.theme_mode, other.theme_mode);
        overlay(&mut self.shell, other.shell);
        overlay(&mut self.snapshot, other.snapshot);

        for (name, incoming) in other.agent {
            self.agent.entry(name).or_default().merge(incoming);
        }
        for (name, incoming) in other.command {
            match self.command.get_mut(&name) {
                Some(existing) => existing.merge(incoming),
                None => {
                    self.command.insert(name, incoming);
                }
            }
        }
        self.keybinds.extend(other.keybinds);
        // An entry replaces wholesale rather than merging field by field, as
        // `agent` and `command` do: the two shapes carry different keys, so a
        // closer tier turning a `local` server into a `remote` one would
        // otherwise leave the command it no longer has behind.
        self.mcp.extend(other.mcp);
        merge_lsp(&mut self.lsp, other.lsp);
        self.permission.merge(&other.permission);

        for instruction in other.instructions {
            if !self.instructions.contains(&instruction) {
                self.instructions.push(instruction);
            }
        }
    }
}

/// Reads `paths` in order and merges each onto the one before it.
///
/// # Errors
///
/// Returns the first [`ConfigError`] any of them produced. A path that is not
/// there contributes nothing.
fn merge_files(paths: &[PathBuf]) -> Result<Config, ConfigError> {
    let mut config = Config::default();
    for path in paths {
        if let Some(tier) = read(path)? {
            config.merge(tier);
        }
    }

    Ok(config)
}

/// Overlays one tier's `lsp` onto the tier below it.
///
/// Upstream's `mergeDeep` read at this key: two maps merge entry by entry, and
/// anything else replaces. A closer tier writing `false` therefore switches
/// LSP off outright rather than being quietly merged into a `true` above it,
/// and a project adding one server does not lose the global tier's.
fn merge_lsp(slot: &mut Option<LspConfig>, incoming: Option<LspConfig>) {
    let Some(incoming) = incoming else {
        return;
    };
    match (slot.as_mut(), incoming) {
        (Some(LspConfig::Servers(existing)), LspConfig::Servers(entries)) => {
            existing.extend(entries);
        }
        (_, incoming) => *slot = Some(incoming),
    }
}

/// Replaces `slot` when `incoming` says something.
fn overlay<T>(slot: &mut Option<T>, incoming: Option<T>) {
    if incoming.is_some() {
        *slot = incoming;
    }
}

/// Splits a `"provider/model"` string on its **first** slash.
///
/// Upstream's `parseModel`: everything after the first slash is the model, so
/// `openrouter/anthropic/claude-3` asks openrouter for `anthropic/claude-3`. A
/// string with no slash names a model and no provider.
#[must_use]
pub fn split_model(model: &str) -> (Option<&str>, &str) {
    match model.split_once('/') {
        Some((provider, rest)) => (Some(provider), rest),
        None => (None, model),
    }
}

/// The global config directory, `<XDG config>/ganja`.
///
/// [`None`] when there is no home directory to resolve it against, which is
/// reported once and then behaves like an empty global config — there is
/// nowhere for one to have been written either.
fn global_dir() -> Option<PathBuf> {
    match Xdg::new() {
        Ok(base) => Some(base.config_dir().join(DIRECTORY)),
        Err(error) => {
            tracing::warn!(
                %error,
                "the home directory holding the global config could not be located; \
                 only project config applies"
            );
            None
        }
    }
}

/// The global tier's files, in merge order.
///
/// Reversed out of [`FILES`] order for the same reason the project walk
/// reverses: merging applies later over earlier, so the name that has to win —
/// `ganja.jsonc` where both sit in one directory — must be merged last.
fn global_files() -> Vec<PathBuf> {
    global_dir()
        .map(|dir| {
            let mut files = existing(&dir);
            files.reverse();
            files
        })
        .unwrap_or_default()
}

/// The one file a flag or [`CONFIG_ENV`] named, if either did. The flag wins:
/// a caller that passed `--config` meant that file and not the one already in
/// the environment.
fn explicit_file(overrides: &Overrides) -> Option<PathBuf> {
    overrides.config_file.clone().or_else(|| {
        env::var(CONFIG_ENV)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    })
}

/// Every project-tier file, outermost first so the closest directory wins.
fn project_files(cwd: &Path) -> Vec<PathBuf> {
    // Canonicalised the same way `Project::resolve` canonicalises its root, or
    // the walk would never recognise the root it is supposed to stop at. The
    // ancestor walk terminates at the filesystem root either way, so the worst
    // a path that cannot be canonicalised costs is a longer walk.
    let start = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let stop = Project::resolve(cwd).root().to_path_buf();

    let mut found = Vec::new();
    for directory in start.ancestors() {
        found.extend(existing(directory));
        if directory == stop {
            break;
        }
    }
    found.reverse();

    found
}

/// The config files that exist in `directory`, in [`FILES`] order.
fn existing(directory: &Path) -> Vec<PathBuf> {
    FILES
        .iter()
        .map(|name| directory.join(name))
        .filter(|path| path.is_file())
        .collect()
}

/// Reads and parses one config file, or [`None`] when it is not there.
///
/// Absence is checked by reading rather than by asking first: the file may
/// vanish between the two, and a missing file at this point means the same
/// thing either way.
fn read(path: &Path) -> Result<Option<Config>, ConfigError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };

    // Through `Option` so that an empty file, or one holding nothing but
    // comments, is an empty config rather than a type error about `null`.
    let config = jsonc_parser::parse_to_serde_value::<Option<Config>>(&text, &parse_options())
        .map(Option::unwrap_or_default)
        .map_err(|error| ConfigError::Parse {
            path: path.to_owned(),
            message: error.to_string(),
        })?;

    // Checked per file rather than after the merge, so the complaint names the
    // file that said it. Merging only ever replaces a whole entry, so every
    // entry that survives has been through here.
    check_mcp(&config.mcp).map_err(|message| ConfigError::Parse {
        path: path.to_owned(),
        message,
    })?;
    check_lsp(config.lsp.as_ref()).map_err(|message| ConfigError::Parse {
        path: path.to_owned(),
        message,
    })?;

    Ok(Some(config))
}

/// Refuses an `lsp` entry that describes a server nothing could start.
///
/// Two rules, both upstream's. A `command` is required, because a server is a
/// program and an entry with no program is not one — with the single exception
/// of a `disabled` entry, which is how a builtin is switched off and is the
/// only command-less shape there is. And a server this build ships no
/// definition for must bring its own `extensions`, because there is nothing
/// for it to inherit them from; the message is upstream's, word for word
/// (`v1/config/lsp.ts:62-75`).
///
/// Checked per file for [`check_mcp`]'s reason: the complaint names the file
/// that said it.
fn check_lsp(config: Option<&LspConfig>) -> Result<(), String> {
    let Some(LspConfig::Servers(entries)) = config else {
        return Ok(());
    };

    for (name, entry) in entries {
        if entry.disabled {
            continue;
        }
        if entry.command.is_none() {
            return Err(format!(
                "lsp server \"{name}\" has no command; only a disabled server may leave it out"
            ));
        }
        if entry.extensions.is_none() && !crate::lsp::server::BUILTIN_IDS.contains(&name.as_str()) {
            return Err(format!(
                "lsp server \"{name}\": For custom LSP servers, 'extensions' array is required."
            ));
        }
    }

    Ok(())
}

/// Refuses an MCP entry that describes a server nothing could connect to.
///
/// Two things are decided here rather than at connect time. A `command` with
/// nothing in it is a server with no program, and finding that out one turn
/// later hides it behind a status line. A remote URL that is neither `https`
/// nor loopback is the same refusal [`crate::provider`] makes about a base URL
/// and for the same reason — `headers` is where a token goes, and plain HTTP
/// to somewhere else puts it on the wire in the clear. Literally the same:
/// the predicate is [`crate::provider::reachable_in_the_clear`], and only the
/// message below is this module's.
///
/// Neither message quotes the URL. A remote entry is configuration, and
/// configuration is allowed to carry a credential in its userinfo, so echoing
/// one back is how it reaches a log.
fn check_mcp(servers: &BTreeMap<String, McpServer>) -> Result<(), String> {
    for (name, server) in servers {
        match server {
            McpServer::Local(local) if local.command.is_empty() => {
                return Err(format!("mcp server \"{name}\" has an empty command"));
            }
            McpServer::Local(_) => {}
            McpServer::Remote(remote) => {
                let parsed = Url::parse(&remote.url)
                    .map_err(|error| format!("mcp server \"{name}\" has no valid url: {error}"))?;
                if !crate::provider::reachable_in_the_clear(&parsed) {
                    return Err(format!(
                        "mcp server \"{name}\" must be reached over https, or over http to \
                         loopback; anything else puts its headers on the wire in the clear"
                    ));
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use tempfile::TempDir;

    use super::{
        AgentMode, Config, ConfigError, LspConfig, McpServer, NonZeroU64, Overrides, ThemeMode,
        existing, merge_files, project_files, read, split_model,
    };
    use crate::permission::{Action, RuleSet};

    fn temporary() -> TempDir {
        TempDir::new().expect("a temporary directory is creatable")
    }

    /// Writes `text` to `path`, creating whatever directories it needs.
    fn plant(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("the fixture tree is creatable");
        }
        fs::write(path, text).expect("the fixture file is writable");
    }

    /// `config`'s permission rules as borrowed tuples, which is the shape the
    /// order assertions read in.
    fn flattened(config: &Config) -> Vec<(&str, &str, Action)> {
        config
            .permission
            .entries()
            .iter()
            .flat_map(|(tool, set)| match set {
                RuleSet::All(action) => vec![(tool.as_str(), "*", action.clone())],
                RuleSet::Patterns(patterns) => patterns
                    .iter()
                    .map(|(pattern, action)| (tool.as_str(), pattern.as_str(), action.clone()))
                    .collect(),
            })
            .collect()
    }

    /// Parses `text` as a config file, the way discovery would.
    fn parse(text: &str) -> Result<Config, ConfigError> {
        let directory = temporary();
        let path = directory.path().join("ganja.jsonc");
        plant(&path, text);

        read(&path).map(|config| config.expect("the fixture exists"))
    }

    #[test]
    fn comments_and_trailing_commas_are_part_of_the_dialect() {
        let config = parse(
            r#"{
              // the model this project talks to
              "model": "anthropic/claude-sonnet-5",
              /* and the cheap one */
              "small_model": "anthropic/claude-haiku-4.5",
            }"#,
        )
        .expect("JSONC is what a config file is written in");

        assert_eq!(config.model.as_deref(), Some("anthropic/claude-sonnet-5"));
        assert_eq!(
            config.small_model.as_deref(),
            Some("anthropic/claude-haiku-4.5")
        );
    }

    #[test]
    fn a_file_holding_nothing_is_an_empty_config_rather_than_an_error() {
        for text in ["", "   \n  ", "// nothing but a comment\n"] {
            assert_eq!(
                parse(text).expect("an empty config file is legal"),
                Config::default(),
                "parsing {text:?}"
            );
        }
    }

    #[test]
    fn an_unknown_top_level_key_is_refused_by_name() {
        let error = parse(r#"{"modle": "anthropic/claude-sonnet-5"}"#)
            .expect_err("a misspelled key is a setting that does not work");

        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure, got {error:?}");
        };
        assert!(message.contains("modle"), "{message}");
    }

    /// The one key here whose absence means *yes*. Upstream reads it as
    /// `snapshot !== false`, so a config that never heard of it still snapshots
    /// — which is what makes `/undo` work without anybody configuring it.
    #[test]
    fn snapshots_are_on_until_a_config_says_false() {
        let absent = parse(r#"{"model": "anthropic/claude-sonnet-5"}"#).expect("it parses");
        assert_eq!(absent.snapshot, None);
        assert!(absent.snapshots_enabled());

        let asked = parse(r#"{"snapshot": true}"#).expect("it parses");
        assert_eq!(asked.snapshot, Some(true));
        assert!(asked.snapshots_enabled());

        let refused = parse(r#"{"snapshot": false}"#).expect("it parses");
        assert_eq!(refused.snapshot, Some(false));
        assert!(!refused.snapshots_enabled());
    }

    /// A tier that says nothing about snapshots leaves the tier below it
    /// alone; one that says `false` outranks a `true` above it.
    #[test]
    fn a_closer_tier_decides_snapshots_only_when_it_mentions_them() {
        let mut merged = parse(r#"{"snapshot": true}"#).expect("it parses");
        merged.merge(parse(r#"{"model": "anthropic/claude-sonnet-5"}"#).expect("it parses"));
        assert_eq!(merged.snapshot, Some(true));

        merged.merge(parse(r#"{"snapshot": false}"#).expect("it parses"));
        assert_eq!(merged.snapshot, Some(false));
        assert!(!merged.snapshots_enabled());
    }

    #[test]
    fn an_absent_lsp_key_is_no_language_servers_at_all() {
        let config = parse(r#"{"model": "anthropic/claude-sonnet-5"}"#).expect("it parses");

        assert_eq!(
            config.lsp, None,
            "LSP is opt-in, and this config did not opt in"
        );
    }

    #[test]
    fn the_lsp_key_takes_a_bare_boolean() {
        for (text, expected) in [("true", true), ("false", false)] {
            let config = parse(&format!(r#"{{"lsp": {text}}}"#)).expect("a boolean is a shape");

            assert_eq!(config.lsp, Some(LspConfig::Enabled(expected)), "for {text}");
        }
    }

    #[test]
    fn an_lsp_entry_carries_every_field_it_may_hold() {
        let config = parse(
            r#"{"lsp": {
                "zls": {
                    "command": ["zls", "--enable-debug-log"],
                    "extensions": [".zig", ".zon"],
                    "env": {"ZLS_HOME": "/opt/zls"},
                    "initialization": {"zls": {"enable_build_on_save": true}}
                }
            }}"#,
        )
        .expect("a full entry parses");

        let Some(LspConfig::Servers(entries)) = &config.lsp else {
            panic!("the value is a map of servers");
        };
        let zls = &entries["zls"];
        assert_eq!(
            zls.command.as_deref(),
            Some(["zls".to_owned(), "--enable-debug-log".to_owned()].as_slice())
        );
        assert_eq!(
            zls.extensions.as_deref(),
            Some([".zig".to_owned(), ".zon".to_owned()].as_slice())
        );
        assert!(!zls.disabled);
        assert_eq!(zls.env["ZLS_HOME"], "/opt/zls");
        assert_eq!(
            zls.initialization,
            Some(serde_json::json!({"zls": {"enable_build_on_save": true}}))
        );
    }

    #[test]
    fn disabling_a_builtin_is_the_one_legal_entry_with_no_command() {
        let config = parse(r#"{"lsp": {"rust": {"disabled": true}}}"#)
            .expect("this is how a builtin is switched off");

        let Some(LspConfig::Servers(entries)) = &config.lsp else {
            panic!("the value is a map of servers");
        };
        assert!(entries["rust"].disabled);
        assert_eq!(entries["rust"].command, None);
    }

    #[test]
    fn an_lsp_entry_with_no_command_is_refused_by_name() {
        let error = parse(r#"{"lsp": {"rust": {"extensions": [".rs"]}}}"#)
            .expect_err("a server with no program is not a server");

        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure, got {error:?}");
        };
        assert!(message.contains("rust"), "{message}");
        assert!(message.contains("command"), "{message}");
    }

    #[test]
    fn a_custom_lsp_server_without_extensions_is_refused_in_upstreams_words() {
        let error = parse(r#"{"lsp": {"zls": {"command": ["zls"]}}}"#)
            .expect_err("nothing tells ganja which files zls claims");

        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure, got {error:?}");
        };
        assert!(
            message.contains("For custom LSP servers, 'extensions' array is required."),
            "{message}"
        );
        assert!(
            message.contains("zls"),
            "the message names the entry: {message}"
        );
    }

    #[test]
    fn a_builtin_without_extensions_inherits_them_instead_of_being_refused() {
        let config = parse(r#"{"lsp": {"rust": {"command": ["ra-multiplex"]}}}"#)
            .expect("a builtin has extensions to inherit");

        let Some(LspConfig::Servers(entries)) = &config.lsp else {
            panic!("the value is a map of servers");
        };
        assert_eq!(entries["rust"].extensions, None, "inherited, not written");
    }

    #[test]
    fn an_unknown_field_inside_an_lsp_entry_is_refused_by_name() {
        let error =
            parse(r#"{"lsp": {"rust": {"command": ["x"], "rootMarkers": ["Cargo.toml"]}}}"#)
                .expect_err("upstream has no such key either");

        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure, got {error:?}");
        };
        assert!(message.contains("rootMarkers"), "{message}");
    }

    #[test]
    fn an_mcp_entry_carries_everything_the_two_shapes_hold() {
        let config = parse(
            r#"{"mcp": {
                "fs": {
                    "type": "local",
                    "command": ["bun", "x", "server"],
                    "cwd": "tools",
                    "environment": {"TOKEN": "x"},
                    "timeout": 1234
                },
                "hub": {
                    "type": "remote",
                    "url": "https://mcp.example/mcp",
                    "headers": {"Authorization": "Bearer x"},
                    "enabled": false
                }
            }}"#,
        )
        .expect("both shapes parse");

        let McpServer::Local(local) = &config.mcp["fs"] else {
            panic!("the first entry is local");
        };
        assert_eq!(local.command, ["bun", "x", "server"]);
        assert_eq!(local.cwd.as_deref(), Some("tools"));
        assert_eq!(local.environment["TOKEN"], "x");
        assert!(local.enabled, "an entry that says nothing connects");
        assert_eq!(local.timeout.map(NonZeroU64::get), Some(1234));

        let McpServer::Remote(remote) = &config.mcp["hub"] else {
            panic!("the second entry is remote");
        };
        assert_eq!(remote.url, "https://mcp.example/mcp");
        assert_eq!(remote.headers["Authorization"], "Bearer x");
        assert!(!remote.enabled);
        assert_eq!(remote.timeout, None);
    }

    /// Every one of these is a config that would otherwise have described a
    /// server nothing could reach, silently.
    #[test]
    fn an_mcp_entry_that_describes_no_reachable_server_is_refused_by_name() {
        let cases = [
            // Upstream skips a type-less entry with a log line; a config that
            // names a server means to have one.
            (r#"{"mcp": {"x": {"command": ["a"]}}}"#, "type"),
            // MCP OAuth is not ported, so the key that asks for it fails loud
            // rather than being ignored.
            (
                r#"{"mcp": {"x": {"type": "remote", "url": "https://a.test", "oauth": {}}}}"#,
                "oauth",
            ),
            (r#"{"mcp": {"x": {"type": "local", "command": []}}}"#, "x"),
            (
                r#"{"mcp": {"x": {"type": "remote", "url": "http://mcp.example/mcp"}}}"#,
                "loopback",
            ),
            (
                r#"{"mcp": {"x": {"type": "remote", "url": "not a url"}}}"#,
                "url",
            ),
            // A zero-millisecond budget is not a budget.
            (
                r#"{"mcp": {"x": {"type": "local", "command": ["a"], "timeout": 0}}}"#,
                "0",
            ),
        ];

        for (text, named) in cases {
            let error = parse(text).expect_err(&format!("{text} describes no server"));
            let ConfigError::Parse { message, .. } = &error else {
                panic!("expected a parse failure for {text}, got {error:?}");
            };
            assert!(message.contains(named), "{text}: {message}");
        }
    }

    /// The same rule the provider endpoints obey, and the same reason: a
    /// remote entry's `headers` is where somebody puts a token.
    #[test]
    fn a_remote_server_may_be_plain_http_only_to_loopback() {
        let allowed = [
            "https://mcp.example/mcp",
            "http://127.0.0.1:8000/mcp",
            "http://localhost:8000/mcp",
            "http://[::1]:8000/mcp",
        ];
        for url in allowed {
            let text = format!(r#"{{"mcp": {{"x": {{"type": "remote", "url": "{url}"}}}}}}"#);
            parse(&text).unwrap_or_else(|error| panic!("{url} is reachable: {error}"));
        }

        let refused = [
            // A host that merely contains the address, and a host that merely
            // starts with the name: both belong to whoever registered them.
            "http://127.0.0.1.evil.test/mcp",
            "http://localhost.evil.test/mcp",
            "http://127.0.0.1@evil.test/mcp",
        ];
        for url in refused {
            let text = format!(r#"{{"mcp": {{"x": {{"type": "remote", "url": "{url}"}}}}}}"#);
            parse(&text).expect_err(url);
        }
    }

    /// An entry replaces rather than merging, because the two shapes carry
    /// different keys.
    #[test]
    fn a_closer_tier_replaces_a_whole_mcp_entry() {
        let directory = temporary();
        let outer = directory.path().join("outer.json");
        let inner = directory.path().join("inner.json");
        plant(
            &outer,
            r#"{"mcp": {"x": {"type": "local", "command": ["old"], "cwd": "here"}}}"#,
        );
        plant(
            &inner,
            r#"{"mcp": {"x": {"type": "remote", "url": "https://new.test/mcp"}}}"#,
        );

        let config = merge_files(&[outer, inner]).expect("both tiers parse");
        let McpServer::Remote(remote) = &config.mcp["x"] else {
            panic!("the closer tier decides what the entry is");
        };
        assert_eq!(remote.url, "https://new.test/mcp");
    }

    /// Nested maps stay open on purpose: an agent definition written for a
    /// later build, or for upstream, still loads here.
    #[test]
    fn an_unknown_key_inside_an_agent_is_carried_rather_than_refused() {
        let config = parse(
            r#"{"agent": {"build": {"temperature": 0.2, "steps": 40, "model": "openai/gpt-5.6"}}}"#,
        )
        .expect("an agent definition stays open");

        assert_eq!(
            config.agent["build"].model.as_deref(),
            Some("openai/gpt-5.6")
        );
    }

    #[test]
    fn a_malformed_file_names_itself_and_where_it_stopped() {
        let directory = temporary();
        let path = directory.path().join("ganja.json");
        plant(&path, r#"{"model": }"#);

        let error = read(&path).expect_err("a broken config file is fatal");
        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure, got {error:?}");
        };

        assert!(message.contains("line 1"), "{message}");
        assert!(
            error.to_string().contains(&path.display().to_string()),
            "{error}"
        );
    }

    #[test]
    fn a_config_file_asked_for_by_name_has_to_exist() {
        let directory = temporary();
        let missing = directory.path().join("nowhere.jsonc");
        let overrides = Overrides {
            config_file: Some(missing.clone()),
            ..Overrides::default()
        };

        let error = Config::load_with(directory.path(), &overrides)
            .expect_err("an explicit config file is a request");

        assert!(matches!(error, ConfigError::Missing { path } if path == missing));
    }

    /// The order rules were written in is the order they are evaluated in, and
    /// evaluation is last-match-wins — so a map that sorted its keys would
    /// silently change which rule decides a call.
    #[test]
    fn permission_rules_keep_the_order_they_were_written_in() {
        let config = parse(
            r#"{
              "permission": {
                "webfetch": "allow",
                "bash": { "git status": "allow", "git *": "ask", "*": "deny" },
                "edit": "ask"
              }
            }"#,
        )
        .expect("a permission object is a config key");

        assert_eq!(
            flattened(&config),
            vec![
                ("webfetch", "*", Action::Allow),
                ("bash", "git status", Action::Allow),
                ("bash", "git *", Action::Ask),
                // A rule this build cannot carry out is still a rule: `deny`
                // survives as itself rather than being flattened to `ask`.
                ("bash", "*", Action::Deny),
                ("edit", "*", Action::Ask),
            ]
        );
    }

    #[test]
    fn a_bare_action_covers_every_tool() {
        let config = parse(r#"{"permission": "ask"}"#).expect("a bare action is legal");

        let rules = config.permission.rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].permission, "*");
        assert_eq!(rules[0].pattern, "*");
        assert_eq!(rules[0].action, Action::Ask);
    }

    /// Upstream's `mergeDeep`: a re-specified key keeps its position and takes
    /// the new value, a new key appends, and a value that is not an object
    /// replaces rather than merging.
    #[test]
    fn merging_permissions_keeps_positions_and_adds_what_is_new() {
        let mut base =
            parse(r#"{"permission": {"bash": {"git *": "allow", "*": "ask"}, "edit": "ask"}}"#)
                .expect("the base tier parses");
        let project = parse(
            r#"{"permission": {"bash": {"*": "deny", "cargo *": "allow"}, "webfetch": "allow"}}"#,
        )
        .expect("the project tier parses");

        base.merge(project);

        let rules: Vec<(String, String, Action)> = base
            .permission
            .rules()
            .into_iter()
            .map(|rule| (rule.permission, rule.pattern, rule.action))
            .collect();
        assert_eq!(
            rules,
            vec![
                ("bash".to_owned(), "git *".to_owned(), Action::Allow),
                ("bash".to_owned(), "*".to_owned(), Action::Deny),
                ("bash".to_owned(), "cargo *".to_owned(), Action::Allow),
                ("edit".to_owned(), "*".to_owned(), Action::Ask),
                ("webfetch".to_owned(), "*".to_owned(), Action::Allow),
            ]
        );
    }

    #[test]
    fn a_bare_action_replaces_the_rules_it_is_merged_over() {
        let mut base =
            parse(r#"{"permission": {"bash": "allow", "edit": "allow"}}"#).expect("base parses");
        base.merge(parse(r#"{"permission": "ask"}"#).expect("the override parses"));

        let rules = base.permission.rules();
        assert_eq!(rules.len(), 1, "{rules:?}");
        assert_eq!(rules[0].permission, "*");
        assert_eq!(rules[0].action, Action::Ask);
    }

    #[test]
    fn merging_replaces_scalars_deepens_objects_and_concatenates_instructions() {
        let mut base = parse(
            r#"{
              "model": "anthropic/claude-sonnet-5",
              "theme": "gruvbox",
              "instructions": ["docs/style.md", "docs/shared.md"],
              "agent": {"build": {"model": "openai/gpt-5.6", "description": "builds"}},
              "keybinds": {"app_exit": "ctrl+c"}
            }"#,
        )
        .expect("the base tier parses");
        base.merge(
            parse(
                r#"{
                  "model": "openai/gpt-5.6",
                  "theme_mode": "light",
                  "instructions": ["docs/shared.md", "docs/local.md"],
                  "agent": {"build": {"description": "still builds", "hidden": true}},
                  "keybinds": {"palette_open": "ctrl+p"}
                }"#,
            )
            .expect("the project tier parses"),
        );

        assert_eq!(base.model.as_deref(), Some("openai/gpt-5.6"));
        assert_eq!(
            base.theme.as_deref(),
            Some("gruvbox"),
            "untouched keys stay"
        );
        assert_eq!(base.theme_mode, Some(ThemeMode::Light));
        assert_eq!(
            base.instructions,
            vec!["docs/style.md", "docs/shared.md", "docs/local.md"],
            "instructions concatenate, deduplicated, in order"
        );
        let build = &base.agent["build"];
        assert_eq!(build.model.as_deref(), Some("openai/gpt-5.6"));
        assert_eq!(build.description.as_deref(), Some("still builds"));
        assert_eq!(build.hidden, Some(true));
        assert_eq!(base.keybinds.len(), 2);
    }

    #[test]
    fn the_curated_keys_all_parse() {
        let config = parse(
            r#"{
              "$schema": "https://ganja.invalid/config.json",
              "model": "anthropic/claude-sonnet-5",
              "small_model": "anthropic/claude-haiku-4.5",
              "default_agent": "plan",
              "agent": {"plan": {"mode": "primary", "disable": false}},
              "permission": {"bash": "ask"},
              "instructions": ["AGENTS.md"],
              "theme": "tokyonight",
              "theme_mode": "dark",
              "keybinds": {"agent_cycle": "tab"},
              "shell": "/bin/zsh",
              "command": {"ship": {"template": "release $ARGUMENTS", "agent": "build"}},
              "mcp": {"fs": {"type": "local", "command": ["bun", "x", "mcp-fs"]}}
            }"#,
        )
        .expect("every curated key is a key");

        assert!(config.schema.is_some());
        assert_eq!(config.default_agent.as_deref(), Some("plan"));
        assert_eq!(config.agent["plan"].mode, Some(AgentMode::Primary));
        assert_eq!(config.agent["plan"].disable, Some(false));
        assert_eq!(config.theme_mode, Some(ThemeMode::Dark));
        assert_eq!(config.shell.as_deref(), Some("/bin/zsh"));
        assert_eq!(config.command["ship"].template, "release $ARGUMENTS");
        assert_eq!(config.command["ship"].agent.as_deref(), Some("build"));
        assert!(config.command["ship"].description.is_none());
        assert!(matches!(config.mcp["fs"], McpServer::Local(_)));
    }

    #[test]
    fn a_model_string_splits_on_its_first_slash() {
        let cases = [
            (
                "anthropic/claude-sonnet-5",
                Some("anthropic"),
                "claude-sonnet-5",
            ),
            (
                "openrouter/anthropic/claude-3",
                Some("openrouter"),
                "anthropic/claude-3",
            ),
            ("claude-sonnet-5", None, "claude-sonnet-5"),
        ];

        for (spelled, provider, model) in cases {
            assert_eq!(
                split_model(spelled),
                (provider, model),
                "splitting {spelled}"
            );
        }
    }

    #[test]
    fn a_directory_offers_jsonc_before_json_so_the_reversal_makes_jsonc_win() {
        let directory = temporary();
        plant(&directory.path().join("ganja.json"), "{}");
        plant(&directory.path().join("ganja.jsonc"), "{}");

        let found = existing(directory.path());
        assert_eq!(found.len(), 2);
        assert!(found[0].ends_with("ganja.jsonc"), "{found:?}");
        assert!(found[1].ends_with("ganja.json"), "{found:?}");
    }

    /// Every ancestor up to the project root contributes, outermost first, so
    /// that the closest directory has the last word.
    #[test]
    fn the_project_walk_stacks_from_the_root_down_to_the_working_directory() {
        let directory = temporary();
        let root = directory.path().join("api");
        let nested = root.join("crates").join("core");
        fs::create_dir_all(&nested).expect("the fixture tree is creatable");
        fs::create_dir(root.join(".git")).expect("the fixture repository is creatable");
        plant(&root.join("ganja.json"), "{}");
        plant(&root.join("ganja.jsonc"), "{}");
        plant(&nested.join("ganja.jsonc"), "{}");

        let found = project_files(&nested);
        let names: Vec<String> = found
            .iter()
            .map(|path| {
                let parent = path.parent().expect("a config file has a directory");
                format!(
                    "{}/{}",
                    parent.file_name().unwrap_or_default().to_string_lossy(),
                    path.file_name().unwrap_or_default().to_string_lossy()
                )
            })
            .collect();

        assert_eq!(
            names,
            vec!["api/ganja.json", "api/ganja.jsonc", "core/ganja.jsonc"],
            "root first, and jsonc after json within a directory"
        );
    }

    #[test]
    fn the_walk_stops_at_the_project_root() {
        let directory = temporary();
        let root = directory.path().join("api");
        fs::create_dir_all(&root).expect("the fixture tree is creatable");
        fs::create_dir(root.join(".git")).expect("the fixture repository is creatable");
        plant(&directory.path().join("ganja.jsonc"), "{}");
        plant(&root.join("ganja.jsonc"), "{}");

        let found = project_files(&root);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with(fs::canonicalize(&root).expect("the fixture exists")));
    }

    /// A closer file wins the keys it names and leaves the rest alone, which is
    /// the whole point of stacking them.
    #[test]
    fn the_closest_project_file_wins() {
        let directory = temporary();
        let root = directory.path().join("api");
        let nested = root.join("crates");
        fs::create_dir_all(&nested).expect("the fixture tree is creatable");
        fs::create_dir(root.join(".git")).expect("the fixture repository is creatable");
        plant(
            &root.join("ganja.jsonc"),
            r#"{"model": "anthropic/claude-sonnet-5", "theme": "gruvbox"}"#,
        );
        plant(
            &nested.join("ganja.jsonc"),
            r#"{"model": "openai/gpt-5.6"}"#,
        );

        // The project tier alone, so the machine running the suite cannot
        // contribute a global config of its own. Which tiers stack in which
        // order is `tests/config.rs`'s to prove, where the environment that
        // decides it can be set.
        let config = merge_files(&project_files(&nested)).expect("both tiers parse");

        assert_eq!(config.model.as_deref(), Some("openai/gpt-5.6"));
        assert_eq!(config.theme.as_deref(), Some("gruvbox"));
    }

    #[test]
    fn jsonc_beats_json_in_the_same_directory() {
        let directory = temporary();
        fs::create_dir(directory.path().join(".git")).expect("the fixture repository is creatable");
        plant(
            &directory.path().join("ganja.json"),
            r#"{"model": "anthropic/claude-sonnet-5", "theme": "gruvbox"}"#,
        );
        plant(
            &directory.path().join("ganja.jsonc"),
            r#"{"model": "openai/gpt-5.6"}"#,
        );

        let config = merge_files(&project_files(directory.path())).expect("both files parse");

        assert_eq!(config.model.as_deref(), Some("openai/gpt-5.6"));
        assert_eq!(config.theme.as_deref(), Some("gruvbox"));
    }

    /// Flags travel on the config rather than into it, so that the tier
    /// between them and the files — the environment, read in
    /// [`crate::provider::select`] — still has somewhere to sit.
    #[test]
    fn overrides_travel_on_the_loaded_config() {
        let directory = temporary();
        let overrides = Overrides {
            model: Some("openai/gpt-5.6".to_owned()),
            agent: Some("plan".to_owned()),
            config_file: None,
        };

        let config =
            Config::load_with(directory.path(), &overrides).expect("an empty tree still loads");

        assert_eq!(config.overrides, overrides);
    }
}
