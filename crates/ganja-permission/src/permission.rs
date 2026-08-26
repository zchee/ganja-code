//! Decides which tool calls run unasked and which wait for the user.
//!
//! Spec: upstream `packages/opencode/src/permission/`. Reading, searching and
//! planning are free; anything that writes files, runs commands, or reaches the
//! network asks first. An "always allow" answer becomes a rule, and rules are
//! stored per project so a decision outlives the process.
//!
//! # How a call is decided
//!
//! A call is turned into one or more *patterns* — for a shell command, the
//! source of each command it runs; for a fetch, its URL; for a write or an
//! edit, the file it names — and each pattern is looked up in the rules. The
//! last rule whose tool and pattern both match wins, mirroring upstream's
//! `evaluate` in `permission/index.ts`, so a later rule can loosen or tighten
//! an earlier one. A pattern no rule covers falls back to the defaults in
//! [`ASK_BY_DEFAULT`]. Every pattern has to come back allowed for the call to
//! run unasked, which is what keeps `cargo test` from carrying `&& rm -rf /`
//! in with it.
//!
//! That fallback — and only that fallback — is a caller's to move.
//! [`Permissions::gate_with_default`] takes an effective default for the call
//! and consults it exactly where no rule matched, ahead of the static lists;
//! [`Permissions::gate`] is the same walk with nothing handed in. The caller
//! computes the default from state this crate deliberately cannot see —
//! whether the session holds a team, what a config key resolved to — and
//! hands in only the conclusion, so the crate still reads no config and stays
//! ignorant of policy. The alternative — a provenance field on the verdict,
//! overridden above this crate — was rejected: whoever overrides a verdict
//! re-derives the precedence ladder outside it, and a second spelling of the
//! ladder is free to disagree with the first. In here the ladder is spelled
//! once.
//!
//! # Where a call may run
//!
//! Those patterns say *what* a call does and nothing about *where*, and the
//! rules they produce are deliberately coarse — one answer about `cargo test`
//! covers every way of running the tests, and one answer about `write` covers
//! every file. So upstream gates the location separately, under a permission
//! of its own ([`EXTERNAL_DIRECTORY`]) that a call raises alongside the tool's
//! when it would work outside the project: a shell command on its `workdir`
//! *and* on every path its file-naming commands are handed (`tool/shell.ts`,
//! `collect` and `ask`), a write or an edit on the directory holding the file
//! (`tool/write.ts`, `tool/edit.ts`). All of them have to come back allowed.
//! Without that second gate a remembered `cargo test` would run in any
//! checkout the model names, build script and all — and a remembered `rm *`
//! would reach `/etc/passwd`, since the pattern that remembers *what* runs
//! says nothing about what it is pointed at.
//!
//! A rule naming a tool cannot answer this one — `write` is not
//! `external_directory` — which is what keeps an "always" given before this
//! gate existed meaning what its user meant. A rule whose permission is `*`
//! does answer it, because that is what writing `*` asks for.
//!
//! # What "always" remembers
//!
//! For a shell command, upstream does not remember the command; it remembers
//! the *kind* of command, by keeping the tokens that name it and wildcarding
//! the arguments — `cargo test --release` becomes `cargo *`, `npm run dev`
//! becomes `npm run dev *`. How many tokens name a command comes from
//! upstream's table in `permission/arity.ts`, ported here verbatim. For
//! every other tool, "always" is a rule covering the whole tool, as upstream's
//! tools ask with `always: ["*"]`. A call that also raised the location gate
//! leaves a rule behind for each directory it named, so that answering the
//! dialog the user actually saw does not leave them answering it again.
//!
//! # Storage
//!
//! Rules live in `permissions.json` under the project's data directory (see
//! [`crate::project`]):
//!
//! ```json
//! {
//!   "version": 1,
//!   "rules": [
//!     { "permission": "shell", "pattern": "cargo *", "action": "allow" },
//!     { "permission": "external_directory", "pattern": "/tmp/scratch/*", "action": "allow" },
//!     { "permission": "write", "pattern": "*", "action": "allow" }
//!   ]
//! }
//! ```
//!
//! `permission` is what the rule speaks for — a tool, or [`EXTERNAL_DIRECTORY`]
//! — and `pattern` is what it covers within that; both are matched as
//! wildcards, so a configuration phase can
//! write `{ "permission": "*", "pattern": "*", "action": "ask" }` and have it
//! mean every call. An `action` this build does not know is kept as it was
//! written and treated as `ask`, so a rule from a newer build can only ever
//! make this one more cautious, never less.
//!
//! # Where the rules come from
//!
//! Two layers, and the boundary between them is who wrote them. The
//! **baseline** is what a build decided — the ruleset of the agent the session
//! runs as, which already carries the config's own `permission` block
//! (`ganja-core`'s `agent`) — and it is replaced wholesale whenever the agent
//! changes. The **stored** rules are the answers a person gave, and they sit
//! on top, so an "always allow" is never undone by switching agents.
//! Evaluation walks the concatenation backwards, which is the same
//! last-match-wins walk it always was.
//!
//! Nothing here can fail a turn. A store that cannot be read is quarantined or
//! ignored with a warning and the session falls back to the defaults; a store
//! that cannot be written costs the answer its persistence and nothing else.

use std::{
    borrow::Cow,
    fmt, fs, io,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{
    Deserialize, Serialize,
    de::{self, MapAccess, Visitor},
};

use crate::project::Project;

/// File the rules are stored in, under the project's data directory.
pub const FILE: &str = "permissions.json";

/// Format the rules are written in. A store carrying a higher version was
/// written by a build that knows something this one does not, and is left
/// alone rather than read or rewritten.
pub const VERSION: u32 = 1;

/// Suffix a store that could not be parsed is moved aside with, so that a
/// session can carry on with the defaults without the file being lost.
const QUARANTINE: &str = "permissions.json.corrupt";

/// The pattern that covers everything, and the only pattern non-shell rules
/// use.
const ANY: &str = "*";

/// The end of a pattern whose tail is optional: upstream rewrites a trailing
/// `" *"` as `( .*)?`, so `ls *` covers a bare `ls` as well as `ls -la`.
const OPTIONAL_TAIL: &[char] = &[' ', '*'];

/// Permission a call raises when it would work somewhere the project does not
/// reach, spelled as upstream spells it because the name travels in the stored
/// file.
///
/// Not a tool, and deliberately not one: upstream raises it *beside* the
/// tool's own permission rather than instead of it (`tool/shell.ts`, `ask`),
/// so that a rule saying which commands may run says nothing about where.
pub const EXTERNAL_DIRECTORY: &str = "external_directory";

/// Names that ask by default: every tool that changes state outside the
/// conversation, and the location gate. Anything else — reading, searching,
/// listing, planning — runs unasked unless a rule says otherwise.
///
/// Tools that this build does not register are listed anyway, because the
/// answer to "may I run a shell command" must not depend on what the tool
/// happens to be called this week.
pub const ASK_BY_DEFAULT: &[&str] = &[
    "apply_patch",
    "bash",
    "edit",
    EXTERNAL_DIRECTORY,
    "shell",
    TASK,
    "webfetch",
    "websearch",
    "write",
];

/// What every tool an MCP server contributed is named with.
///
/// A prefix rather than a list, because the names are not known until a
/// server has been connected and asked. Anything wearing it asks by default,
/// which is what closes the "an id nobody listed runs unasked" default for the
/// whole namespace: an MCP tool is somebody else's code, reached over somebody
/// else's transport, and the build has no idea what it does.
///
/// Upstream reaches the same answer by a different road — nothing matched, and
/// its default for nothing-matched is `ask` (`permission/index.ts:28-38`).
pub const MCP_PREFIX: &str = "mcp__";

/// Tool that runs a whole second agent loop, which is why it asks: everything
/// the subagent goes on to do is done under an answer given here.
pub const TASK: &str = "task";

/// Tools whose argument is a shell command, and which therefore get a rule per
/// command rather than one for the whole tool.
const SHELL_LIKE: &[&str] = &["bash", "shell"];

/// Tools whose call names a URL, which is what upstream checks them against
/// (`tool/webfetch.ts`: `patterns: [params.url]`).
const URL_LIKE: &[&str] = &["webfetch"];

/// Tools whose call names one file, which upstream checks them against as a
/// path relative to the project (`tool/write.ts`, `tool/edit.ts`,
/// `tool/read.ts`: `patterns: [path.relative(instance.worktree, filepath)]`).
///
/// `read` is here for the same reason it is upstream — it asks with the file
/// it would read, and calls `assertExternalDirectory` on it — and without it
/// the shared `read: {"*.env": "ask"}` default would be a rule about a pattern
/// no read call ever produces.
const FILE_LIKE: &[&str] = &["edit", "read", "write"];

/// Argument carrying the command a shell-like tool would run.
const COMMAND: &str = "command";

/// Argument carrying the directory a shell-like tool would run in.
const WORKDIR: &str = "workdir";

/// Argument carrying the URL a fetch would reach.
const URL: &str = "url";

/// Argument carrying the agent a task call would spawn. Upstream asks with
/// `patterns: [subagent_type]` (`tool/task.ts`), so a rule can name one
/// subagent without naming the rest.
const SUBAGENT_TYPE: &str = "subagent_type";

/// Argument carrying the file a write or an edit would change. Upstream's
/// camelCase spelling, because the model is what sends it.
const FILE_PATH: &str = "filePath";

/// What upstream names the project root itself with, a relative path having no
/// characters of its own to be named by (`location-mutation.ts`, `resolve`).
const HERE: &str = ".";

/// Commands that only move the shell around. Upstream leaves them out of the
/// patterns a call is checked against (`tool/shell.ts`, `CWD`), so `cd build`
/// on its own needs no permission and `cd build && make` is judged on `make`.
const CWD_COMMANDS: &[&str] = &[
    "cd",
    "chdir",
    "popd",
    "pushd",
    "push-location",
    "set-location",
];

/// Commands whose arguments name files, so that what they are handed says
/// *where* a call would work and not only what it would do.
///
/// Upstream's `FILES` (`tool/shell.ts:29-50`), POSIX subset: the PowerShell
/// aliases listed beside these (`get-content`, `copy-item`, …) and the separate
/// `CMD_FILES` set are inert here, because `default_shell` only ever picks zsh,
/// bash or sh.
///
/// Upstream builds the set as `[...CWD, …]`, and the directory moves belonging
/// to it is the point rather than an oversight: `cd /etc` names /etc as surely
/// as `cat /etc/passwd` does, and it takes every later command in the same
/// shell along with it.
const FILE_COMMANDS: &[&str] = &["cat", "chmod", "chown", "cp", "mkdir", "mv", "rm", "touch"];

/// Characters that end one command and begin the next.
///
/// This is not a shell parser. Upstream runs the command through a real
/// grammar and takes one pattern per command node; here the text is split at
/// the operators that separate commands, ignoring any inside quotes. The
/// difference shows up on constructs a split cannot see through — a command
/// substitution's text lands in the surrounding pattern as well as its own —
/// and always in the direction of producing more patterns, each of which has
/// to be allowed, so the error is towards asking.
const SEPARATORS: &[char] = &[';', '\n', '&', '|', '(', ')', '`'];

/// Distinguishes the temporary files of writes that overlap, so that two of
/// them cannot end up sharing one and renaming each other's half-written
/// bytes into place.
static WRITES: AtomicU64 = AtomicU64::new(0);

/// How many tokens name a command, by prefix.
///
/// Ported verbatim from upstream's `packages/opencode/src/permission/arity.ts`,
/// re-sorted so it can be searched: `git` is two tokens, so `git checkout main`
/// is named by `git checkout`; `npm run` is three, so `npm run dev` keeps its
/// script. A prefix that is not listed names itself in one token.
const ARITY: &[(&str, usize)] = &[
    ("aws", 3),
    ("az", 3),
    ("bazel", 2),
    ("brew", 2),
    ("bun", 2),
    ("bun run", 3),
    ("bun x", 3),
    ("cargo", 2),
    ("cargo add", 3),
    ("cargo run", 3),
    ("cat", 1),
    ("cd", 1),
    ("cdk", 2),
    ("cf", 2),
    ("chmod", 1),
    ("chown", 1),
    ("cmake", 2),
    ("composer", 2),
    ("consul", 2),
    ("consul kv", 3),
    ("cp", 1),
    ("crictl", 2),
    ("deno", 2),
    ("deno task", 3),
    ("docker", 2),
    ("docker builder", 3),
    ("docker compose", 3),
    ("docker container", 3),
    ("docker image", 3),
    ("docker network", 3),
    ("docker volume", 3),
    ("doctl", 3),
    ("echo", 1),
    ("eksctl", 2),
    ("eksctl create", 3),
    ("env", 1),
    ("export", 1),
    ("firebase", 2),
    ("flyctl", 2),
    ("gcloud", 3),
    ("gh", 3),
    ("git", 2),
    ("git config", 3),
    ("git remote", 3),
    ("git stash", 3),
    ("go", 2),
    ("gradle", 2),
    ("grep", 1),
    ("helm", 2),
    ("heroku", 2),
    ("hugo", 2),
    ("ip", 2),
    ("ip addr", 3),
    ("ip link", 3),
    ("ip netns", 3),
    ("ip route", 3),
    ("kill", 1),
    ("killall", 1),
    ("kind", 2),
    ("kind create", 3),
    ("kubectl", 2),
    ("kubectl kustomize", 3),
    ("kubectl rollout", 3),
    ("kustomize", 2),
    ("ln", 1),
    ("ls", 1),
    ("make", 2),
    ("mc", 2),
    ("mc admin", 3),
    ("minikube", 2),
    ("mkdir", 1),
    ("mongosh", 2),
    ("mv", 1),
    ("mvn", 2),
    ("mysql", 2),
    ("ng", 2),
    ("npm", 2),
    ("npm exec", 3),
    ("npm init", 3),
    ("npm run", 3),
    ("npm view", 3),
    ("nvm", 2),
    ("nx", 2),
    ("openssl", 2),
    ("openssl req", 3),
    ("openssl x509", 3),
    ("pip", 2),
    ("pipenv", 2),
    ("pnpm", 2),
    ("pnpm dlx", 3),
    ("pnpm exec", 3),
    ("pnpm run", 3),
    ("podman", 2),
    ("podman container", 3),
    ("podman image", 3),
    ("poetry", 2),
    ("ps", 1),
    ("psql", 2),
    ("pulumi", 2),
    ("pulumi stack", 3),
    ("pwd", 1),
    ("pyenv", 2),
    ("python", 2),
    ("rake", 2),
    ("rbenv", 2),
    ("redis-cli", 2),
    ("rm", 1),
    ("rmdir", 1),
    ("rustup", 2),
    ("serverless", 2),
    ("sfdx", 3),
    ("skaffold", 2),
    ("sleep", 1),
    ("sls", 2),
    ("source", 1),
    ("sst", 2),
    ("swift", 2),
    ("systemctl", 2),
    ("tail", 1),
    ("terraform", 2),
    ("terraform workspace", 3),
    ("tmux", 2),
    ("touch", 1),
    ("turbo", 2),
    ("ufw", 2),
    ("unset", 1),
    ("vault", 2),
    ("vault auth", 3),
    ("vault kv", 3),
    ("vercel", 2),
    ("volta", 2),
    ("which", 1),
    ("wp", 2),
    ("yarn", 2),
    ("yarn dlx", 3),
    ("yarn run", 3),
];

/// What to do with a tool call before it runs.
///
/// Ordered by how much it stops: a call that produces several answers takes
/// the strongest of them, so one denied pattern refuses the whole call and one
/// unfamiliar one asks about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Decision {
    /// Run it without asking.
    Allow,
    /// Put it in front of the user first.
    Ask,
    /// Refuse it outright. No dialog: a rule already answered, and answering
    /// again is not the user's to do. The model reads the refusal as the
    /// call's result and carries on — a denial is information, never a turn
    /// abort.
    Deny,
}

/// What a rule does with the calls it covers.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Run them without asking.
    Allow,
    /// Put them in front of the user.
    Ask,
    /// Refuse them, without asking anyone. Upstream's third action
    /// (`permission/index.ts`, `ask`), and what a config `permission` block
    /// writes to take a tool away from an agent.
    Deny,
    /// Something a newer build wrote. Kept exactly as it was found so a
    /// rewrite does not drop it, and treated as [`Action::Ask`]: a rule this
    /// build cannot carry out is still a rule saying this call is not routine.
    #[serde(untagged)]
    Other(String),
}

impl Action {
    /// What this action decides on its own.
    ///
    /// Public because a caller above this crate reads rules the engine never
    /// turns into a call — the teammate spawn gate judges a flag and a
    /// directory that no tool has arguments for — and a second hand-written
    /// match over these four variants is a second place for
    /// [`Action::Other`]'s reading to drift.
    #[must_use]
    pub fn decision(&self) -> Decision {
        match self {
            Self::Allow => Decision::Allow,
            Self::Deny => Decision::Deny,
            Self::Ask | Self::Other(_) => Decision::Ask,
        }
    }
}

/// One stored decision about a family of calls.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Rule {
    /// Tool the rule speaks for, matched as a wildcard pattern.
    pub permission: String,
    /// Which of that tool's calls it covers, matched as a wildcard pattern.
    pub pattern: String,
    /// What to do with them.
    pub action: Action,
}

/// What one tool's key in a `permission` object said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleSet {
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
/// It lives here rather than beside the rest of the config because what it
/// describes is rules: a config file is one of the places [`Rule`]s come from,
/// and the type a document parses into is this layer's business.
///
/// Order is semantic. Evaluation is last-match-wins ([`crate::permission`]), so
/// which of two rules covering the same call was written second is the whole
/// answer — which is why this is a list rather than a map, and why nothing here
/// ever sorts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PermissionConfig {
    /// One entry per tool key, in the order the document spelled them.
    /// Private even though the config layer's tests read it — order is
    /// semantic here (the last matching rule wins), and a field an outside
    /// writer could push onto would let the entries and the scalar flag
    /// disagree about what the object means. [`PermissionConfig::entries`]
    /// lends it out instead.
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

    /// Overlays `other`, replicating upstream's `mergeDeep` at both levels: a
    /// re-specified tool keeps its position and merges, a tool that is new is
    /// appended, and a bare action on either side replaces rather than merges.
    ///
    /// Reached from the config layer, which is where two files become one.
    pub fn merge(&mut self, other: &Self) {
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

/// Everything one gated call needs decided, judged in a single look.
///
/// A call is judged once and acted on in several places: the loop that runs
/// it, the dialog that discloses it, and — a user round-trip later — the store
/// that remembers the answer. Each of those used to ask the ruleset again, and
/// each of those derivations had to agree with the others for the dialog to be
/// about the call that was actually judged, and for the answer to be about the
/// dialog. Carrying them together makes that agreement structural instead of a
/// thing kept true by hand.
///
/// Holding one of these across the wait for an answer is safe because none of
/// it is a snapshot of anything mutable: `Permissions::outside_dirs` reads
/// the project's bounds and the call's arguments, and what an "always" would
/// learn is a function of those same two. Only [`CallDecision::action`] and
/// [`CallDecision::rules`] consult the rules at all, and both are read before
/// anybody is asked anything.
#[derive(Debug)]
pub struct CallDecision {
    /// What to do with the call.
    pub action: Decision,
    /// The rules with anything to say about the tool, which is what a refusal
    /// hands the model.
    pub rules: Vec<Rule>,
    /// The directories outside the project the call would work in.
    ///
    /// A dialog has to name them: the question "may this run" is not
    /// answerable without "where", and one that showed the command and not the
    /// directory would be asking about something narrower than what an answer
    /// covers.
    pub directories: Vec<PathBuf>,
    /// What an "always" answer would leave behind — a subset of `directories`
    /// (see [`means_itself`]) plus the tool's own rules.
    ///
    /// Not public, and not for anyone but [`Permissions::remember`]: it is the
    /// one part of a decision that is an instruction rather than a fact, and a
    /// forged one would store rules nobody was shown.
    learned: Vec<Rule>,
}

/// The project's permission rules, layered over the defaults.
#[derive(Debug, Default)]
pub struct Permissions {
    /// What the build decided, beneath everything a person did: the ruleset of
    /// the agent this session runs as, config rules and all. Empty until
    /// [`Permissions::set_baseline`] installs one, which is what keeps a set
    /// built by [`Permissions::default`] judging exactly what it always did.
    baseline: Vec<Rule>,
    /// Stored rules, then the ones this session added, in the order they were
    /// decided: the last one that matches a call wins.
    rules: Vec<Rule>,
    /// Where an answer is persisted, when it can be. [`None`] leaves the
    /// session's answers in memory — there is nowhere to write, or writing
    /// would tread on a store this build does not understand.
    store: Option<Store>,
    /// Where the project starts, resolved, so that a call naming somewhere
    /// else can be told from one naming a file in the checkout.
    ///
    /// [`None`] means there is nothing to compare a directory against, and the
    /// location gate then does not apply at all. That is not a way past it:
    /// the only constructor that leaves it empty is [`Permissions::default`],
    /// which stores no rules either, so nothing it judges was ever allowed by
    /// an answer. Every path a session is actually built on goes through
    /// [`Permissions::load`], which always fills this in — the test
    /// `a_loaded_permission_set_knows_where_its_project_is` is what holds that
    /// true.
    root: Option<PathBuf>,
    /// Where a relative path in a call resolves from, resolved, mirroring the
    /// shell tool's own `ctx.cwd.join(workdir)`. Filled in and left empty
    /// together with the root above, so there is no set to reason about that
    /// knows one and not the other.
    cwd: Option<PathBuf>,
}

impl Permissions {
    /// Loads the rules for the project at `cwd`, falling back to the defaults
    /// when nothing is stored or the store cannot be read.
    #[must_use]
    pub fn load(cwd: &Path) -> Self {
        let project = Project::resolve(cwd);
        let mut permissions = match project.data_dir() {
            Ok(directory) => Self::open(directory.join(FILE)),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "permission answers cannot be stored and will not outlive this session"
                );
                Self::default()
            }
        };

        // Set whichever way the store went: where the project is does not
        // depend on whether its rules can be written, and a session that
        // cannot remember an answer still has to judge where a command runs.
        permissions.root = Some(resolve(project.root()));
        permissions.cwd = Some(resolve(cwd));

        permissions
    }

    /// Replaces the rules beneath the stored ones — the agent's ruleset.
    ///
    /// Wholesale rather than appended: an agent switch changes which build-side
    /// rules apply, and layering the new set on top of the old one would leave
    /// a denial from an agent nobody is running any more.
    pub fn set_baseline(&mut self, rules: Vec<Rule>) {
        self.baseline = rules;
    }

    /// A second ruleset for the same project, judging by `baseline` instead of
    /// this one's.
    ///
    /// Everything a person decided comes across unchanged, and so does the
    /// store, so an "always" given inside the derived set outlives the process
    /// exactly as one given outside it would. What does *not* come across is
    /// the baseline: the caller is running as somebody else — a command that
    /// named its own agent — and that is the whole point of deriving rather
    /// than sharing.
    ///
    /// This is the *attended* derivation: the person who answered those
    /// dialogs is watching the turn it is used for. Anything unattended wants
    /// [`Permissions::derive_subagent`] instead.
    #[must_use]
    pub fn derive(&self, baseline: Vec<Rule>) -> Self {
        Self {
            baseline,
            rules: self.rules.clone(),
            store: self.store.clone(),
            root: self.root.clone(),
            cwd: self.cwd.clone(),
        }
    }

    /// The same, for work nobody is watching: the answers a person gave stay
    /// behind.
    ///
    /// [`Permissions::inherited_by_subagent`] describes which of the parent's
    /// rules a subagent is bound by, and allows are deliberately not among
    /// them — but carrying the stored tier across would have put them back,
    /// and at the *top* of the order, where they outrank the subagent's own
    /// rules and every refusal inherited beneath them. One "always, run
    /// `cargo`" answered about a supervised turn would then quietly authorize
    /// every later delegation, which is the opposite of what the dialog asked.
    ///
    /// The store still comes across, so a dialog the *child* raises can be
    /// answered "always" and outlive the process the same way — what is
    /// dropped is the parent's answers, not the ability to give one.
    #[must_use]
    pub fn derive_subagent(&self, baseline: Vec<Rule>) -> Self {
        Self {
            baseline,
            rules: Vec::new(),
            store: self.store.clone(),
            root: self.root.clone(),
            cwd: self.cwd.clone(),
        }
    }

    /// The rules a subagent inherits from the session that spawned it.
    ///
    /// Upstream's `deriveSubagentSessionPermission`
    /// (`agent/subagent-permissions.ts`): only refusals and the location gate
    /// travel down. A parent's *allows* deliberately do not — a subagent runs
    /// unattended, and inheriting "yes, run cargo" from a dialog the user
    /// answered about the parent's own work would hand that answer to work
    /// nobody watched.
    ///
    /// Ganja reads the whole ordered set rather than upstream's session tier
    /// alone (deviation: subagent-inherits-every-deny), because a ganja session
    /// has no way to hold a deny of its own: config denies land in the agent's
    /// baseline, and a config that took `webfetch` away from the session has to
    /// take it away from what the session delegates.
    #[must_use]
    pub fn inherited_by_subagent(&self) -> Vec<Rule> {
        self.ordered()
            .filter(|rule| rule.action == Action::Deny || rule.permission == EXTERNAL_DIRECTORY)
            .cloned()
            .collect()
    }

    /// Whether any rule here has an opinion about `permission`, which is what
    /// decides whether a subagent's ruleset gets the `task`/`todowrite` denials
    /// appended (upstream's "unless the set mentions it").
    #[must_use]
    pub fn baseline_mentions(rules: &[Rule], permission: &str) -> bool {
        rules.iter().any(|rule| rule.permission == permission)
    }

    /// What to do with a call to `tool` carrying `args`, and everything acting
    /// on that answer will need.
    ///
    /// One look at the rules per call. The location gate, the tool's own
    /// patterns, the rules a refusal quotes and the rules an "always" would
    /// leave behind are all read off the same call at the same moment, so
    /// there is no way for the dialog, the decision and the stored answer to
    /// be about three subtly different things.
    #[must_use]
    pub fn gate(&self, tool: &str, args: &serde_json::Value) -> CallDecision {
        self.gate_with_default(tool, args, None)
    }

    /// [`Permissions::gate`], with the caller's own answer for the layer
    /// where nothing matched.
    ///
    /// `unmatched` sits at exactly one rung: beneath every rule — the agent's
    /// baseline, the config's, the answers a person stored — and ahead of the
    /// static [`ASK_BY_DEFAULT`]/allow defaults. [`None`] is that static
    /// ladder untouched, which is what [`Permissions::gate`] hands in.
    /// Everything above the rung is out of the caller's reach by
    /// construction: an explicit rule still wins, a stored "always allow"
    /// still allows, and a deny still denies.
    ///
    /// The default speaks for `tool` and for nothing else. The location gate
    /// a call raises beside its own permission ([`EXTERNAL_DIRECTORY`]) keeps
    /// the static ladder: what a caller knows about the tool says nothing
    /// about *where* the call may work, and a default that also answered the
    /// location question would cover more than the caller computed.
    #[must_use]
    pub fn gate_with_default(
        &self,
        tool: &str,
        args: &serde_json::Value,
        unmatched: Option<Decision>,
    ) -> CallDecision {
        let directories = self.outside_dirs(tool, args);

        // Upstream raises this one first and on its own (`tool/shell.ts`,
        // `ask`), and it is asked about even when the call produces no
        // patterns of its own: `cd build` in somebody else's checkout is
        // still somebody else's checkout.
        //
        // Every directory the call names has to come back allowed, the same
        // all-or-nothing rule the patterns below get: a call naming three
        // directories is stopped by the one that was never answered for.
        //
        // And it keeps the static ladder whatever the caller handed in:
        // `unmatched` speaks for the tool, and where a call may work is not a
        // question the caller's computation was about.
        let located = directories
            .iter()
            .map(|directory| self.decide(EXTERNAL_DIRECTORY, &covering(directory), None))
            .max()
            .unwrap_or(Decision::Allow);

        let patterns = self.patterns(tool, args);

        // Nothing to judge means the call is nothing but directory moves,
        // which [`moves_only`] has already proven cannot run anything else.
        // Spelled out rather than left to `max` over an empty set, because
        // "produced no patterns" and "every pattern is allowed" are different
        // facts and only one of them is safe to answer with silence.
        //
        // Upstream asks unless every pattern the call produces is allowed, so
        // one unfamiliar command in a chain is enough to stop the whole chain
        // — and one denied command refuses it, whatever the rest said.
        let asked = patterns
            .iter()
            .map(|pattern| self.decide(tool, pattern, unmatched))
            .max()
            .unwrap_or(Decision::Allow);

        // Upstream answers the location dialog with the globs it asked with
        // (`tool/shell.ts`, `ask`: `always: globs`), so an "always" given to
        // the dialog the user saw covers the whole of what they saw. Leaving
        // it out would leave them answering the same question every turn.
        //
        // Each rule's pattern is `dir/*`, and [`glob`]'s `*` spans separators,
        // so every one of them is **recursive**: answering for `/tmp/scratch`
        // answers for everything beneath it too. That is upstream's own shape
        // (`always: globs`), but a call naming three directories now leaves
        // three such rules behind where it used to leave one — worth weighing
        // before widening what [`Permissions::outside_dirs`] collects.
        //
        // A narrower list than the one the dialog is handed, and deliberately:
        // what is disclosed is where the call would work, and what is learned
        // is which of those can be written down without meaning more than the
        // person agreed to.
        let mut learned = Vec::new();
        for directory in &directories {
            // A directory whose *name* carries a wildcard would be remembered
            // as one, and `/tmp/build*/*` covers every sibling sharing the
            // prefix. There is no escaping it — [`glob`] has no escape syntax
            // — so such a directory is not remembered and keeps asking, the
            // same answer [`always_rules`] gives a command spelled that way.
            // The call's other directories are still remembered: a partial
            // answer beats one the user has to give again from scratch.
            if means_itself(&directory.to_string_lossy()) {
                learned.push(Rule {
                    permission: EXTERNAL_DIRECTORY.to_owned(),
                    pattern: covering(directory),
                    action: Action::Allow,
                });
            }
        }
        learned.extend(always_rules(tool, args));

        CallDecision {
            action: located.max(asked),
            rules: self.relevant(tool),
            directories,
            learned,
        }
    }

    /// Records the "always allow" answer a person gave to `decision`.
    ///
    /// The rules are remembered for the session whatever happens to the store;
    /// a store that cannot be written is a warning, never a failed turn.
    ///
    /// Everything stored comes from the decision, and the call it was about is
    /// not readable from here on purpose. The answer arrives a round-trip after
    /// the judgement, and deriving the rules again at this point would be a
    /// second derivation free to disagree with the one the dialog disclosed —
    /// storing something other than what the person was shown agreeing to.
    pub fn remember(&mut self, decision: &CallDecision) {
        if decision.learned.is_empty() {
            return;
        }
        for rule in &decision.learned {
            if !self.rules.contains(rule) {
                self.rules.push(rule.clone());
            }
        }

        let Some(store) = &self.store else {
            return;
        };
        if let Err(error) = store.remember(&decision.learned) {
            tracing::warn!(
                path = %store.path.display(),
                %error,
                "an always-allow answer could not be stored and will not outlive this session"
            );
        }
    }

    /// The rules that have anything to say about `tool`, which is what upstream
    /// hands the model when it refuses a call
    /// (`packages/core/src/v1/permission.ts`, `DeniedError`).
    fn relevant(&self, tool: &str) -> Vec<Rule> {
        self.ordered()
            .filter(|rule| matches(tool, &rule.permission))
            .cloned()
            .collect()
    }

    /// Loads the rules stored at `path`, deciding along the way whether this
    /// session may write there.
    fn open(path: PathBuf) -> Self {
        let store = Store { path };
        let (rules, store) = match store.read() {
            Ok(document) => (document.rules, Some(store)),
            Err(StoreError::Missing) => (Vec::new(), Some(store)),
            // Whatever the file is, it is not a ruleset. Moving it aside keeps
            // it for whoever wants to look while letting this session store
            // answers again.
            Err(error @ StoreError::Corrupt(_)) => {
                tracing::warn!(
                    path = %store.path.display(),
                    %error,
                    "stored permission rules could not be read; starting from the defaults"
                );
                store.quarantine();
                (Vec::new(), Some(store))
            }
            // A store this build does not understand is not this build's to
            // rewrite: overwriting it would throw away rules whose meaning it
            // cannot even represent.
            Err(error) => {
                tracing::warn!(
                    path = %store.path.display(),
                    %error,
                    "stored permission rules were left untouched; \
                     this session runs on the defaults and stores nothing"
                );
                (Vec::new(), None)
            }
        };

        // Where the project is comes from [`Permissions::load`], which is the
        // only caller that knows: opening a ruleset says nothing about which
        // directory the session was started in.
        Self {
            baseline: Vec::new(),
            rules,
            store,
            root: None,
            cwd: None,
        }
    }

    /// Every rule that applies, weakest first — the build's beneath the
    /// person's, which is the order precedence runs in.
    fn ordered(&self) -> impl DoubleEndedIterator<Item = &Rule> {
        self.baseline.iter().chain(self.rules.iter())
    }

    /// The directories this call would work in that the project does not reach.
    ///
    /// Empty is "nothing more to ask about": the call names nothing outside, or
    /// there is no project to compare anything against.
    ///
    /// Three things become a directory here, which is upstream's `collect`
    /// together with the line that follows it (`tool/shell.ts`, `collect`, and
    /// 626):
    ///
    /// - a shell command's `workdir`, whenever the project does not reach it —
    ///   it *is* where the command runs;
    /// - every path argument of a command that names files ([`names_files`]),
    ///   reduced to the directory holding it, because that is the boundary an
    ///   answer covers;
    /// - for a write or an edit, the directory holding the file it names
    ///   (`packages/core/src/location-mutation.ts:135-136`) — one answer covers
    ///   the file the user was shown and its siblings, which is the boundary
    ///   they were actually reasoning about.
    ///
    /// Sorted and deduplicated: each of these can become a stored rule, and the
    /// order they are stored in is part of what a person reads back later.
    ///
    /// This list leaves the module whole, on [`CallDecision::directories`], and
    /// no caller derives it: a permission dialog has to name these, and one
    /// naming a set the judgement never saw would be asking about a different
    /// call than the one being decided.
    fn outside_dirs(&self, tool: &str, args: &serde_json::Value) -> Vec<PathBuf> {
        let (Some(root), Some(cwd)) = (&self.root, &self.cwd) else {
            return Vec::new();
        };
        let text = |name| args.get(name).and_then(serde_json::Value::as_str);
        let mut found: Vec<PathBuf> = Vec::new();

        if SHELL_LIKE.contains(&tool) {
            // Where the command runs — and so also where its own relative
            // arguments resolve from, which is why the scan below shares this
            // base rather than the session's directory. Upstream hands
            // `collect` the very same one.
            let base = text(WORKDIR).map_or_else(|| cwd.clone(), |workdir| against(cwd, workdir));
            if !base.starts_with(root) {
                found.push(base.clone());
            }

            for chunk in text(COMMAND).map(chunks).unwrap_or_default() {
                let tokens = tokens(&chunk);
                if !tokens.first().is_some_and(|verb| names_files(verb)) {
                    continue;
                }
                for argument in path_args(&tokens) {
                    let Some(path) = arg_path(argument, &base) else {
                        continue;
                    };
                    if path.starts_with(root) {
                        continue;
                    }
                    found.push(holding(path));
                }
            }
        } else if FILE_LIKE.contains(&tool)
            && let Some(named) = text(FILE_PATH)
        {
            let path = against(cwd, named);
            if !path.starts_with(root) {
                found.push(holding(path));
            }
        }

        found.sort();
        found.dedup();

        found
    }

    /// The patterns a call has to have allowed before it can run.
    ///
    /// A shell command produces one per command it runs, and one that only
    /// moves the shell around produces none. Every other tool produces the one
    /// thing upstream checks it against: a fetch its URL, a write or an edit
    /// its file, anything else the pattern its whole-tool rules are written
    /// with.
    fn patterns(&self, tool: &str, args: &serde_json::Value) -> Vec<String> {
        let argument = |name| {
            args.get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };

        if SHELL_LIKE.contains(&tool) {
            return argument(COMMAND)
                .map_or_else(|| vec![ANY.to_owned()], |command| commands(&command));
        }
        if URL_LIKE.contains(&tool) {
            return vec![argument(URL).unwrap_or_else(|| ANY.to_owned())];
        }
        if FILE_LIKE.contains(&tool) {
            return vec![self.file_pattern(args).unwrap_or_else(|| ANY.to_owned())];
        }
        if tool == TASK {
            return vec![argument(SUBAGENT_TYPE).unwrap_or_else(|| ANY.to_owned())];
        }

        vec![ANY.to_owned()]
    }

    /// Where a call's file sits, named as upstream names it: relative to the
    /// project for a file inside it, and the resolved path itself for one
    /// outside (`packages/core/src/location-mutation.ts`, `resolve`).
    ///
    /// [`None`] when the call names no file, or when there is no project to
    /// name it relative to — a rule written against a path cannot be applied
    /// without knowing where paths start from, and answering [`ANY`] instead
    /// leaves such a set judging exactly what it judged before.
    fn file_pattern(&self, args: &serde_json::Value) -> Option<String> {
        let (root, cwd) = (self.root.as_ref()?, self.cwd.as_ref()?);
        let file = args.get(FILE_PATH).and_then(serde_json::Value::as_str)?;
        let resolved = against(cwd, file);

        Some(match resolved.strip_prefix(root) {
            Ok(relative) if relative.as_os_str().is_empty() => HERE.to_owned(),
            Ok(relative) => relative.to_string_lossy().into_owned(),
            Err(_) => resolved.to_string_lossy().into_owned(),
        })
    }

    /// What the rules say about one pattern, or — when they say nothing —
    /// what the caller handed in, or the static defaults beneath that.
    fn decide(&self, tool: &str, pattern: &str, unmatched: Option<Decision>) -> Decision {
        let matched = self
            .ordered()
            .rev()
            .find(|rule| matches(tool, &rule.permission) && matches(pattern, &rule.pattern));

        match (matched, unmatched) {
            (Some(rule), _) => rule.action.decision(),
            // The caller's default, and this arm's position is the API's
            // whole contract: beneath every rule, ahead of the static lists.
            (None, Some(default)) => default,
            (None, None) if ASK_BY_DEFAULT.contains(&tool) => Decision::Ask,
            // The one default decided by shape rather than by name; see
            // [`MCP_PREFIX`]. Below the rules, so a config that answered for
            // `"mcp__github__*"` still answers.
            (None, None) if tool.starts_with(MCP_PREFIX) => Decision::Ask,
            (None, None) => Decision::Allow,
        }
    }
}

/// The stored ruleset, as it sits on disk.
#[derive(Debug, Deserialize, Serialize)]
struct Document {
    /// Format the rules are written in; see [`VERSION`].
    version: u32,
    /// The rules, in the order they were decided.
    #[serde(default)]
    rules: Vec<Rule>,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            version: VERSION,
            rules: Vec::new(),
        }
    }
}

/// A stored ruleset could not be used.
#[derive(Debug, thiserror::Error)]
enum StoreError {
    /// Nothing is stored yet, which is every project's first run.
    #[error("no rules are stored yet")]
    Missing,
    /// The file could not be reached.
    #[error("{0}")]
    Io(#[from] io::Error),
    /// The file is not the JSON a ruleset is.
    #[error("the file is not a stored ruleset: {0}")]
    Corrupt(#[from] serde_json::Error),
    /// The file was written by a build that knows a later format.
    #[error(
        "the file was written by a newer build (version {0}, this build understands {VERSION})"
    )]
    Newer(u32),
}

/// The file a project's rules live in.
///
/// Cloneable because a derived ruleset — a subagent's, or a command running as
/// another agent — answers for the same project and persists an "always" to the
/// same file. Two handles on one path is what the file format already tolerates:
/// [`Store::remember`] re-reads before it writes.
#[derive(Clone, Debug)]
struct Store {
    path: PathBuf,
}

impl Store {
    /// The stored ruleset, or why there is none to work with.
    fn read(&self) -> Result<Document, StoreError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(StoreError::Missing);
            }
            Err(error) => return Err(StoreError::Io(error)),
        };

        let document: Document = serde_json::from_slice(&bytes)?;
        if document.version > VERSION {
            return Err(StoreError::Newer(document.version));
        }

        Ok(document)
    }

    /// Adds `learned` to what is stored, keeping everything already there.
    ///
    /// The file is re-read rather than written from memory, so rules another
    /// process stored while this session was running survive.
    fn remember(&self, learned: &[Rule]) -> Result<(), StoreError> {
        let mut document = match self.read() {
            Ok(document) => document,
            Err(StoreError::Missing) => Document::default(),
            // The file was readable at load and is not now; it is no more
            // usable than it was then, and the answer still has to land.
            Err(error @ StoreError::Corrupt(_)) => {
                tracing::warn!(
                    path = %self.path.display(),
                    %error,
                    "stored permission rules could not be read; starting from the defaults"
                );
                self.quarantine();
                Document::default()
            }
            Err(error) => return Err(error),
        };

        for rule in learned {
            if !document.rules.contains(rule) {
                document.rules.push(rule.clone());
            }
        }
        document.version = VERSION;

        self.write(&document)
    }

    /// Replaces the file's contents.
    ///
    /// The bytes land in a sibling that is renamed into place, so a write that
    /// is interrupted — or one that races another process — can only leave the
    /// old ruleset or the new one, never half of either. The sibling's name
    /// carries a counter as well as the process id so that two writes from one
    /// process cannot pick the same one.
    fn write(&self, document: &Document) -> Result<(), StoreError> {
        let parent = self.path.parent().ok_or_else(|| {
            StoreError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                "the ruleset has no directory to be created in",
            ))
        })?;
        fs::create_dir_all(parent)?;

        let mut json = serde_json::to_vec_pretty(document)?;
        json.push(b'\n');

        let temporary = self.path.with_file_name(format!(
            "{FILE}.{}.{}.tmp",
            std::process::id(),
            WRITES.fetch_add(1, Ordering::Relaxed)
        ));
        crate::project::write_new(&temporary, &json)?;

        fs::rename(&temporary, &self.path).map_err(|error| {
            let _ = fs::remove_file(&temporary);
            StoreError::Io(error)
        })
    }

    /// Moves a file that is not a ruleset aside, so the next write starts from
    /// something this build wrote.
    ///
    /// Failing to move it is not worth reporting twice: whatever stopped the
    /// rename will stop the write too, and that is already a warning.
    fn quarantine(&self) {
        let aside = self.path.with_file_name(QUARANTINE);
        match fs::rename(&self.path, &aside) {
            Ok(()) => tracing::warn!(path = %aside.display(), "the unreadable file was kept here"),
            Err(error) => tracing::debug!(
                path = %self.path.display(),
                %error,
                "the unreadable file could not be moved aside"
            ),
        }
    }
}

/// The pattern that covers a directory, as upstream writes it (`tool/shell.ts`,
/// `ask`: `path.join(dir, "*")`).
///
/// Separators are left as this platform writes them, because [`matches`]
/// normalises both sides before comparing, so a rule stored on either kind of
/// system is read by the other.
fn covering(directory: &Path) -> String {
    directory.join(ANY).to_string_lossy().into_owned()
}

/// Where `path` really is: absolute, with every symbolic link resolved, so
/// that two spellings of one directory cannot be answered differently.
///
/// A path that does not exist yet cannot be canonicalized, and skipping it
/// would let a directory the model is about to create walk straight past the
/// gate. So the longest ancestor that *does* exist is canonicalized and the
/// rest appended to it, which is upstream's own fallback
/// (`packages/core/src/location-mutation.ts`, `resolvePath`). Whatever `..`
/// survives in that remainder is collapsed lexically by `lexical`: it stands
/// on a canonical prefix by then, so there is no link left for it to mean
/// something else through.
///
/// Resolving before comparing is what makes the walk in the other direction —
/// `..` back out of the project, or a link planted inside it — land outside
/// where it belongs.
///
/// Every canonical answer goes through `plain` first, because on Windows
/// `canonicalize` answers in the verbatim spelling and this function's output
/// is what rules are written from and compared against.
///
/// Public because it is the only correct way to say where the gate thinks a
/// path is: `fs::canonicalize` is not that answer on Windows, and anything that
/// compares its own spelling of a directory against a disclosed one — a test
/// asserting on what a dialog named, most of all — is otherwise comparing two
/// spellings of the same place and calling them different.
#[must_use]
pub fn resolve(path: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        return plain(canonical);
    }

    let mut ancestor: Vec<Component> = path.components().collect();
    let mut rest: Vec<Component> = Vec::new();
    while let Some(component) = ancestor.pop() {
        rest.push(component);

        let existing: PathBuf = ancestor.iter().collect();
        if existing.as_os_str().is_empty() {
            continue;
        }
        if let Ok(resolved) = fs::canonicalize(&existing) {
            let mut resolved = plain(resolved);
            resolved.extend(rest.iter().rev().map(|component| component.as_os_str()));
            return lexical(&resolved);
        }
    }

    // Nothing along it exists — a path under a mount point that is gone, or
    // one the process cannot look at. Lexical is all that is left, and it is
    // still a definite answer to compare.
    lexical(&std::path::absolute(path).unwrap_or_else(|_| path.to_owned()))
}

/// `path` in the spelling a person writes, rather than the one
/// [`fs::canonicalize`] answers in.
///
/// Windows canonicalises to a **verbatim** path — `\\?\C:\work\api`, or
/// `\\?\UNC\server\share\…` for a network location — which is the form that
/// skips the Win32 path parser entirely. Two things break when that spelling
/// reaches the rules.
///
/// The first is that the prefix carries a literal `?`, and `?` is a [`glob`]
/// metacharacter. [`means_itself`] would therefore judge *every* canonicalised
/// directory on Windows to be wildcard-named, and no `external_directory` rule
/// would ever be stored: an "always" answer would be taken from the person,
/// disclosed back to them, and then quietly forgotten, so the same dialog would
/// return every turn. That is the whole of the defect this rewrite exists for.
///
/// The second is that a rule written down as `\\?\C:\work\api\*` is in a
/// spelling nobody types into a config file, and these rules are a file people
/// read and edit.
///
/// Only the two verbatim forms with an ordinary equivalent are rewritten. A
/// bare `\\?\` over a device path — a pipe, a volume GUID — has no
/// non-verbatim spelling to be rewritten *to*, so it is left exactly as it
/// came rather than mangled into something that names nothing.
///
/// Nothing to do anywhere else: every other platform's `canonicalize` answers
/// in the only spelling it has.
fn plain(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    let path = unverbatim(path);

    path
}

/// The rewrite [`plain`] documents, which only Windows has a use for.
#[cfg(windows)]
fn unverbatim(path: PathBuf) -> PathBuf {
    use std::{ffi::OsString, path::Prefix};

    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return path;
    };
    let root = match prefix.kind() {
        Prefix::VerbatimDisk(letter) => {
            let mut root = OsString::from(char::from(letter).to_string());
            root.push(r":\");
            root
        }
        Prefix::VerbatimUNC(server, share) => {
            let mut root = OsString::from(r"\\");
            root.push(server);
            root.push(r"\");
            root.push(share);
            root
        }
        _ => return path,
    };

    let mut rewritten = PathBuf::from(root);
    rewritten.extend(
        path.components()
            .skip_while(|component| matches!(component, Component::Prefix(_) | Component::RootDir))
            .map(Component::as_os_str),
    );

    rewritten
}

/// `path` with its `.` and `..` components applied by text rather than by
/// asking the filesystem.
///
/// A `..` above the root is the root, which is what every kernel resolves it
/// to.
fn lexical(path: &Path) -> PathBuf {
    let mut collapsed = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                collapsed.pop();
            }
            component => collapsed.push(component),
        }
    }

    collapsed
}

/// The rules an "always allow" answer to this call leaves behind.
fn always_rules(tool: &str, args: &serde_json::Value) -> Vec<Rule> {
    let allow = |pattern: String| Rule {
        permission: tool.to_owned(),
        pattern,
        action: Action::Allow,
    };

    if SHELL_LIKE.contains(&tool)
        && let Some(command) = args.get(COMMAND).and_then(serde_json::Value::as_str)
    {
        // One rule per command, naming the command and wildcarding its
        // arguments. A command that needed no permission leaves no rule.
        return commands(command)
            .iter()
            .filter_map(|command| {
                // A directory move that survived [`commands`] carries what a
                // move cannot — see [`moves_only`]. Remembering it as `cd *`
                // would hand every later `cd`, substitution and all, the
                // answer given to this one, so what is remembered is this
                // command and nothing wider.
                if names_a_directory(command) {
                    // Except that a rule's pattern is a wildcard, so a move
                    // reaching the dialog *because* it is spelled with a `*`
                    // would be remembered as one — and `cd "logs*"` answered
                    // once would then cover `cd "logs$(curl … | sh)"`, undoing
                    // the hardening above through the door meant to narrow it.
                    // There is no escape syntax to reach for ([`glob`] has
                    // none), so such a move is remembered not at all and keeps
                    // asking.
                    means_itself(command).then(|| allow(command.to_owned()))
                } else {
                    // The pattern here is built from [`name_of`], which is the
                    // command's leading tokens, so `rm *.log` still remembers
                    // the harmless `rm *`. Only a wildcard in the *name* — a
                    // command literally called `rm*` — would widen the rule.
                    let name = name_of(command);
                    means_itself(&name).then(|| allow(format!("{name} {ANY}")))
                }
            })
            .collect();
    }

    vec![allow(ANY.to_owned())]
}

/// The commands `command` runs that need a permission of their own, as the text
/// of each.
///
/// A chunk that only moves the shell around is left out, which is upstream's
/// `!CWD.has(cmd)` guard on pattern collection (`tool/shell.ts`, `collect`).
/// The path scan does **not** use this view — see [`chunks`].
fn commands(command: &str) -> Vec<String> {
    chunks(command)
        .into_iter()
        .filter(|chunk| !moves_only(chunk))
        .collect()
}

/// Every chunk `command` splits into at the operators that separate one command
/// from the next.
///
/// Quoted separators belong to the command they sit in, so
/// `git commit -m "a && b"` is one chunk, not two.
///
/// This is the whole list, directory moves included, and it is the view
/// [`Permissions::outside_dirs`] scans for paths. Upstream keeps the same two
/// views over one parse (`tool/shell.ts`, `collect`): its `FILES` scan visits
/// every command node, while only the nodes that are *not* directory moves
/// contribute a pattern. Scanning [`commands`] instead would never see
/// `cd /etc && cat passwd`, where the move is what takes the command after it
/// out of the project.
fn chunks(command: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut characters = command.chars().peekable();

    while let Some(character) = characters.next() {
        match character {
            '\\' if quote != Some('\'') => {
                current.push(character);
                if let Some(escaped) = characters.next() {
                    current.push(escaped);
                }
            }
            '\'' | '"' => {
                match quote {
                    Some(open) if open == character => quote = None,
                    None => quote = Some(character),
                    Some(_) => {}
                }
                current.push(character);
            }
            _ if quote.is_some() => current.push(character),
            _ if SEPARATORS.contains(&character) => {
                // `&&` and `||` separate one command from the next exactly as
                // their single-character forms do.
                if characters.peek() == Some(&character) && matches!(character, '&' | '|') {
                    characters.next();
                }
                push_chunk(&mut found, &mut current);
            }
            _ => current.push(character),
        }
    }
    push_chunk(&mut found, &mut current);

    found
}

/// Adds what has been collected to `found`, unless there is nothing there.
///
/// Whether a chunk is worth *asking* about is [`commands`]'s question rather
/// than this one: the path scan needs the chunks that filter throws away.
fn push_chunk(found: &mut Vec<String>, current: &mut String) {
    let chunk = std::mem::take(current);
    let chunk = chunk.trim();

    if chunk.is_empty() {
        return;
    }

    found.push(chunk.to_owned());
}

/// Whether `text` still means itself once it is read as a pattern.
///
/// Rules match by wildcard ([`glob`]), so text becoming a pattern has to be
/// free of the two characters that would stop standing for themselves. There
/// is no escaping them — [`glob`] has no escape syntax, and [`normalize`]
/// rewrites `\` before the matcher ever sees it — so text that carries one
/// cannot be remembered at all.
fn means_itself(text: &str) -> bool {
    !text.contains(['*', '?'].as_slice())
}

/// Whether `command` names one of the commands that move the shell around.
fn names_a_directory(command: &str) -> bool {
    tokens(command)
        .first()
        .is_some_and(|first| CWD_COMMANDS.contains(&first.as_str()))
}

/// Characters a directory move may be spelled with.
///
/// An allow-list, and deliberately so — see [`moves_only`]. Letters and digits
/// are [`char::is_alphanumeric`] rather than ASCII, because a directory named
/// in someone's own script is still a directory. The rest is what a path is
/// written with: separators, the characters that appear in real file names,
/// quotes, and the space they exist to protect.
const MOVE_CHARACTERS: &[char] = &[
    '_', '-', '.', '/', '\\', '~', ':', '+', ',', '@', '\'', '"', ' ', '\t',
];

/// Whether `command` does nothing but move the shell to another directory,
/// and so needs no permission of its own.
///
/// Upstream can answer this exactly: it walks a real shell grammar, so the
/// substitution in `cd "$(curl … | sh)"` is its own command node and is judged
/// on its own merits (`tool/shell.ts`, `collect`). The split in [`commands`]
/// cannot see inside quotes, which would make that whole call one `cd` chunk
/// and drop it — so being named `cd` is not enough here.
///
/// What counts instead is an **allow-list**: a move may contain only the
/// characters a path is written with ([`MOVE_CHARACTERS`]), and anything else
/// makes it a command to ask about. This is a deliberate reversal. Listing the
/// syntaxes that can execute — `$(…)`, backticks, redirects — is a losing
/// game, because the list is per-shell and it grows: bash 5.3 added value
/// substitution `${ cmd; }` and `${| cmd; }`, which runs a command list, is
/// not parameter expansion, and would sail past a test for `$(`. zsh has its
/// own forms. A move that may only be spelled out of path characters cannot
/// acquire a new way to execute when someone ships a new shell.
///
/// The cost is a divergence from upstream, in the direction of asking: `cd
/// $HOME` and `cd ${WORK}/api` are asked about here where upstream's grammar
/// would see an inert `cd` node and let them run. Literal paths — `cd build`,
/// `cd ../a-b.c`, `cd "my dir"` — still move unasked, which is the case this
/// exists for.
fn moves_only(command: &str) -> bool {
    names_a_directory(command)
        && command
            .chars()
            .all(|character| character.is_alphanumeric() || MOVE_CHARACTERS.contains(&character))
}

/// The tokens that name `command`, joined as they were written.
///
/// Ported from upstream's `BashArity.prefix`: the longest listed prefix wins,
/// and its arity says how many tokens to keep. Flags are tokens like any
/// other, which is upstream's behaviour — `git -C build commit` is named
/// `git -C`.
fn name_of(command: &str) -> String {
    let tokens = tokens(command);

    for length in (1..=tokens.len()).rev() {
        let candidate = tokens[..length].join(" ");
        if let Ok(found) = ARITY.binary_search_by(|(prefix, _)| (*prefix).cmp(candidate.as_str())) {
            let arity = ARITY[found].1.min(tokens.len());
            return tokens[..arity].join(" ");
        }
    }

    tokens.first().cloned().unwrap_or_default()
}

/// `command` split at the whitespace that is not inside quotes, with the
/// quotes themselves dropped.
fn tokens(command: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    let mut characters = command.chars().peekable();

    while let Some(character) = characters.next() {
        match character {
            '\\' if quote != Some('\'') => {
                started = true;
                if let Some(escaped) = characters.next() {
                    current.push(escaped);
                }
            }
            '\'' | '"' => {
                started = true;
                match quote {
                    Some(open) if open == character => quote = None,
                    None => quote = Some(character),
                    Some(_) => current.push(character),
                }
            }
            _ if quote.is_some() => current.push(character),
            _ if character.is_whitespace() => {
                if started {
                    found.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            _ => {
                started = true;
                current.push(character);
            }
        }
    }
    if started {
        found.push(current);
    }

    found
}

/// Whether `verb` is one of the commands whose arguments name files.
///
/// Upstream's `FILES.has(cmd)`, which is [`FILE_COMMANDS`] together with the
/// directory moves in [`CWD_COMMANDS`].
fn names_files(verb: &str) -> bool {
    FILE_COMMANDS.contains(&verb) || CWD_COMMANDS.contains(&verb)
}

/// The arguments in `tokens` that name paths, as upstream's `pathArgs` picks
/// them out (`tool/shell.ts:188-218`, POSIX branch): everything after the verb
/// except `-flag`s, and except `chmod`'s `+mode`.
///
/// Upstream's third filter — dropping `/switch`-style arguments — belongs to its
/// `cmd.exe` branch and is left out for the same reason its PowerShell aliases
/// are.
///
/// The `+mode` exception is upstream's, and is kept rather than tidied: `chmod
/// +x` drops the mode while `chmod 755` does not, so a bare numeric mode is
/// scanned as though it named a file. Both spellings are relative text that
/// resolves inside the project and falls out at the next step, so neither is
/// observable through [`Permissions::gate`] — matching upstream costs nothing
/// here, and diverging would cost the reason to trust the rest of the port.
fn path_args(tokens: &[String]) -> Vec<&str> {
    let verb = tokens.first().map_or("", String::as_str);

    tokens
        .iter()
        .skip(1)
        .map(String::as_str)
        .filter(|token| !token.starts_with('-'))
        .filter(|token| !(verb == "chmod" && token.starts_with('+')))
        .collect()
}

/// The path `argument` names, resolved against `base`, or [`None`] when this
/// scan cannot say what it names.
///
/// Upstream's `argPath` in the POSIX order (`tool/shell.ts:369-376`): unquote,
/// expand a leading `~`, cut the text before the first glob metacharacter, and
/// give up on anything a shell would substitute into.
///
/// **Divergence, deliberate.** The unquoting is already done, and done wider:
/// [`tokens`] drops quotes wherever they appear and applies backslash escapes,
/// while upstream's `unquote` only strips a matched outer pair off the token's
/// own text. So `rm "/etc"/passwd` names `/etc/passwd` here and stays the
/// literal text `"/etc"/passwd` upstream — which upstream then resolves *inside*
/// the project and asks nothing about. This port asks. The difference runs in
/// the direction of asking, and closing it would mean writing quote handling
/// that is deliberately worse than the one already here.
fn arg_path(argument: &str, base: &Path) -> Option<PathBuf> {
    let expanded = expand_home(argument);
    let named = glob_prefix(&expanded)?;

    if named.is_empty() || dynamic(named) {
        return None;
    }

    Some(against(base, named))
}

/// `text` with a leading `~` replaced by this user's home directory, which is
/// upstream's `home` (`tool/shell.ts:135-139`).
///
/// `~user` is left alone, as upstream leaves it: only this process's own home is
/// known without asking the system who else has one.
fn expand_home(text: &str) -> Cow<'_, str> {
    let Some(rest) = text.strip_prefix('~') else {
        return Cow::Borrowed(text);
    };
    let tail = if rest.is_empty() {
        None
    } else if let Some(tail) = rest.strip_prefix(['/', '\\'].as_slice()) {
        Some(tail)
    } else {
        return Cow::Borrowed(text);
    };
    // No home directory to expand against is not a way past the gate: the text
    // is judged as it was written, which resolves relative to the call's own
    // directory and is compared like any other path.
    let Ok(home) = etcetera::home_dir() else {
        return Cow::Borrowed(text);
    };

    let expanded = match tail {
        Some(tail) => home.join(tail),
        None => home,
    };
    Cow::Owned(expanded.to_string_lossy().into_owned())
}

/// `text` cut before the first glob metacharacter, or [`None`] when there is no
/// literal text in front of one.
///
/// Upstream's `prefix` (`tool/shell.ts:181-186`) returns nothing at all when the
/// metacharacter is the very first character, and its `argPath` then skips the
/// argument. Cutting to an empty string instead would resolve to the directory
/// the command runs in and collect *that*, so `rm *.log` would name a directory
/// nobody wrote — which upstream never does.
fn glob_prefix(text: &str) -> Option<&str> {
    match text.find(['*', '?', '['].as_slice()) {
        Some(0) => None,
        Some(index) => Some(&text[..index]),
        None => Some(text),
    }
}

/// Whether `text` carries something a shell would run or expand, which makes the
/// path it ends up naming unknowable until it does.
///
/// Upstream's `dynamic` (`tool/shell.ts:174-179`): a leading `(` or `@(`, or a
/// substitution anywhere. It tests `$(`, `${` and a bare `$` separately because
/// its PowerShell branch treats them differently; on POSIX any `$` is already
/// enough and subsumes the first two.
///
/// Upstream calls this scan advisory (`packages/core/src/tool/bash.ts:109`) and
/// so is this one: an argument bearing a substitution is invisible to *both*
/// sides, so `rm "$(echo /etc/passwd)"` is gated by its ordinary pattern and not
/// by the location gate.
fn dynamic(text: &str) -> bool {
    text.starts_with('(') || text.starts_with("@(") || text.contains(['$', '`'].as_slice())
}

/// `named` resolved against `base`: absolute as it was written, relative joined
/// to `base`, and [`resolve`]d either way.
///
/// Every path this module judges goes through here, because a gate that resolves
/// a path differently from the code that will use it is gating a different path.
fn against(base: &Path, named: &str) -> PathBuf {
    // A POSIX-shell spelling of a Windows drive is translated before anything
    // else looks at it, so the gate and the tools judge one spelling. See
    // [`from_posix_drive`] for what would go wrong otherwise.
    #[cfg(windows)]
    if let Some(native) = from_posix_drive(named) {
        return resolve(&native);
    }

    let path = Path::new(named);

    if path.is_absolute() {
        resolve(path)
    } else {
        resolve(&base.join(path))
    }
}

/// `text` read as one of the POSIX spellings a Windows drive is reached by
/// under a POSIX shell, or [`None`] when it is not one.
///
/// `/c/work`, `/c:/work`, `/cygdrive/c/work` and `/mnt/c/work` are the four in
/// circulation — MSYS2 and Git Bash write the first two, Cygwin the third, WSL
/// the fourth — and every one of them names `C:\work`. A command running under
/// Git Bash produces them constantly, because that is what its own tools print:
/// `git rev-parse --show-toplevel` answers `/c/work/api`, and a model that
/// pastes that answer into the next call has named a path no rule stored as
/// `C:/work/api/*` would ever match. The gate would then ask about a directory
/// the person has already answered for, every turn, forever.
///
/// A single letter is the whole test. `/mnt/data` and `/usr/bin` keep their own
/// meaning, because `data` and `usr` are not drives — which is also why the
/// `cygdrive` and `mnt` prefixes are stripped before the letter is read rather
/// than treated as evidence on their own.
///
/// Windows-only at the call site: on a unix machine `/c/work` is an ordinary
/// path and rewriting it would invent a drive that does not exist. The function
/// itself is left compiled everywhere so its rules can be asserted on from any
/// machine.
#[cfg_attr(not(windows), allow(dead_code))]
fn from_posix_drive(text: &str) -> Option<PathBuf> {
    let rest = text.strip_prefix('/')?;
    let rest = rest
        .strip_prefix("cygdrive/")
        .or_else(|| rest.strip_prefix("mnt/"))
        .unwrap_or(rest);

    let (head, tail) = rest.split_once('/').unwrap_or((rest, ""));
    // `/c` and `/c:` name the drive itself; the colon is Git Bash's own second
    // spelling of the same thing.
    let head = head.strip_suffix(':').unwrap_or(head);
    let mut characters = head.chars();
    let letter = characters.next().filter(char::is_ascii_alphabetic)?;
    if characters.next().is_some() {
        return None;
    }

    let mut native = String::from(letter.to_ascii_uppercase());
    native.push_str(":\\");
    native.push_str(&tail.replace('/', "\\"));

    Some(PathBuf::from(native))
}

/// The directory an answer about `path` would cover: `path` itself when it is
/// one, and the directory holding it otherwise.
fn holding(path: PathBuf) -> PathBuf {
    if path.is_dir() {
        return path;
    }

    path.parent().map_or(path.clone(), Path::to_path_buf)
}

/// Whether `text` is covered by `pattern`.
///
/// Ported from upstream's `util/wildcard.ts`, which builds an anchored regular
/// expression: `*` stands for any run of characters, `?` for exactly one, and
/// everything else is literal. Two details of that translation carry real
/// weight:
///
/// - a pattern ending in a space and a star also matches the text without
///   either, so `ls *` covers a bare `ls` as well as `ls -la`, while never
///   covering `lst`;
/// - separators are normalised, so a rule written with either kind of slash
///   covers a path written with the other.
pub fn matches(text: &str, pattern: &str) -> bool {
    let text = normalize(text);
    let pattern = normalize(pattern);

    if glob(&text, &pattern) {
        return true;
    }

    // The optional-tail case: `ls *` is `ls( .*)?`, so `ls` matches too.
    pattern
        .strip_suffix(OPTIONAL_TAIL)
        .is_some_and(|head| glob(&text, head))
}

/// A string as the matcher compares it: one kind of separator, and on Windows,
/// one case — upstream matches case-insensitively there.
fn normalize(text: &str) -> Vec<char> {
    let separators = text.replace('\\', "/");

    #[cfg(windows)]
    let separators = separators.to_lowercase();

    separators.chars().collect()
}

/// Whether `text` matches `pattern` with `*` and `?` as the only two
/// metacharacters, anchored at both ends.
///
/// A `*` swallows anything, newlines included, matching the `s` flag upstream
/// compiles its expression with. The walk backtracks to the last `*` on a
/// mismatch rather than recursing, so a pattern full of them cannot put a
/// session on the stack.
fn glob(text: &[char], pattern: &[char]) -> bool {
    let (mut at, mut against) = (0, 0);
    let mut star: Option<usize> = None;
    let mut resume = 0;

    while at < text.len() {
        match pattern.get(against) {
            Some('*') => {
                star = Some(against);
                against += 1;
                resume = at;
            }
            Some('?') => {
                at += 1;
                against += 1;
            }
            Some(expected) if *expected == text[at] => {
                at += 1;
                against += 1;
            }
            // Give the last `*` one more character and try again.
            _ => match star {
                Some(position) => {
                    against = position + 1;
                    resume += 1;
                    at = resume;
                }
                None => return false,
            },
        }
    }

    pattern[against.min(pattern.len())..]
        .iter()
        .all(|character| *character == '*')
}

#[cfg(test)]
#[path = "permission_tests.rs"]
mod tests;
