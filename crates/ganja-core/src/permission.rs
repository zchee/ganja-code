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
//! source of each command it runs; for everything else, `*` — and each pattern
//! is looked up in the rules. The last rule whose tool and pattern both match
//! wins, mirroring upstream's `evaluate` in `permission/index.ts`, so a later
//! rule can loosen or tighten an earlier one. A pattern no rule covers falls
//! back to the defaults in [`ASK_BY_DEFAULT`]. Every pattern has to come back
//! allowed for the call to run unasked, which is what keeps `cargo test` from
//! carrying `&& rm -rf /` in with it.
//!
//! # What "always" remembers
//!
//! For a shell command, upstream does not remember the command; it remembers
//! the *kind* of command, by keeping the tokens that name it and wildcarding
//! the arguments — `cargo test --release` becomes `cargo *`, `npm run dev`
//! becomes `npm run dev *`. How many tokens name a command comes from
//! upstream's table in `permission/arity.ts`, ported here verbatim. For
//! every other tool, "always" is a rule covering the whole tool, as upstream's
//! tools ask with `always: ["*"]`.
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
//!     { "permission": "write", "pattern": "*", "action": "allow" }
//!   ]
//! }
//! ```
//!
//! `permission` is the tool the rule speaks for and `pattern` is what it covers
//! within that tool; both are matched as wildcards, so a configuration phase can
//! write `{ "permission": "*", "pattern": "*", "action": "ask" }` and have it
//! mean every call. An `action` this build does not know — upstream also has
//! `deny` — is kept as it was written and treated as `ask`, so a rule from a
//! newer build can only ever make this one more cautious, never less.
//!
//! Nothing here can fail a turn. A store that cannot be read is quarantined or
//! ignored with a warning and the session falls back to the defaults; a store
//! that cannot be written costs the answer its persistence and nothing else.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

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

/// Tools that ask by default: everything that changes state outside the
/// conversation. Anything else — reading, searching, listing, planning —
/// runs unasked unless a rule says otherwise.
///
/// Names that this build does not register are listed anyway, because the
/// answer to "may I run a shell command" must not depend on what the tool
/// happens to be called this week.
pub const ASK_BY_DEFAULT: &[&str] = &[
    "apply_patch",
    "bash",
    "edit",
    "shell",
    "webfetch",
    "websearch",
    "write",
];

/// Tools whose argument is a shell command, and which therefore get a rule per
/// command rather than one for the whole tool.
const SHELL_LIKE: &[&str] = &["bash", "shell"];

/// Argument carrying the command a shell-like tool would run.
const COMMAND: &str = "command";

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Run it without asking.
    Allow,
    /// Put it in front of the user first.
    Ask,
}

/// What a rule does with the calls it covers.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Run them without asking.
    Allow,
    /// Put them in front of the user.
    Ask,
    /// Something a newer build wrote — upstream's `deny`, or whatever comes
    /// after it. Kept exactly as it was found so a rewrite does not drop it,
    /// and treated as [`Action::Ask`]: a rule this build cannot carry out is
    /// still a rule saying this call is not routine.
    #[serde(untagged)]
    Other(String),
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

/// The project's permission rules, layered over the defaults.
#[derive(Debug, Default)]
pub struct Permissions {
    /// Stored rules, then the ones this session added, in the order they were
    /// decided: the last one that matches a call wins.
    rules: Vec<Rule>,
    /// Where an answer is persisted, when it can be. [`None`] leaves the
    /// session's answers in memory — there is nowhere to write, or writing
    /// would tread on a store this build does not understand.
    store: Option<Store>,
}

impl Permissions {
    /// Loads the rules for the project at `cwd`, falling back to the defaults
    /// when nothing is stored or the store cannot be read.
    #[must_use]
    pub fn load(cwd: &Path) -> Self {
        let project = Project::resolve(cwd);
        match project.data_dir() {
            Ok(directory) => Self::open(directory.join(FILE)),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "permission answers cannot be stored and will not outlive this session"
                );
                Self::default()
            }
        }
    }

    /// What to do with a call to `tool` carrying `args`.
    #[must_use]
    pub fn check(&self, tool: &str, args: &serde_json::Value) -> Decision {
        // Upstream asks unless every pattern the call produces is allowed, so
        // one unfamiliar command in a chain is enough to stop the whole chain.
        if patterns(tool, args)
            .iter()
            .all(|pattern| self.decide(tool, pattern) == Decision::Allow)
        {
            Decision::Allow
        } else {
            Decision::Ask
        }
    }

    /// Records an "always allow" answer for calls like this one.
    ///
    /// The rules are remembered for the session whatever happens to the store;
    /// a store that cannot be written is a warning, never a failed turn.
    pub fn remember_always(&mut self, tool: &str, args: &serde_json::Value) {
        let learned = always_rules(tool, args);
        if learned.is_empty() {
            return;
        }
        for rule in &learned {
            if !self.rules.contains(rule) {
                self.rules.push(rule.clone());
            }
        }

        let Some(store) = &self.store else {
            return;
        };
        if let Err(error) = store.remember(&learned) {
            tracing::warn!(
                path = %store.path.display(),
                %error,
                "an always-allow answer could not be stored and will not outlive this session"
            );
        }
    }

    /// Loads the rules stored at `path`, deciding along the way whether this
    /// session may write there.
    fn open(path: PathBuf) -> Self {
        let store = Store { path };
        match store.read() {
            Ok(document) => Self {
                rules: document.rules,
                store: Some(store),
            },
            Err(StoreError::Missing) => Self {
                rules: Vec::new(),
                store: Some(store),
            },
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
                Self {
                    rules: Vec::new(),
                    store: Some(store),
                }
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
                Self {
                    rules: Vec::new(),
                    store: None,
                }
            }
        }
    }

    /// What the rules say about one pattern, or what the defaults say when
    /// they say nothing.
    fn decide(&self, tool: &str, pattern: &str) -> Decision {
        let matched = self
            .rules
            .iter()
            .rev()
            .find(|rule| matches(tool, &rule.permission) && matches(pattern, &rule.pattern));

        match matched {
            Some(rule) if rule.action == Action::Allow => Decision::Allow,
            Some(_) => Decision::Ask,
            None if ASK_BY_DEFAULT.contains(&tool) => Decision::Ask,
            None => Decision::Allow,
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
#[derive(Debug)]
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
        write_new(&temporary, &json)?;

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

/// Writes `bytes` to a newly created file.
///
/// `create_new` is `O_CREAT | O_EXCL`, which does not follow a symbolic link at
/// the final component: the name is predictable enough for someone sharing the
/// machine to plant one, and an open that followed it would write through to
/// wherever it led and then rename that file over the ruleset.
fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;

    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        // Either a write that died before its rename, or something planted to
        // catch this one. Unlinking the name and creating it again exclusively
        // settles both: what is removed is the name, never whatever it pointed
        // at, and a link planted in between fails the retry outright.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            fs::remove_file(path)?;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?
        }
        result => result?,
    };
    file.write_all(bytes)?;

    file.sync_all()
}

/// The patterns a call has to have allowed before it can run.
///
/// A shell command produces one per command it runs; everything else produces
/// the one pattern its whole-tool rules are written with. A shell command that
/// only moves the shell around produces none, and a call with nothing to check
/// is a call nobody needs to be asked about.
fn patterns(tool: &str, args: &serde_json::Value) -> Vec<String> {
    if !SHELL_LIKE.contains(&tool) {
        return vec![ANY.to_owned()];
    }

    args.get(COMMAND)
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| vec![ANY.to_owned()], commands)
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
            .map(|command| allow(format!("{} {ANY}", name_of(command))))
            .collect();
    }

    vec![allow(ANY.to_owned())]
}

/// The commands `command` runs, as the text of each.
///
/// Quoted separators belong to the command they sit in, so
/// `git commit -m "a && b"` is one command, not two.
fn commands(command: &str) -> Vec<String> {
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
                push_command(&mut found, &mut current);
            }
            _ => current.push(character),
        }
    }
    push_command(&mut found, &mut current);

    found
}

/// Adds what has been collected to `found`, unless it is nothing worth asking
/// about.
fn push_command(found: &mut Vec<String>, current: &mut String) {
    let command = std::mem::take(current);
    let command = command.trim();

    let names_a_directory = tokens(command)
        .first()
        .is_some_and(|first| CWD_COMMANDS.contains(&first.as_str()));
    if command.is_empty() || names_a_directory {
        return;
    }

    found.push(command.to_owned());
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
fn matches(text: &str, pattern: &str) -> bool {
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
mod tests {
    use std::{fs, path::PathBuf, sync::Arc, thread};

    use serde_json::json;
    use tempfile::TempDir;

    use super::{
        ARITY, Action, Decision, Document, FILE, Permissions, QUARANTINE, Rule, VERSION, matches,
        name_of,
    };

    /// A permission set with nowhere to store anything, which is what every
    /// test that is not about storage wants.
    fn memory() -> Permissions {
        Permissions::default()
    }

    /// A permission set stored in `directory`, exercising the real file.
    fn stored(directory: &TempDir) -> Permissions {
        Permissions::open(directory.path().join(FILE))
    }

    fn temporary() -> TempDir {
        TempDir::new().expect("a temporary directory is creatable")
    }

    fn path_of(directory: &TempDir) -> PathBuf {
        directory.path().join(FILE)
    }

    fn read(directory: &TempDir) -> serde_json::Value {
        serde_json::from_slice(&fs::read(path_of(directory)).expect("the store exists"))
            .expect("the store is JSON")
    }

    fn shell(command: &str) -> serde_json::Value {
        json!({ "command": command })
    }

    #[test]
    fn state_changing_tools_ask_and_read_only_tools_do_not() {
        let permissions = memory();
        let none = json!({});

        for tool in [
            "read",
            "glob",
            "grep",
            "todo",
            "todoread",
            "todowrite",
            "lsp",
        ] {
            assert_eq!(permissions.check(tool, &none), Decision::Allow, "{tool}");
        }
        for tool in [
            "write",
            "edit",
            "shell",
            "bash",
            "webfetch",
            "websearch",
            "apply_patch",
        ] {
            assert_eq!(permissions.check(tool, &none), Decision::Ask, "{tool}");
        }
    }

    #[test]
    fn an_always_answer_stops_the_asking() {
        let mut permissions = memory();
        let args = shell("cargo test");

        assert_eq!(permissions.check("shell", &args), Decision::Ask);
        permissions.remember_always("shell", &args);
        assert_eq!(permissions.check("shell", &args), Decision::Allow);
    }

    /// Answering "always" to one `cargo test` answers for every way of running
    /// the tests, which is the point of remembering the command rather than the
    /// invocation. It answers for nothing else: not another subcommand, and not
    /// a command that merely starts with the same letters.
    #[test]
    fn remembering_a_command_covers_its_family_and_nothing_that_merely_looks_like_it() {
        let mut permissions = memory();
        permissions.remember_always("shell", &shell("cargo test --release"));

        for allowed in [
            "cargo test",
            "cargo test --lib",
            "cargo test -- --nocapture",
            "cargo test  --doc",
        ] {
            assert_eq!(
                permissions.check("shell", &shell(allowed)),
                Decision::Allow,
                "{allowed}"
            );
        }
        for asked in [
            "cargo build",
            "cargo",
            "cargo testify",
            "cargonaut",
            "cargo-deny check",
            "sudo cargo test",
        ] {
            assert_eq!(
                permissions.check("shell", &shell(asked)),
                Decision::Ask,
                "{asked}"
            );
        }
    }

    /// The reason a call is checked pattern by pattern: a remembered command
    /// must not smuggle an unremembered one in behind it.
    #[test]
    fn a_chain_is_only_allowed_when_every_command_in_it_is() {
        let mut permissions = memory();
        permissions.remember_always("shell", &shell("cargo test"));

        assert_eq!(
            permissions.check("shell", &shell("cargo test --lib && cargo test --doc")),
            Decision::Allow
        );
        for chained in [
            "cargo test && rm -rf /",
            "rm -rf / ; cargo test",
            "cargo test | tee out",
            "cargo test $(rm -rf /)",
            "cargo test\nrm -rf /",
        ] {
            assert_eq!(
                permissions.check("shell", &shell(chained)),
                Decision::Ask,
                "{chained}"
            );
        }

        // Answering for the whole chain remembers each of its commands.
        permissions.remember_always("shell", &shell("cargo test && rm -rf /"));
        assert_eq!(
            permissions.check("shell", &shell("rm -rf /tmp/x")),
            Decision::Allow
        );
    }

    /// A separator inside quotes is part of an argument, not the end of a
    /// command.
    #[test]
    fn a_quoted_separator_does_not_start_a_new_command() {
        let mut permissions = memory();
        permissions.remember_always("shell", &shell(r#"git commit -m "a && b""#));

        assert_eq!(
            permissions.check("shell", &shell(r#"git commit -m "c ; d""#)),
            Decision::Allow
        );
        assert_eq!(
            permissions.check("shell", &shell("git push")),
            Decision::Ask,
            "the rule names `git commit`, not all of git"
        );
    }

    /// Upstream leaves directory changes out of the patterns entirely, so a
    /// command that only moves around needs no permission and a chain is
    /// judged on the part that does something.
    #[test]
    fn moving_around_needs_no_permission() {
        let mut permissions = memory();

        assert_eq!(
            permissions.check("shell", &shell("cd crates/ganja-core")),
            Decision::Allow
        );
        assert_eq!(
            permissions.check("shell", &shell("cd build && make all")),
            Decision::Ask
        );

        permissions.remember_always("shell", &shell("make all"));
        assert_eq!(
            permissions.check("shell", &shell("cd build && make all")),
            Decision::Allow
        );

        // There was nothing to remember, so nothing was remembered.
        let mut nothing = memory();
        nothing.remember_always("shell", &shell("cd /tmp"));
        assert_eq!(nothing.check("shell", &shell("rm -rf /")), Decision::Ask);
    }

    /// Every other tool is remembered whole, the way upstream's tools ask with
    /// `always: ["*"]`.
    #[test]
    fn a_tool_that_is_not_a_shell_is_remembered_whole() {
        let mut permissions = memory();
        permissions.remember_always("write", &json!({ "filePath": "a.txt" }));

        assert_eq!(
            permissions.check("write", &json!({ "filePath": "b.txt" })),
            Decision::Allow
        );
        assert_eq!(
            permissions.check("edit", &json!({ "filePath": "a.txt" })),
            Decision::Ask,
            "answering for one tool must not answer for another"
        );
    }

    /// Upstream's arity table decides how much of a command names it. These
    /// are its own worked examples.
    #[test]
    fn a_command_is_named_by_as_many_tokens_as_its_arity() {
        for (command, expected) in [
            ("touch foo.txt", "touch"),
            ("git checkout main", "git checkout"),
            ("npm install", "npm install"),
            ("npm run dev", "npm run dev"),
            ("python script.py", "python script.py"),
            ("ls -la", "ls"),
            ("cargo build --release", "cargo build"),
            ("docker compose up -d", "docker compose up"),
            ("git", "git"),
            ("./configure --prefix=/usr", "./configure"),
            (r#"echo "hello world""#, "echo"),
            ("", ""),
        ] {
            assert_eq!(name_of(command), expected, "{command}");
        }
    }

    /// The table is searched, not scanned, so its order is load-bearing.
    #[test]
    fn the_arity_table_is_sorted() {
        assert!(
            ARITY.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "the arity table has to stay sorted to be searchable"
        );
    }

    /// The matcher's own semantics, including the two upstream translates by
    /// hand.
    #[test]
    fn patterns_match_the_way_upstream_compiles_them() {
        for (text, pattern, expected) in [
            ("anything at all", "*", true),
            ("ls", "ls *", true),
            ("ls -la", "ls *", true),
            ("ls  -la", "ls *", true),
            ("lst", "ls *", false),
            ("lst -la", "ls *", false),
            ("", "ls *", false),
            ("cargo test", "cargo *", true),
            ("cargotest", "cargo *", false),
            ("a", "?", true),
            ("ab", "?", false),
            ("a.b", "a.b", true),
            ("axb", "a.b", false),
            ("a+b", "a+b", true),
            ("cargo test\nrm -rf /", "cargo *", true),
            ("src/main.rs", "src/*", true),
            ("src\\main.rs", "src/*", true),
            ("src/main.rs", "src\\*", true),
            ("shell", "*", true),
            ("shell", "sh", false),
            ("shell", "sh*", true),
            ("shell", "*ell", true),
            ("shell", "s*l*l", true),
            ("shell", "s*x*l", false),
        ] {
            assert_eq!(matches(text, pattern), expected, "{text:?} vs {pattern:?}");
        }
    }

    /// A rule's tool is a pattern too, which is what lets a configuration
    /// phase write one rule that speaks for everything.
    #[test]
    fn a_rule_can_speak_for_more_than_one_tool() {
        let directory = temporary();
        write_store(
            &directory,
            &json!({
                "version": VERSION,
                "rules": [{ "permission": "*", "pattern": "*", "action": "ask" }],
            }),
        );

        let permissions = stored(&directory);
        assert_eq!(
            permissions.check("read", &json!({})),
            Decision::Ask,
            "a rule has to be able to tighten a default, not only loosen it"
        );
    }

    #[test]
    fn a_remembered_answer_outlives_the_session_that_gave_it() {
        let directory = temporary();

        let mut first = stored(&directory);
        first.remember_always("shell", &shell("cargo test --all"));
        first.remember_always("write", &json!({ "filePath": "a.txt" }));
        drop(first);

        let written = read(&directory);
        assert_eq!(written["version"], VERSION);
        assert_eq!(
            written["rules"],
            json!([
                { "permission": "shell", "pattern": "cargo test *", "action": "allow" },
                { "permission": "write", "pattern": "*", "action": "allow" },
            ])
        );

        let second = stored(&directory);
        assert_eq!(
            second.check("shell", &shell("cargo test --lib")),
            Decision::Allow
        );
        assert_eq!(second.check("write", &json!({})), Decision::Allow);
        assert_eq!(
            second.check("shell", &shell("npm install")),
            Decision::Ask,
            "storing an answer must not answer everything"
        );

        // The same answer twice is one rule.
        let mut third = stored(&directory);
        third.remember_always("shell", &shell("cargo test --all"));
        assert_eq!(
            read(&directory)["rules"].as_array().map(Vec::len),
            Some(2),
            "a repeated answer must not grow the file"
        );
        assert!(
            fs::read_dir(directory.path())
                .expect("the directory lists")
                .filter_map(Result::ok)
                .all(|entry| entry.file_name() == FILE),
            "no temporary file should outlive a write"
        );
    }

    /// A store nobody can parse must not take the session down with it, and
    /// must not be deleted either.
    #[test]
    fn a_store_that_is_not_a_ruleset_is_moved_aside_and_the_defaults_take_over() {
        for corrupt in [
            "{ this is not json".as_bytes(),
            b"[]",
            br#"{"version": 1, "rules": "all of them"}"#,
            b"",
        ] {
            let directory = temporary();
            fs::write(path_of(&directory), corrupt).expect("the fixture writes");

            let mut permissions = stored(&directory);
            assert_eq!(permissions.check("shell", &shell("ls")), Decision::Ask);
            assert_eq!(permissions.check("read", &json!({})), Decision::Allow);

            assert_eq!(
                fs::read(directory.path().join(QUARANTINE)).expect("the file was kept"),
                corrupt,
                "an unreadable file has to be kept, not dropped"
            );

            // And the session can store answers again.
            permissions.remember_always("shell", &shell("ls -la"));
            assert_eq!(read(&directory)["rules"][0]["pattern"], "ls *");
        }
    }

    /// A store from a newer build is not this build's to interpret or to
    /// overwrite.
    #[test]
    fn a_store_from_a_newer_build_is_neither_read_nor_written() {
        let directory = temporary();
        let future = json!({
            "version": VERSION + 1,
            "rules": [{ "permission": "shell", "pattern": "*", "action": "allow" }],
        });
        write_store(&directory, &future);

        let mut permissions = stored(&directory);
        assert_eq!(
            permissions.check("shell", &shell("rm -rf /")),
            Decision::Ask,
            "rules whose format is unknown cannot be honoured"
        );

        permissions.remember_always("shell", &shell("ls"));
        assert_eq!(
            permissions.check("shell", &shell("ls -la")),
            Decision::Allow,
            "the answer still holds for this session"
        );
        assert_eq!(read(&directory), future, "the newer file has to survive");
    }

    /// An action from a newer build is kept as it was written, and until this
    /// build understands it, it means "not routine".
    #[test]
    fn an_unknown_action_asks_and_survives_a_rewrite() {
        let directory = temporary();
        let denied = json!({ "permission": "shell", "pattern": "rm *", "action": "deny" });
        write_store(
            &directory,
            &json!({ "version": VERSION, "rules": [denied] }),
        );

        let mut permissions = stored(&directory);
        assert_eq!(
            permissions.check("shell", &shell("rm -rf /")),
            Decision::Ask
        );

        permissions.remember_always("shell", &shell("ls"));
        let rules = read(&directory)["rules"].clone();
        assert_eq!(rules[0], denied, "a rule this build cannot honour is kept");
        assert_eq!(rules[1]["pattern"], "ls *");
    }

    /// The last matching rule wins, so a later answer can overrule an earlier
    /// one — upstream's `findLast`.
    #[test]
    fn the_last_rule_that_matches_is_the_one_that_counts() {
        let directory = temporary();
        write_store(
            &directory,
            &json!({
                "version": VERSION,
                "rules": [
                    { "permission": "shell", "pattern": "*", "action": "allow" },
                    { "permission": "shell", "pattern": "rm *", "action": "ask" },
                ],
            }),
        );

        let permissions = stored(&directory);
        assert_eq!(
            permissions.check("shell", &shell("ls -la")),
            Decision::Allow
        );
        assert_eq!(
            permissions.check("shell", &shell("rm -rf /")),
            Decision::Ask
        );
    }

    /// Answers from several threads at once, each with its own view of the
    /// store, may lose a rule to the last writer but must never leave the file
    /// unreadable.
    #[test]
    fn overlapping_answers_cannot_corrupt_the_store() {
        let directory = Arc::new(temporary());
        let answers = 16;

        let threads: Vec<_> = (0..answers)
            .map(|index| {
                let directory = Arc::clone(&directory);
                thread::spawn(move || {
                    let mut permissions = Permissions::open(directory.path().join(FILE));
                    permissions.remember_always("shell", &shell(&format!("tool{index} run")));
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("no answer panicked");
        }

        let document: Document =
            serde_json::from_slice(&fs::read(path_of(&directory)).expect("the store exists"))
                .expect("overlapping writes left the store readable");
        assert_eq!(document.version, VERSION);
        assert!(!document.rules.is_empty());
        for rule in &document.rules {
            assert_eq!(rule.action, Action::Allow);
            assert_eq!(rule.permission, "shell");
            assert!(rule.pattern.ends_with(" *"), "{}", rule.pattern);
        }
        assert!(
            fs::read_dir(directory.path())
                .expect("the directory lists")
                .filter_map(Result::ok)
                .all(|entry| entry.file_name() == FILE),
            "no temporary file should outlive a write"
        );
    }

    /// The directory a store lives in is created on the way to writing it,
    /// because resolving a project deliberately creates nothing.
    #[test]
    fn storing_an_answer_creates_the_directory_it_needs() {
        let directory = temporary();
        let nested = directory
            .path()
            .join("project")
            .join("api-0123456789abcdef");

        let mut permissions = Permissions::open(nested.join(FILE));
        permissions.remember_always("shell", &shell("ls"));

        assert!(nested.join(FILE).is_file());
    }

    /// The rule type is the storage format, so a rule that round trips through
    /// JSON has to come back as itself.
    #[test]
    fn a_rule_round_trips_through_json() {
        for (rule, expected) in [
            (
                Rule {
                    permission: "shell".to_owned(),
                    pattern: "cargo *".to_owned(),
                    action: Action::Allow,
                },
                json!({ "permission": "shell", "pattern": "cargo *", "action": "allow" }),
            ),
            (
                Rule {
                    permission: "read".to_owned(),
                    pattern: "*".to_owned(),
                    action: Action::Ask,
                },
                json!({ "permission": "read", "pattern": "*", "action": "ask" }),
            ),
            (
                Rule {
                    permission: "shell".to_owned(),
                    pattern: "*".to_owned(),
                    action: Action::Other("deny".to_owned()),
                },
                json!({ "permission": "shell", "pattern": "*", "action": "deny" }),
            ),
        ] {
            assert_eq!(
                serde_json::to_value(&rule).expect("a rule serializes"),
                expected
            );
            assert_eq!(
                serde_json::from_value::<Rule>(expected).expect("a rule deserializes"),
                rule
            );
        }
    }

    fn write_store(directory: &TempDir, document: &serde_json::Value) {
        fs::write(
            path_of(directory),
            serde_json::to_vec_pretty(document).expect("the fixture serializes"),
        )
        .expect("the fixture writes");
    }
}
