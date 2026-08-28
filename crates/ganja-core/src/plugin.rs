//! Claude Code plugins, installed under ganja's own config home and read
//! back as config contributors.
//!
//! Spec: the `.claude-plugin` plugin and marketplace format, Claude Code
//! 2.1.x (code.claude.com/docs/en/plugins-reference and
//! code.claude.com/docs/en/plugin-marketplaces, read 2026-08-12). Upstream
//! opencode v1.18.22 has no plugin system at all, so the whole surface is a
//! named divergence, **D472** (`claude-plugin-spec`): the manifest and
//! marketplace shapes are Claude's verbatim, and what an installed plugin
//! contributes maps onto the six config surfaces ganja already owns —
//! `hooks`, `mcp`, `skills`, `command`, `agent`, `lsp`.
//!
//! # Foreign files are tolerated; ganja's own names are not
//!
//! Two postures live side by side here, on purpose. `plugin.json` and
//! `marketplace.json` are **Claude's files**, not ganja's config: Claude Code
//! itself ignores manifest keys it does not recognise, so this parser does
//! too, and a manifest that doubles as somebody's `package.json` still loads.
//! What is *not* tolerated is a name that would escape the install store — a
//! plugin or marketplace name carrying a path separator or a `..` walks out
//! of `installed/` the moment it is joined onto a path, so those are refused
//! at parse, before anything touches the disk. The same rule holds for a
//! marketplace entry's relative `source`, which may point only *into* its own
//! marketplace.
//!
//! # Where plugins merge, and how
//!
//! `apply` is called from exactly one point in the config load path
//! (`Config::load_with`, after the file tiers merge), and merges per surface
//! rather than feeding plugins through `Config::merge` as a fourth tier —
//! that merge replaces a closer tier's `hooks` lists per event key, which
//! would silently kill every plugin hook for any user with hooks of their
//! own. The per-surface semantics are **D473** (`plugin-component-merge`),
//! spelled at each merge site below: hooks append, MCP servers and commands
//! arrive under a namespaced key, skills roots concatenate, agents and LSP
//! entries merge per key with the explicit config winning and the collision
//! reported by name.
//!
//! # Component files that do not parse
//!
//! A plugin's component files are the plugin author's, encountered long after
//! the person who installed the plugin stopped watching, so a hooks file that
//! does not parse or an MCP entry naming an unreachable URL is **skipped
//! with a warning naming the plugin and the file** rather than failing the
//! session's startup — the log-and-continue posture every optional surface
//! here takes. Ganja's *own* state file (`plugins.json`) gets the opposite
//! treatment: it is written by this module alone, so one that cannot be read
//! back is a hard error, exactly like a config file that stopped parsing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::{fs, io};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{
    AgentConfig, AgentMode, CommandConfig, Config, HookCommand, HookHandler, HookMatcher,
    LspConfig, LspEntry, McpLocal, McpRemote, McpServer, config_home,
};

/// The directory a plugin (or a marketplace) keeps its own manifest in, at
/// its root — the one directory of the spec that holds metadata rather than
/// components.
const CLAUDE_PLUGIN_DIR: &str = ".claude-plugin";

/// The manifest file inside [`CLAUDE_PLUGIN_DIR`].
const MANIFEST_FILE: &str = "plugin.json";

/// The marketplace catalog inside [`CLAUDE_PLUGIN_DIR`].
const MARKETPLACE_FILE: &str = "marketplace.json";

/// The placeholder Claude's component files spell a plugin's own directory
/// with, substituted here with the installed root before anything is merged.
const PLUGIN_ROOT_VAR: &str = "${CLAUDE_PLUGIN_ROOT}";

/// Where the install store hangs under the config home.
const STORE_DIR: &str = "plugins";

/// The store's own state file: which marketplace each plugin came from and
/// whether it is enabled. Ganja's file, not part of the config surface — the
/// config's curated key set did not grow for this, because everything the
/// load path needs lives here.
const STATE_FILE: &str = "plugins.json";

/// Subdirectory of the store holding one copy of each added marketplace.
const MARKETPLACES_DIR: &str = "marketplaces";

/// Subdirectory of the store holding one copy of each installed plugin.
const INSTALLED_DIR: &str = "installed";

/// Something about a plugin, a marketplace, or the store could not be done.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// A manifest, marketplace file, or the store's own state file exists and
    /// does not describe what it must.
    #[error("{what}: {message}")]
    Parse {
        /// What was being read, for the person fixing it.
        what: String,
        /// What was wrong with it.
        message: String,
    },
    /// The filesystem said no.
    #[error("{what}: {source}")]
    Io {
        /// What was being touched.
        what: String,
        /// What the filesystem said.
        #[source]
        source: io::Error,
    },
    /// `git` exited nonzero. The captured stderr is the message, because a
    /// clone fails for reasons (auth, proxies, typos) only git can name.
    #[error("git {verb} failed: {stderr}")]
    Git {
        /// What git was asked to do.
        verb: String,
        /// What it said on the way out.
        stderr: String,
    },
    /// A name was asked about that the store does not hold.
    #[error("{0}")]
    Unknown(String),
}

/// A plugin's `.claude-plugin/plugin.json`, read tolerantly.
///
/// No `deny_unknown_fields`, deliberately — the opposite of every shape in
/// `config.rs`, because this is Claude's file rather than ganja's: Claude Code
/// documents that unrecognised manifest fields are ignored so one manifest
/// can double as another ecosystem's, and a port that refused them would
/// refuse plugins that load fine in the tool they were written for.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Manifest {
    /// The plugin's identity, and the one field the spec requires.
    pub name: String,
    /// Semantic version, when the author pins one.
    pub version: Option<String>,
    /// One line about what it does.
    pub description: Option<String>,
    /// Who wrote it — `{name, email?, url?}` in the spec, carried whole and
    /// unvalidated: metadata about a person is not a path anything joins.
    pub author: Option<Author>,
}

/// The `author` object of a [`Manifest`], as the spec spells it.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Author {
    /// Name of the author or team.
    pub name: Option<String>,
    /// Contact email.
    pub email: Option<String>,
    /// Website or profile.
    pub url: Option<String>,
}

impl Manifest {
    /// Parses a `plugin.json`, refusing a name that could escape the store.
    ///
    /// # Errors
    ///
    /// [`PluginError::Parse`] for text that is not the manifest, or a
    /// manifest whose name is empty, carries a path separator, or traverses.
    pub fn parse(text: &str) -> Result<Self, PluginError> {
        let manifest: Self = serde_json::from_str(text).map_err(|error| PluginError::Parse {
            what: MANIFEST_FILE.to_owned(),
            message: error.to_string(),
        })?;
        check_name("plugin", &manifest.name)?;

        Ok(manifest)
    }
}

/// A marketplace's `.claude-plugin/marketplace.json`, read tolerantly like
/// the [`Manifest`] and for the same reason — with the same exception: every
/// name in it becomes a directory under the store, so names are checked.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Marketplace {
    /// The marketplace's identity — the half after the `@` in
    /// `install <plugin>@<marketplace>`.
    pub name: String,
    /// Who maintains it. Required by the spec; carried, not consulted.
    pub owner: Owner,
    /// The plugins it offers.
    pub plugins: Vec<MarketplaceEntry>,
}

/// The `owner` object of a [`Marketplace`].
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Owner {
    /// Name of the maintainer or team — the one field the spec requires.
    pub name: Option<String>,
    /// Contact email.
    pub email: Option<String>,
    /// Website or profile.
    pub url: Option<String>,
}

/// One plugin a [`Marketplace`] offers, and where to fetch it from.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct MarketplaceEntry {
    /// The name the plugin installs under. The spec makes this the identity
    /// even when the plugin's own manifest says otherwise.
    pub name: String,
    /// Where the plugin lives; see [`Source`].
    pub source: Source,
    /// One line shown by listings.
    pub description: Option<String>,
}

/// A marketplace entry's `source`, in the two shapes the spec writes.
///
/// A string is a path relative to the marketplace root. An object is one of
/// the spec's remote forms, carried verbatim for [`Store::install`] to fetch
/// (2026-08-15, retiring the deferral): the `url`/`git`/`git-subdir` shapes
/// — a git URL, an optional `sha`/`ref` pin, an optional `path` inside the
/// repository — and the `github` shorthand. A kind beyond those is refused
/// by name at install, never by this parser refusing the whole marketplace
/// it appears in.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// A path relative to the marketplace root, pointing only into it.
    Path(String),
    /// One of the spec's remote source objects, carried verbatim.
    Remote(Value),
}

impl<'de> Deserialize<'de> for Source {
    /// Hand-written for the reason `config.rs`'s two-shape keys are: an
    /// `untagged` enum reports only that nothing matched, and the useful
    /// error here is which shape was being read when it went wrong.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::String(path) => Ok(Self::Path(path)),
            object @ Value::Object(_) => Ok(Self::Remote(object)),
            other => Err(serde::de::Error::custom(format!(
                "a source is a relative path or a source object, not {other}"
            ))),
        }
    }
}

impl Marketplace {
    /// Parses a `marketplace.json`, refusing what could escape the store: a
    /// marketplace or plugin name that traverses, a duplicate plugin name
    /// (two entries claiming one install directory), and a path source that
    /// is absolute or walks up out of the marketplace.
    ///
    /// # Errors
    ///
    /// [`PluginError::Parse`], naming the offender.
    pub fn parse(text: &str) -> Result<Self, PluginError> {
        let market: Self = serde_json::from_str(text).map_err(|error| PluginError::Parse {
            what: MARKETPLACE_FILE.to_owned(),
            message: error.to_string(),
        })?;
        check_name("marketplace", &market.name)?;

        let mut seen = std::collections::BTreeSet::new();
        for entry in &market.plugins {
            check_name("plugin", &entry.name)?;
            if !seen.insert(entry.name.as_str()) {
                return Err(PluginError::Parse {
                    what: MARKETPLACE_FILE.to_owned(),
                    message: format!(
                        "plugin \"{}\" is listed twice; two entries cannot share one \
                         install directory",
                        entry.name
                    ),
                });
            }
            if let Source::Path(path) = &entry.source {
                check_relative_source(&entry.name, path)?;
            }
        }

        Ok(market)
    }
}

/// Refuses a name that is empty or that would escape the store when joined
/// onto a path. The check is the reason hostile names are safe to join
/// everywhere below: nothing that passes here can name anything outside the
/// directory it is joined to.
fn check_name(kind: &str, name: &str) -> Result<(), PluginError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(PluginError::Parse {
            what: format!("{kind} name"),
            message: format!("a {kind} name cannot be empty"),
        });
    }
    if trimmed.contains(['/', '\\']) {
        return Err(PluginError::Parse {
            what: format!("{kind} name"),
            message: format!("\"{trimmed}\" carries a path separator, which no {kind} name may"),
        });
    }
    if trimmed == "." || trimmed == ".." {
        return Err(PluginError::Parse {
            what: format!("{kind} name"),
            message: format!("\"{trimmed}\" names a directory relation, not a {kind}"),
        });
    }

    Ok(())
}

/// Refuses a path source that is absolute or that walks up — the spec's own
/// rule ("don't use `../` to reference paths outside the marketplace root"),
/// enforced rather than trusted, because the joined path decides what gets
/// copied into the store.
fn check_relative_source(plugin: &str, path: &str) -> Result<(), PluginError> {
    let as_path = Path::new(path);
    if as_path.is_absolute() {
        return Err(PluginError::Parse {
            what: MARKETPLACE_FILE.to_owned(),
            message: format!(
                "plugin \"{plugin}\" has an absolute source path; a source may point \
                 only into its own marketplace"
            ),
        });
    }
    if as_path.components().any(|part| matches!(part, std::path::Component::ParentDir)) {
        return Err(PluginError::Parse {
            what: MARKETPLACE_FILE.to_owned(),
            message: format!(
                "plugin \"{plugin}\" has a source that walks up with \"..\"; a source \
                 may point only into its own marketplace"
            ),
        });
    }

    Ok(())
}

/// The store's own record of what is installed: `plugins.json`, written and
/// read only by this module.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct State {
    /// Marketplaces that have been added, by name, with where each came from.
    #[serde(default)]
    pub marketplaces: BTreeMap<String, MarketplaceState>,
    /// Plugins that have been installed, by name.
    #[serde(default)]
    pub plugins: BTreeMap<String, PluginState>,
}

/// One installed plugin in full, as `ganja plugin details` shows it
/// (2026-08-15, Claude Code's own `details` surface): identity, inventory,
/// and the projected token cost of what it puts in front of the model.
#[derive(Clone, Debug, PartialEq)]
pub struct Details {
    /// The plugin's name — the marketplace entry's, which is the identity.
    pub name: String,
    /// The version its own manifest pins, when it pins one.
    pub version: Option<String>,
    /// One line about what it does: the manifest's, or the marketplace
    /// entry's where the manifest says nothing.
    pub description: Option<String>,
    /// The marketplace it was installed from.
    pub marketplace: String,
    /// Whether the load path currently reads it.
    pub enabled: bool,
    /// Skills, each with its projected cost.
    pub skills: Vec<ComponentCost>,
    /// Agents, each with its projected cost.
    pub agents: Vec<ComponentCost>,
    /// Commands, each with its projected cost — always-on zero, because the
    /// palette is the UI's and nothing reaches the model until one runs.
    pub commands: Vec<ComponentCost>,
    /// Hook event names.
    pub hooks: Vec<String>,
    /// MCP server names, plugin-local.
    pub mcp: Vec<String>,
    /// LSP server names.
    pub lsp: Vec<String>,
}

/// One component's projected token cost, both figures the chars-over-four
/// estimate the disclaimers own up to.
#[derive(Clone, Debug, PartialEq)]
pub struct ComponentCost {
    /// The component's plugin-local name.
    pub name: String,
    /// Tokens added to **every** session: the name-and-description line the
    /// roster or `<available_skills>` block carries.
    pub always_on: u64,
    /// Tokens paid **each time it fires**: the body loaded on invocation.
    pub on_invoke: u64,
}

impl Details {
    /// The always-on figures summed: what installing this plugin adds to
    /// every session before anything fires.
    #[must_use]
    pub fn always_on_total(&self) -> u64 {
        self.skills
            .iter()
            .chain(&self.agents)
            .chain(&self.commands)
            .map(|component| component.always_on)
            .sum()
    }
}

/// Tokens a text is projected to cost: characters over four, rounded up —
/// the same order-of-magnitude arithmetic every "~" in the output admits to.
fn projected_tokens(text: &str) -> u64 {
    u64::try_from(text.chars().count().div_ceil(4)).unwrap_or(u64::MAX)
}

/// Walks a plugin's `skills/` for every `SKILL.md` and prices each: the
/// frontmatter's name and description are what `<available_skills>` carries
/// into every session, the body is what an invocation loads. A file with no
/// frontmatter still counts — its directory names it, its whole text is the
/// invocation.
fn collect_skill_costs(root: &Path, into: &mut Vec<ComponentCost>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_skill_costs(&path, into);
            continue;
        }
        if entry.file_name() != "SKILL.md" {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let (fields, body) = split_frontmatter(&text);
        // The shared reader answers a valueless key as present-and-empty, so
        // the fallback has to be told an empty name is no name — the same
        // guard the skill loader itself pairs with that reader.
        let name =
            fields.get("name").cloned().filter(|name| !name.is_empty()).unwrap_or_else(|| {
                path.parent()
                    .and_then(Path::file_name)
                    .map_or_else(|| "skill".to_owned(), |dir| dir.to_string_lossy().into_owned())
            });
        let description = fields.get("description").map_or("", String::as_str);
        into.push(ComponentCost {
            always_on: projected_tokens(&name) + projected_tokens(description),
            on_invoke: projected_tokens(body),
            name,
        });
    }
}

/// One added marketplace, as `ganja plugin marketplace list` shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct MarketplaceListing {
    /// The marketplace's name, which is also its directory in the store.
    pub name: String,
    /// The git URL or local path its add recorded.
    pub origin: String,
    /// The plugin names its file offers — or, for a copy whose own file no
    /// longer reads, the reason it does not: one bad marketplace is its own
    /// bad news, never the listing's.
    pub offered: Result<Vec<String>, String>,
    /// The installed plugins that came from it.
    pub installed: Vec<String>,
}

/// One added marketplace, as the state file records it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MarketplaceState {
    /// The git URL or local path `marketplace add` was given, kept so a
    /// listing can say where a marketplace came from.
    pub origin: String,
}

/// One installed plugin, as the state file records it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PluginState {
    /// The marketplace it was installed from.
    pub marketplace: String,
    /// Whether the load path reads it. A disabled plugin stays on disk and
    /// contributes nothing.
    pub enabled: bool,
}

/// What one installed plugin holds, collected by the same walk whether a
/// listing is asking or the load path is: the `ganja plugin list` column and
/// what a session actually serves cannot disagree, because they are one
/// function's answer.
#[derive(Clone, Debug, Default)]
pub struct Contribution {
    /// Hook groups by event name, already filtered to events this build
    /// fires and handlers it can run.
    pub hooks: BTreeMap<String, Vec<HookMatcher>>,
    /// MCP servers by their **plugin-local** name; `apply` namespaces them.
    pub mcp: BTreeMap<String, McpServer>,
    /// The plugin's `skills/` directory, when it has one.
    pub skills_root: Option<PathBuf>,
    /// Slash commands by their **plugin-local** name, from `commands/*.md`;
    /// `apply` namespaces them.
    pub commands: BTreeMap<String, CommandConfig>,
    /// Agents by name, from `agents/*.md`.
    pub agents: BTreeMap<String, AgentConfig>,
    /// Language servers by name, from `.lsp.json`.
    pub lsp: BTreeMap<String, LspEntry>,
}

impl Contribution {
    /// One line per component, for listings: `hook PreToolUse`,
    /// `mcp db`, `skills`, `command review`, `agent reviewer`, `lsp go`.
    #[must_use]
    pub fn described(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for event in self.hooks.keys() {
            lines.push(format!("hook {event}"));
        }
        for server in self.mcp.keys() {
            lines.push(format!("mcp {server}"));
        }
        if self.skills_root.is_some() {
            lines.push("skills".to_owned());
        }
        // The plugin-local name, like every other row here: what the listing
        // shows is what the plugin's own directory holds, and the `<plugin>:`
        // prefix `apply` adds is a property of the merged config rather than
        // of the component.
        for command in self.commands.keys() {
            lines.push(format!("command {command}"));
        }
        for agent in self.agents.keys() {
            lines.push(format!("agent {agent}"));
        }
        for server in self.lsp.keys() {
            lines.push(format!("lsp {server}"));
        }

        lines
    }
}

/// One row of `ganja plugin list`.
#[derive(Clone, Debug)]
pub struct Listing {
    /// The plugin's name.
    pub name: String,
    /// Whether the load path reads it.
    pub enabled: bool,
    /// The marketplace it came from.
    pub marketplace: String,
    /// What it holds, whether or not it is enabled — a disabled row still
    /// says what enabling it would add.
    pub components: Vec<String>,
}

/// The source tag the skill listings show for a skill discovered under
/// `origin`: the plugin's name for a root inside the install store, `user`
/// for everything else — ganja's own homes and whatever `skills.paths` named
/// are all directories the user placed or configured, and a fixture's
/// temporary roots match no store at all.
#[must_use]
pub fn skill_source(origin: &Path) -> String {
    Store::discover().and_then(|store| store.plugin_of(origin)).unwrap_or_else(|| "user".to_owned())
}

/// The install store: everything under `<config home>/plugins/`.
#[derive(Clone, Debug)]
pub struct Store {
    /// The `plugins/` directory itself.
    root: PathBuf,
}

impl Store {
    /// The store under an explicit root — the constructor everything testable
    /// goes through, so no test has to mutate the environment to aim one at a
    /// temporary directory.
    #[must_use]
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    /// The store under the config home, resolved the way everything else
    /// ganja owns is — through `config::config_home`, so `GANJA_CONFIG_HOME`
    /// moves the plugins with the config and the skills they sit beside.
    #[must_use]
    pub fn discover() -> Option<Self> {
        config_home().map(|home| Self::at(home.join(STORE_DIR)))
    }

    /// The state file's path, for callers that name it in errors.
    #[must_use]
    pub fn state_path(&self) -> PathBuf {
        self.root.join(STATE_FILE)
    }

    fn marketplaces_dir(&self) -> PathBuf {
        self.root.join(MARKETPLACES_DIR)
    }

    fn installed_dir(&self) -> PathBuf {
        self.root.join(INSTALLED_DIR)
    }

    /// Where one installed plugin lives.
    #[must_use]
    pub fn plugin_root(&self, plugin: &str) -> PathBuf {
        self.installed_dir().join(plugin)
    }

    /// The plugin a directory belongs to, when it sits inside this store's
    /// `installed/` tree — what the skill listings tag a row with. [`None`]
    /// is a directory the user placed or configured themselves.
    #[must_use]
    pub fn plugin_of(&self, path: &Path) -> Option<String> {
        path.strip_prefix(self.installed_dir())
            .ok()?
            .components()
            .next()
            .map(|name| name.as_os_str().to_string_lossy().into_owned())
    }

    /// Reads the state file, or the empty state when there is none yet.
    ///
    /// # Errors
    ///
    /// [`PluginError`] for a file that exists and cannot be read or parsed —
    /// this is ganja's own file, so a broken one is a hard error rather than
    /// a shrug, exactly like a config file that stopped parsing.
    pub fn state(&self) -> Result<State, PluginError> {
        let path = self.state_path();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(State::default()),
            Err(source) => {
                return Err(PluginError::Io { what: path.display().to_string(), source });
            }
        };

        serde_json::from_str(&text).map_err(|error| PluginError::Parse {
            what: path.display().to_string(),
            message: error.to_string(),
        })
    }

    /// Writes the state file whole, through a sibling and a rename so a
    /// crash mid-write leaves the old state rather than half a document.
    fn write_state(&self, state: &State) -> Result<(), PluginError> {
        let path = self.state_path();
        let staged = self.root.join(format!("{STATE_FILE}.tmp"));
        let text = serde_json::to_string_pretty(state).expect("the state shape serializes");

        fs::create_dir_all(&self.root)
            .map_err(|source| PluginError::Io { what: self.root.display().to_string(), source })?;
        fs::write(&staged, text)
            .map_err(|source| PluginError::Io { what: staged.display().to_string(), source })?;
        fs::rename(&staged, &path)
            .map_err(|source| PluginError::Io { what: path.display().to_string(), source })
    }

    /// Adds a marketplace: clones a git URL or copies a local directory into
    /// a staging directory, validates its `marketplace.json` there, and only
    /// then moves it into place — so no failure leaves a half-added
    /// marketplace behind, and nothing is trusted before it is read
    /// (validation *precedes* the store accepting the copy; for a git source
    /// the clone lands in staging precisely so the file can be read before
    /// anything is kept).
    ///
    /// A marketplace already added under the same name is replaced, which is
    /// the spec's own behavior for re-adding a name.
    ///
    /// # Errors
    ///
    /// [`PluginError`] for a clone that failed (with git's stderr), a source
    /// with no marketplace file, or one whose file does not validate.
    pub fn add_marketplace(&self, origin: &str) -> Result<String, PluginError> {
        let (staging, market) = self.stage_origin(origin)?;

        let final_dir = self.marketplaces_dir().join(&market.name);
        replace_dir(&staging.keep, &final_dir)?;
        drop(staging);

        let mut state = self.state()?;
        state
            .marketplaces
            .insert(market.name.clone(), MarketplaceState { origin: origin.to_owned() });
        self.write_state(&state)?;

        Ok(market.name)
    }

    /// Fetches `origin` — a git clone or a directory copy — into a fresh
    /// staging directory and parses its marketplace file: the shared front
    /// half of an add and an update, which is what keeps the two validating
    /// identically (2026-08-15).
    fn stage_origin(&self, origin: &str) -> Result<(Staging, Marketplace), PluginError> {
        let staging = self.staging_dir("marketplace")?;
        let staged = staging.keep.clone();

        if looks_like_git(origin) {
            clone(origin, &staged)?;
        } else {
            let from = Path::new(origin);
            if !from.is_dir() {
                return Err(PluginError::Unknown(format!(
                    "{origin} is not a directory, and does not look like a git URL"
                )));
            }
            copy_tree(from, &staged)?;
        }

        let manifest_path = staged.join(CLAUDE_PLUGIN_DIR).join(MARKETPLACE_FILE);
        let text = fs::read_to_string(&manifest_path).map_err(|source| PluginError::Io {
            what: format!("{origin} has no readable {CLAUDE_PLUGIN_DIR}/{MARKETPLACE_FILE}"),
            source,
        })?;
        let market = Marketplace::parse(&text)?;

        Ok((staging, market))
    }

    /// Every added marketplace, sorted by name: where it came from, the
    /// plugins it offers — or why its own file no longer reads — and the
    /// installed plugins that came from it.
    ///
    /// # Errors
    ///
    /// [`PluginError`] only for a state file that does not read; a single
    /// marketplace whose copy has gone bad is reported inside its own
    /// listing rather than sinking the others.
    pub fn marketplaces(&self) -> Result<Vec<MarketplaceListing>, PluginError> {
        let state = self.state()?;

        Ok(state
            .marketplaces
            .iter()
            .map(|(name, market)| {
                let manifest = self
                    .marketplaces_dir()
                    .join(name)
                    .join(CLAUDE_PLUGIN_DIR)
                    .join(MARKETPLACE_FILE);
                let offered = fs::read_to_string(&manifest)
                    .map_err(|error| error.to_string())
                    .and_then(|text| Marketplace::parse(&text).map_err(|error| error.to_string()))
                    .map(|market| market.plugins.into_iter().map(|plugin| plugin.name).collect());
                let installed = state
                    .plugins
                    .iter()
                    .filter(|(_, plugin)| plugin.marketplace == *name)
                    .map(|(plugin, _)| plugin.clone())
                    .collect();

                MarketplaceListing {
                    name: name.clone(),
                    origin: market.origin.clone(),
                    offered,
                    installed,
                }
            })
            .collect())
    }

    /// Removes one added marketplace: its copy and its state entry.
    ///
    /// Refused while plugins installed from it remain, naming them: an
    /// installed plugin is its own copy and would keep working, but it
    /// could never update again, and `ganja plugin remove` first is the
    /// honest spelling of that choice.
    ///
    /// # Errors
    ///
    /// [`PluginError`] for a marketplace never added, one with installed
    /// plugins, or a copy that would not delete.
    pub fn remove_marketplace(&self, name: &str) -> Result<(), PluginError> {
        check_name("marketplace", name)?;
        let mut state = self.state()?;
        if !state.marketplaces.contains_key(name) {
            return Err(self.no_such_marketplace(&state, name));
        }
        let dependents = state
            .plugins
            .iter()
            .filter(|(_, plugin)| plugin.marketplace == name)
            .map(|(plugin, _)| plugin.as_str())
            .collect::<Vec<_>>();
        if !dependents.is_empty() {
            return Err(PluginError::Unknown(format!(
                "plugins installed from \"{name}\" are still present ({}); `ganja plugin \
                 remove` them first",
                dependents.join(", ")
            )));
        }

        let dir = self.marketplaces_dir().join(name);
        if dir.exists() {
            fs::remove_dir_all(&dir)
                .map_err(|source| PluginError::Io { what: dir.display().to_string(), source })?;
        }
        state.marketplaces.remove(name);
        self.write_state(&state)
    }

    /// Re-fetches one added marketplace from the origin its add recorded,
    /// staged and validated exactly as the add was, and answers that origin
    /// for the caller's own report.
    ///
    /// A re-fetch whose file names a **different** marketplace is refused
    /// rather than silently forked — remove and re-add is the honest
    /// spelling of a rename.
    ///
    /// # Errors
    ///
    /// [`PluginError`] for a marketplace never added, a fetch that failed,
    /// a file that does not validate, or one that changed its name.
    pub fn update_marketplace(&self, name: &str) -> Result<String, PluginError> {
        check_name("marketplace", name)?;
        let state = self.state()?;
        let Some(market) = state.marketplaces.get(name) else {
            return Err(self.no_such_marketplace(&state, name));
        };
        let origin = market.origin.clone();

        let (staging, fetched) = self.stage_origin(&origin)?;
        if fetched.name != name {
            return Err(PluginError::Unknown(format!(
                "{origin} now names itself \"{}\" rather than \"{name}\"; `ganja plugin \
                 marketplace remove {name}` and a fresh add are how a rename is taken",
                fetched.name
            )));
        }
        replace_dir(&staging.keep, &self.marketplaces_dir().join(name))?;
        drop(staging);

        Ok(origin)
    }

    /// Everything `ganja plugin details` prints about one installed plugin:
    /// identity off the manifest (description falling back to the
    /// marketplace entry's), the component inventory off the same collector
    /// the load path uses, and each prompt-bearing component's projected
    /// token cost — always-on for what its roster line adds to every
    /// session, on-invoke for the body loaded when it fires.
    ///
    /// # Errors
    ///
    /// [`PluginError`] for a plugin the store does not hold, or state that
    /// does not read; a missing or unparseable manifest costs the identity
    /// fields, never the details.
    pub fn details(&self, plugin: &str) -> Result<Details, PluginError> {
        let state = self.state()?;
        let Some(installed) = state.plugins.get(plugin) else {
            return Err(self.no_such_plugin(&state, plugin));
        };
        let root = self.plugin_root(plugin);

        let manifest = fs::read_to_string(root.join(CLAUDE_PLUGIN_DIR).join(MANIFEST_FILE))
            .ok()
            .and_then(|text| Manifest::parse(&text).ok());
        // The marketplace entry's description stands in where the manifest
        // says nothing — it is what the person chose the plugin by.
        let description =
            manifest.as_ref().and_then(|manifest| manifest.description.clone()).or_else(|| {
                let manifest = self
                    .marketplaces_dir()
                    .join(&installed.marketplace)
                    .join(CLAUDE_PLUGIN_DIR)
                    .join(MARKETPLACE_FILE);
                let market = Marketplace::parse(&fs::read_to_string(manifest).ok()?).ok()?;

                market.plugins.into_iter().find(|entry| entry.name == plugin)?.description
            });

        let contribution = collect(&root, plugin);
        let mut skills = Vec::new();
        if let Some(skills_root) = &contribution.skills_root {
            collect_skill_costs(skills_root, &mut skills);
            skills.sort_by(|a, b| a.name.cmp(&b.name));
        }
        let agents = contribution
            .agents
            .iter()
            .map(|(name, agent)| ComponentCost {
                name: name.clone(),
                always_on: projected_tokens(name)
                    + agent.description.as_deref().map_or(0, projected_tokens),
                on_invoke: agent.prompt.as_deref().map_or(0, projected_tokens),
            })
            .collect();
        let commands = contribution
            .commands
            .iter()
            .map(|(name, command)| ComponentCost {
                name: name.clone(),
                always_on: 0,
                on_invoke: projected_tokens(&command.template),
            })
            .collect();

        Ok(Details {
            name: plugin.to_owned(),
            version: manifest.and_then(|manifest| manifest.version),
            description,
            marketplace: installed.marketplace.clone(),
            enabled: installed.enabled,
            skills,
            agents,
            commands,
            hooks: contribution.hooks.keys().cloned().collect(),
            mcp: contribution.mcp.keys().cloned().collect(),
            lsp: contribution.lsp.keys().cloned().collect(),
        })
    }

    /// The refusal for a marketplace nobody added, naming what was.
    fn no_such_marketplace(&self, state: &State, name: &str) -> PluginError {
        let added = state.marketplaces.keys().map(String::as_str).collect::<Vec<_>>().join(", ");
        let added = if added.is_empty() { "none" } else { &added };

        PluginError::Unknown(format!("no marketplace \"{name}\" has been added; added: {added}"))
    }

    /// Installs one plugin from an added marketplace: resolves the entry's
    /// relative source inside the marketplace's own copy, stages the plugin
    /// directory, validates its manifest when it has one, and moves it into
    /// `installed/<plugin>/` enabled.
    ///
    /// Install is deliberately the **only** door into `installed/` — nothing
    /// here installs as a side effect of anything else, because a plugin's
    /// hooks and MCP servers run with the user's own authority and the typed
    /// command is the consent.
    ///
    /// # Errors
    ///
    /// [`PluginError`] for a marketplace never added, an entry the
    /// marketplace does not list, a remote source this build cannot fetch or
    /// whose fetch fails, a source directory that is missing, or a manifest
    /// that does not parse.
    pub fn install(&self, plugin: &str, marketplace: &str) -> Result<(), PluginError> {
        check_name("plugin", plugin)?;
        let state = self.state()?;
        if !state.marketplaces.contains_key(marketplace) {
            return Err(PluginError::Unknown(format!(
                "no marketplace \"{marketplace}\" has been added; `ganja plugin marketplace \
                 add` is how one arrives"
            )));
        }

        let market_dir = self.marketplaces_dir().join(marketplace);
        let manifest_path = market_dir.join(CLAUDE_PLUGIN_DIR).join(MARKETPLACE_FILE);
        let text = fs::read_to_string(&manifest_path).map_err(|source| PluginError::Io {
            what: manifest_path.display().to_string(),
            source,
        })?;
        let market = Marketplace::parse(&text)?;

        let Some(entry) = market.plugins.iter().find(|entry| entry.name == plugin) else {
            let offered = market
                .plugins
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(PluginError::Unknown(format!(
                "marketplace \"{marketplace}\" offers no plugin \"{plugin}\"; it offers: \
                 {offered}"
            )));
        };
        // A remote source clones into its own staging first (2026-08-15,
        // retiring the deferral): kept alive past the copy below, cleaned on
        // drop like every staging here.
        let _fetched;
        let source_dir = match &entry.source {
            Source::Path(relative) => {
                // `Marketplace::parse` already refused absolute and `..`
                // sources, so the join cannot leave `market_dir`; the
                // existence check is about catalogs that drifted from their
                // own tree.
                let source_dir = market_dir.join(relative);
                if !source_dir.is_dir() {
                    return Err(PluginError::Unknown(format!(
                        "plugin \"{plugin}\" points at {relative}, which is not a directory \
                         in marketplace \"{marketplace}\""
                    )));
                }

                source_dir
            }
            Source::Remote(object) => {
                let staging = self.staging_dir("fetch")?;
                let dir = fetch_remote(plugin, object, &staging.keep)?;
                _fetched = staging;

                dir
            }
        };

        // The manifest is optional in the spec (components are discovered by
        // location); when present it must parse, and its name is *not*
        // required to match — the marketplace entry's name is the identity,
        // which is also the spec's rule.
        let manifest = source_dir.join(CLAUDE_PLUGIN_DIR).join(MANIFEST_FILE);
        if manifest.is_file() {
            let text = fs::read_to_string(&manifest).map_err(|source| PluginError::Io {
                what: manifest.display().to_string(),
                source,
            })?;
            Manifest::parse(&text)?;
        }

        let staging = self.staging_dir("plugin")?;
        copy_tree(&source_dir, &staging.keep)?;
        replace_dir(&staging.keep, &self.plugin_root(plugin))?;
        drop(staging);

        let mut state = self.state()?;
        state.plugins.insert(
            plugin.to_owned(),
            PluginState { marketplace: marketplace.to_owned(), enabled: true },
        );
        self.write_state(&state)
    }

    /// Marks one installed plugin enabled or disabled.
    ///
    /// # Errors
    ///
    /// [`PluginError::Unknown`] for a plugin the store does not hold.
    pub fn set_enabled(&self, plugin: &str, enabled: bool) -> Result<(), PluginError> {
        let mut state = self.state()?;
        let Some(entry) = state.plugins.get_mut(plugin) else {
            return Err(self.no_such_plugin(&state, plugin));
        };
        entry.enabled = enabled;

        self.write_state(&state)
    }

    /// Removes one installed plugin: its directory and its state entry. The
    /// marketplace it came from stays added.
    ///
    /// # Errors
    ///
    /// [`PluginError::Unknown`] for a plugin the store does not hold;
    /// [`PluginError::Io`] when its directory will not delete.
    pub fn remove(&self, plugin: &str) -> Result<(), PluginError> {
        let mut state = self.state()?;
        if state.plugins.remove(plugin).is_none() {
            return Err(self.no_such_plugin(&state, plugin));
        }

        let dir = self.plugin_root(plugin);
        if dir.is_dir() {
            fs::remove_dir_all(&dir)
                .map_err(|source| PluginError::Io { what: dir.display().to_string(), source })?;
        }

        self.write_state(&state)
    }

    /// Every installed plugin, with what each holds — enabled or not, so a
    /// disabled row still says what enabling it would add.
    ///
    /// # Errors
    ///
    /// [`PluginError`] only for a state file that will not read; a plugin
    /// whose directory has gone missing is listed with no components rather
    /// than hiding the state entry that still names it.
    pub fn list(&self) -> Result<Vec<Listing>, PluginError> {
        let state = self.state()?;
        Ok(state
            .plugins
            .iter()
            .map(|(name, plugin)| Listing {
                name: name.clone(),
                enabled: plugin.enabled,
                marketplace: plugin.marketplace.clone(),
                components: collect(&self.plugin_root(name), name).described(),
            })
            .collect())
    }

    /// The refusal for a plugin the store does not hold, listing what it
    /// does — the same courtesy every unknown-name refusal in the config
    /// gives.
    fn no_such_plugin(&self, state: &State, plugin: &str) -> PluginError {
        let installed = state.plugins.keys().map(String::as_str).collect::<Vec<_>>().join(", ");
        PluginError::Unknown(if installed.is_empty() {
            format!("no plugin \"{plugin}\" is installed; none are")
        } else {
            format!("no plugin \"{plugin}\" is installed; installed: {installed}")
        })
    }

    /// A fresh staging directory under the store, deleted on drop unless the
    /// caller moved it into place first. Uniqueness comes from the process id
    /// and a counter, which is enough for a store only ever written by the
    /// person's own commands.
    fn staging_dir(&self, label: &str) -> Result<Staging, PluginError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let dir = self.root.join(format!(
            ".staging-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir)
            .map_err(|source| PluginError::Io { what: dir.display().to_string(), source })?;

        Ok(Staging { keep: dir })
    }
}

/// A staging directory that cleans itself up when dropped — which after a
/// successful `rename` out of it is an empty shell, and after any failure is
/// the partial state that must not survive.
struct Staging {
    keep: PathBuf,
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.keep);
    }
}

/// Whether `marketplace add`'s argument names a git source rather than a
/// local directory: a URL scheme git speaks, an scp-style `git@` remote, or
/// anything ending in `.git` (which is also how a local bare repository is
/// reached — through a clone, like any other git source).
fn looks_like_git(origin: &str) -> bool {
    origin.starts_with("http://")
        || origin.starts_with("https://")
        || origin.starts_with("git://")
        || origin.starts_with("ssh://")
        || origin.starts_with("git@")
        || origin.starts_with("file://")
        || origin.ends_with(".git")
}

/// Runs one git command. The spawn keeps `hook.rs`'s discipline where it
/// applies to a non-shell child: stdin is null (nothing is ever typed at
/// git), both output streams are captured, and the captured stderr *is* the
/// error, because git fails for reasons only git can name.
fn git<I, S>(verb: &str, args: I) -> Result<(), PluginError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = Command::new("git")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|source| PluginError::Io { what: "spawning git".to_owned(), source })?;
    if !output.status.success() {
        return Err(PluginError::Git {
            verb: verb.to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    Ok(())
}

/// Clones `origin` into `into`, shallow — history is not what a marketplace
/// copy is for.
fn clone(origin: &str, into: &Path) -> Result<(), PluginError> {
    git(
        "clone",
        [
            std::ffi::OsStr::new("clone"),
            std::ffi::OsStr::new("--depth"),
            std::ffi::OsStr::new("1"),
            std::ffi::OsStr::new("--"),
            std::ffi::OsStr::new(origin),
            into.as_os_str(),
        ],
    )
}

/// Puts the tree `pin` names from `origin` into `into` — a plugin install
/// takes the exact commit the marketplace reviewed, never whatever the
/// branch moved to since (2026-08-15).
///
/// Two roads, because servers disagree about naming a bare commit: first a
/// pinpoint fetch (`init`/`fetch --depth 1 origin <pin>`/`checkout
/// FETCH_HEAD`), which GitHub answers for any sha; where the server refuses
/// unadvertised objects, a full clone and a detached checkout of the pin.
/// With no pin at all the marketplace said "wherever the default branch is",
/// and the shallow clone says exactly that.
fn fetch_pinned(origin: &str, pin: Option<&str>, into: &Path) -> Result<(), PluginError> {
    let Some(pin) = pin else {
        return clone(origin, into);
    };

    let pinpoint = git("init", [std::ffi::OsStr::new("init"), into.as_os_str()])
        .and_then(|()| {
            git(
                "remote add",
                [
                    std::ffi::OsStr::new("-C"),
                    into.as_os_str(),
                    std::ffi::OsStr::new("remote"),
                    std::ffi::OsStr::new("add"),
                    std::ffi::OsStr::new("origin"),
                    std::ffi::OsStr::new(origin),
                ],
            )
        })
        .and_then(|()| {
            git(
                "fetch",
                [
                    std::ffi::OsStr::new("-C"),
                    into.as_os_str(),
                    std::ffi::OsStr::new("fetch"),
                    std::ffi::OsStr::new("--depth"),
                    std::ffi::OsStr::new("1"),
                    std::ffi::OsStr::new("origin"),
                    std::ffi::OsStr::new(pin),
                ],
            )
        })
        .and_then(|()| {
            git(
                "checkout",
                [
                    std::ffi::OsStr::new("-C"),
                    into.as_os_str(),
                    std::ffi::OsStr::new("checkout"),
                    std::ffi::OsStr::new("--detach"),
                    std::ffi::OsStr::new("FETCH_HEAD"),
                ],
            )
        });
    if pinpoint.is_ok() {
        return Ok(());
    }

    // The pinpoint road failed — wipe its half-made state so the clone finds
    // clean ground, and take the long way.
    let _ = fs::remove_dir_all(into);
    git(
        "clone",
        [
            std::ffi::OsStr::new("clone"),
            std::ffi::OsStr::new("--"),
            std::ffi::OsStr::new(origin),
            into.as_os_str(),
        ],
    )?;
    git(
        "checkout",
        [
            std::ffi::OsStr::new("-C"),
            into.as_os_str(),
            std::ffi::OsStr::new("checkout"),
            std::ffi::OsStr::new("--detach"),
            std::ffi::OsStr::new(pin),
        ],
    )
}

/// Resolves a marketplace entry's remote source object into a directory
/// under `into` holding the plugin: the spec's `url` and `git-subdir` forms
/// (a git URL, an optional `sha`/`ref` pin, an optional `path` inside the
/// repository) and its `github` shorthand (`repo` as `owner/name`). Read
/// tolerantly the way every foreign file here is — unknown keys pass — but
/// a kind this build cannot fetch, a missing URL, or a `path` that walks
/// out of the clone is a refusal by name.
fn fetch_remote(plugin: &str, object: &Value, into: &Path) -> Result<PathBuf, PluginError> {
    let kind = object.get("source").and_then(Value::as_str).unwrap_or_default();
    let url = match kind {
        "url" | "git" | "git-subdir" => {
            object.get("url").and_then(Value::as_str).map(str::to_owned)
        }
        "github" => object
            .get("repo")
            .and_then(Value::as_str)
            .map(|repo| format!("https://github.com/{repo}.git")),
        _ => None,
    }
    .ok_or_else(|| {
        PluginError::Unknown(format!(
            "plugin \"{plugin}\" has a source of kind \"{kind}\" this build cannot fetch; \
             it fetches url, git, git-subdir and github sources"
        ))
    })?;

    // The sha outranks the ref when the entry carries both: the sha is what
    // was reviewed, the ref is where it happened to live.
    let pin = object.get("sha").or_else(|| object.get("ref")).and_then(Value::as_str);
    fetch_pinned(&url, pin, into)?;

    match object.get("path").and_then(Value::as_str) {
        None => Ok(into.to_path_buf()),
        Some(path) => {
            let relative = Path::new(path);
            if relative.is_absolute()
                || relative.components().any(|part| matches!(part, std::path::Component::ParentDir))
            {
                return Err(PluginError::Unknown(format!(
                    "plugin \"{plugin}\" names a path that leaves its own repository: {path}"
                )));
            }
            let dir = into.join(relative);
            if !dir.is_dir() {
                return Err(PluginError::Unknown(format!(
                    "plugin \"{plugin}\" points at {path}, which is not a directory in the \
                     fetched repository"
                )));
            }

            Ok(dir)
        }
    }
}

/// Copies a directory tree, skipping `.git` — a marketplace copy is the
/// files, not the history — and following what `fs::copy` follows.
fn copy_tree(from: &Path, to: &Path) -> Result<(), PluginError> {
    let failed = |what: &Path, source| PluginError::Io { what: what.display().to_string(), source };

    fs::create_dir_all(to).map_err(|source| failed(to, source))?;
    let entries = fs::read_dir(from).map_err(|source| failed(from, source))?;
    for entry in entries {
        let entry = entry.map_err(|source| failed(from, source))?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let source_path = entry.path();
        let target = to.join(&name);
        if source_path.is_dir() {
            copy_tree(&source_path, &target)?;
        } else {
            fs::copy(&source_path, &target).map_err(|source| failed(&source_path, source))?;
        }
    }

    Ok(())
}

/// Moves `from` into place at `to`, replacing whatever was there — the two
/// halves (delete, rename) ordered so the worst crash leaves an absent
/// directory rather than a half-written one.
fn replace_dir(from: &Path, to: &Path) -> Result<(), PluginError> {
    let failed = |what: &Path, source| PluginError::Io { what: what.display().to_string(), source };

    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(|source| failed(parent, source))?;
    }
    if to.is_dir() {
        fs::remove_dir_all(to).map_err(|source| failed(to, source))?;
    }
    fs::rename(from, to).map_err(|source| failed(from, source))
}

/// What one installed plugin contributes, collected from the spec's default
/// component locations: `hooks/hooks.json`, `.mcp.json`, `skills/`,
/// `commands/*.md`, `agents/*.md`, `.lsp.json`.
///
/// A component file that does not parse is skipped with a warning naming the
/// plugin and the file — the module doc says why — and a manifest that names
/// *custom* component paths is not followed in this build: the default
/// locations are the surface, and a manifest key this walk does not consult
/// changes nothing it reports.
#[must_use]
pub fn collect(root: &Path, plugin: &str) -> Contribution {
    let mut found = Contribution::default();
    if !root.is_dir() {
        return found;
    }
    let root_text = root.display().to_string();

    found.hooks = collect_hooks(root, plugin, &root_text);
    found.mcp = collect_mcp(root, plugin, &root_text);
    let skills = root.join("skills");
    if skills.is_dir() {
        found.skills_root = Some(skills);
    }
    found.commands = collect_commands(root, &root_text);
    found.agents = collect_agents(root, plugin);
    found.lsp = collect_lsp(root, plugin, &root_text);

    found
}

/// Reads `hooks/hooks.json` — the spec's `{"hooks": {event: [groups]}}`
/// wrapper — keeping only what this build can fire and run.
///
/// The tolerance is Claude's vocabulary being larger than ganja's: a plugin
/// written for Claude Code may name events (`Setup`, `PostToolBatch`) and
/// handler types (`http`, `prompt`) this build has not ported, and refusing
/// the whole plugin over them would refuse plugins that are otherwise
/// entirely usable. What cannot fire is skipped **with a log line naming
/// it**, so the silence is never unexplained; what remains is held to the
/// same rules `check_hooks` holds a config to (a compilable matcher, a
/// nonempty command), enforced here because plugin hooks join the config
/// *after* its per-file checks have run.
fn collect_hooks(
    root: &Path,
    plugin: &str,
    plugin_root: &str,
) -> BTreeMap<String, Vec<HookMatcher>> {
    let path = root.join("hooks").join("hooks.json");
    let Some(document) = read_component(&path, plugin) else {
        return BTreeMap::new();
    };
    let Some(events) = document.get("hooks").and_then(Value::as_object) else {
        tracing::warn!(
            plugin,
            file = %path.display(),
            "a hooks file with no \"hooks\" object contributes nothing"
        );
        return BTreeMap::new();
    };

    let mut hooks: BTreeMap<String, Vec<HookMatcher>> = BTreeMap::new();
    for (event, groups) in events {
        if crate::hook::HookEvent::from_name(event).is_none() {
            tracing::warn!(plugin, event, "skipping hooks for an event this build does not fire");
            continue;
        }
        let Some(groups) = groups.as_array() else {
            tracing::warn!(plugin, event, "skipping a hooks entry that is not a list");
            continue;
        };
        for group in groups {
            let matcher = group.get("matcher").and_then(Value::as_str).map(str::to_owned);
            if let Some(pattern) = &matcher
                && !pattern.is_empty()
                && regex::Regex::new(pattern).is_err()
            {
                tracing::warn!(
                    plugin,
                    event,
                    matcher = pattern,
                    "skipping a hook group whose matcher is not a regular expression"
                );
                continue;
            }

            let mut handlers = Vec::new();
            for handler in group.get("hooks").and_then(Value::as_array).into_iter().flatten() {
                if handler.get("type").and_then(Value::as_str) != Some("command") {
                    tracing::warn!(
                        plugin,
                        event,
                        "skipping a hook handler of a type this build does not run"
                    );
                    continue;
                }
                let Some(command) = handler
                    .get("command")
                    .and_then(Value::as_str)
                    .map(|command| command.replace(PLUGIN_ROOT_VAR, plugin_root))
                    .filter(|command| !command.trim().is_empty())
                else {
                    tracing::warn!(plugin, event, "skipping a hook handler with no command");
                    continue;
                };
                handlers.push(HookHandler::Command(HookCommand {
                    command,
                    timeout: handler.get("timeout").and_then(Value::as_u64),
                }));
            }
            if !handlers.is_empty() {
                hooks
                    .entry(event.clone())
                    .or_default()
                    .push(HookMatcher { matcher, hooks: handlers });
            }
        }
    }

    hooks
}

/// Reads `.mcp.json` — the spec's `{"mcpServers": {name: entry}}` — and
/// translates each entry into the shape ganja's own `mcp` table holds: a
/// `command`/`args` entry becomes a local server, a `url` entry a remote
/// one. The remote URL is held to `check_mcp`'s clear-wire rule here,
/// because a plugin's entries join the config after its per-file checks ran.
fn collect_mcp(root: &Path, plugin: &str, plugin_root: &str) -> BTreeMap<String, McpServer> {
    let path = root.join(".mcp.json");
    let Some(document) = read_component(&path, plugin) else {
        return BTreeMap::new();
    };
    let Some(entries) = document.get("mcpServers").and_then(Value::as_object) else {
        tracing::warn!(
            plugin,
            file = %path.display(),
            "an mcp file with no \"mcpServers\" object contributes nothing"
        );
        return BTreeMap::new();
    };

    let substitute = |text: &str| text.replace(PLUGIN_ROOT_VAR, plugin_root);
    let mut servers = BTreeMap::new();
    for (name, entry) in entries {
        if let Some(command) = entry.get("command").and_then(Value::as_str) {
            let mut argv = vec![substitute(command)];
            argv.extend(
                entry
                    .get("args")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(substitute),
            );
            let environment = entry
                .get("env")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), substitute(value)))
                })
                .collect();
            servers.insert(
                name.clone(),
                McpServer::Local(McpLocal {
                    command: argv,
                    cwd: None,
                    environment,
                    enabled: true,
                    timeout: None,
                    output_limit: None,
                }),
            );
        } else if let Some(url) = entry.get("url").and_then(Value::as_str) {
            let reachable = url::Url::parse(url)
                .is_ok_and(|parsed| crate::provider::reachable_in_the_clear(&parsed));
            if !reachable {
                tracing::warn!(
                    plugin,
                    server = name,
                    "skipping an mcp server that is not https or loopback; its headers \
                     would travel in the clear"
                );
                continue;
            }
            let headers = entry
                .get("headers")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_owned()))
                })
                .collect();
            servers.insert(
                name.clone(),
                McpServer::Remote(McpRemote {
                    url: url.to_owned(),
                    headers,
                    enabled: true,
                    timeout: None,
                    output_limit: None,
                    oauth: None,
                }),
            );
        } else {
            tracing::warn!(
                plugin,
                server = name,
                "skipping an mcp entry with neither a command nor a url"
            );
        }
    }

    servers
}

/// Reads `commands/*.md` — Claude's markdown-with-frontmatter command files —
/// into the `command` table's own shape.
///
/// Not a parser of its own: the walk, the frontmatter grammar, the size cap
/// and every named refusal are [`crate::command::file_commands`], the same
/// function that reads a project's `.ganja/commands`. A plugin's command file
/// and a project's are therefore one file format read once, which is the whole
/// reason this surface waited for that loader to exist rather than growing a
/// second command system beside it.
///
/// What is added here is the placeholder substitution every other component
/// gets: a template is where a plugin reaches for a script it shipped, so
/// `${CLAUDE_PLUGIN_ROOT}` means the installed root in a `` !`command` `` or an
/// `@path` exactly as it does in a hook's command line.
fn collect_commands(root: &Path, plugin_root: &str) -> BTreeMap<String, CommandConfig> {
    crate::command::file_commands(&root.join("commands"))
        .into_iter()
        .map(|command| {
            (
                command.name,
                CommandConfig {
                    template: command.template.replace(PLUGIN_ROOT_VAR, plugin_root),
                    description: command.description,
                    agent: command.agent,
                    model: command.model,
                },
            )
        })
        .collect()
}

/// Reads `agents/*.md` — Claude's markdown-with-frontmatter agent files —
/// into the `agent` table's own shape. The frontmatter fields this build can
/// act on (`name`, `description`, `model`) are read; the body becomes the
/// prompt; the mode is subagent, which is what a Claude plugin agent is.
fn collect_agents(root: &Path, plugin: &str) -> BTreeMap<String, AgentConfig> {
    let dir = root.join("agents");
    let Ok(entries) = fs::read_dir(&dir) else {
        return BTreeMap::new();
    };

    let mut agents = BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("md") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            tracing::warn!(plugin, file = %path.display(), "skipping an unreadable agent file");
            continue;
        };
        let (front, body) = split_frontmatter(&text);
        // Present-and-empty must not short-circuit the file-stem fallback:
        // agent.rs loads the same file under its stem, and a plugin agent
        // parsing differently from a project's is the drift the shared
        // reader exists to prevent.
        let name = front
            .get("name")
            .cloned()
            .filter(|name| !name.is_empty())
            .or_else(|| path.file_stem().and_then(|stem| stem.to_str()).map(str::to_owned))
            .filter(|name| check_name("agent", name).is_ok());
        let Some(name) = name else {
            tracing::warn!(plugin, file = %path.display(), "skipping an agent with no usable name");
            continue;
        };

        agents.insert(
            name,
            AgentConfig {
                model: front.get("model").cloned().filter(|model| !model.is_empty()),
                prompt: Some(body.trim().to_owned()).filter(|prompt| !prompt.is_empty()),
                description: front
                    .get("description")
                    .cloned()
                    .filter(|description| !description.is_empty()),
                mode: Some(AgentMode::Subagent),
                hidden: None,
                disable: None,
                permission: Default::default(),
            },
        );
    }

    agents
}

/// Reads `.lsp.json` — the spec's `{name: {command, args, extensionToLanguage,
/// …}}` — into the `lsp` table's own shape: `command`+`args` become the argv,
/// `extensionToLanguage`'s keys become `extensions`, `initializationOptions`
/// becomes `initialization`. An entry missing either required field is
/// skipped, which is Claude's own behavior for one.
fn collect_lsp(root: &Path, plugin: &str, plugin_root: &str) -> BTreeMap<String, LspEntry> {
    let path = root.join(".lsp.json");
    let Some(document) = read_component(&path, plugin) else {
        return BTreeMap::new();
    };
    let Some(entries) = document.as_object() else {
        tracing::warn!(
            plugin,
            file = %path.display(),
            "an lsp file that is not an object contributes nothing"
        );
        return BTreeMap::new();
    };

    let substitute = |text: &str| text.replace(PLUGIN_ROOT_VAR, plugin_root);
    let mut servers = BTreeMap::new();
    for (name, entry) in entries {
        let Some(command) = entry.get("command").and_then(Value::as_str) else {
            tracing::warn!(plugin, server = name, "skipping an lsp entry with no command");
            continue;
        };
        let Some(extensions) = entry.get("extensionToLanguage").and_then(Value::as_object) else {
            tracing::warn!(
                plugin,
                server = name,
                "skipping an lsp entry with no extensionToLanguage"
            );
            continue;
        };

        let mut argv = vec![substitute(command)];
        argv.extend(
            entry
                .get("args")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(substitute),
        );
        let env = entry
            .get("env")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), substitute(value))))
            .collect();

        servers.insert(
            name.clone(),
            LspEntry {
                command: Some(argv),
                extensions: Some(extensions.keys().cloned().collect()),
                disabled: false,
                env,
                initialization: entry.get("initializationOptions").cloned(),
            },
        );
    }

    servers
}

/// Reads one component file as JSON, or [`None`] — absent silently (most
/// plugins carry one or two surfaces, not five), unreadable or unparseable
/// with the warning the module doc promises.
fn read_component(path: &Path, plugin: &str) -> Option<Value> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(plugin, file = %path.display(), %error, "skipping an unreadable component file");
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(document) => Some(document),
        Err(error) => {
            tracing::warn!(plugin, file = %path.display(), %error, "skipping a component file that is not JSON");
            None
        }
    }
}

/// Splits a markdown file into its `---`-fenced frontmatter (as flat
/// `key: value` pairs) and its body — through [`ganja_tool::frontmatter`],
/// the same reader a skill's, an agent's and a command file's own loaders
/// use, so a plugin's markdown and a project's cannot parse two ways (that
/// module's own "two parsers waiting to disagree" hazard; this file carried
/// the second parser until it did disagree, on block scalars among others).
/// A file with no frontmatter is all body.
fn split_frontmatter(text: &str) -> (BTreeMap<String, String>, &str) {
    match ganja_tool::frontmatter::split(text) {
        Some((front, body)) => (ganja_tool::frontmatter::fields(front), body),
        None => (BTreeMap::new(), text),
    }
}

/// Merges every enabled plugin's contributions into a loaded [`Config`] —
/// the one point the load path calls, per surface rather than through
/// `Config::merge` (**D473**, `plugin-component-merge`; the module doc says
/// why that merge cannot carry these).
///
/// One log line per contributed component (name, origin, surface), so what a
/// session picked up from plugins is readable from its log.
///
/// # Errors
///
/// [`PluginError`] only for a state file that will not read — ganja's own
/// file. A plugin directory that has gone missing, and any component file
/// that will not parse, degrade to a warning instead; the module doc draws
/// that line.
pub(crate) fn apply(store: &Store, config: &mut Config) -> Result<(), PluginError> {
    let state = store.state()?;
    for (name, plugin) in &state.plugins {
        if !plugin.enabled {
            tracing::debug!(
                plugin = name,
                "an installed plugin is disabled and contributes nothing"
            );
            continue;
        }
        let root = store.plugin_root(name);
        if !root.is_dir() {
            tracing::warn!(
                plugin = name,
                root = %root.display(),
                "an installed plugin's directory is missing; it contributes nothing"
            );
            continue;
        }
        let contribution = collect(&root, name);

        // Hooks **append** beside config-declared groups for the same event,
        // never displacing them — the `.claude-plugin` spec's additive
        // behavior, and the exact opposite of the per-event-key replacement
        // `Config::merge` applies between tiers (D473). The shape itself is
        // Claude's own, already ganja's hooks shape (D456).
        for (event, groups) in contribution.hooks {
            tracing::info!(
                plugin = name,
                surface = "hook",
                component = event,
                "plugin contributed"
            );
            config.hooks.entry(event).or_default().extend(groups);
        }

        // MCP servers join under `plugin:<plugin>:<server>` — collision-free
        // by construction, since no sane config spells a server that way, and
        // ask-by-default inherited automatically because that is every MCP
        // tool's default. The guard is for the config that spelled one
        // anyway: explicit config wins, like every other collision here.
        for (server, entry) in contribution.mcp {
            let key = format!("plugin:{name}:{server}");
            match config.mcp.entry(key.clone()) {
                std::collections::btree_map::Entry::Occupied(_) => {
                    tracing::warn!(
                        plugin = name,
                        server = key,
                        "the config already declares this mcp key; the config's entry wins"
                    );
                }
                std::collections::btree_map::Entry::Vacant(slot) => {
                    tracing::info!(
                        plugin = name,
                        surface = "mcp",
                        component = server,
                        "plugin contributed"
                    );
                    slot.insert(entry);
                }
            }
        }

        // Skills roots concatenate onto `skills.paths`, after the config's
        // own entries so an explicitly named directory keeps outranking a
        // plugin's on a duplicated skill name.
        if let Some(skills) = contribution.skills_root {
            tracing::info!(plugin = name, surface = "skills", component = %skills.display(), "plugin contributed");
            config.skills.paths.push(skills.display().to_string());
        }

        // Commands join the `command` table under `<plugin>:<name>` — Claude
        // Code's own spelling for a plugin-provided command, so `/review` from
        // a plugin is typed `/<plugin>:review` in either tool. The prefix is
        // also what keeps this surface collision-free against the builtins and
        // against a project's own `commands/` files, neither of which can
        // carry a `:` they did not ask for. As everywhere else here, a config
        // that spelled the namespaced key anyway wins it, reported by name.
        for (command, entry) in contribution.commands {
            let key = format!("{name}:{command}");
            match config.command.entry(key.clone()) {
                std::collections::btree_map::Entry::Occupied(_) => {
                    tracing::warn!(
                        plugin = name,
                        command = key,
                        "the config already declares this command; the config's entry wins"
                    );
                }
                std::collections::btree_map::Entry::Vacant(slot) => {
                    tracing::info!(
                        plugin = name,
                        surface = "command",
                        component = command,
                        "plugin contributed"
                    );
                    slot.insert(entry);
                }
            }
        }

        // Agents merge per key with the explicit config winning, and the
        // collision reported by name — a config that defines `reviewer` has
        // said what `reviewer` is, and a plugin quietly rewriting it would
        // change what a running session does without anything saying so.
        for (agent, entry) in contribution.agents {
            match config.agent.entry(agent.clone()) {
                std::collections::btree_map::Entry::Occupied(_) => {
                    tracing::warn!(
                        plugin = name,
                        agent,
                        "the config already defines this agent; the config's definition wins"
                    );
                }
                std::collections::btree_map::Entry::Vacant(slot) => {
                    tracing::info!(
                        plugin = name,
                        surface = "agent",
                        component = agent,
                        "plugin contributed"
                    );
                    slot.insert(entry);
                }
            }
        }

        // LSP entries merge per key under the same rule, with one reading of
        // the `lsp` key's tri-state decided here: an **absent** key stops
        // meaning "no servers" for someone who explicitly installed a plugin
        // that ships one — the install is the opt-in act, the way placing a
        // skill file is — while an explicit `false` stays a refusal no plugin
        // may overturn, and `true` keeps meaning the builtins with the
        // plugin's entries merged over them by name (which is what a
        // `Servers` map already means).
        if !contribution.lsp.is_empty() {
            match &mut config.lsp {
                Some(LspConfig::Enabled(false)) => {
                    tracing::warn!(
                        plugin = name,
                        "the config disables lsp outright; the plugin's lsp entries are withheld"
                    );
                }
                Some(LspConfig::Servers(existing)) => {
                    for (server, entry) in contribution.lsp {
                        match existing.entry(server.clone()) {
                            std::collections::btree_map::Entry::Occupied(_) => {
                                tracing::warn!(
                                    plugin = name,
                                    server,
                                    "the config already configures this lsp server; the \
                                     config's entry wins"
                                );
                            }
                            std::collections::btree_map::Entry::Vacant(slot) => {
                                tracing::info!(
                                    plugin = name,
                                    surface = "lsp",
                                    component = server,
                                    "plugin contributed"
                                );
                                slot.insert(entry);
                            }
                        }
                    }
                }
                slot => {
                    for server in contribution.lsp.keys() {
                        tracing::info!(plugin = name, surface = "lsp", component = %server, "plugin contributed");
                    }
                    *slot = Some(LspConfig::Servers(contribution.lsp));
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
#[path = "plugin_tests.rs"]
mod tests;
