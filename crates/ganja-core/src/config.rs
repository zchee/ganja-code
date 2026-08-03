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
    path::{Path, PathBuf},
};

use etcetera::base_strategy::{BaseStrategy as _, Xdg};
use serde::{
    Deserialize,
    de::{self, MapAccess, Visitor},
};

use crate::{
    permission::{Action, Rule},
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

/// The pattern that covers every call to a tool, mirroring the private `ANY` in
/// [`crate::permission`]: a rule written as `"bash": "ask"` is a rule about all
/// of `bash`, and that is how it has to be spelled once flattened.
const ANY: &str = "*";

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
    /// A config file was named explicitly and is not there. Discovery treats an
    /// absent file as nothing to merge; an explicit one is a request.
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

/// What one tool's key in a `permission` object said.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RuleSet {
    /// `"bash": "ask"` — one action for every call to the tool.
    All(Action),
    /// `"bash": { "git *": "allow", "*": "ask" }` — an action per pattern, in
    /// the order they were written.
    Patterns(Vec<(String, Action)>),
}

impl RuleSet {
    /// Overlays `other`, replicating upstream's `mergeDeep`: two objects merge
    /// key by key, with a re-specified pattern keeping its original position
    /// and taking the new action, and anything else replacing wholesale.
    fn merge(&mut self, other: &Self) {
        match (self, other) {
            (Self::Patterns(mine), Self::Patterns(theirs)) => {
                for (pattern, action) in theirs {
                    match mine.iter_mut().find(|(name, _)| name == pattern) {
                        Some(slot) => slot.1 = action.clone(),
                        None => mine.push((pattern.clone(), action.clone())),
                    }
                }
            }
            (slot, replacement) => *slot = replacement.clone(),
        }
    }
}

impl<'de> Deserialize<'de> for RuleSet {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// Accepts either spelling a tool's key may take.
        struct Shape;

        impl<'de> Visitor<'de> for Shape {
            type Value = RuleSet;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an action, or an object mapping patterns to actions")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Action::deserialize(de::value::StrDeserializer::new(value)).map(RuleSet::All)
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut patterns = Vec::with_capacity(map.size_hint().unwrap_or_default());
                while let Some((pattern, action)) = map.next_entry::<String, Action>()? {
                    patterns.push((pattern, action));
                }

                Ok(RuleSet::Patterns(patterns))
            }
        }

        deserializer.deserialize_any(Shape)
    }
}

/// The `permission` object of a config file, with its key order intact.
///
/// Order is semantic. Evaluation is last-match-wins ([`crate::permission`]), so
/// which of two rules covering the same call was written second is the whole
/// answer — which is why this is a list rather than a map, and why nothing here
/// ever sorts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PermissionConfig {
    /// One entry per tool key, in the order the document spelled them.
    entries: Vec<(String, RuleSet)>,
    /// Set when the whole value was a bare action rather than an object.
    /// Upstream's merge only recurses when both sides are objects, so a bare
    /// action replaces everything underneath it instead of merging into it.
    scalar: bool,
}

impl PermissionConfig {
    /// The rules this config asks for, flattened and in order.
    #[must_use]
    pub fn rules(&self) -> Vec<Rule> {
        self.entries
            .iter()
            .flat_map(|(tool, set)| match set {
                RuleSet::All(action) => vec![Rule {
                    permission: tool.clone(),
                    pattern: ANY.to_owned(),
                    action: action.clone(),
                }],
                RuleSet::Patterns(patterns) => patterns
                    .iter()
                    .map(|(pattern, action)| Rule {
                        permission: tool.clone(),
                        pattern: pattern.clone(),
                        action: action.clone(),
                    })
                    .collect(),
            })
            .collect()
    }

    /// Whether the config asked for no rules at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Overlays `other`, replicating upstream's `mergeDeep` at both levels: a
    /// re-specified tool keeps its position and merges, a tool that is new is
    /// appended, and a bare action on either side replaces rather than merges.
    fn merge(&mut self, other: &Self) {
        if other.scalar {
            *self = other.clone();
            return;
        }

        for (tool, incoming) in &other.entries {
            match self.entries.iter_mut().find(|(name, _)| name == tool) {
                Some(slot) => slot.1.merge(incoming),
                None => self.entries.push((tool.clone(), incoming.clone())),
            }
        }
    }
}

impl<'de> Deserialize<'de> for PermissionConfig {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// Accepts either spelling the `permission` key may take.
        struct Shape;

        impl<'de> Visitor<'de> for Shape {
            type Value = PermissionConfig;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an action, or an object mapping tools to actions")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                let action = Action::deserialize(de::value::StrDeserializer::new(value))?;

                Ok(PermissionConfig {
                    entries: vec![(ANY.to_owned(), RuleSet::All(action))],
                    scalar: true,
                })
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut entries = Vec::with_capacity(map.size_hint().unwrap_or_default());
                while let Some((tool, set)) = map.next_entry::<String, RuleSet>()? {
                    entries.push((tool, set));
                }

                Ok(PermissionConfig {
                    entries,
                    scalar: false,
                })
            }
        }

        deserializer.deserialize_any(Shape)
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
    /// What the caller decided before any of this was read. Not a config key —
    /// `deny_unknown_fields` would reject one — and above every tier here.
    #[serde(skip)]
    pub overrides: Overrides,
}

impl Config {
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
fn global_files() -> Vec<PathBuf> {
    global_dir().map(|dir| existing(&dir)).unwrap_or_default()
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
    jsonc_parser::parse_to_serde_value::<Option<Config>>(&text, &parse_options())
        .map(|config| Some(config.unwrap_or_default()))
        .map_err(|error| ConfigError::Parse {
            path: path.to_owned(),
            message: error.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use tempfile::TempDir;

    use super::{
        ANY, AgentMode, Config, ConfigError, Overrides, ThemeMode, existing, merge_files,
        project_files, read, split_model,
    };
    use crate::permission::Action;

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
            .entries
            .iter()
            .flat_map(|(tool, set)| match set {
                super::RuleSet::All(action) => vec![(tool.as_str(), ANY, action.clone())],
                super::RuleSet::Patterns(patterns) => patterns
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
                ("bash", "*", Action::Other("deny".to_owned())),
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
                (
                    "bash".to_owned(),
                    "*".to_owned(),
                    Action::Other("deny".to_owned())
                ),
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
              "command": {"ship": {"template": "release $ARGUMENTS", "agent": "build"}}
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
