//! What a project asks of ganja, read from its config files.
//!
//! Spec: upstream `packages/opencode/src/config/config.ts` and
//! `packages/opencode/src/config/paths.ts`.
//!
//! The file is `ganja.toml`, the dialect the rest of this tree already speaks,
//! and it is the only one this module reads. The two names it used to go by,
//! [`LEGACY_FILES`], are refused by path with the command that converts them
//! — the same refuse-don't-ignore posture an unknown key gets, applied to the
//! format itself. The old dialect survives in exactly one place, [`legacy`],
//! which exists for `ganja config migrate` to read what it converts.
//!
//! Files are found in three places, and later beats earlier:
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
//!
//! That rule is what shapes [`ProviderConfig`]: upstream's `provider.<id>`
//! block carries `options.apiKey`, and ganja's entry carries `key_env`, the
//! *name* of the variable holding it, instead.

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::{env, fmt, fs, io};

use etcetera::base_strategy::{BaseStrategy as _, Xdg};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
    // The `permission` block parses straight into the types the permission
    // layer evaluates: a config file describes rules, and what a rule *is* is
    // not this module's to say.
    permission::PermissionConfig,
    project::Project,
    // Same reason: what wires a `provider` entry may name is the provider
    // layer's to say, and a second enum here would be a second opinion about
    // which wires exist.
    provider::Dialect,
};

/// Environment variable naming one extra config **file** to read.
///
/// The near-twin of [`CONFIG_HOME_ENV`], and the pair is worth reading
/// together: this one names a *file* to merge in between the global tier and
/// the project's, and does not move where any other file is looked for. The
/// other names the *directory* ganja's own things live in. Setting one says
/// nothing about the other.
pub const CONFIG_ENV: &str = "GANJA_CONFIG";

/// Environment variable naming the **directory** ganja keeps its own things
/// in, outright: the global `ganja.toml`, the global `AGENTS.md`, and the
/// `skills/` folder beneath it.
///
/// Distinct from [`CONFIG_ENV`], which names a file. Set, this outranks both
/// discovered locations unconditionally — see [`config_home`], which is the one
/// place any of the three is resolved.
pub const CONFIG_HOME_ENV: &str = "GANJA_CONFIG_HOME";

/// Directory ganja's global config lives in, under the XDG *config* home.
/// Every other store this crate keeps is state rather than configuration and
/// hangs off the data home instead.
const DIRECTORY: &str = "ganja";

/// The dot-directory under the home directory that [`config_home`] falls back
/// to, and the same name a project keeps ganja's things under.
///
/// One namespace at both levels — `~/.ganja` and `<project root>/.ganja` — so
/// somebody who has learned where ganja puts things in a checkout has learned
/// where it puts them in a home directory too.
const HOME_DIRECTORY: &str = ".ganja";

/// The directory a project keeps ganja's own things in, at its root.
///
/// Namespaced on purpose: a bare `skills/` at a repository root is a name
/// somebody else's project may already be using for something else, where
/// `.ganja/` says whose it is. See [`default_skill_dirs`] — and
/// [`crate::command`], which reads the same pair of homes for its command
/// files, which is why this name is visible to the crate rather than spelled a
/// second time there.
pub(crate) const PROJECT_DIRECTORY: &str = HOME_DIRECTORY;

/// What both of ganja's own homes call the folder skills live in. One
/// spelling, not the two upstream accepts — a second name to remember buys
/// nothing when neither is inherited from another tool.
const SKILLS_SUBDIR: &str = "skills";

/// The config file name, in every tier.
///
/// A list of one, still a list, because the merge order it feeds is upstream's
/// `toReversed()` and that shape is what makes the outermost ancestor merge
/// first so the closest directory wins. A second name would have to earn the
/// dialect that comes with it.
const FILES: [&str; 1] = ["ganja.toml"];

/// The names ganja's config used to go by, in the order the loader used to
/// probe for them.
///
/// Not read. Found, they are [`ConfigError::Legacy`] — naming the file and the
/// command that converts it — because a config file whose author still
/// believes it applies is exactly what the curated key set already refuses to
/// let happen one key at a time.
///
/// Public because the two commands that answer for such a file, `ganja config
/// migrate` and `ganja mcp add`, have to recognise one by the same list rather
/// than by a second copy of it.
pub const LEGACY_FILES: [&str; 2] = ["ganja.jsonc", "ganja.json"];

/// The dialect this build has left, and the one reader still allowed to speak
/// it. Its own module doc says who is left asking.
pub mod legacy;

pub use legacy::parse_options;

/// A config file could not be used.
///
/// There is no variant for "absent": a config file that is not there is not an
/// error, it is the common case. A file that *is* there and cannot be read or
/// understood is fatal, deliberately — upstream degrades a broken global config
/// to `{}` and this port does not, because a setting that silently stopped
/// applying is worse than a startup that says why.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file exists and is not valid in the dialect its name claims, or
    /// does not describe a config. The message carries the position the parser
    /// stopped at, built from the parser's own accessors rather than its
    /// `Display` — so the line it points at is named and never reproduced,
    /// which is what keeps a config's own bytes out of a message somebody
    /// pastes.
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
    /// A file in the dialect this build has left ([`LEGACY_FILES`], or a path
    /// named by [`CONFIG_ENV`] or `--config` that ends in one of their
    /// extensions).
    ///
    /// Refused rather than read, and refused rather than ignored: reading it
    /// would be two config formats forever, and ignoring it would leave its
    /// author believing settings apply that do not. The message carries the
    /// path that triggered it — not the format in general — because the walk
    /// can see more than one, and the one command that answers for it.
    #[error(
        "{} is in the config format this build has left; \
         run `ganja config migrate` to convert it to ganja.toml",
        path.display()
    )]
    Legacy {
        /// The file that has to be converted.
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
///
/// [`Serialize`] as well as [`Deserialize`], so a caller that *writes* an entry
/// — `ganja mcp add` building one, a listing asked for JSON — spells it out of
/// the same type the loader reads back rather than by hand.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
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

    /// Bytes one tool call to this server may return before the result is
    /// clamped, which the entry may set with `output_limit`.
    ///
    /// `fallback` is [`ganja_tool::truncate::MAX_CHARS`], the budget every
    /// other tool in the registry answers to — an entry that says nothing
    /// gets exactly that, so an MCP server is no more or less generous with
    /// the model's context than a builtin tool is by default.
    ///
    /// Plain [`u64`] rather than [`NonZeroU64`] like [`McpServer::timeout`]:
    /// `output_limit: 0` is refused by name in `check_mcp`, naming the
    /// server it describes, rather than by a generic "expected a nonzero"
    /// message from serde — a byte budget of nothing is a server whose every
    /// result would be entirely cut, which is worth saying plainly.
    #[must_use]
    pub fn output_limit(&self, fallback: u64) -> u64 {
        let asked = match self {
            Self::Local(local) => local.output_limit,
            Self::Remote(remote) => remote.output_limit,
        };

        asked.unwrap_or(fallback)
    }

    /// Refuses an entry that describes a server nothing could connect to, or
    /// one nothing could ever return a result from.
    ///
    /// Three things are decided here rather than later. A `command` with
    /// nothing in it is a server with no program, and finding that out one turn
    /// later hides it behind a status line. A remote URL that is neither
    /// `https` nor loopback is the same refusal [`crate::provider`] makes about
    /// a base URL and for the same reason — `headers` is where a token goes,
    /// and plain HTTP to somewhere else puts it on the wire in the clear.
    /// Literally the same: the predicate is
    /// [`crate::provider::reachable_in_the_clear`], and only the message below
    /// is this module's. And an `output_limit` of zero is a byte budget nothing
    /// could ever fit, discovered otherwise only the first time a tool call
    /// comes back empty for no reason anybody wrote down.
    ///
    /// **The one authority for all three.** It is a method rather than a
    /// private helper of the loader because three callers make exactly this
    /// judgment about exactly this type: the loader (`check_mcp`, per file),
    /// `ganja mcp add` before it writes an entry, and `ganja config
    /// import-opencode` before it writes a whole file. Each of the three used
    /// to re-spell the refusals, which is three places for them to drift into
    /// disagreeing about what the *next launch* will accept — and the writer
    /// exists precisely to not write a file the next launch refuses.
    ///
    /// `name` is the server's key in the `mcp` table, and appears in every
    /// message. Neither URL message quotes the URL: a remote entry is
    /// configuration, and configuration is allowed to carry a credential in its
    /// userinfo, so echoing one back is how it reaches a log.
    ///
    /// # Errors
    ///
    /// The refusal, spelled for whoever has to fix the entry.
    pub fn check(&self, name: &str) -> Result<(), String> {
        let output_limit = match self {
            Self::Local(local) if local.command.is_empty() => {
                return Err(format!("mcp server \"{name}\" has an empty command"));
            }
            Self::Local(local) => local.output_limit,
            Self::Remote(remote) => {
                let parsed = Url::parse(&remote.url)
                    .map_err(|error| format!("mcp server \"{name}\" has no valid url: {error}"))?;
                if !crate::provider::reachable_in_the_clear(&parsed) {
                    return Err(format!(
                        "mcp server \"{name}\" must be reached over https, or over http to \
                         loopback; anything else puts its headers on the wire in the clear"
                    ));
                }
                remote.output_limit
            }
        };
        // A budget of nothing is not a budget: every result from this server
        // would be entirely cut, which is not a thing anybody means to write.
        if output_limit == Some(0) {
            return Err(format!(
                "mcp server \"{name}\" has an output_limit of 0; a byte budget of nothing \
                 refuses every result"
            ));
        }

        Ok(())
    }
}

/// A local MCP server: a command this session runs and talks to over pipes.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
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
    /// Byte budget on one tool result; see [`McpServer::output_limit`].
    pub output_limit: Option<u64>,
}

/// A remote MCP server: an endpoint this session posts to.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
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
    /// Byte budget on one tool result; see [`McpServer::output_limit`].
    pub output_limit: Option<u64>,
    /// Discovers this server's own authorization server and logs in against
    /// it, rather than sending a static [`headers`](Self::headers) entry —
    /// see [`crate::mcp`]'s "OAuth" section for the flow this unlocks
    /// (**D466**). A marker rather than a settings object: nothing about the
    /// flow is configurable in this build yet, so there is nothing to write
    /// beyond naming that the server wants it.
    pub oauth: Option<McpOauth>,
}

/// Turns on OAuth for a [`McpRemote`] entry. Carries nothing today —
/// discovery finds the endpoints, and dynamic registration finds the client —
/// but is its own type rather than a bare `bool` so a future field (a
/// preregistered `client_id`, a scope) has somewhere to land without another
/// migration of this shape.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpOauth {}

/// What `enabled` means when an entry does not say: upstream connects unless
/// told not to (`mcp/index.ts:514-517`).
fn connect_by_default() -> bool {
    true
}

/// One endpoint a config declares, and how to talk to it.
///
/// Spec: upstream's `provider.<id>` block, narrowed to what this build can act
/// on. Three of its keys have counterparts here — `options.baseURL`
/// (`provider/provider.ts:356`) is [`base_url`](Self::base_url),
/// `options.headers` is [`headers`](Self::headers), and the `npm` package that
/// decides which SDK loads the provider is [`dialect`](Self::dialect), spelled
/// as the wire rather than as somebody's `node_modules`. The fourth,
/// `options.apiKey`, deliberately has **no** counterpart: it holds a key, and
/// ganja's keys travel the environment or `auth.json` in a `SecretString` end
/// to end. [`key_env`](Self::key_env) names the variable instead.
///
/// The entry is a **curated key set** like every other shape here: a field
/// this build does not have is refused by name rather than ignored, because
/// an ignored endpoint setting is one whose author still believes it applies.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Which request/response mapping the endpoint speaks. Required: the
    /// wires encode a message differently, and guessing from a URL is how a
    /// session sends an Anthropic body to a chat-completions server.
    pub dialect: Dialect,
    /// Where it lives. Refused unless it is `https`, or `http` to loopback —
    /// the rule [`crate::provider`] applies to every base URL, for the reason
    /// it applies there: the credential travels in a header on every request.
    pub base_url: String,
    /// The environment variable holding the endpoint's key, consulted before
    /// the credential store.
    ///
    /// Absent means the store alone answers, under this entry's own id —
    /// `ganja auth login <id>` writes exactly there.
    pub key_env: Option<String>,
    /// Headers sent with every request, beside the credential.
    ///
    /// Empty for an endpoint that asks for nothing but the key, which is most
    /// of them. This is also somewhere a token fits, which is the second
    /// reason [`base_url`](Self::base_url) is held to the rule above.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
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

/// One group of hooks for one event: which subjects it applies to, and what to
/// run for them.
///
/// Spec: Claude Code's `hooks` block (2.1.x), whose shape this keeps verbatim
/// (**D456**). The array-of-groups shape is that spelling: one event maps to a
/// list of `{ matcher, hooks }` objects rather than to a flat list of
/// commands, because the matcher is a property of the group and several groups
/// may match one call.
///
/// The shape is kept for migration familiarity — somebody who knows Claude's
/// block knows this one — but the *paste* it used to allow is gone with the
/// format, and was traded deliberately rather than lost: `ganja config
/// import-claude-hooks` reads a `.claude/settings.json`, maps its hooks block
/// onto a `ganja.toml`, and reports every event, handler type and field it
/// could not carry. A command that names what it skipped is worth more than a
/// paste that silently drops what this build has no `hooks` roster for.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HookMatcher {
    /// A regular expression the event's subject must match — the tool name for
    /// `PreToolUse`/`PostToolUse`, the trigger for `PreCompact`, the source for
    /// `SessionStart`. Absent, or empty, matches everything; so does any value
    /// at all on an event with no subject to match against.
    pub matcher: Option<String>,
    /// What to run when it matches, all of them concurrently.
    #[serde(default)]
    pub hooks: Vec<HookHandler>,
}

/// One handler in a [`HookMatcher`], tagged by `type` — the same internally
/// tagged shape [`McpServer`] uses, and for the same reason: an unknown `type`
/// is then refused by serde naming the value and listing what would have
/// worked, rather than surfacing as a missing field belonging to a kind of
/// handler this build cannot run at all.
///
/// One variant today. Claude's `http`, `prompt` and `agent` handlers are
/// recorded follow-ups rather than silently accepted keys.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HookHandler {
    /// A command line handed to a shell, with the event's JSON envelope on its
    /// standard input.
    Command(HookCommand),
}

/// A `type: "command"` handler.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HookCommand {
    /// The command line, run by the same POSIX shell the `bash` tool uses.
    /// Refused empty at load: a handler with nothing to run is not one.
    pub command: String,
    /// How long it may take, in **seconds**. Absent is
    /// [`crate::hook::DEFAULT_TIMEOUT`].
    pub timeout: Option<u64>,
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

/// What [`Config::defer_threshold`] answers when no tier wrote the key:
/// Claude Code's own order of magnitude for a roster worth deferring, small
/// enough that a genuinely giant server defers and large enough that a
/// typical handful of servers never notices the machinery exists.
pub const DEFAULT_TOOL_DEFER_THRESHOLD: usize = 32;

/// Everything the config files asked for, merged.
///
/// The curated posture — an unknown key is refused **by name** — was
/// upstream's too until v1.18.22 moved to ignoring excess properties
/// (`config/parse.ts`, #41312). Ganja keeps the refusal deliberately: an
/// ignored setting is one whose author still believes it applies, and the
/// schema drift tests in `tests/config_schema.rs` lean on serde's own
/// enumeration of what exists.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Editor schema reference. Read so that writing one is not an error;
    /// nothing consults it.
    ///
    /// A quoted `"$schema"` key is legal TOML, so the field survives the
    /// format change unchanged — but it is no longer how an editor finds the
    /// schema. Taplo, which is what completes a `.toml` file, takes a
    /// `#:schema <url>` directive on the document's first line instead, and
    /// reads the same `schema/ganja-config.schema.json` this key used to point
    /// at. That directive is a comment, so it reaches no parser and needs
    /// nothing here; this key stays readable for the reason it always was —
    /// refusing what somebody's editor wrote would be a startup failure about
    /// an annotation.
    #[serde(rename = "$schema")]
    pub schema: Option<String>,
    /// Default model, `"provider/model"`, split on the **first** slash so that
    /// `openrouter/anthropic/claude-3` names the model `anthropic/claude-3`.
    ///
    /// The provider half **binds**: this key names a model of that provider's
    /// and of nobody else's, so a session running as another one leaves it
    /// alone rather than stripping the prefix and forwarding the rest — see
    /// [`model_bound_to`].
    pub model: Option<String>,
    /// Cheaper model for the requests a session makes **about** itself, which
    /// is exactly one request: the title.
    ///
    /// Spelled `"provider/model"` like [`model`](Self::model) and bound the
    /// same way ([`model_bound_to`]): a spec naming the provider this session
    /// runs as — or a bare one — is what the title request asks for, and one
    /// naming another provider is left alone. Upstream instead resolves the
    /// spec in whichever provider it names and can title across providers;
    /// ganja's title rides the session's own wire, so it does not
    /// (**D490**, `small-model-provider-bound`, stated at `session.rs`'s
    /// `title_model`). A model the wire then refuses
    /// costs one round trip and falls back to the session's own model through
    /// the retry `session.rs`'s `request_title` already had; a config carrying
    /// none leaves the pick to the catalog's cheapest chat-capable row.
    ///
    /// **Titles only, and summaries deliberately not**, which is upstream's
    /// own division: `provider.ts`'s `getSmallModel` is read by `ensureTitle`
    /// in `packages/opencode/src/session/prompt.ts:220` and by nothing else in
    /// a session, while `compaction.create` is handed `lastUser.model` — the
    /// turn's own. Upstream's `specs/v2/config.md:210` says the same in prose.
    pub small_model: Option<String>,
    /// Provider a session runs as when no other tier names one: a builtin id,
    /// or one of [`provider`](Self::provider)'s.
    ///
    /// Not a key upstream has. It supplies only the provider — the model still
    /// comes from the named-model tiers or the catalog's default — and it sits
    /// *below* [`model`](Self::model)'s provider half in
    /// [`crate::provider::select`]'s chain, because a key that names both is
    /// more specific than one that names one. An id nothing ships or declares
    /// is refused at selection rather than here: the `provider` table it may
    /// point into can arrive from another tier, so a per-file check would
    /// refuse a config that merges into a valid one.
    pub default_provider: Option<String>,
    /// Agent a session starts on.
    pub default_agent: Option<String>,
    /// Catalog effort a *fresh* session starts under, as `/effort` and
    /// `run --effort` name them.
    ///
    /// A **default, not an override**: a resumed session runs under the effort
    /// its own row stored, and this seeds only the sessions that carry none.
    /// Nothing is validated here — which names exist depends on the model the
    /// tiers below settle on, and a name that model's catalog row does not
    /// carry is cleared at adoption exactly as a model switch clears one,
    /// never refused. Not a key upstream has; upstream pins efforts per agent,
    /// where ganja makes them a property of the session.
    pub effort: Option<String>,
    /// Agent definitions, by name.
    #[serde(default)]
    pub agent: BTreeMap<String, AgentConfig>,
    /// How this session *runs* agents, as opposed to what they are; see
    /// [`AgentsConfig`].
    #[serde(default)]
    pub agents: AgentsConfig,
    /// How this session runs the *teammates* it spawns, as opposed to what
    /// they are; see [`TeammateConfig`] (**D509**).
    #[serde(default)]
    pub teammates: TeammateConfig,
    /// What this session, leading a team, does with a peer message arriving
    /// from **outside** that team, before anything is delivered (**D523**):
    /// `"accept"` delivers it, `"hold"` parks it for a person's review,
    /// `"refuse"` drops it.
    ///
    /// **Absent is unset**, and unset is not a fourth policy: the engine then
    /// decides per receiver class, and an explicit value here always wins
    /// over that default — v2 §"Explicit values", evidence 680146-680160.
    ///
    /// # Which tier may say what
    ///
    /// The reference resolves this key through a chain ganja's tiers map
    /// onto almost exactly — *almost*, because **ganja has no managed-policy
    /// tier**: nothing here answers to the reference's administrator-owned
    /// top tier, and this table says so rather than leaving the row implied.
    ///
    /// | reference tier | ganja tier | rule |
    /// |---|---|---|
    /// | managed policy | **none** | — |
    /// | the `--settings` file | the file [`CONFIG_ENV`] or `--config` names | outranks the global tier, by merge order |
    /// | user settings | the global config home | ordinary overlay |
    /// | repository settings | the project files | **tighten-only**, per file |
    ///
    /// The project tier is the one tier whose author is the checkout rather
    /// than the person running it, which is why it diverges from this file's
    /// ordinary later-wins: a project file replaces the standing value only
    /// when **strictly more severe** on the order `accept (0) < hold (1) <
    /// refuse (2)` — a repository can tighten a user's choice but never
    /// loosen it (v2 §"Source precedence and repository tightening (`MRf`)",
    /// evidence 620378-620481). An unset standing value has nothing to
    /// loosen, so the first tier to say anything — a project file included —
    /// establishes the value; the ancestor walk can find several project
    /// files, and each may only escalate further. `merge_project` is the
    /// seam.
    ///
    /// Not a key a plugin can contribute: [`crate::plugin`]'s `apply` merges
    /// per surface, and D473's six surfaces do not include it.
    ///
    /// The sender-side sibling is [`Config::teamless_send`] (**D531**, live
    /// since **D543**): `"hold"` here configures **receiver-side** human
    /// review of what arrives, while that key's `"ask"` adds **sender-side**
    /// dialogs on what leaves — two independent knobs on the two ends of one
    /// wire.
    pub cross_session_inbound: Option<InboundPolicy>,
    /// How long a **held** peer message's review dialog waits for a person
    /// before it expires (**D523**): `"60s"`, `"5m"`, `"10m"`, or
    /// `"never"`, which maps to no deadline at all.
    ///
    /// **Absent is `"5m"`** — [`Config::dialog_expiry()`] is what reads it —
    /// and an [`Option`] rather than a bare value for [`Config::snapshot`]'s
    /// reason: a tier that says nothing has to leave the tier below it
    /// alone. The vocabulary, the default and the trusted-sources
    /// restriction are all v2 §"`dialogExpiry` is narrower than its name
    /// suggests", evidence 322685-322708.
    ///
    /// **Trusted sources only.** The global config home and the file
    /// [`CONFIG_ENV`] or `--config` names may set it; a project file that
    /// sets it fails the load naming this key and that file — a checkout
    /// must not stretch or shrink the human review window. The reference's
    /// own restriction is the same shape (local and project settings cannot
    /// set it, same section); ganja refuses loudly where the reference
    /// ignores, because this file's whole posture is that an ignored setting
    /// is one its author still believes applies.
    ///
    /// The reference also honors an environment override for this deadline
    /// (`CLAUDE_CODE_USER_DIALOG_TIMEOUT_MS`, same section), and it is
    /// **deliberately not ported**: ganja's environment surface is curated,
    /// no ruling asked for it, and a test that needs a short deadline sets
    /// this key. Not a key a plugin can contribute either:
    /// [`crate::plugin`]'s `apply` merges per surface, and D473's six
    /// surfaces do not include it.
    pub dialog_expiry: Option<DialogExpiry>,
    /// Whether a session that leads **no team** asks a person before its
    /// `send_message` leaves for another session (**D531**, user-ratified
    /// 2026-08-26): `"unasked"` sends as any tool call the rules allow,
    /// `"ask"` raises the ordinary permission dialog on each send.
    ///
    /// **Absent is `"unasked"`** — [`Config::teamless_send()`] is what reads
    /// it — and the default is deliberate: every cross-session delivery
    /// already terminates in the *receiver's* admission gate (D523–D525),
    /// built exactly for the foreign socket peer, so the sender-side dialog
    /// this key can add re-asks a question the receiving side answers with
    /// machinery. In a session that **holds a team** the key has no effect
    /// at all: in-team sends stay D498's, and the engine computes the
    /// tool's effective default from the live team state, so a team
    /// spawning mid-session reverts to that posture with no rule mutation.
    ///
    /// The dialogs `"ask"` raises are ordinary storable permission answers:
    /// a stored "always allow" outranks the computed default exactly as it
    /// does for every tool, and a deny rule still denies. One deliberate
    /// asymmetry: the D479 `--yolo`/`--auto` drain **does** auto-answer
    /// these dialogs — they are ordinary asks with `PermissionId`s — unlike
    /// the held-message dialog, whose immunity is structural (D524). A
    /// bypass session that also wants sender-side asks is asking for two
    /// contradictory things, and the flags win as they do everywhere.
    ///
    /// # Which tier may say what
    ///
    /// Ganja's own key, no reference chain to map (**D531**); the tier rule
    /// is the `cross_session_inbound` precedent applied:
    ///
    /// | ganja tier | rule |
    /// |---|---|
    /// | the file [`CONFIG_ENV`] or `--config` names | outranks the global tier, by merge order |
    /// | the global config home | ordinary overlay |
    /// | the project files | **tighten-only**, per file |
    ///
    /// The project tier merges by severity on the order `unasked (0) <
    /// ask (1)`: a checkout may demand more human oversight of what its
    /// sessions send out, and can never loosen a person's `"ask"` back to
    /// `"unasked"` — the replace happens only when strictly more severe and
    /// silently fails to loosen otherwise, exactly as the sibling key's
    /// project-tier `accept` fails to loosen `refuse`. `merge_project` is
    /// the seam, and `tighten` is the one spelling both keys merge
    /// through. Not a key a plugin can contribute: [`crate::plugin`]'s
    /// `apply` merges per surface, and D473's six surfaces do not include
    /// it.
    ///
    /// The receiver-side sibling is [`Config::cross_session_inbound`]:
    /// that key configures **receiver-side** human review of what arrives,
    /// while this one adds **sender-side** dialogs on what leaves — two
    /// independent knobs on the two ends of one wire.
    ///
    /// # What it does in this build
    ///
    /// It acts — since **D543** (2026-08-30, bead `ganja-code-3tng`), and
    /// this paragraph is kept because for two days it said the opposite.
    /// **D542** had found the key inert: the only assembly that ever
    /// installed a solo postbox was deleted as structurally unreachable, and
    /// a flag no installer set meant the computed default never returned
    /// `Ask`. D543 removed the flag rather than the key: a session is
    /// teamless when **its own registry holds no members**, read live at
    /// each call, which every interactive session that has spawned no
    /// teammate is. So `"ask"` now really does put a dialog in front of each
    /// `send_message` that leaves such a session, and a teammate spawning
    /// mid-session really does revert it to D498's ladder.
    pub teamless_send: Option<TeamlessSend>,
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
    /// What the terminal frontend does beyond drawing frames; see
    /// [`TuiConfig`]. Kept in core for `keybinds`'s reason: the curated key
    /// set lives here, even though only the TUI acts on the value.
    #[serde(default)]
    pub tui: TuiConfig,
    /// What one gateway is asked to run on its own side; see
    /// [`OpenRouterConfig`] (**D489**).
    ///
    /// A provider-specific top-level key, which `tui` is the precedent for: a
    /// curated table exists where the thing it configures has exactly one
    /// consumer and no other table can honestly hold it. The `provider` table
    /// below is *not* that place — it declares compat endpoints, and the
    /// builtin this configures is not one of its entries.
    #[serde(default)]
    pub openrouter: OpenRouterConfig,
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
    /// Defer whole MCP servers' tool schemas — largest servers first — until
    /// the advertised `mcp__*` total is at or under this (**D492**).
    ///
    /// **Absent is 32** ([`Config::defer_threshold`] is what reads it). `0`
    /// defers every server and a huge value disables deferral — both are
    /// meaningful, so nothing beyond serde's own unsigned-integer check is
    /// refused. Top-level rather than a key inside [`mcp`](Self::mcp), whose
    /// map a server actually named `defer_threshold` would collide with — the
    /// D462 lesson. Counted in tools, not schema bytes: the unit a person can
    /// count in `/mcp`'s own listing. Size is not coldness — the deferred
    /// giant may be the main server; its tools return to the roster the
    /// moment they are touched, and raising this key is the one-line off
    /// switch.
    pub tool_defer_threshold: Option<usize>,
    /// Commands this session runs at the nine moments [`crate::hook`] names,
    /// keyed by the event's own spelling (`"PreToolUse"`, `"SessionStart"`, …).
    ///
    /// A key nothing answers to is refused by name at load, like every other
    /// curated name here: a hook that never fires is indistinguishable from a
    /// hook that fires and does nothing, and only one of those is worth telling
    /// somebody about.
    ///
    /// These run with the **user's own authority** and pass no permission
    /// gate — see [`crate::hook`] for whose decision that is and why the
    /// authorship of this file is the trust boundary.
    #[serde(default)]
    pub hooks: BTreeMap<String, Vec<HookMatcher>>,
    /// Language servers this session may run. **Absent is none of them**; see
    /// [`LspConfig`].
    pub lsp: Option<LspConfig>,
    /// Endpoints this build can talk to besides the ones it ships, by id.
    ///
    /// The id is what a session names — `GANJA_PROVIDER`, `--model <id>/…`,
    /// this file's own `model` key — and what a stored credential and a
    /// permission rule are filed under, so it is the whole identity of the
    /// provider. [`crate::provider::select`] consults this after the builtins;
    /// an entry naming a builtin is refused here rather than left to lose
    /// silently there.
    #[serde(default)]
    pub provider: BTreeMap<String, ProviderConfig>,
    /// What the `webfetch` tool may reach; see [`WebfetchConfig`].
    #[serde(default)]
    pub webfetch: WebfetchConfig,
    /// Where this session looks for skills besides ganja's own two homes; see
    /// [`SkillsConfig`] and [`default_skill_dirs`].
    #[serde(default)]
    pub skills: SkillsConfig,
    /// Whether this session carries its project's own memory — the
    /// `MEMORY.md` index and the topic files beside it, kept under the
    /// project's data directory and maintained by the model itself
    /// (**D478**, declared at [`crate::instruction::memory_dir`]).
    ///
    /// **Absent is off**, which is the deliberate divergence from Claude
    /// Code's default-on: switching it on gives a session standing prompt
    /// weight it did not have and a door to write files *outside* the
    /// worktree, and neither is something a checkout should acquire by being
    /// opened. [`Config::memory_enabled`] is what reads it.
    ///
    /// An [`Option`] rather than a bare `bool` for [`Config::snapshot`]'s
    /// reason: a tier that says nothing has to leave the tier below it alone,
    /// where a plain `bool` would have every tier assert a default and a
    /// project file with no opinion would switch a global `true` back off.
    pub memory: Option<bool>,
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
    /// The tier that established [`Config::cross_session_inbound`]'s winning
    /// value (**D523**), recovered at the one seam that still sees tiers:
    /// the merge itself keeps only the winner, and the admission gate's
    /// resolver wants the pair — the review surfaces name which tier said
    /// `hold`, and [`Config::inbound_policy`] is what hands it over.
    ///
    /// Not a config key — `#[serde(skip)]`, `overrides`' own pattern, so the
    /// schema and the drift test never see it — and filled only by
    /// [`Config::load_with`]: a `Config` built by hand carries [`None`] here,
    /// which the accessor reads as the global tier, the least specific claim.
    #[serde(skip)]
    pub cross_session_inbound_source: Option<ganja_protocol::PolicySource>,
}

/// How this session runs the agents it has, rather than what those agents are.
///
/// **`agents` and [`Config::agent`] are two different keys and that is
/// deliberate**, not a typo either way. `agent` is a *map* keyed by agent name,
/// so a setting written into it would collide with an agent called
/// `concurrency`; this is the settings object beside it. The plural reads as
/// "about the agents", the singular as "this agent, by name" — which is also
/// how a reader tells at a glance which one a config line meant.
///
/// Not a key upstream has: upstream runs one subagent at a time, so it has
/// nothing to size (**D462**).
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentsConfig {
    /// How many `task` calls from one assistant step may run at the same time.
    ///
    /// **Absent is [`AgentsConfig::DEFAULT_CONCURRENCY`]**, and an
    /// [`Option`] rather than a bare number for the reason
    /// [`Config::snapshot`] is one: a tier that says nothing has to leave the
    /// tier below it alone. Refused at load when it is zero — a cap of nothing
    /// is a session where every delegation hangs, which is not a thing anybody
    /// means to write.
    pub concurrency: Option<usize>,
}

impl AgentsConfig {
    /// How many children run at once when nothing says otherwise.
    ///
    /// Small on purpose. Each child is a whole agent loop — its own provider
    /// requests, its own tool calls, its own permission dialogs crossing to the
    /// one person watching — so the ceiling that matters is not this machine's
    /// cores but the vendor's rate limit and the reader's attention. Four is
    /// enough that the common "look at these three files" fan-out never queues,
    /// and low enough that a model asking for a dozen at once still spends the
    /// session's token budget in batches somebody can follow. Whoever wants
    /// more can say so; whoever wants one gets upstream's behavior back.
    pub const DEFAULT_CONCURRENCY: usize = 4;

    /// The cap this config asks for, or the default when it asks for nothing.
    #[must_use]
    pub fn concurrency(&self) -> usize {
        self.concurrency.unwrap_or(Self::DEFAULT_CONCURRENCY)
    }
}

/// How this session runs the teammates it spawns, rather than what they are
/// (**D509**).
///
/// **`teammates` is plural for [`AgentsConfig`]'s reason**, and the plural is
/// load-bearing rather than taste: this file already spells that distinction
/// once — [`Config::agent`] is the name-keyed map, [`Config::agents`] is the
/// settings object beside it — so a settings object called `teammate` would
/// invert the very pair it copies, and would spend the singular a later
/// per-name teammate map will want.
///
/// Not a key upstream has, and not one Claude Code has either: upstream
/// opencode has no teammates at all, and Claude's own teammates are agents of
/// its harness rather than somebody else's CLI, so nothing over there has a
/// turn to bound.
///
/// Its one key has outlived the spawn shape it was written for. Since P28
/// (**D512**) what `shim_turn_timeout` bounds is the *headless* shim
/// machinery, which no spawn door in this build reaches; the field's own doc
/// says why a key nobody's session reads is still spelled here rather than
/// removed.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TeammateConfig {
    /// How long one **headless** foreign-CLI teammate's turn may run before
    /// the shim ends its process group and mails the lead, in **seconds**.
    ///
    /// **Absent is the per-CLI default** the shim derives — this is the one
    /// number a person can move without a source edit, so it overrides all
    /// three at once. An [`Option`] rather than a bare number for
    /// [`AgentsConfig::concurrency`]'s reason: a tier that says nothing has to
    /// leave the tier below it alone. Refused at load when it is zero — a
    /// deadline of nothing is a teammate whose every turn is killed before it
    /// speaks.
    ///
    /// Seconds rather than milliseconds follows
    /// [`HookCommand::timeout`](crate::config::HookCommand::timeout), the
    /// closest analogue in this file: a whole external process's budget.
    ///
    /// # What it governs, and what it no longer reaches (**D512**)
    ///
    /// It is the deadline of `ganja_teammate_local::shim`, the machinery that
    /// drives a foreign CLI *headlessly* through a pipe. Since P28 every
    /// codex, agy and grok spawn instead opens that CLI's own native TUI in a
    /// tmux pane (`ganja_teammate_local::shim_tui`), and a pane-mode shim runs
    /// under **no per-turn deadline at all** — nothing on that path reads this
    /// key, so on this build the number a config writes here moves nothing a
    /// session does. Since **D538** it is not even read at assembly: the
    /// deadline is a headless backend's own constructor argument, and no
    /// production caller builds one.
    ///
    /// It stays curated anyway, and not out of sentiment: the headless
    /// machinery is still in the tree and still driven by the testkit, and a
    /// key spelled in a config file somebody already has must keep loading —
    /// `deny_unknown_fields` refuses the *whole file* over one name it does
    /// not know, so retiring the key would turn a stale line into a session
    /// that will not start.
    ///
    /// # Why only that shape of teammate ever had one (**D509**)
    ///
    /// Every other duration on the teammate path is an unwind budget, and no
    /// native teammate's *turn* is bounded at all. A headless foreign child
    /// earned a bound because it was the only shape whose progress ganja could
    /// not observe: an in-process teammate streams events into this process
    /// and a `ganja` pane is a ganja whose own status bar a person can look
    /// at, while a shim child that has stopped writing to a pipe is
    /// indistinguishable from one that is thinking. The deadline is what
    /// turned that ambiguity into mail — which is also why a pane took it
    /// away rather than inheriting it: the CLI's own TUI is a thing a person
    /// can look at. `ganja_teammate_local::shim_tui`'s module doc owns that half.
    pub shim_turn_timeout: Option<u64>,
    /// The shell a fresh teammate pane holds until its launch line is typed
    /// into it (**D520**), as a command line: `"/bin/zsh -f"`, or just
    /// `"/bin/bash"`. Absent is `/bin/sh -s`.
    ///
    /// Resolved **once** by the frontend that assembles this session's
    /// backends and handed to the two pane backends there (**D538**; it rode
    /// the registry and every `SpawnSpec` until then), as
    /// `ganja_teammate_local::pane::PaneShell` rather than as a config type — the
    /// D520 rule that no backend names one is kept while the state moves.
    ///
    /// Split into words the way a shell would (`shlex`), and kept at two
    /// words or more by the pane door — `-s` is appended to a lone program —
    /// because tmux runs a one-word command through the person's login
    /// shell, whose startup files re-import exactly what the enumerated
    /// environment withheld (measured 2026-08-17; `pane::SHELL`'s own doc
    /// carries the story). **A shell named here runs its own startup files
    /// too**, and what they export enters the pane: that is the person's
    /// choice, made in this key, and `-f` (zsh) or `--norc` (bash) is how a
    /// shell is told not to. The launch line is typed as a POSIX shell reads
    /// it, so the shell named has to be one. Refused at load when it is
    /// empty or cannot be split into words.
    pub shell: Option<String>,
    /// How much of the window's width the teammates' column takes when the
    /// first teammate opens it, in **percent** — the lead keeps the rest.
    /// Absent is 65 (`| lead 35% | teammates 65% |`, user directive
    /// 2026-08-25; 70 before that). Refused at load outside 1..=99: a column
    /// of nothing or of everything is a lead with no screen or no teammate.
    ///
    /// Resolved once and handed to the pane backends at assembly, exactly as
    /// [`TeammateConfig::shell`] is and for the same reason (**D538**), as
    /// `ganja_teammate_local::pane::PaneShare`.
    pub pane_share: Option<u8>,
}

impl TeammateConfig {
    /// The pane shell's words, when the config names one — the command line
    /// split as a shell would split it. [`None`] leaves the pane door's own
    /// default alone.
    ///
    /// Read once, where this session's backends are assembled.
    #[must_use]
    pub fn pane_shell(&self) -> Option<Vec<String>> {
        self.shell.as_deref().and_then(shlex::split).filter(|words| !words.is_empty())
    }

    /// The teammates' column's share of the width, in percent, when the
    /// config names one. [`None`] leaves the pane door's own default alone.
    ///
    /// Read once, beside [`TeammateConfig::pane_shell`].
    #[must_use]
    pub fn pane_share(&self) -> Option<u8> {
        self.pane_share
    }

    /// The per-turn deadline this config asks for, if it asks for one.
    ///
    /// [`None`] leaves the per-CLI default alone, which is where the real
    /// numbers live — this file has no business knowing that `agy` is four
    /// minutes.
    #[must_use]
    pub fn shim_turn_timeout(&self) -> Option<std::time::Duration> {
        self.shim_turn_timeout.map(std::time::Duration::from_secs)
    }
}

/// Policy for a peer message arriving from outside this session's own team
/// (**D523**): the vocabulary of the `cross_session_inbound` key, and of
/// nothing else.
///
/// [`InboundPolicy::severity`] carries the tightening order the project tier
/// merges under; the config spellings are the lowercase variant names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboundPolicy {
    /// Deliver it.
    Accept,
    /// Park it for a person's review.
    Hold,
    /// Drop it.
    Refuse,
}

impl InboundPolicy {
    /// Where this value sits on the tightening order a project file merges
    /// under: `accept (0) < hold (1) < refuse (2)` — v2 §"Source precedence
    /// and repository tightening (`MRf`)", evidence 620378-620481. Spelled
    /// as a number rather than an `Ord` derive, so the order cannot drift
    /// under a reordered declaration.
    #[must_use]
    pub const fn severity(self) -> u8 {
        match self {
            Self::Accept => 0,
            Self::Hold => 1,
            Self::Refuse => 2,
        }
    }
}

impl<'de> Deserialize<'de> for InboundPolicy {
    /// Hand-written for [`Notifications`]'s reason taken one step further:
    /// the derived refusal names the value and the vocabulary but not the
    /// key, and `cross_session_inbound` is a policy line somebody will grep
    /// a config for. The type belongs to exactly one key, which is what lets
    /// the key's name live in its error honestly.
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// The one spelling the key takes.
        struct Shape;

        impl Visitor<'_> for Shape {
            type Value = InboundPolicy;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("cross_session_inbound as \"accept\", \"hold\" or \"refuse\"")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                match value {
                    "accept" => Ok(InboundPolicy::Accept),
                    "hold" => Ok(InboundPolicy::Hold),
                    "refuse" => Ok(InboundPolicy::Refuse),
                    other => Err(E::custom(format!(
                        "cross_session_inbound is \"accept\", \"hold\" or \"refuse\", \
                         not {other:?}"
                    ))),
                }
            }
        }

        deserializer.deserialize_str(Shape)
    }
}

/// How long a held peer message's review dialog waits (**D523**): the
/// vocabulary of the `dialog_expiry` key, and of nothing else.
///
/// The four spellings are the reference's own — v2 §"`dialogExpiry` is
/// narrower than its name suggests", evidence 322685-322708 — and
/// [`DialogExpiry::deadline`] is their mapping onto wall-clock time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DialogExpiry {
    /// `"60s"`.
    OneMinute,
    /// `"5m"`, what an absent key means.
    #[default]
    FiveMinutes,
    /// `"10m"`.
    TenMinutes,
    /// `"never"`: no deadline at all.
    Never,
}

impl DialogExpiry {
    /// The wall-clock deadline this value names, or [`None`] for
    /// [`DialogExpiry::Never`] — `never` maps to no deadline.
    #[must_use]
    pub const fn deadline(self) -> Option<std::time::Duration> {
        match self {
            Self::OneMinute => Some(std::time::Duration::from_secs(60)),
            Self::FiveMinutes => Some(std::time::Duration::from_secs(5 * 60)),
            Self::TenMinutes => Some(std::time::Duration::from_secs(10 * 60)),
            Self::Never => None,
        }
    }
}

impl<'de> Deserialize<'de> for DialogExpiry {
    /// Hand-written for [`InboundPolicy`]'s reason: the refusal names the
    /// key, which one type serving one key can do honestly.
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// The one spelling the key takes.
        struct Shape;

        impl Visitor<'_> for Shape {
            type Value = DialogExpiry;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("dialog_expiry as \"60s\", \"5m\", \"10m\" or \"never\"")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                match value {
                    "60s" => Ok(DialogExpiry::OneMinute),
                    "5m" => Ok(DialogExpiry::FiveMinutes),
                    "10m" => Ok(DialogExpiry::TenMinutes),
                    "never" => Ok(DialogExpiry::Never),
                    other => Err(E::custom(format!(
                        "dialog_expiry is \"60s\", \"5m\", \"10m\" or \"never\", \
                         not {other:?}"
                    ))),
                }
            }
        }

        deserializer.deserialize_str(Shape)
    }
}

/// Whether a teamless session's `send_message` asks a person first
/// (**D531**, user-ratified 2026-08-26): the vocabulary of the
/// `teamless_send` key, and of nothing else.
///
/// Ganja's own key, not a port — the posture and both spellings are the
/// 2026-08-26 ruling, with [`InboundPolicy`] as the mechanism precedent.
/// [`TeamlessSend::severity`] carries the tightening order the project tier
/// merges under; the config spellings are the lowercase variant names.
///
/// **Live since D543** (2026-08-30): a session is teamless when its own
/// registry holds no members, read at each call, which every session that
/// has spawned no teammate is — so `Ask` really does raise a dialog per
/// send there. It read *inert* for two days, between **D542** finding no
/// shipped session teamless in this key's sense and D543 deriving the state
/// instead of latching it; [`Config::teamless_send`]'s own doc carries that
/// history in full.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TeamlessSend {
    /// Send without a dialog, what an absent key means.
    #[default]
    Unasked,
    /// Raise the ordinary permission dialog on each send.
    Ask,
}

impl TeamlessSend {
    /// Where this value sits on the tightening order a project file merges
    /// under: `unasked (0) < ask (1)` — [`InboundPolicy::severity`]'s
    /// pattern on this key's own two values. Spelled as a number rather
    /// than an `Ord` derive, so the order cannot drift under a reordered
    /// declaration.
    #[must_use]
    pub const fn severity(self) -> u8 {
        match self {
            Self::Unasked => 0,
            Self::Ask => 1,
        }
    }
}

impl<'de> Deserialize<'de> for TeamlessSend {
    /// Hand-written for [`InboundPolicy`]'s reason: the refusal names the
    /// key, which one type serving one key can do honestly.
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// The one spelling the key takes.
        struct Shape;

        impl Visitor<'_> for Shape {
            type Value = TeamlessSend;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("teamless_send as \"unasked\" or \"ask\"")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                match value {
                    "unasked" => Ok(TeamlessSend::Unasked),
                    "ask" => Ok(TeamlessSend::Ask),
                    other => Err(E::custom(format!(
                        "teamless_send is \"unasked\" or \"ask\", not {other:?}"
                    ))),
                }
            }
        }

        deserializer.deserialize_str(Shape)
    }
}

/// What OpenRouter is asked to run on its own side (**D489**).
///
/// Spec: that vendor's `docs/guides/features/server-tools`, read 2026-08-14 —
/// tools the *model* decides to call and the *gateway* executes, zero to N
/// times per request, asked for as `{"type": "openrouter:<name>"}` rows in the
/// same `tools` array a session's own tools ride in. No upstream counterpart:
/// opencode has no server tools at all.
///
/// **Opt-in, because they spend.** Each call bills — a web search is a search
/// somebody pays for — and each changes what the model does with a turn. A
/// session that names none sends none, which is every session that has not
/// asked.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OpenRouterConfig {
    /// Which of that vendor's own tools the model may call, by the name after
    /// the `openrouter:` prefix.
    ///
    /// Validated against
    /// [`openrouter::SERVER_TOOLS`](crate::provider::openrouter::SERVER_TOOLS)
    /// at load and **refused by name**: a misspelling forwarded verbatim is a
    /// 400 in the middle of a turn, where the same typo caught here is a line
    /// somebody can read. Replaced rather than concatenated across tiers, like
    /// every list here but `instructions`.
    #[serde(default)]
    pub server_tools: Vec<String>,
}

/// What the terminal frontend does beyond drawing frames.
///
/// Core carries it the way it carries `keybinds`: parsed and refused-unknown
/// here, acted on only by the TUI, because a curated key set that stopped at
/// the crate boundary would leave a misspelled `tui` key silently ignored —
/// the exact failure this module refuses everywhere else. Not a key upstream
/// has: what it configures is the Codex CLI's focus-gated terminal
/// notification, which opencode does not make (**D468**).
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TuiConfig {
    /// Which moments are announced while the terminal is not looking.
    ///
    /// **Absent is none of them**, exactly as `false` is — a terminal that
    /// rings unasked is noise, not a notification. `true` is both moments; a
    /// list is exactly the moments it names.
    pub notifications: Option<Notifications>,
    /// How an announcement reaches the terminal.
    ///
    /// **Absent is [`NotificationMethod::Osc9`]**, and an [`Option`] for
    /// [`AgentsConfig::concurrency`]'s reason: a tier that says nothing has
    /// to leave the tier below it alone. [`TuiConfig::notification_method`]
    /// is what reads it.
    pub notification_method: Option<NotificationMethod>,
    /// How the status bar is composed; see [`StatuslineConfig`].
    ///
    /// **Absent is the default roster** — the bar this build has always
    /// drawn.
    pub statusline: Option<StatuslineConfig>,
}

impl TuiConfig {
    /// Whether this config asks for `event` to be announced.
    #[must_use]
    pub fn notifies(&self, event: NotificationEvent) -> bool {
        self.notifications.as_ref().is_some_and(|asked| asked.includes(event))
    }

    /// The method this config asked for, or OSC 9 when it asked for nothing.
    #[must_use]
    pub fn notification_method(&self) -> NotificationMethod {
        self.notification_method.unwrap_or_default()
    }
}

/// What the status bar renders, in what order, and how wide (**D469**,
/// `hud-statusline`).
///
/// Not a key upstream has: the segment roster ports the oh-my-claudecode
/// HUD's behavior onto the bar ganja already draws, which opencode's TUI does
/// not offer. The Codex CLI's `[tui] status_line` is the same *idea* — a
/// user-ordered element list — and this table is where ganja spells it.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StatuslineConfig {
    /// The elements to render, left to right, exactly and only these.
    ///
    /// **Absent is the default roster** — every segment today's bar carries,
    /// in the order it carries them. An element name nothing renders is
    /// refused at load naming it, [`NotificationEvent`]'s posture.
    pub elements: Option<Vec<StatuslineElement>>,
    /// Widest the bar may draw, in terminal cells.
    ///
    /// **Absent is the terminal's own width.** Anything past it is truncated
    /// with an ellipsis rather than wrapped — the OMC HUD's `maxWidth`
    /// behavior.
    pub max_width: Option<u16>,
    /// Whether elements that carry more than a segment's worth — todos, for
    /// now — may draw a detail line under the bar.
    ///
    /// **Absent is no**: an extra line is a row taken from the transcript,
    /// which only somebody who asked for it should pay.
    pub detail: Option<bool>,
}

/// One thing the status bar can render, named the way a config names it.
///
/// The first block is today's bar, segment for segment; the second is the
/// HUD vocabulary the P14 screenshot pinned; the last is [`Self::Rate`].
///
/// P14 recorded here that rate-bucket elements were deliberately absent for
/// want of a data source. **P16 found one that is not the missing usage API**
/// (**D484**): the rate-limit headers every response already carries. What
/// stays absent is what still has no honest source — the subscription plan's
/// 5h/weekly meters and any account-wide spend figure, which need the vendor
/// usage API ganja holds no credential tier for (**D471**). A name that
/// renders nothing would still be a lie the loader let through; `rate` renders
/// something whenever the vendor said something, and nothing when it did not.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum StatuslineElement {
    /// What the engine is doing now, with the spinner while it streams.
    Activity,
    /// The agent the next turn runs as.
    Agent,
    /// The `model (effort)` pair, shown only while an effort is selected.
    Effort,
    /// How many messages are waiting for the running turn.
    Queued,
    /// How many background `bash` jobs are running.
    Jobs,
    /// How many delegated children the running turn has in flight.
    Tasks,
    /// How many permission dialogs are waiting behind the open one.
    Dialogs,
    /// How many inbound peer messages the admission gate is holding for
    /// review (**D524**), as `N held` — present only while there are any.
    ///
    /// Naming it moves nothing by default: the absent-config bar carries the
    /// segment exactly where it always did, beside [`Self::Dialogs`]. The
    /// name is what lets a configured roster place and order the count like
    /// every other element instead of inheriting the default bar's seat.
    Held,
    /// How many teammates this session is leading (**D503**).
    ///
    /// Beside [`Self::Jobs`] and [`Self::Tasks`] because it answers their
    /// question about a third kind of work this session started and no longer
    /// waits on. It earns its place more than either: the default backend is
    /// in-process and has no window of its own, so without a count a teammate
    /// that is thinking and a teammate that has wedged look identical — which
    /// is what the segment, and `/team`'s ring under it, exist to tell apart.
    Teammates,
    /// The session's token and dollar totals.
    Tokens,
    /// The notice beside the state — failures, MCP servers out of reach.
    Notice,
    /// The key reminders, right-aligned as they always were — which shell
    /// mode is now the only mode to have.
    Hints,
    /// The repository and branch, on their own line above the bar.
    Git,
    /// The active model, in the screenshot's `Model: <name>` label form.
    Model,
    /// The context meter, `ctx:[####----]NN%`.
    Context,
    /// How long this session has been open, `session:2m`.
    Session,
    /// The working directory's name.
    Cwd,
    /// Todo progress, `todos:N/M` plus the in-progress title.
    Todos,
    /// The tightest of the vendor's own rate-limit windows, as the context
    /// meter's shape (**D484**) — `rate:[####----]NN%` over the bucket with
    /// least left, the one that will stop a turn first.
    ///
    /// Renders **nothing at all** when the wire has heard no such headers, or
    /// when every window it heard is past its own reset: an element that has
    /// nothing true to say yields no cell rather than a stale number.
    Rate,
}

/// Which notification moments a `tui.notifications` key asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notifications {
    /// `true` for both moments, `false` for none of them.
    Enabled(bool),
    /// Exactly the moments named.
    Events(Vec<NotificationEvent>),
}

impl Notifications {
    /// Whether `event` is among the moments asked for.
    #[must_use]
    pub fn includes(&self, event: NotificationEvent) -> bool {
        match self {
            Self::Enabled(enabled) => *enabled,
            Self::Events(events) => events.contains(&event),
        }
    }
}

impl<'de> Deserialize<'de> for Notifications {
    /// Hand-written for [`LspConfig`]'s reason: `untagged` discards the error
    /// every variant produced, and the refusal that matters here — an event
    /// name nothing announces — must name that event, not report that nothing
    /// matched.
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// Accepts either spelling the `notifications` key may take.
        struct Shape;

        impl<'de> Visitor<'de> for Shape {
            type Value = Notifications;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("true, false, or a list of notification event names")
            }

            fn visit_bool<E: de::Error>(self, enabled: bool) -> Result<Self::Value, E> {
                Ok(Notifications::Enabled(enabled))
            }

            fn visit_seq<S: SeqAccess<'de>>(self, seq: S) -> Result<Self::Value, S::Error> {
                Vec::deserialize(de::value::SeqAccessDeserializer::new(seq))
                    .map(Notifications::Events)
            }
        }

        deserializer.deserialize_any(Shape)
    }
}

/// One moment the TUI can announce.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum NotificationEvent {
    /// A turn finished.
    TurnComplete,
    /// A permission or question dialog is waiting for an answer.
    ApprovalRequested,
}

/// How an announcement reaches the terminal.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NotificationMethod {
    /// OSC 9, which terminals that carry it surface as a desktop
    /// notification. The default because of how it fails: a terminal that
    /// ignores it shows nothing, where an unwanted bell is a beep every turn.
    #[default]
    Osc9,
    /// BEL, the terminal bell.
    Bel,
}

/// What the `webfetch` tool may reach.
///
/// Its own object rather than a flat key, because what it configures is one
/// tool's reach and the next question of that kind belongs beside this one.
/// Not a key upstream has: it configures a refusal upstream does not make.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WebfetchConfig {
    /// Whether `webfetch` may fetch a URL resolving onto this machine or a
    /// private network.
    ///
    /// **Absent is no.** The tool refuses those by default — the URL is one a
    /// model chose, and a model chooses it after reading files and pages other
    /// people wrote — and this is how somebody with an intranet wiki or a
    /// service on their own machine says so. An [`Option`] rather than a
    /// `bool` for the reason [`Config::snapshot`] is one: a tier that says
    /// nothing has to leave the tier below it alone.
    pub allow_private: Option<bool>,
}

/// Where a session looks for skills besides ganja's own two homes.
///
/// Upstream's two keys, spelled as upstream spells them
/// (`packages/core/src/v1/config/skills.ts`), so a config written for opencode
/// keeps meaning what it meant. What differs is what they sit on top of:
/// upstream adds them to four tiers it walks unasked, and this build walks the
/// two in [`default_skill_dirs`] — both its own — and nothing foreign. See
/// `tool::skill`'s `nothing-foreign-is-discovered`, which records whose ruling
/// that is.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkillsConfig {
    /// Further directories holding skills, each scanned for `SKILL.md` files
    /// below it, and each ranking above ganja's own two. `~/` expands against
    /// the home directory and a relative path resolves against the session's
    /// working directory, exactly as upstream resolves them
    /// (`skill/index.ts:211-220`) — which is what makes `["~/.claude/skills"]`
    /// the one line that hands this build a tier upstream helps itself to.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Endpoints skills would be downloaded from.
    ///
    /// Accepted and **not fetched**; see [`Config::skill_urls`]. The key is
    /// read so that a config carrying one still loads, which is the whole
    /// reason it is here rather than refused as unknown.
    #[serde(default)]
    pub urls: Vec<String>,
}

impl Config {
    /// The directories `skills.paths` named, resolved and existing.
    ///
    /// A path that names nothing is warned about and dropped rather than
    /// carried as a root that can never match — upstream logs the same warning
    /// (`skill/index.ts:214-217`), and a discovery walk over a directory that
    /// is not there is a walk that quietly explains nothing.
    #[must_use]
    pub fn skill_paths(&self, cwd: &Path) -> Vec<PathBuf> {
        self.skills
            .paths
            .iter()
            .filter_map(|entry| {
                let expanded = match entry.strip_prefix("~/") {
                    Some(rest) => Xdg::new().ok()?.home_dir().join(rest),
                    None => PathBuf::from(entry),
                };
                let path = if expanded.is_absolute() { expanded } else { cwd.join(expanded) };

                if !path.is_dir() {
                    tracing::warn!(path = %path.display(), "a skills path names no directory");
                    return None;
                }

                Some(path)
            })
            .collect()
    }

    /// The endpoints `skills.urls` named, each of which this build declines to
    /// fetch.
    ///
    /// The same posture **D2** takes for `http(s)` entries in `instructions`,
    /// and for the same reason: composing a system prompt is not a good moment
    /// to depend on somebody else's host being up. Returning them rather than
    /// warning in here leaves the warning at the one place that knows a prompt
    /// is being composed.
    #[must_use]
    pub fn skill_urls(&self) -> &[String] {
        &self.skills.urls
    }

    /// Whether this session snapshots the working tree; see
    /// [`Config::snapshot`].
    #[must_use]
    pub fn snapshots_enabled(&self) -> bool {
        self.snapshot != Some(false)
    }

    /// Whether this session carries its project's memory; see
    /// [`Config::memory`].
    ///
    /// The mirror image of [`Config::snapshots_enabled`], and deliberately
    /// spelled the other way round: that one is on until a tier says `false`,
    /// this one is off until a tier says `true`.
    #[must_use]
    pub fn memory_enabled(&self) -> bool {
        self.memory == Some(true)
    }

    /// The advertised `mcp__*` schema budget; see
    /// [`Config::tool_defer_threshold`] for the key. Absent is
    /// [`DEFAULT_TOOL_DEFER_THRESHOLD`].
    #[must_use]
    pub fn defer_threshold(&self) -> usize {
        self.tool_defer_threshold.unwrap_or(DEFAULT_TOOL_DEFER_THRESHOLD)
    }

    /// Whether `webfetch` may reach a private address; see
    /// [`WebfetchConfig::allow_private`].
    #[must_use]
    pub fn webfetch_allows_private(&self) -> bool {
        self.webfetch.allow_private == Some(true)
    }

    /// The held-dialog review window this config asks for, or the default
    /// when it asks for nothing — [`DialogExpiry::FiveMinutes`], the
    /// `dialog_expiry` key's own doc carries why (**D523**).
    #[must_use]
    pub fn dialog_expiry(&self) -> DialogExpiry {
        self.dialog_expiry.unwrap_or_default()
    }

    /// The explicit inbound policy with the tier that established it, in the
    /// shape the admission gate's resolver takes (**D523**) — [`None`] when
    /// no tier set `cross_session_inbound`, which is the class-dependent
    /// default and not a fourth policy.
    ///
    /// A value without a recorded source — a `Config` built by hand rather
    /// than through [`Config::load_with`] — answers the global tier, the
    /// least specific claim a review surface could name.
    #[must_use]
    pub fn inbound_policy(&self) -> Option<(InboundPolicy, ganja_protocol::PolicySource)> {
        Some((
            self.cross_session_inbound?,
            self.cross_session_inbound_source.unwrap_or(ganja_protocol::PolicySource::Global),
        ))
    }

    /// The sender-side posture a **teamless** session's `send_message` runs
    /// under, or the default when no tier says — [`TeamlessSend::Unasked`],
    /// the `teamless_send` key's own doc carries why (**D531**). The engine
    /// computes the tool's effective default from this and the live team
    /// state (**D543**): in a session that holds a team the value changes
    /// nothing, and in one leading nobody it decides whether each
    /// `send_message` raises an ordinary storable dialog.
    #[must_use]
    pub fn teamless_send(&self) -> TeamlessSend {
        self.teamless_send.unwrap_or_default()
    }

    /// Loads the config for a session working in `cwd`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] for a file that exists and cannot be read or
    /// understood, including one naming a key this build does not have — or,
    /// for a project file, one setting `dialog_expiry`, the key the project
    /// tier may not set (**D523**). One of [`LEGACY_FILES`] found in any tier
    /// is [`ConfigError::Legacy`], whether or not a `ganja.toml` sits beside
    /// it. A file that is simply absent is not an error.
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

        // The trusted tiers merge in two steps rather than one vector, so the
        // tier that establishes `cross_session_inbound` is knowable while it
        // still is a tier (**D523**): the fold is the same sequential merge
        // either way, split exactly at the global/explicit boundary.
        let mut config = merge_files(&global_files()?)?;
        let mut source = config.cross_session_inbound.map(|_| ganja_protocol::PolicySource::Global);
        if let Some(path) = explicit
            && let Some(tier) = read(&path)?
        {
            if tier.cross_session_inbound.is_some() {
                source = Some(ganja_protocol::PolicySource::ExplicitFile);
            }
            config.merge(tier);
        }
        // The project tier does not join the trusted merge (**D523**): a
        // checkout's file may only *tighten* `cross_session_inbound` and may
        // not set `dialog_expiry` at all, and `merge_project` is where both
        // rules live — every other key keeps the later-wins the trusted
        // tiers just used. Per file, because the ancestor walk can find
        // several and each may tighten further. The source moves only when
        // the value did: a project file that merely agrees with the standing
        // policy did not establish it.
        for path in project_files(cwd)? {
            if let Some(tier) = read(&path)? {
                let standing = config.cross_session_inbound;
                config.merge_project(tier, &path)?;
                if config.cross_session_inbound != standing {
                    source = Some(ganja_protocol::PolicySource::Project);
                }
            }
        }
        config.cross_session_inbound_source = source;
        // Installed plugins contribute here — below every explicit tier,
        // above the builtin defaults each surface resolves later — and per
        // surface rather than as a fourth tier through [`Config::merge`],
        // whose per-event-key `hooks` replacement would silently kill every
        // plugin hook (**D473**; the merge table is `crate::plugin`'s module
        // doc). No store, no state file: nothing to apply, silently.
        if let Some(store) = crate::plugin::Store::discover() {
            crate::plugin::apply(&store, &mut config).map_err(|error| ConfigError::Parse {
                path: store.state_path(),
                message: error.to_string(),
            })?;
        }
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
        overlay(&mut self.default_provider, other.default_provider);
        overlay(&mut self.default_agent, other.default_agent);
        overlay(&mut self.effort, other.effort);
        overlay(&mut self.theme, other.theme);
        overlay(&mut self.theme_mode, other.theme_mode);
        overlay(&mut self.shell, other.shell);
        overlay(&mut self.memory, other.memory);
        overlay(&mut self.snapshot, other.snapshot);
        overlay(&mut self.tool_defer_threshold, other.tool_defer_threshold);
        overlay(&mut self.agents.concurrency, other.agents.concurrency);
        overlay(&mut self.teammates.shim_turn_timeout, other.teammates.shim_turn_timeout);
        overlay(&mut self.teammates.shell, other.teammates.shell);
        overlay(&mut self.teammates.pane_share, other.teammates.pane_share);
        // The two D523 keys — and D531's sender-side sibling — ride this
        // ordinary overlay between **trusted** tiers only: a project file
        // reaches them through `merge_project`, which refuses one and
        // tightens the other two before handing the rest of its file back
        // here.
        overlay(&mut self.cross_session_inbound, other.cross_session_inbound);
        overlay(&mut self.dialog_expiry, other.dialog_expiry);
        overlay(&mut self.teamless_send, other.teamless_send);
        overlay(&mut self.tui.notifications, other.tui.notifications);
        overlay(&mut self.tui.notification_method, other.tui.notification_method);
        // Field by field, like the rest of the `tui` table: a project that
        // only reorders `elements` keeps the global tier's `max_width`, and
        // the element list itself replaces wholesale — arrays replace, this
        // file's rule everywhere but `instructions`.
        if let Some(incoming) = other.tui.statusline {
            match &mut self.tui.statusline {
                Some(existing) => {
                    overlay(&mut existing.elements, incoming.elements);
                    overlay(&mut existing.max_width, incoming.max_width);
                    overlay(&mut existing.detail, incoming.detail);
                }
                vacant => *vacant = Some(incoming),
            }
        }
        overlay(&mut self.webfetch.allow_private, other.webfetch.allow_private);
        // Arrays replace, which is this file's rule everywhere but
        // `instructions`: a project that names its own skill directories means
        // those, and a global tier that keeps applying underneath would be a
        // list nobody wrote.
        if !other.skills.paths.is_empty() {
            self.skills.paths = other.skills.paths;
        }
        if !other.skills.urls.is_empty() {
            self.skills.urls = other.skills.urls;
        }

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
        // An entry replaces wholesale for `mcp`'s reason turned around: the
        // fields are one description of one endpoint, and a closer tier that
        // moved a provider to another host without repeating its `key_env`
        // would otherwise present the old credential to the new endpoint.
        self.provider.extend(other.provider);
        // Per event, and wholesale: a project that lists what to run before a
        // tool call has said what to run before a tool call, and appending the
        // global tier's list underneath would run commands the closest file
        // deliberately left out. Concatenating would also make a global hook
        // unremovable from a checkout, which is the direction that matters —
        // these are commands, not settings.
        self.hooks.extend(other.hooks);
        merge_lsp(&mut self.lsp, other.lsp);
        self.permission.merge(&other.permission);

        for instruction in other.instructions {
            if !self.instructions.contains(&instruction) {
                self.instructions.push(instruction);
            }
        }
    }

    /// Overlays one **project-tier** file onto the running result (**D523**).
    ///
    /// The project tier is the one tier whose author is the checkout rather
    /// than the person running it, so three keys diverge from
    /// [`Config::merge`]'s later-wins: `dialog_expiry` is refused outright —
    /// the complaint names the key and `path` — while `cross_session_inbound`
    /// and `teamless_send` (**D531**) replace the running result only when
    /// strictly more severe on their own [`InboundPolicy::severity`] /
    /// [`TeamlessSend::severity`] orders, so a checkout can tighten the
    /// person's policy and never loosen it. Every other key merges exactly
    /// as [`Config::merge`] merges it.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Parse`] naming `dialog_expiry` and the file when the
    /// file sets it.
    fn merge_project(&mut self, mut other: Self, path: &Path) -> Result<(), ConfigError> {
        if other.dialog_expiry.is_some() {
            return Err(ConfigError::Parse {
                path: path.to_owned(),
                message: "dialog_expiry is set by trusted tiers only — the global config, \
                          or the file GANJA_CONFIG or --config names — never by a project \
                          file: a checkout must not stretch or shrink the human review \
                          window"
                    .to_owned(),
            });
        }
        tighten(
            &mut self.cross_session_inbound,
            other.cross_session_inbound.take(),
            InboundPolicy::severity,
        );
        tighten(&mut self.teamless_send, other.teamless_send.take(), TeamlessSend::severity);
        self.merge(other);

        Ok(())
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

/// Replaces `standing` only when `incoming` is strictly more severe on
/// `severity`'s order — the project tier's tightening rule, one spelling for
/// both keys that merge under it (**D523**, **D531**).
fn tighten<T: Copy>(standing: &mut Option<T>, incoming: Option<T>, severity: impl Fn(T) -> u8) {
    match (*standing, incoming) {
        // An equal or less severe project value leaves the standing one:
        // tightening is the only direction a checkout has.
        (Some(held), Some(new)) if severity(new) <= severity(held) => {}
        // A strictly more severe value replaces, and an unset standing
        // value has nothing to loosen — the first tier to say anything
        // establishes the policy.
        (_, Some(new)) => *standing = Some(new),
        (_, None) => {}
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

/// The model half of a `"provider/model"` config spec, when that spec belongs
/// to the provider this session actually runs as.
///
/// A prefix is a claim about *whose* model this is, and the claim is honored
/// in every direction:
///
/// - a spec naming the selected provider applies. The comparison is against
///   whichever id was selected, so an entry from [`Config::provider`]'s own
///   table binds exactly as a builtin's does — there is no list of names here
///   to fall out of date;
/// - a **bare** spec applies everywhere, because it claims nothing;
/// - a spec naming somebody else is skipped, with one `tracing::info` naming
///   both providers: the probe for "why was my model ignored".
///
/// Skipped rather than stripped, and skipped rather than refused. Stripping is
/// the bug this exists to end — a config `model: "cursor/claude-x"` under
/// `GANJA_PROVIDER=openai` sent openai a bare `claude-x` and earned a live 400
/// — and refusing at load would let one standing config line break every
/// session that runs on another provider, which is the same reason the
/// [`effort`](Config::effort) key clears rather than refuses.
///
/// `named_by` is what the log line calls the spec, so the line says which key
/// was passed over.
#[must_use]
pub fn model_bound_to<'a>(spec: &'a str, provider_id: &str, named_by: &str) -> Option<&'a str> {
    match split_model(spec) {
        (Some(named), _) if named != provider_id => {
            tracing::info!(
                key = named_by,
                spec = spec,
                names = named,
                running_as = provider_id,
                "the config names another provider's model; leaving it alone"
            );
            None
        }
        (_, model) => Some(model),
    }
}

/// **The** directory ganja keeps its own things in, resolved once for
/// everything that needs it: the global `ganja.toml`, the global `AGENTS.md`,
/// and the `skills/` folder of [`default_skill_dirs`].
///
/// Three places, in this order:
///
/// 1. [`CONFIG_HOME_ENV`] — taken as written, whether or not it exists;
/// 2. `<XDG config>/ganja`, if that directory is there;
/// 3. `~/.ganja`, if *that* directory is there.
///
/// [`None`] only when there is no home directory to resolve anything against,
/// which is reported once and then behaves like an empty global config — there
/// is nowhere for one to have been written either.
///
/// # Why existence decides between 2 and 3, and what happens when neither is there
///
/// Precedence alone would make the third place unreachable: `Xdg::new()`
/// succeeds on every machine that has a `$HOME`, so an unconditional
/// `<XDG config>/ganja` means `~/.ganja` is *never* read — which is not what a
/// fallback is for. Existence is therefore what picks between them, and it is
/// checked per call rather than cached, so a directory created while a session
/// is open is found by that session.
///
/// When **neither** exists there is nothing to read either way, so the answer
/// only matters to whoever writes next, and that answer is
/// `<XDG config>/ganja` — the modern convention, and already where
/// `ganja config import-opencode --global` puts the file it writes. `~/.ganja`
/// is for people who have one, not a thing this build creates.
///
/// The consequence worth stating: this is **one home, not a merge**. Somebody
/// holding both directories is served the XDG one and the dotted one is
/// invisible, rather than the two being read together — a config that is
/// half-read from two places is worse than one that says which place it came
/// from. [`CONFIG_HOME_ENV`] is the way to name the other one, and
/// `skills.paths` the way to add a skills directory without moving anything
/// else.
#[must_use]
pub fn config_home() -> Option<PathBuf> {
    if let Some(named) = env::var(CONFIG_HOME_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        // Taken as written, as [`CONFIG_ENV`]'s file is: a relative value
        // resolves against the process working directory, and the two
        // variables agreeing about that is worth more than a rule only one of
        // them enforces.
        return Some(PathBuf::from(named));
    }

    let base = match Xdg::new() {
        Ok(base) => base,
        Err(error) => {
            tracing::warn!(
                %error,
                "the home directory holding the global config could not be located; \
                 only project config applies"
            );
            return None;
        }
    };
    Some(discovered(base.config_dir().join(DIRECTORY), base.home_dir().join(HOME_DIRECTORY)))
}

/// Which of [`config_home`]'s two *discovered* candidates answers, given what
/// is on disk.
///
/// Split out of the lookup around it because this is the half that was a
/// judgment call rather than a reading of the ruling — and because a rule about
/// what exists can then be tested against two temporary directories instead of
/// against the home directory of whoever is running the suite.
fn discovered(xdg: PathBuf, dotted: PathBuf) -> PathBuf {
    if xdg.is_dir() {
        return xdg;
    }
    if dotted.is_dir() {
        return dotted;
    }

    xdg
}

/// The global tier's files, in merge order.
///
/// Reversed out of [`FILES`] order for the same reason the project walk
/// reverses: merging applies later over earlier, so the name that has to win
/// must be merged last. Which name that is, is [`FILES`]' doc to say and not
/// this one's — a ranking spelled in two places is a ranking that can
/// disagree with itself.
fn global_files() -> Result<Vec<PathBuf>, ConfigError> {
    let Some(dir) = config_home() else {
        return Ok(Vec::new());
    };
    let mut files = existing(&dir)?;
    files.reverse();

    Ok(files)
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

/// Ganja's own two homes, which a session scans for skills without being told
/// to: `skills/` under [`config_home`], and `<project root>/.ganja/skills`.
///
/// Global first, project second, so a skill in the checkout wins the name — the
/// order every layered thing here resolves in, and the order
/// [`skill::Roots`](crate::tool::skill::Roots) reads as precedence.
///
/// The first is spelled through [`config_home`] rather than against the XDG
/// path directly, which is the whole point of that function: `GANJA_CONFIG_HOME`
/// or a `~/.ganja` moves this build's global config, its global `AGENTS.md` and
/// its skills together, and a session cannot end up reading one of the three
/// out of a directory the other two are not in.
///
/// # Why these two and no others
///
/// The standing ruling this build follows is that **nothing foreign** is
/// discovered: not `~/.claude/skills`, not `~/.agents/skills`, not those names
/// walked up from the working directory, and not a bare `skill/` or `skills/`
/// at a project root, which is a name somebody else's repository may already
/// mean something by. These two are ganja's own — a file only arrives in either
/// because somebody put it there for *this* tool — so placing one **is** the
/// opt-in act, which is what a config key would otherwise have to stand in for.
/// `skills.paths` remains the way to name anything else, these two included if
/// somebody wants them twice.
///
/// The seams are the ones the rest of the crate already uses: the global half
/// is [`config_home`] (wherever ganja's own things live — `GANJA_CONFIG_HOME`,
/// else `<XDG config>/ganja`, else `~/.ganja` — the directory that holds
/// `ganja.toml`), and
/// the project half hangs off `Project::resolve`, the same worktree resolution
/// `project_files` stops its walk at and the permission engine files its
/// answers under. Nothing here invents a way to find a directory.
#[must_use]
pub fn default_skill_dirs(cwd: &Path) -> Vec<PathBuf> {
    // The `Project::resolve` step is this function's own: skills are asked for
    // by working directory where agents and commands are asked for by worktree.
    home_dirs(Project::resolve(cwd).root(), SKILLS_SUBDIR)
}

/// Ganja's own two homes narrowed to `subdir`: `<config home>/<subdir>` first,
/// `<root>/.ganja/<subdir>` second, in the order every layered thing here
/// resolves in.
///
/// One walk for the three rosters that keep files in ganja's homes — skills
/// ([`default_skill_dirs`]), agent definitions and command files. They had a
/// copy each, and the copies had to agree about three things: which two
/// directories, in which order, and what to do when they turn out to be one
/// directory.
///
/// **The two collapse into one** for somebody whose `<root>/.ganja` *is* the
/// directory [`config_home`] landed on — running in `~` with a `~/.ganja`, or
/// pointing `GANJA_CONFIG_HOME` at the checkout. Scanning it twice would find
/// every file twice and warn about each as a duplicate claiming its own name,
/// which is a warning about nothing. The comparison is textual, and `root` has
/// usually been through `Project::resolve` (which resolves symbolic links)
/// while a configured home has not, so two spellings of one directory do not
/// collapse; `tests/two_homes_collapse.rs` pins the case that does.
pub(crate) fn home_dirs(root: &Path, subdir: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Some(global) = config_home() {
        found.push(global.join(subdir));
    }
    let project = root.join(PROJECT_DIRECTORY).join(subdir);
    if !found.contains(&project) {
        found.push(project);
    }

    found
}

/// Every project-tier file, outermost first so the closest directory wins.
fn project_files(cwd: &Path) -> Result<Vec<PathBuf>, ConfigError> {
    // Canonicalised the same way `Project::resolve` canonicalises its root, or
    // the walk would never recognise the root it is supposed to stop at. The
    // ancestor walk terminates at the filesystem root either way, so the worst
    // a path that cannot be canonicalised costs is a longer walk.
    let start = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let stop = Project::resolve(cwd).root().to_path_buf();

    let mut found = Vec::new();
    for directory in start.ancestors() {
        found.extend(existing(directory)?);
        if directory == stop {
            break;
        }
    }
    found.reverse();

    Ok(found)
}

/// The config files that exist in `directory`, in [`FILES`] order — or the
/// refusal a file in the old dialect earns.
///
/// The legacy probe runs **first**, and runs whether or not a `ganja.toml` is
/// sitting beside it. Reading the new file and saying nothing about the old
/// one would be the ignored-setting failure wearing a different hat: whoever
/// left a `ganja.jsonc` in a directory believes it is doing something, and the
/// only honest moment to say otherwise is the launch that would have skipped
/// it.
fn existing(directory: &Path) -> Result<Vec<PathBuf>, ConfigError> {
    if let Some(path) =
        LEGACY_FILES.iter().map(|name| directory.join(name)).find(|path| path.is_file())
    {
        return Err(ConfigError::Legacy { path });
    }

    Ok(FILES.iter().map(|name| directory.join(name)).filter(|path| path.is_file()).collect())
}

/// Whether `path` is read as a config file at all, rather than refused as one
/// in the dialect this build has left.
///
/// Decided by extension rather than by whole file name, because discovery is
/// not the only way a path gets here: [`CONFIG_ENV`] and `--config` name one
/// that can be anything, and a `~/mine.jsonc` deserves the same sentence a
/// discovered `ganja.jsonc` gets. One rule answers both.
///
/// An extension this does not know, or none at all, is read as TOML. That is
/// the direction the default has to fall now that there is one format: a file
/// somebody named explicitly is a file they want read, and the only way to be
/// wrong about it is to *look* like the format that is gone.
fn is_toml(path: &Path) -> bool {
    !path.extension().is_some_and(|extension| {
        LEGACY_FILES.iter().any(|name| {
            Path::new(name).extension().is_some_and(|legacy| extension.eq_ignore_ascii_case(legacy))
        })
    })
}

/// Reads and parses one config file, or [`None`] when it is not there.
///
/// Absence is checked by reading rather than by asking first: the file may
/// vanish between the two, and a missing file at this point means the same
/// thing either way. The dialect is checked before that, because a file that
/// will be refused for its name is refused whether or not it can be read.
///
/// `toml::de` walks the parsed document rather than a re-sorted map, so
/// [`Config`] is decoded from a reader that sees keys in the order the file
/// spelled them. That is not a nicety: permission rules are evaluated
/// last-match-wins, so document order is the answer to which rule decides
/// (`config_tests.rs` pins it, headers and dotted keys alike).
fn read(path: &Path) -> Result<Option<Config>, ConfigError> {
    if !is_toml(path) {
        return Err(ConfigError::Legacy { path: path.to_owned() });
    }

    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Read { path: path.to_owned(), source });
        }
    };

    let config = toml::from_str::<Config>(&text).map_err(|error| ConfigError::Parse {
        path: path.to_owned(),
        message: located(error.message(), error.span(), &text),
    })?;

    checked(path, config).map(Some)
}

/// What the parser said and where, and deliberately nothing else.
///
/// `toml::de::Error`'s own `Display` reproduces the offending line with a
/// caret under it, which is a fine thing for a compiler and the wrong thing
/// here: the line that failed to parse is a line of somebody's config, and an
/// `mcp` entry's `headers` map is where a bearer token lives. This build
/// withholds header *values* even from `ganja mcp get`, so a parse error must
/// not print one back through a log or a terminal somebody shares. The
/// accessors carry exactly the two facts that help — what went wrong, and
/// where to look — with none of the file's own bytes.
///
/// Columns are counted in characters rather than bytes, so a line holding
/// multi-byte text still points at the character somebody would count to.
/// A span-less error (serde's own `custom`, which carries no position) renders
/// as the message alone rather than inventing a line 1.
///
/// The guarantee is about the *line*, which is the whole of what `Display`
/// adds and the whole of what a neighbouring key on it would give away. It is
/// not a claim that no byte of the file can appear: a serde type mismatch
/// names the value it rejected ("invalid type: integer `1`"), the way every
/// serde-backed loader does and the way this build's own JSONC reader always
/// did. That is one value, chosen because the message is useless without it,
/// rather than every value that shared a line with it.
fn located(message: &str, span: Option<Range<usize>>, text: &str) -> String {
    let Some(span) = span else {
        return message.to_owned();
    };

    // Walked rather than sliced, because a span is a byte range and slicing at
    // one that is not a character boundary would have to fall back to
    // something — and every "something" here reports a position that is not
    // the one that failed.
    let (mut line, mut column) = (1usize, 1usize);
    for (index, character) in text.char_indices() {
        if index >= span.start {
            break;
        }
        if character == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }

    format!("{message} at line {line}, column {column}")
}

/// The seven refusals a decoded config still has to pass, and the one place
/// they are spelled.
///
/// Checked per file rather than after the merge, so the complaint names the
/// file that said it. Merging only ever replaces a whole entry, so every entry
/// that survives has been through here.
///
/// Format-independent, all of them: what they refuse is what a decoded config
/// *says*, not how it was spelled. That is why [`legacy::read`] runs them too
/// — a source this build would decline at launch is declined at the read that
/// converts it, instead of translating cleanly into a file the next launch
/// refuses.
fn checked(path: &Path, config: Config) -> Result<Config, ConfigError> {
    let refused = |message: String| ConfigError::Parse { path: path.to_owned(), message };

    check_mcp(&config.mcp).map_err(refused)?;
    check_lsp(config.lsp.as_ref()).map_err(refused)?;
    check_providers(&config.provider).map_err(refused)?;
    check_hooks(&config.hooks).map_err(refused)?;
    check_agents(&config.agents).map_err(refused)?;
    check_teammates(&config.teammates).map_err(refused)?;
    check_openrouter(&config.openrouter).map_err(refused)?;

    Ok(config)
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

/// Refuses a `provider` entry that describes an endpoint no session could use.
///
/// Three things are decided here rather than at selection time, each because
/// finding it out later hides it behind a status line or, worse, behind
/// nothing at all.
///
/// An id this build already ships is the one that must be caught here.
/// [`crate::provider::select`] matches the builtins first, so such an entry
/// would never be reached and its author would be left with an endpoint that
/// silently does nothing — the exact failure this module's curated key set
/// exists to prevent. A builtin's endpoint is moved with its own variable, and
/// the message says which.
///
/// A `base_url` that is neither `https` nor loopback is the same refusal
/// [`check_mcp`] makes, literally: the predicate is
/// [`crate::provider::reachable_in_the_clear`] and only the message is this
/// module's. The credential travels in a header on every request, and so does
/// anything in `headers`.
///
/// A `key_env` that is blank names no variable. It would fall through to the
/// credential store and read as "there is no key", which sends somebody to fix
/// a store that was never the problem.
///
/// No message quotes the URL. A provider entry is configuration, and
/// configuration is allowed to carry a credential in its userinfo.
fn check_providers(providers: &BTreeMap<String, ProviderConfig>) -> Result<(), String> {
    for (id, entry) in providers {
        if crate::provider::PROVIDERS.contains(&id.as_str()) {
            return Err(format!(
                "provider \"{id}\" is one this build already ships, so a `provider` entry \
                 for it would never be reached; point the builtin somewhere else with its \
                 own base-URL variable instead"
            ));
        }
        if entry.key_env.as_ref().is_some_and(|var| var.trim().is_empty()) {
            return Err(format!(
                "provider \"{id}\" has a blank `key_env`, which names no variable"
            ));
        }

        let parsed = Url::parse(&entry.base_url)
            .map_err(|error| format!("provider \"{id}\" has no valid base_url: {error}"))?;
        if !crate::provider::reachable_in_the_clear(&parsed) {
            return Err(format!(
                "provider \"{id}\" must be reached over https, or over http to loopback; \
                 anything else puts its credential on the wire in the clear"
            ));
        }
    }

    Ok(())
}

/// Holds every `mcp` entry in one file to [`McpServer::check`], which is where
/// the three refusals and the reasoning behind them live.
///
/// This function is what makes them *per file*: the complaint names the file
/// that said it, which a check run after the merge could not.
fn check_mcp(servers: &BTreeMap<String, McpServer>) -> Result<(), String> {
    servers.iter().try_for_each(|(name, server)| server.check(name))
}

/// Refuses a `hooks` block that names an event nothing fires, a handler with
/// nothing to run, or a matcher no engine could compile.
///
/// All three are decided here for [`check_mcp`]'s reason and one of its own:
/// the complaint names the file that said it, and every one of these failures
/// is otherwise **silent**. A misspelled event name is a hook that never fires;
/// an empty command is a shell invocation with nothing in it; a matcher that is
/// not a regular expression is a group that matches nothing forever. None of
/// the three announces itself at the moment it fails to happen, which is the
/// whole argument for refusing them at the moment somebody could still fix
/// them.
fn check_hooks(hooks: &BTreeMap<String, Vec<HookMatcher>>) -> Result<(), String> {
    for (event, groups) in hooks {
        if crate::hook::HookEvent::from_name(event).is_none() {
            return Err(format!(
                "hooks names no event \"{event}\"; this build fires {}",
                crate::hook::EVENTS.iter().map(|known| known.name()).collect::<Vec<_>>().join(", ")
            ));
        }

        for group in groups {
            if let Some(matcher) = &group.matcher
                && !matcher.is_empty()
                && let Err(error) = regex::Regex::new(matcher)
            {
                return Err(format!(
                    "hooks.{event} has a matcher that is not a regular expression: {error}"
                ));
            }
            for handler in &group.hooks {
                let HookHandler::Command(command) = handler;
                if command.command.trim().is_empty() {
                    return Err(format!("hooks.{event} has a command handler with no command"));
                }
            }
        }
    }

    Ok(())
}

/// Refuses an `agents` block asking for a concurrency of nothing.
///
/// Checked here for [`check_mcp`]'s reason and this key's own: zero is the one
/// value whose consequence is invisible until a turn delegates, and then it is
/// a batch that never starts. One is a perfectly good answer — it is upstream's
/// behavior — so the refusal is only about the value that means "never".
fn check_agents(agents: &AgentsConfig) -> Result<(), String> {
    if agents.concurrency == Some(0) {
        return Err("agents.concurrency must be at least 1; a cap of 0 is a session where every \
             delegation waits forever"
            .to_owned());
    }

    Ok(())
}

/// Refuses a `teammates` block asking for a deadline of nothing.
///
/// [`check_agents`]'s shape and [`check_agents`]'s reason: zero is the one
/// value whose consequence is invisible until a headless teammate takes a
/// turn, and then it is a turn killed before the child has written a byte.
/// Every positive value is somebody's real answer — a second is absurd but it
/// is an answer — so the refusal is only about the value that means "never
/// finish".
///
/// Still refused although **D512** left no spawn door reading the key: a file
/// is checked for what it says, not for whether this build happens to act on
/// it, and a zero silently accepted today is a zero that bites whoever drives
/// the headless machinery next. The sentence says which turns it would kill
/// so nobody reads the refusal as a claim about their pane-mode teammates.
fn check_teammates(teammates: &TeammateConfig) -> Result<(), String> {
    if let Some(shell) = &teammates.shell
        && !shlex::split(shell).is_some_and(|words| !words.is_empty())
    {
        return Err(format!(
            "teammates.shell must name a shell as a command line, like \"/bin/zsh -f\"; \
             {shell:?} is empty or cannot be split into words"
        ));
    }
    if let Some(share) = teammates.pane_share
        && !(1..=99).contains(&share)
    {
        return Err(format!(
            "teammates.pane_share must be a percentage of the window width between 1 and 99 for \
             the teammates' column (the lead keeps the rest; absent is 65); {share} would leave \
             one side of the split with no screen at all"
        ));
    }
    if teammates.shim_turn_timeout == Some(0) {
        return Err(
            "teammates.shim_turn_timeout must be at least 1 second; a deadline of 0 would kill \
             every headless foreign-CLI turn before the child has written a byte (pane-mode shim \
             teammates, the only shape a spawn reaches since D512, run no per-turn deadline and \
             do not read this key)"
                .to_owned(),
        );
    }

    Ok(())
}

/// Refuses a server tool the gateway does not publish (**D489**).
///
/// By name and with the roster beside it, because the two ways to get this
/// wrong are a typo and a tool read about somewhere else — and the answer to
/// both is the list. Checked per file for [`check_mcp`]'s reason: whoever has
/// to fix it is told which file said it.
fn check_openrouter(config: &OpenRouterConfig) -> Result<(), String> {
    for name in &config.server_tools {
        if !crate::provider::openrouter::serves_server_tool(name) {
            return Err(format!(
                "openrouter.server_tools names \"{name}\", which is not one this gateway \
                 serves; the roster is {}",
                crate::provider::openrouter::SERVER_TOOLS.join(", ")
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
