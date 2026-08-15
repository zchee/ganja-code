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
//!
//! That rule is what shapes [`ProviderConfig`]: upstream's `provider.<id>`
//! block carries `options.apiKey`, and ganja's entry carries `key_env`, the
//! *name* of the variable holding it, instead.

use std::{
    collections::BTreeMap,
    env, fmt, fs, io,
    num::NonZeroU64,
    path::{Path, PathBuf},
};

use etcetera::base_strategy::{BaseStrategy as _, Xdg};
use serde::{
    Deserialize, Serialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
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
/// in, outright: the global `ganja.jsonc`/`ganja.json`, the global `AGENTS.md`,
/// and the `skills/` folder beneath it.
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
///
/// Public because two more readers of the same files live outside this crate —
/// `ganja config import-opencode`, which writes one, and `ganja mcp add`,
/// which edits one in place — and the dialect has to agree in **both**
/// directions: looser there and they would edit a file the next launch
/// refuses; stricter and they would refuse to touch a file that loads
/// perfectly well. They each kept a copy with a doc comment promising to track
/// this one, which is a promise a compiler cannot keep.
#[must_use]
pub fn parse_options() -> jsonc_parser::ParseOptions {
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
    /// Which request/response mapping the endpoint speaks. Required: the two
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
/// so a `.claude/settings.json` block can be pasted into a `ganja.jsonc` and
/// still mean what it meant (**D456**). The array-of-groups shape is that
/// spelling: one event maps to a list of `{ matcher, hooks }` objects rather
/// than to a flat list of commands, because the matcher is a property of the
/// group and several groups may match one call.
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
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Editor schema reference. Read so that writing one is not an error;
    /// nothing consults it.
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
        self.notifications
            .as_ref()
            .is_some_and(|asked| asked.includes(event))
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
                let path = if expanded.is_absolute() {
                    expanded
                } else {
                    cwd.join(expanded)
                };

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
        self.tool_defer_threshold
            .unwrap_or(DEFAULT_TOOL_DEFER_THRESHOLD)
    }

    /// Whether `webfetch` may reach a private address; see
    /// [`WebfetchConfig::allow_private`].
    #[must_use]
    pub fn webfetch_allows_private(&self) -> bool {
        self.webfetch.allow_private == Some(true)
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
        overlay(&mut self.tui.notifications, other.tui.notifications);
        overlay(
            &mut self.tui.notification_method,
            other.tui.notification_method,
        );
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
        overlay(
            &mut self.webfetch.allow_private,
            other.webfetch.allow_private,
        );
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
/// everything that needs it: the global `ganja.jsonc`/`ganja.json`, the global
/// `AGENTS.md`, and the `skills/` folder of [`default_skill_dirs`].
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
    Some(discovered(
        base.config_dir().join(DIRECTORY),
        base.home_dir().join(HOME_DIRECTORY),
    ))
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
/// reverses: merging applies later over earlier, so the name that has to win —
/// `ganja.jsonc` where both sit in one directory — must be merged last.
fn global_files() -> Vec<PathBuf> {
    config_home()
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
/// `ganja.jsonc`), and
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
    check_providers(&config.provider).map_err(|message| ConfigError::Parse {
        path: path.to_owned(),
        message,
    })?;
    check_hooks(&config.hooks).map_err(|message| ConfigError::Parse {
        path: path.to_owned(),
        message,
    })?;
    check_agents(&config.agents).map_err(|message| ConfigError::Parse {
        path: path.to_owned(),
        message,
    })?;
    check_openrouter(&config.openrouter).map_err(|message| ConfigError::Parse {
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
        if entry
            .key_env
            .as_ref()
            .is_some_and(|var| var.trim().is_empty())
        {
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
    servers
        .iter()
        .try_for_each(|(name, server)| server.check(name))
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
                crate::hook::EVENTS
                    .iter()
                    .map(|known| known.name())
                    .collect::<Vec<_>>()
                    .join(", ")
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
                    return Err(format!(
                        "hooks.{event} has a command handler with no command"
                    ));
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
        return Err(
            "agents.concurrency must be at least 1; a cap of 0 is a session where every \
             delegation waits forever"
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
mod tests {
    use std::{fs, path::Path};

    use tempfile::TempDir;

    use super::{
        AgentMode, AgentsConfig, Config, ConfigError, Dialect, HookCommand, HookHandler,
        HookMatcher, LspConfig, McpOauth, McpServer, NonZeroU64, NotificationEvent,
        NotificationMethod, Notifications, Overrides, StatuslineConfig, StatuslineElement,
        ThemeMode, existing, merge_files, model_bound_to, project_files, read, split_model,
    };
    use crate::permission::{Action, Rule};

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

    /// The rules `spelled` names, in the shape [`PermissionConfig::rules`]
    /// hands them over — which is what the order assertions read in.
    fn rules(spelled: &[(&str, &str, Action)]) -> Vec<Rule> {
        spelled
            .iter()
            .map(|(permission, pattern, action)| Rule {
                permission: (*permission).to_owned(),
                pattern: (*pattern).to_owned(),
                action: action.clone(),
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

    /// And the key that reads the other way round: memory is **off** until a
    /// config asks for it (**D478**), because switching it on adds standing
    /// prompt weight and a door to write outside the worktree.
    #[test]
    fn memory_is_off_until_a_config_asks_for_it() {
        let absent = parse(r#"{"model": "anthropic/claude-sonnet-5"}"#).expect("it parses");
        assert_eq!(absent.memory, None);
        assert!(!absent.memory_enabled());

        let asked = parse(r#"{"memory": true}"#).expect("it parses");
        assert_eq!(asked.memory, Some(true));
        assert!(asked.memory_enabled());

        // A tier that says nothing leaves the tier below it alone, and a
        // closer `false` still wins — the reason the field is an `Option`.
        let mut merged = asked;
        merged.merge(parse(r#"{"model": "anthropic/claude-sonnet-5"}"#).expect("it parses"));
        assert!(merged.memory_enabled(), "silence is not a refusal");
        merged.merge(parse(r#"{"memory": false}"#).expect("it parses"));
        assert!(!merged.memory_enabled());
    }

    /// The schema budget for MCP tools (**D492**): absent is 32, and the two
    /// extremes both mean something — 0 defers every server, huge disables
    /// deferral — so neither is refused.
    #[test]
    fn tool_defer_threshold_is_thirty_two_until_a_config_says_otherwise() {
        let absent = parse(r#"{"model": "anthropic/claude-sonnet-5"}"#).expect("it parses");
        assert_eq!(absent.tool_defer_threshold, None);
        assert_eq!(absent.defer_threshold(), 32);

        let zero = parse(r#"{"tool_defer_threshold": 0}"#).expect("0 defers every server");
        assert_eq!(zero.defer_threshold(), 0);

        let huge =
            parse(r#"{"tool_defer_threshold": 100000}"#).expect("a huge budget disables deferral");
        assert_eq!(huge.defer_threshold(), 100_000);

        // A tier that says nothing leaves the tier below it alone; a closer
        // tier's number wins.
        let mut merged = zero;
        merged.merge(parse(r#"{"model": "anthropic/claude-sonnet-5"}"#).expect("it parses"));
        assert_eq!(merged.defer_threshold(), 0, "silence is not an opinion");
        merged.merge(parse(r#"{"tool_defer_threshold": 8}"#).expect("it parses"));
        assert_eq!(merged.defer_threshold(), 8);
    }

    /// The key takes a count and nothing else: a string, a float or a
    /// negative is serde's own type refusal — positioned, like every curated
    /// scalar's here; it is unknown *keys* that are refused by name.
    #[test]
    fn a_tool_defer_threshold_that_is_not_a_count_is_refused() {
        for wrong in [
            r#"{"tool_defer_threshold": "many"}"#,
            r#"{"tool_defer_threshold": 1.5}"#,
            r#"{"tool_defer_threshold": -1}"#,
        ] {
            let error = parse(wrong).expect_err("a threshold is an unsigned integer");
            assert!(
                error.to_string().contains("expected usize"),
                "the refusal says what the key takes: {error}"
            );
        }
    }

    /// Claude's own block, pasted whole: the shape is kept so that it can be.
    #[test]
    fn a_hooks_block_parses_into_its_groups_and_handlers() {
        let config = parse(
            r#"{
              "hooks": {
                "PreToolUse": [
                  {
                    "matcher": "Edit|Write",
                    "hooks": [
                      { "type": "command", "command": "./check.sh", "timeout": 5 },
                      { "type": "command", "command": "./log.sh" }
                    ]
                  }
                ],
                "SessionStart": [
                  { "hooks": [{ "type": "command", "command": "git status" }] }
                ]
              }
            }"#,
        )
        .expect("the documented shape parses");

        let pre = &config.hooks["PreToolUse"];
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0].matcher.as_deref(), Some("Edit|Write"));
        assert_eq!(
            pre[0].hooks,
            vec![
                HookHandler::Command(HookCommand {
                    command: "./check.sh".to_owned(),
                    timeout: Some(5),
                }),
                HookHandler::Command(HookCommand {
                    command: "./log.sh".to_owned(),
                    timeout: None,
                }),
            ]
        );
        // An absent matcher is the common case and stays absent rather than
        // becoming an empty string that means the same thing in one more way.
        assert_eq!(config.hooks["SessionStart"][0].matcher, None);
    }

    #[test]
    fn an_unknown_hook_event_is_refused_by_name() {
        let error = parse(
            r#"{"hooks": {"PreToolUsage": [{"hooks": [{"type": "command", "command": "x"}]}]}}"#,
        )
        .expect_err("a hook that never fires is worse than one that fails");

        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure, got {error:?}");
        };
        assert!(message.contains("PreToolUsage"), "{message}");
        assert!(
            message.contains("PreToolUse") && message.contains("PreCompact"),
            "the useful half of \"no such event\" is which ones there are: {message}"
        );
    }

    #[test]
    fn an_unknown_hook_handler_type_is_refused_by_name() {
        let error = parse(r#"{"hooks": {"Stop": [{"hooks": [{"type": "webhook", "url": "x"}]}]}}"#)
            .expect_err("this build runs command handlers and says so");

        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure, got {error:?}");
        };
        assert!(
            message.contains("webhook") && message.contains("command"),
            "{message}"
        );
    }

    #[test]
    fn a_hook_handler_with_no_command_is_refused() {
        for text in [
            r#"{"hooks": {"Stop": [{"hooks": [{"type": "command", "command": ""}]}]}}"#,
            r#"{"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "   "}]}]}}"#,
        ] {
            let error = parse(text).expect_err("a handler with nothing to run is not one");
            let ConfigError::Parse { message, .. } = &error else {
                panic!("expected a parse failure, got {error:?}");
            };
            assert!(message.contains("no command"), "{message}");
        }
    }

    /// A matcher that is not a regular expression would match nothing, forever,
    /// without saying so — which is the one failure mode a config check exists
    /// for.
    #[test]
    fn a_matcher_that_is_not_a_regular_expression_is_refused() {
        let error = parse(
            r#"{"hooks": {"PreToolUse": [{"matcher": "(unclosed", "hooks": [{"type": "command", "command": "x"}]}]}}"#,
        )
        .expect_err("a matcher nothing can compile is a group that never fires");

        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure, got {error:?}");
        };
        assert!(message.contains("PreToolUse"), "{message}");
    }

    /// The singular key is a map of definitions and the plural one is a
    /// settings object; a config may write either without the other, and a
    /// concurrency nobody named is the documented default.
    #[test]
    fn agents_is_the_settings_object_beside_the_agent_map() {
        let config = parse(r#"{"agent": {"plan": {"description": "plans"}}}"#).expect("it parses");
        assert_eq!(
            config.agents.concurrency(),
            AgentsConfig::DEFAULT_CONCURRENCY,
            "absent is the default, not zero"
        );

        let config = parse(r#"{"agents": {"concurrency": 1}}"#).expect("it parses");
        assert_eq!(
            config.agents.concurrency(),
            1,
            "one is upstream's behavior, asked for on purpose"
        );
        assert!(
            config.agent.is_empty(),
            "and it defined no agents while doing it"
        );
    }

    /// Zero is the one value whose consequence is a batch that never starts,
    /// and it is invisible until a turn delegates.
    #[test]
    fn a_concurrency_of_zero_is_refused_by_name() {
        let error =
            parse(r#"{"agents": {"concurrency": 0}}"#).expect_err("a cap of nothing is not a cap");

        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure, got {error:?}");
        };
        assert!(message.contains("agents.concurrency"), "{message}");
    }

    /// `true` is both moments and `false` is none — the same answer absent
    /// gives — so a config only ever writes the boolean to say something.
    #[test]
    fn a_boolean_notifications_key_switches_both_moments_at_once() {
        let config = parse(r#"{"tui": {"notifications": true}}"#).expect("it parses");
        assert!(config.tui.notifies(NotificationEvent::TurnComplete));
        assert!(config.tui.notifies(NotificationEvent::ApprovalRequested));

        let config = parse(r#"{"tui": {"notifications": false}}"#).expect("it parses");
        assert!(!config.tui.notifies(NotificationEvent::TurnComplete));
        assert!(!config.tui.notifies(NotificationEvent::ApprovalRequested));

        let config = parse("{}").expect("it parses");
        assert!(
            !config.tui.notifies(NotificationEvent::TurnComplete),
            "absent is none of them"
        );
    }

    /// The list form is a selection, not a switch: only the moments it names
    /// are announced.
    #[test]
    fn a_notification_list_names_exactly_the_moments_it_announces() {
        let config = parse(r#"{"tui": {"notifications": ["turn-complete"]}}"#).expect("it parses");
        assert!(config.tui.notifies(NotificationEvent::TurnComplete));
        assert!(!config.tui.notifies(NotificationEvent::ApprovalRequested));
    }

    /// A misspelled event would be a notification that never fires, silently —
    /// `check_hooks`'s argument, applied to a smaller vocabulary.
    #[test]
    fn an_event_name_nothing_announces_is_refused_by_name() {
        let error = parse(r#"{"tui": {"notifications": ["turn-done"]}}"#)
            .expect_err("an event nothing announces is refused");

        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure, got {error:?}");
        };
        assert!(message.contains("turn-done"), "{message}");
        assert!(
            message.contains("turn-complete"),
            "and the refusal lists what would have worked: {message}"
        );
    }

    /// The method vocabulary is as closed as the event one.
    #[test]
    fn a_notification_method_nothing_sends_is_refused_by_name() {
        let error = parse(r#"{"tui": {"notification_method": "toast"}}"#)
            .expect_err("a method nothing sends is refused");

        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure, got {error:?}");
        };
        assert!(message.contains("toast"), "{message}");
        assert!(
            message.contains("osc9"),
            "and the refusal lists what would have worked: {message}"
        );
    }

    /// The `tui` table is curated like the top level: a key it does not have
    /// is a setting that would silently not work.
    #[test]
    fn a_key_the_tui_table_does_not_have_is_refused_by_name() {
        let error = parse(r#"{"tui": {"zzz_probe": 1}}"#)
            .expect_err("an unknown key inside tui is refused");

        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure, got {error:?}");
        };
        assert!(message.contains("zzz_probe"), "{message}");
    }

    /// The gateway's own tools, opted into by name (**D489**) — and the two
    /// ways to get the key wrong, both refused where somebody can read the
    /// refusal instead of a mid-turn 400.
    #[test]
    fn the_openrouter_table_takes_a_roster_and_refuses_a_name_outside_it() {
        let config = parse(r#"{"openrouter": {"server_tools": ["web_search", "datetime"]}}"#)
            .expect("a roster of published names parses");
        assert_eq!(config.openrouter.server_tools, ["web_search", "datetime"]);

        // Absent is empty, which is the whole opt-in: they bill per call.
        assert!(
            parse("{}")
                .expect("an empty config parses")
                .openrouter
                .server_tools
                .is_empty()
        );

        let error = parse(r#"{"openrouter": {"server_tools": ["web_serach"]}}"#)
            .expect_err("a name this gateway does not serve is refused");
        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure, got {error:?}");
        };
        assert!(message.contains("web_serach"), "{message}");
        assert!(
            message.contains("web_search") && message.contains("experimental__search_models"),
            "and the refusal lists what would have worked: {message}"
        );

        // The table is curated like every other one here.
        let error = parse(r#"{"openrouter": {"zzz_probe": 1}}"#)
            .expect_err("an unknown key inside openrouter is refused");
        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure, got {error:?}");
        };
        assert!(message.contains("zzz_probe"), "{message}");
    }

    /// OSC 9 degrades to nothing on a terminal that ignores it, which is the
    /// right failure for a default.
    #[test]
    fn the_notification_method_is_osc9_until_a_config_says_otherwise() {
        let config = parse(r#"{"tui": {}}"#).expect("it parses");
        assert_eq!(config.tui.notification_method(), NotificationMethod::Osc9);

        let config = parse(r#"{"tui": {"notification_method": "bel"}}"#).expect("it parses");
        assert_eq!(config.tui.notification_method(), NotificationMethod::Bel);
    }

    /// The table merges the way `agents` and `webfetch` do: field by field,
    /// with a tier that says nothing leaving the tier below it alone.
    #[test]
    fn a_closer_tier_overlays_the_tui_table_field_by_field() {
        let mut merged = parse(r#"{"tui": {"notifications": true, "notification_method": "bel"}}"#)
            .expect("it parses");
        merged.merge(
            parse(r#"{"tui": {"notifications": ["approval-requested"]}}"#).expect("it parses"),
        );

        assert_eq!(
            merged.tui.notifications,
            Some(Notifications::Events(vec![
                NotificationEvent::ApprovalRequested
            ])),
            "the closer tier's selection replaces"
        );
        assert_eq!(
            merged.tui.notification_method(),
            NotificationMethod::Bel,
            "the method it said nothing about stays"
        );
    }

    /// The roster is user-ordered and exact: what the config names, in the
    /// order it names it, is what the bar renders (**D469**).
    #[test]
    fn a_statusline_roster_keeps_the_order_the_config_wrote() {
        let config = parse(
            r#"{"tui": {"statusline": {
              "elements": ["model", "context", "tokens"],
              "max_width": 120,
              "detail": true
            }}}"#,
        )
        .expect("it parses");

        let statusline = config.tui.statusline.expect("the table was written");
        assert_eq!(
            statusline.elements,
            Some(vec![
                StatuslineElement::Model,
                StatuslineElement::Context,
                StatuslineElement::Tokens,
            ])
        );
        assert_eq!(statusline.max_width, Some(120));
        assert_eq!(statusline.detail, Some(true));
    }

    /// An element name nothing renders is refused naming it — serde's closed
    /// enum, the same refusal an unknown notification event gets.
    #[test]
    fn an_element_name_nothing_renders_is_refused_by_name() {
        let error = parse(r#"{"tui": {"statusline": {"elements": ["contextbar"]}}}"#)
            .expect_err("an unknown element name is refused");

        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure, got {error:?}");
        };
        assert!(message.contains("contextbar"), "{message}");
        assert!(
            message.contains("context"),
            "the refusal should list what the roster does have: {message}"
        );
    }

    /// The statusline table is curated like the `tui` table above it.
    #[test]
    fn a_key_the_statusline_table_does_not_have_is_refused_by_name() {
        let error = parse(r#"{"tui": {"statusline": {"zzz_probe": 1}}}"#)
            .expect_err("an unknown key inside statusline is refused");

        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure, got {error:?}");
        };
        assert!(message.contains("zzz_probe"), "{message}");
    }

    /// Field by field, like the rest of the `tui` table: a project that only
    /// reorders elements keeps the global tier's width cap, and a tier that
    /// says nothing leaves the whole table alone.
    #[test]
    fn a_closer_tier_overlays_the_statusline_table_field_by_field() {
        let mut merged =
            parse(r#"{"tui": {"statusline": {"elements": ["model"], "max_width": 100}}}"#)
                .expect("it parses");
        merged.merge(
            parse(r#"{"tui": {"statusline": {"elements": ["context", "tokens"]}}}"#)
                .expect("it parses"),
        );

        let statusline = merged.tui.statusline.expect("the table survives the merge");
        assert_eq!(
            statusline.elements,
            Some(vec![StatuslineElement::Context, StatuslineElement::Tokens]),
            "the closer tier's list replaces wholesale"
        );
        assert_eq!(
            statusline.max_width,
            Some(100),
            "the width it said nothing about stays"
        );

        let mut untouched =
            parse(r#"{"tui": {"statusline": {"detail": true}}}"#).expect("it parses");
        untouched.merge(parse(r#"{"tui": {}}"#).expect("it parses"));
        assert_eq!(
            untouched.tui.statusline,
            Some(StatuslineConfig {
                elements: None,
                max_width: None,
                detail: Some(true),
            }),
            "a tier that says nothing leaves the table alone"
        );
    }

    /// Per event, wholesale — the `mcp` arm's semantics, applied for the reason
    /// stated at the merge: these are commands, and a global one a project
    /// deliberately left out must not keep running underneath it.
    #[test]
    fn a_closer_tier_replaces_one_hook_event_and_leaves_the_others() {
        let mut merged = parse(
            r#"{
              "hooks": {
                "PreToolUse": [{"hooks": [{"type": "command", "command": "global-pre"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "global-stop"}]}]
              }
            }"#,
        )
        .expect("it parses");
        merged.merge(
            parse(
                r#"{
                  "hooks": {
                    "PreToolUse": [{"hooks": [{"type": "command", "command": "project-pre"}]}]
                  }
                }"#,
            )
            .expect("it parses"),
        );

        assert_eq!(
            merged.hooks["PreToolUse"],
            vec![HookMatcher {
                matcher: None,
                hooks: vec![HookHandler::Command(HookCommand {
                    command: "project-pre".to_owned(),
                    timeout: None,
                })],
            }],
            "the project's list is the list, not an addition to the global one"
        );
        assert_eq!(
            merged.hooks["Stop"][0].hooks,
            vec![HookHandler::Command(HookCommand {
                command: "global-stop".to_owned(),
                timeout: None,
            })],
            "an event the closer tier said nothing about is untouched"
        );
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
                    "timeout": 1234,
                    "output_limit": 4096
                },
                "hub": {
                    "type": "remote",
                    "url": "https://mcp.example/mcp",
                    "headers": {"Authorization": "Bearer x"},
                    "enabled": false
                },
                "auth": {
                    "type": "remote",
                    "url": "https://oauth.example/mcp",
                    "oauth": {}
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
        assert_eq!(local.output_limit, Some(4096));

        let McpServer::Remote(remote) = &config.mcp["hub"] else {
            panic!("the second entry is remote");
        };
        assert_eq!(remote.url, "https://mcp.example/mcp");
        assert_eq!(remote.headers["Authorization"], "Bearer x");
        assert!(!remote.enabled);
        assert_eq!(remote.timeout, None);
        assert_eq!(
            remote.output_limit, None,
            "an entry that says nothing about its budget gets the global default"
        );
        assert_eq!(
            remote.oauth, None,
            "an entry that says nothing has no login"
        );

        let McpServer::Remote(auth) = &config.mcp["auth"] else {
            panic!("the third entry is remote");
        };
        assert_eq!(
            auth.oauth,
            Some(McpOauth::default()),
            "`oauth: {{}}` turns discovery and dynamic registration on for this server"
        );
    }

    /// Every one of these is a config that would otherwise have described a
    /// server nothing could reach, silently.
    #[test]
    fn an_mcp_entry_that_describes_no_reachable_server_is_refused_by_name() {
        let cases = [
            // Upstream skips a type-less entry with a log line; a config that
            // names a server means to have one.
            (r#"{"mcp": {"x": {"command": ["a"]}}}"#, "type"),
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
            // A zero-byte output budget refuses every result, named by
            // `check_mcp` rather than by serde's generic NonZeroU64 message —
            // `output_limit` is a plain `u64` for exactly this reason.
            (
                r#"{"mcp": {"x": {"type": "local", "command": ["a"], "output_limit": 0}}}"#,
                "output_limit",
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

    /// The three refusals above, reached the way the two *writers* reach them
    /// — `ganja mcp add` and `ganja config import-opencode` both hold a decoded
    /// [`McpServer`] and no config file, and both call this method rather than
    /// spelling the rules a second and third time.
    ///
    /// Pinned here as well as through the loader because "one authority" is
    /// only true while the method itself refuses all three: the loader test
    /// above would still pass if a writer's copy of a rule drifted.
    #[test]
    fn the_post_decode_checks_are_one_method_three_callers_share() {
        let cases = [
            (r#"{"type": "local", "command": []}"#, "empty command"),
            (
                r#"{"type": "local", "command": ["a"], "output_limit": 0}"#,
                "output_limit of 0",
            ),
            (
                r#"{"type": "remote", "url": "http://mcp.example/mcp"}"#,
                "loopback",
            ),
            (r#"{"type": "remote", "url": "not a url"}"#, "no valid url"),
        ];

        for (text, named) in cases {
            let server: McpServer =
                serde_json::from_str(text).unwrap_or_else(|error| panic!("{text}: {error}"));
            let message = server
                .check("x")
                .expect_err(&format!("{text} describes no usable server"));
            assert!(message.contains(named), "{text}: {message}");
            assert!(message.contains("\"x\""), "and names the server: {message}");
        }

        let fine: McpServer = serde_json::from_str(r#"{"type": "local", "command": ["a"]}"#)
            .expect("the fixture is a server");
        assert_eq!(fine.check("x"), Ok(()));
    }

    /// `Serialize` beside `Deserialize`, so a caller that writes an entry —
    /// `ganja mcp add` building one, a listing asked for JSON — spells it out
    /// of the type the loader reads back rather than by hand.
    #[test]
    fn an_mcp_entry_survives_the_round_trip_through_its_own_serialization() {
        for text in [
            r#"{"mcp": {"x": {"type": "local", "command": ["bun", "x"], "environment": {"K": "v"}, "output_limit": 4096}}}"#,
            r#"{"mcp": {"x": {"type": "remote", "url": "https://mcp.example/mcp", "headers": {"X-A": "1"}, "oauth": {}}}}"#,
        ] {
            let config = parse(text).unwrap_or_else(|error| panic!("{text}: {error}"));
            let written = serde_json::to_string(&config.mcp["x"]).expect("the entry serializes");
            let read: McpServer = serde_json::from_str(&written)
                .unwrap_or_else(|error| panic!("what was written reads back: {written}: {error}"));
            assert_eq!(read, config.mcp["x"], "{written}");
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
    fn a_provider_entry_carries_every_field_it_may_hold() {
        let config = parse(
            r#"{"provider": {
                "local-llama": {
                    "dialect": "openai-chat-completions",
                    "base_url": "http://127.0.0.1:11434/v1",
                    "key_env": "LLAMA_API_KEY",
                    "headers": {"x-route": "gpu-0"}
                },
                "gateway": {
                    "dialect": "anthropic-messages",
                    "base_url": "https://messages.example/v1"
                }
            }}"#,
        )
        .expect("both dialects parse");

        let local = &config.provider["local-llama"];
        assert_eq!(local.dialect, Dialect::OpenaiChatCompletions);
        assert_eq!(local.base_url, "http://127.0.0.1:11434/v1");
        assert_eq!(local.key_env.as_deref(), Some("LLAMA_API_KEY"));
        assert_eq!(local.headers["x-route"], "gpu-0");

        let gateway = &config.provider["gateway"];
        assert_eq!(gateway.dialect, Dialect::AnthropicMessages);
        assert_eq!(
            gateway.key_env, None,
            "an entry that names no variable is answered by the store alone"
        );
        assert!(gateway.headers.is_empty());
    }

    /// Every one of these is a config that would otherwise have described an
    /// endpoint no session could reach — or, in the first case, one that would
    /// have been written, loaded and then never consulted.
    #[test]
    fn a_provider_entry_that_describes_no_usable_endpoint_is_refused_by_name() {
        let cases = [
            // Selection matches the builtins first, so this entry would be
            // dead the moment it loaded.
            (
                r#"{"provider": {"anthropic": {"dialect": "anthropic-messages",
                   "base_url": "https://proxy.example"}}}"#,
                "anthropic",
            ),
            // A dialect is a request/response mapping, and there is no arm for
            // one this build does not implement.
            (
                r#"{"provider": {"x": {"dialect": "gemini", "base_url": "https://a.test"}}}"#,
                "gemini",
            ),
            // Required: guessing the wire from a URL is how an Anthropic body
            // reaches a chat-completions server.
            (
                r#"{"provider": {"x": {"base_url": "https://a.test"}}}"#,
                "dialect",
            ),
            (
                r#"{"provider": {"x": {"dialect": "anthropic-messages"}}}"#,
                "base_url",
            ),
            // A key in a config file is the one thing that must not travel, so
            // the key upstream spells it with is not a key here.
            (
                r#"{"provider": {"x": {"dialect": "anthropic-messages",
                   "base_url": "https://a.test", "api_key": "sk-canary"}}}"#,
                "api_key",
            ),
            (
                r#"{"provider": {"x": {"dialect": "openai-chat-completions",
                   "base_url": "http://gateway.example/v1"}}}"#,
                "loopback",
            ),
            (
                r#"{"provider": {"x": {"dialect": "openai-chat-completions",
                   "base_url": "not a url"}}}"#,
                "base_url",
            ),
            // A blank variable names none, and would read as "there is no key"
            // — which sends somebody to fix a store that was never the problem.
            (
                r#"{"provider": {"x": {"dialect": "openai-chat-completions",
                   "base_url": "https://a.test", "key_env": "  "}}}"#,
                "key_env",
            ),
        ];

        for (text, named) in cases {
            let error = parse(text).expect_err(&format!("{text} describes no endpoint"));
            let ConfigError::Parse { message, .. } = &error else {
                panic!("expected a parse failure for {text}, got {error:?}");
            };
            assert!(message.contains(named), "{text}: {message}");
        }

        // A dialect nobody implements is refused with the two that exist named
        // back, because "gemini is not one of them" is only half an answer.
        let error =
            parse(r#"{"provider": {"x": {"dialect": "gemini", "base_url": "https://a.test"}}}"#)
                .expect_err("there is no third mapping");
        let ConfigError::Parse { message, .. } = &error else {
            panic!("expected a parse failure, got {error:?}");
        };
        assert!(
            message.contains("openai-chat-completions") && message.contains("anthropic-messages"),
            "{message}"
        );
    }

    /// The same rule the provider endpoints obey, and the same reason twice
    /// over: the credential travels in a header on every request, and so does
    /// anything in `headers`.
    #[test]
    fn a_configured_endpoint_may_be_plain_http_only_to_loopback() {
        let allowed = [
            "https://gateway.example/v1",
            "http://127.0.0.1:11434/v1",
            "http://localhost:8080",
            "http://[::1]:8080/v1",
        ];
        let refused = [
            "http://gateway.example/v1",
            "http://127.0.0.1.evil.test/v1",
            "http://localhost.evil.test/v1",
            "http://127.0.0.1@evil.test/v1",
        ];

        for base_url in allowed {
            let text = format!(
                r#"{{"provider": {{"x": {{"dialect": "openai-chat-completions",
                   "base_url": "{base_url}"}}}}}}"#
            );
            parse(&text).unwrap_or_else(|error| panic!("{base_url} is reachable: {error}"));
        }
        for base_url in refused {
            let text = format!(
                r#"{{"provider": {{"x": {{"dialect": "openai-chat-completions",
                   "base_url": "{base_url}"}}}}}}"#
            );
            let error = parse(&text).expect_err(base_url);
            // A base URL is allowed to carry a credential in its userinfo, so
            // the refusal describes the rule rather than quoting the URL.
            assert!(
                !error.to_string().contains(base_url),
                "{base_url} was echoed back by its own refusal: {error}"
            );
        }
    }

    /// A closer tier redeclaring a provider means *that* provider: the fields
    /// are one description of one endpoint, so a half-merged entry would
    /// present the old tier's credential to the new tier's host.
    #[test]
    fn a_closer_tier_replaces_a_whole_provider_entry() {
        let directory = temporary();
        let outer = directory.path().join("outer.json");
        let inner = directory.path().join("inner.json");
        plant(
            &outer,
            r#"{"provider": {"x": {"dialect": "openai-chat-completions",
               "base_url": "https://old.test/v1", "key_env": "OLD_KEY"}}}"#,
        );
        plant(
            &inner,
            r#"{"provider": {"x": {"dialect": "anthropic-messages",
               "base_url": "https://new.test"}}}"#,
        );

        let config = merge_files(&[outer, inner]).expect("both tiers parse");
        let entry = &config.provider["x"];
        assert_eq!(entry.dialect, Dialect::AnthropicMessages);
        assert_eq!(entry.base_url, "https://new.test");
        assert_eq!(
            entry.key_env, None,
            "the replaced entry's variable must not survive onto the new host"
        );
    }

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
            config.permission.rules(),
            rules(&[
                ("webfetch", "*", Action::Allow),
                ("bash", "git status", Action::Allow),
                ("bash", "git *", Action::Ask),
                // A rule this build cannot carry out is still a rule: `deny`
                // survives as itself rather than being flattened to `ask`.
                ("bash", "*", Action::Deny),
                ("edit", "*", Action::Ask),
            ])
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
              "default_provider": "openai",
              "default_agent": "plan",
              "agent": {"plan": {"mode": "primary", "disable": false}},
              "agents": {"concurrency": 3},
              "permission": {"bash": "ask"},
              "instructions": ["AGENTS.md"],
              "theme": "tokyonight",
              "theme_mode": "dark",
              "keybinds": {"agent_cycle": "tab"},
              "shell": "/bin/zsh",
              "command": {"ship": {"template": "release $ARGUMENTS", "agent": "build"}},
              "mcp": {"fs": {"type": "local", "command": ["bun", "x", "mcp-fs"], "output_limit": 8192}},
              "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "notify"}]}]},
              "provider": {"local-llama": {
                "dialect": "openai-chat-completions",
                "base_url": "http://127.0.0.1:11434/v1"
              }}
            }"#,
        )
        .expect("every curated key is a key");

        assert!(config.schema.is_some());
        assert_eq!(config.default_provider.as_deref(), Some("openai"));
        assert_eq!(config.default_agent.as_deref(), Some("plan"));
        assert_eq!(config.agent["plan"].mode, Some(AgentMode::Primary));
        assert_eq!(config.agent["plan"].disable, Some(false));
        assert_eq!(config.agents.concurrency(), 3);
        assert_eq!(config.theme_mode, Some(ThemeMode::Dark));
        assert_eq!(config.shell.as_deref(), Some("/bin/zsh"));
        assert_eq!(config.command["ship"].template, "release $ARGUMENTS");
        assert_eq!(config.command["ship"].agent.as_deref(), Some("build"));
        assert!(config.command["ship"].description.is_none());
        assert!(matches!(config.mcp["fs"], McpServer::Local(_)));
        assert_eq!(config.hooks["Stop"].len(), 1);
        assert_eq!(
            config.provider["local-llama"].dialect,
            Dialect::OpenaiChatCompletions
        );
    }

    /// The key is a scalar like `model`, and merges like one: a tier that
    /// says nothing leaves the tier below it alone, and a closer one replaces.
    #[test]
    fn a_closer_tier_decides_the_default_provider_only_when_it_names_one() {
        let mut merged = parse(r#"{"default_provider": "anthropic"}"#).expect("it parses");
        merged.merge(parse(r#"{"model": "openai/gpt-5.6"}"#).expect("it parses"));
        assert_eq!(merged.default_provider.as_deref(), Some("anthropic"));

        merged.merge(parse(r#"{"default_provider": "openai"}"#).expect("it parses"));
        assert_eq!(merged.default_provider.as_deref(), Some("openai"));
    }

    /// The seeded effort is a scalar too, and merges like every other one —
    /// which is what lets a global file set the house default and one project
    /// run hotter without restating anything else.
    #[test]
    fn a_closer_tier_decides_the_effort_only_when_it_names_one() {
        let mut merged = parse(r#"{"effort": "high"}"#).expect("it parses");
        assert_eq!(merged.effort.as_deref(), Some("high"));

        merged.merge(parse(r#"{"model": "openai/gpt-5.6"}"#).expect("it parses"));
        assert_eq!(merged.effort.as_deref(), Some("high"));

        merged.merge(parse(r#"{"effort": "max"}"#).expect("it parses"));
        assert_eq!(merged.effort.as_deref(), Some("max"));
    }

    /// Nothing about an effort is checked here: which names exist depends on
    /// the model the other tiers settle on, so the loader carries whatever was
    /// written and adoption decides. What is still refused is a *sibling* the
    /// struct has no field for — the discipline every key here holds.
    #[test]
    fn an_effort_name_is_carried_unvalidated_and_its_neighbours_still_are_not() {
        let config = parse(r#"{"effort": "wildly-not-an-effort"}"#).expect("it parses");
        assert_eq!(config.effort.as_deref(), Some("wildly-not-an-effort"));

        let error = parse(r#"{"efort": "high"}"#).expect_err("a typo is refused");
        assert!(format!("{error}").contains("efort"), "got {error}");
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

    /// The binding rule both config model keys ride, in every direction it has
    /// (bead `s4w`): a prefix naming the selected provider applies, a bare
    /// spec applies to whoever is running, and a prefix naming somebody else
    /// yields nothing at all — never the stripped tail, which is what reached
    /// a live openai request as a bare `claude-x` and came back a 400.
    #[test]
    fn a_prefixed_model_binds_to_the_provider_it_names_and_to_no_other() {
        const KEY: &str = "the config's `model` key";

        assert_eq!(
            model_bound_to("anthropic/claude-sonnet-5", "anthropic", KEY),
            Some("claude-sonnet-5")
        );
        assert_eq!(
            model_bound_to("cursor/claude-x", "openai", KEY),
            None,
            "the prefix names cursor, so openai is not asked for its tail"
        );
        assert_eq!(
            model_bound_to("claude-sonnet-5", "openai", KEY),
            Some("claude-sonnet-5"),
            "a bare spec claims no provider, so it applies to whichever is running"
        );

        // Only the first slash separates, so a gateway's own two-part model id
        // survives the rule intact.
        assert_eq!(
            model_bound_to("openrouter/anthropic/claude-3", "openrouter", KEY),
            Some("anthropic/claude-3")
        );

        // A config-declared endpoint is compared exactly as a builtin is:
        // there is no shipped list here to fall out of date, so an id this
        // build has never heard of binds to itself and to nothing else.
        assert_eq!(
            model_bound_to("local-llama/tiny-instruct", "local-llama", KEY),
            Some("tiny-instruct")
        );
        assert_eq!(
            model_bound_to("local-llama/tiny-instruct", "anthropic", KEY),
            None
        );
        assert_eq!(
            model_bound_to("anthropic/claude-sonnet-5", "local-llama", KEY),
            None,
            "a builtin's spec does not travel to a config's endpoint either"
        );
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

    /// The config home's two *discovered* candidates, ruled on by what is
    /// there. The environment tier above them needs the environment and is
    /// pinned in `tests/skills.rs`, which owns that binary's variables; these
    /// three need only two directories, so they say the same thing on every
    /// machine.
    #[test]
    fn the_xdg_home_answers_whenever_it_is_there() {
        let directory = temporary();
        let xdg = directory.path().join("config").join("ganja");
        let dotted = directory.path().join(".ganja");
        fs::create_dir_all(&xdg).expect("the fixture is creatable");

        assert_eq!(super::discovered(xdg.clone(), dotted.clone()), xdg);

        // And still, with the dotted one beside it: this is one home, not a
        // merge, and the higher tier is the one that answers.
        fs::create_dir_all(&dotted).expect("the fixture is creatable");
        assert_eq!(super::discovered(xdg.clone(), dotted), xdg);
    }

    #[test]
    fn the_dotted_home_answers_only_when_the_xdg_one_is_absent() {
        let directory = temporary();
        let xdg = directory.path().join("config").join("ganja");
        let dotted = directory.path().join(".ganja");
        fs::create_dir_all(&dotted).expect("the fixture is creatable");

        assert_eq!(super::discovered(xdg.clone(), dotted.clone()), dotted);

        // A file where the directory would be is not a home either — the check
        // is `is_dir`, not "something is there".
        fs::create_dir_all(xdg.parent().expect("a parent")).expect("creatable");
        fs::write(&xdg, "not a directory").expect("writable");
        assert_eq!(super::discovered(xdg, dotted.clone()), dotted);
    }

    /// Nothing on disk: nothing to read either way, so what comes back is the
    /// one whoever writes next should create. See [`super::config_home`] for
    /// why that is the XDG path and not the dotted one.
    #[test]
    fn with_neither_on_disk_the_answer_is_the_one_a_writer_should_create() {
        let directory = temporary();
        let xdg = directory.path().join("config").join("ganja");
        let dotted = directory.path().join(".ganja");

        assert_eq!(super::discovered(xdg.clone(), dotted), xdg);
    }
}
