//! Tools the agent loop can execute on the model's behalf.
//!
//! Spec: upstream `packages/opencode/src/tool/` — `tool.ts` for the contract,
//! `registry.ts` for the set. Each tool lives in its own module beside this
//! one, and descriptions are ported from upstream's `*.txt` prompt files
//! (MIT, attributed in `THIRD_PARTY_NOTICES.md`).
//!
//! Its own crate, and one that must never depend on the engine. A tool answers
//! to the rules and to the filesystem, never to the loop that called it, and
//! with the engine outside this crate's dependency graph that is the compiler's
//! rule rather than a convention a reviewer has to keep holding. Everything a
//! call needs from the outside arrives in [`ToolCtx`], which is why that type is
//! a bag of values rather than a handle back to a session.

/// Anchored file I/O, shared by the two tools that write. Not public: it is
/// how `write` and `edit` reach the disk, not something a frontend or a
/// third-party tool has any business addressing files through.
mod anchor;
pub mod bash_output;
/// Deferred tool schemas and the resident `tool_search` door back in —
/// ganja's take on Claude Code's ToolSearch, minted **D492** where the
/// engine filters (a direct call to a deferred tool executes).
pub mod deferral;
pub mod edit;
/// The minimal-YAML frontmatter reader, public because `ganja-core`'s agent
/// definition files open with the same fence a `SKILL.md` does and were being
/// read by a second copy of this parser.
pub mod frontmatter;
pub mod glob;
pub mod grep;
pub mod job;
pub mod kill_shell;
pub mod list_sessions;
pub mod plan;
pub mod question;
pub mod read;
pub mod registry;
pub mod send_message;
pub mod shell;
pub mod skill;
pub mod socket;
pub mod task;
pub mod team;
pub mod todo;
pub mod truncate;
/// The stale-read watcher, here because [`FileTimes`] is here: it is built on
/// the announce channel a read registers itself through, and what it reports is
/// a state on that same log. The engine still owns *when* a watcher exists — it
/// constructs one through this module — but nothing about deciding a file went
/// stale belongs on the far side of the boundary from the log that records it.
pub mod watch;
pub mod webfetch;
pub mod websearch;
pub mod write;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

/// The credential store a call must refuse, or the explicit statement that
/// there is none to guard.
///
/// An `Option` would carry the same information and lose it at the one moment
/// that matters: a new surface building a [`ToolCtx`] could write `None` by
/// reflex — or by copying a test fixture — and ship tool calls with the guard
/// off, silently. Spelling [`Credentials::Unguarded`] costs the same one line
/// and cannot pretend to be an accident.
#[derive(Clone, Debug, PartialEq)]
pub enum Credentials {
    /// The store `read` and `grep` refuse, resolved by whoever built the
    /// engine.
    Guarded(PathBuf),
    /// Nothing to guard, on purpose: a fixture, or a surface like the
    /// frontend's file menu that never reads a file's contents at all.
    Unguarded,
}

impl Credentials {
    /// The guarded path, if there is one — the consumption half of the type,
    /// where an option is the honest shape again.
    #[must_use]
    pub fn guarded(&self) -> Option<&Path> {
        match self {
            Self::Guarded(store) => Some(store),
            Self::Unguarded => None,
        }
    }
}

/// What a tool call needs beyond its arguments.
#[derive(Clone, Debug)]
pub struct ToolCtx {
    /// Directory relative paths resolve against.
    pub cwd: PathBuf,
    /// Fires when the turn is cancelled; long work is expected to stop.
    pub cancel: CancellationToken,
    /// The provider's id for this call, for anything a tool records.
    pub call_id: String,
    /// Which files this session has read, shared by every call in it.
    pub files: Arc<FileTimes>,
    /// Where this build keeps its credentials, so that `read` and `grep` can
    /// refuse the file — see [`ToolCtx::is_credential_store`].
    ///
    /// Handed over rather than resolved here: which file holds this machine's
    /// keys is `ganja-core`'s `auth`'s answer, and a tool that went and asked
    /// for it would be a tool that has to know where credentials live.
    /// [`Credentials::Unguarded`] behaves exactly like a store that is not on
    /// this disk — there is then nothing here to protect — but it has to be
    /// written where a reviewer will read it.
    pub credentials: Credentials,
    /// What a call runs a whole second agent loop through, which only
    /// [`task::TaskTool`] does.
    ///
    /// [`None`] on every turn that has no agents to spawn — and on every
    /// *child* turn, which is the entire depth guard stated a second way.
    pub spawn: Option<Arc<dyn task::Subagents>>,
    /// What a call sends a teammate a message through, which only
    /// [`send_message::SendMessageTool`] does.
    ///
    /// [`None`] wherever there is no team — every fixture, every session that
    /// never spawned a teammate — and the tool is then not registered at all,
    /// so a call reaching a `None` here is a build that offered a tool it
    /// cannot serve and gets told so in words rather than a panic.
    ///
    /// The sender's identity is **inside** this value rather than in any
    /// argument: one postbox per engine, carrying the name that engine sends
    /// as. See [`team::Postbox`] for why that is a mechanism and not a taste.
    pub postbox: Option<Arc<dyn team::Postbox>>,
    /// What a call asks the person a question through, which only
    /// [`question::QuestionTool`] does.
    ///
    /// [`None`] where there is nobody to ask — a fixture, a surface that runs
    /// tools outside a turn — and a call then reads back the sentence a
    /// dismissal produces. It is deliberately **not** what makes a headless
    /// run safe: that is a standing permission rule refusing `question` at
    /// every pattern, so the refusal is a rule somebody can see rather than a
    /// field somebody remembered to leave empty.
    pub ask: Option<Arc<dyn question::Asker>>,
    /// What a call switches the session to another agent through, which only
    /// the two plan doors — [`plan::PlanExitTool`] and [`plan::PlanEnterTool`]
    /// — do.
    ///
    /// [`None`] on every child turn, every fixture, and the `!` shell
    /// passthrough — the same depth guard as [`ToolCtx::spawn`]. [`Some`] only
    /// when the engine's registry holds an agent one of those doors leads to:
    /// presence is ability. Which *direction* is possible is decided a step
    /// further in, by which door the engine registered, since one seam now
    /// carries both.
    pub switch: Option<Arc<dyn plan::Switcher>>,
    /// What a call tracks a background job through, which
    /// [`shell::ShellTool`]'s `run_in_background` path, [`bash_output`] and
    /// [`kill_shell`] all reach through this one seam.
    ///
    /// [`None`] on every fixture and every context nobody built a registry
    /// for — the `!` shell passthrough, the `@` file menu's glob walk, a
    /// command template's expansion — where the two new tools refuse
    /// politely and `bash`'s `run_in_background` does the same, rather than
    /// running a background job nothing will ever be able to poll or kill.
    pub jobs: Option<Arc<dyn job::Jobs>>,
}

impl ToolCtx {
    /// Whether `path` is the credential store this call was handed.
    ///
    /// `read` and `grep` run without asking — that is what makes them usable —
    /// and both take a path the model chose, so without this a model acting on
    /// instructions it read in a file or a fetched page could put this
    /// machine's provider API keys straight into the transcript that is sent to
    /// a provider.
    ///
    /// Only ganja's own store is guarded. Which *other* files hold secrets is a
    /// question only the user can answer, and a built-in half-answer would read
    /// as a promise this cannot keep.
    ///
    /// Public because [`ToolCtx::credentials`] is: a third-party tool that
    /// reads files is handed the same path, and should refuse it by the same
    /// identity comparison rather than by comparing the two as text.
    #[must_use]
    pub fn is_credential_store(&self, path: &Path) -> bool {
        self.credentials
            .guarded()
            .is_some_and(|store| is_same_file(path, store))
    }
}

#[cfg(test)]
impl ToolCtx {
    /// A call rooted at `cwd` with every seam empty and nothing guarded — what
    /// an in-module test hands a tool when the seam under test is not one of
    /// them; a test that is about a seam sets that one field on the result.
    /// `cfg(test)` rather than `Default` on purpose: [`Credentials`]'s doc
    /// names copying a test fixture as the way a new surface ships with the
    /// guard off, and a constructor the production build cannot see keeps the
    /// spelled-out literal the only shape a shipped `ToolCtx` can take.
    pub(crate) fn fixture(cwd: PathBuf) -> Self {
        Self {
            cwd,
            cancel: CancellationToken::new(),
            call_id: "call".to_owned(),
            files: Arc::new(FileTimes::default()),
            credentials: Credentials::Unguarded,
            spawn: None,
            postbox: None,
            ask: None,
            switch: None,
            jobs: None,
        }
    }
}

/// What a finished tool call hands back to the model.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutput {
    /// One line saying what ran, fit for a transcript.
    pub title: String,
    /// The result as the model sees it.
    pub output: String,
    /// Structured extras a frontend may render richer than text.
    pub metadata: serde_json::Value,
}

/// A tool call that did not produce output.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The arguments did not fit the tool's schema.
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    /// The tool ran and failed. The message is what the model sees next, so
    /// it says what went wrong in terms the model can act on.
    #[error("{0}")]
    Failed(String),
    /// The turn was cancelled while the tool ran.
    #[error("the call was cancelled")]
    Cancelled,
}

/// One thing the model can do besides talk.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Name the model calls, and the permission engine gates.
    ///
    /// Borrowed from `&self` rather than `'static`, because a tool an MCP
    /// server contributed is named `mcp__<server>__<tool>` out of two strings
    /// nothing knew at compile time. Every builtin still returns a literal.
    fn id(&self) -> &str;

    /// What the model is told about the tool.
    fn description(&self) -> &str;

    /// JSON schema of the arguments object.
    fn schema(&self) -> schemars::Schema;

    /// One line saying what this call would do — `read src/main.rs` — for
    /// permission dialogs and transcript titles. The default names the tool.
    fn describe(&self, args: &serde_json::Value) -> String {
        let _ = args;
        self.id().to_owned()
    }

    /// Runs the call.
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError>;
}

/// A tool as a provider advertises it to the model.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolDefinition {
    /// Name the model calls.
    pub name: String,
    /// What the model is told about it.
    pub description: String,
    /// JSON schema of the arguments object.
    pub schema: serde_json::Value,
}

/// The set of tools one engine executes.
pub struct Registry {
    tools: Vec<Arc<dyn Tool>>,
}

impl Registry {
    /// Builds a registry over exactly `tools`.
    #[must_use]
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        Self { tools }
    }

    /// Every tool this build ships.
    #[must_use]
    pub fn with_builtins() -> Self {
        Self::new(vec![
            Arc::new(read::ReadTool),
            Arc::new(edit::EditTool),
            Arc::new(write::WriteTool),
            Arc::new(glob::GlobTool),
            Arc::new(grep::GrepTool),
            // `bash`, not `shell`: upstream pins the id for compatibility with
            // saved permissions, and the tool renders its prompt against the
            // shell this machine actually offers.
            Arc::new(shell::ShellTool::new()),
            // Upstream registers one todo tool, which owns the list.
            Arc::new(todo::TodoWriteTool),
            Arc::new(webfetch::WebfetchTool::new()),
            Arc::new(websearch::WebsearchTool::new()),
            // The roster's skill tool loads nothing: which directories a
            // session scans is the engine's answer — ganja's own two homes,
            // plus what a config named — and neither half is reachable from
            // in here. One holding them is installed the way `task` and an
            // MCP server's tools are, over the top of this one.
            Arc::new(skill::SkillTool::new()),
            Arc::new(question::QuestionTool),
            Arc::new(bash_output::BashOutputTool),
            Arc::new(kill_shell::KillShellTool),
        ])
    }

    /// The tool named `name`, or nothing.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.iter().find(|tool| tool.id() == name)
    }

    /// The same set with `tool` on the end, replacing any tool already
    /// registered under its id.
    ///
    /// What the engine builds when the session learns which agent it is running
    /// as: the task tool's description is the roster *that* agent may delegate
    /// to, so switching agents rebuilds the registry rather than mutating a
    /// tool in place.
    #[must_use]
    pub fn with(&self, tool: Arc<dyn Tool>) -> Self {
        let mut tools: Vec<Arc<dyn Tool>> = self
            .tools
            .iter()
            .filter(|held| held.id() != tool.id())
            .map(Arc::clone)
            .collect();
        tools.push(tool);

        Self { tools }
    }

    /// The same set with every tool in `tools` on the end, in the order given,
    /// each replacing any tool already registered under its id.
    ///
    /// What the engine builds when a background connect finishes and the model
    /// has a server's tools to be offered: the whole set is rebuilt from the
    /// base rather than mutated, so a turn already holding a snapshot keeps
    /// the tools it started with.
    #[must_use]
    pub fn with_all(&self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) -> Self {
        tools.into_iter().fold(
            Self {
                tools: self.tools.clone(),
            },
            |set, tool| set.with(tool),
        )
    }

    /// What a provider advertises to the model, in registration order.
    #[must_use]
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.id().to_owned(),
                description: tool.description().to_owned(),
                schema: serde_json::to_value(tool.schema())
                    .expect("a schema is JSON by construction"),
            })
            .collect()
    }
}

/// What the read log knows about one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seen {
    /// Read this session, with the modification stamp it had at the time.
    Read(Option<SystemTime>),
    /// Changed on disk after it was read, and not read again since.
    ///
    /// A state rather than a stamp comparison, because what noticed is
    /// [`crate::watch`], at a moment nothing was asking. Re-deriving the
    /// answer when a `write` finally does ask would be a second look at a name
    /// the first look already condemned — and would answer "fresh" for a file
    /// that has been changed and changed back, which is precisely the case the
    /// model needs told.
    Stale,
}

/// Which files were read this session, and the modification stamp each had.
///
/// `write` and `edit` refuse to touch an existing file the model has not
/// read, or one that changed on disk after the read — upstream's
/// read-before-write rule — and this is where the reads are recorded.
///
/// The rule has two ways of noticing a change. A tool asking whether it may
/// write compares stamps, which is the whole of it upstream; and
/// [`crate::watch`] reports changes as the filesystem makes them, so a file
/// somebody edited in another window is condemned before the model acts on
/// what it read, and named to it at the top of the next turn.
#[derive(Debug, Default)]
pub struct FileTimes {
    log: Mutex<Log>,
}

/// The read log's contents, under one lock: what is known about each file,
/// which files the model still has to be told about, and where a new read is
/// announced.
#[derive(Debug, Default)]
struct Log {
    /// What was read, and what became of it.
    read: HashMap<PathBuf, Seen>,
    /// Files that went stale and have not been named to the model yet, in the
    /// order they went. Drained by [`FileTimes::take_stale`] at the top of the
    /// next turn that asks the model anything.
    unannounced: Vec<PathBuf>,
    /// Where each newly recorded path is announced, so [`crate::watch`] can
    /// take a watch on the directory holding it. [`None`] on every session
    /// nobody started a watcher for, which is every scripted and golden run.
    ///
    /// Under the same lock as the map rather than beside it: a record is one
    /// critical section, and the send is a queue push that cannot block, so
    /// nothing here puts a tool call behind a reader.
    reads: Option<tokio::sync::mpsc::UnboundedSender<PathBuf>>,
}

impl FileTimes {
    /// Records that `path` was read just now, with the modification stamp it
    /// currently has.
    pub fn record(&self, path: &Path) {
        self.record_stat(path, modification_stamp(path));
    }

    /// Records that `path` was read or written just now, with a stamp the
    /// caller already has.
    ///
    /// That stamp must come from an `fstat` on the descriptor the call is
    /// reading or writing — `File::metadata`, not `fs::metadata` — because a
    /// fresh look at the path is a second resolution of a name somebody else
    /// may have redefined in between, which is the race `tool/anchor.rs`
    /// exists to close. Recording the stamp of a file other than the one that
    /// was written is how a stale read passes for a fresh one.
    pub fn record_stat(&self, path: &Path, stamp: Option<SystemTime>) {
        let mut log = self.log.lock().expect("the read log is never poisoned");

        log.read.insert(path.to_owned(), Seen::Read(stamp));
        // A file read again before the notice went out has nothing left to
        // say: the model is about to be handed the current contents, and
        // "re-read it" is advice it has already taken.
        log.unannounced.retain(|held| held != path);

        // A queue push, never a syscall: which directory this file lives in
        // and whether it is worth watching are decided on the watcher's own
        // task, so that a tool call costs the same whether or not a session
        // is watching. A closed receiver is a watcher that has gone away.
        if let Some(reads) = &log.reads {
            let _ = reads.send(path.to_owned());
        }
    }

    /// Announces every path recorded from now on, so a watcher can take a
    /// watch on the directory holding it.
    ///
    /// One announcer per log: a second call replaces the first, which is what
    /// makes restarting a watcher leave nothing behind talking to a channel
    /// nobody drains. Unbounded on purpose — the alternative is a `record`
    /// that waits on a background task, and a tool call may never wait for
    /// bookkeeping.
    pub fn announce_reads(&self) -> tokio::sync::mpsc::UnboundedReceiver<PathBuf> {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        self.log
            .lock()
            .expect("the read log is never poisoned")
            .reads = Some(sender);

        receiver
    }

    /// Forgets every read, so that what the model may write is judged against
    /// the conversation it is actually in.
    ///
    /// The rule is per conversation, not per process: a subagent starts with an
    /// empty log for exactly this reason. A session the engine puts down —
    /// cleared or swapped for a stored one — has to leave its reads behind
    /// with it, or the first thing the next conversation does could be to
    /// overwrite a file it never opened.
    pub fn clear(&self) {
        let mut log = self.log.lock().expect("the read log is never poisoned");

        log.read.clear();
        // The queued notice belongs to the conversation that read the files.
        // Telling the next one that files it never opened have moved would be
        // a reminder about somebody else's session.
        log.unannounced.clear();
    }

    /// Records that something outside this session may have touched `path`,
    /// and marks it stale if something did.
    ///
    /// What [`crate::watch`] calls for every filesystem event whose path this
    /// session has read; a path it has not read is not this rule's business
    /// and returns without touching the disk. Stale means the file's
    /// modification stamp differs from the one recorded when it was read, or
    /// the file is gone — an agent's own write records its new stamp as part
    /// of writing, so the event that write causes compares clean.
    pub fn note_change(&self, path: &Path) {
        // The stat happens between the two locks rather than under one: this
        // runs on every event for every watched file, and holding the log
        // across a filesystem call would put a tool call behind whatever the
        // disk is doing.
        let recorded = match self
            .log
            .lock()
            .expect("the read log is never poisoned")
            .read
            .get(path)
        {
            Some(Seen::Read(stamp)) => *stamp,
            // Not read this session, or already condemned: either way there is
            // nothing here to decide.
            Some(Seen::Stale) | None => return,
        };

        let changed = match std::fs::metadata(path) {
            Ok(metadata) => metadata.modified().ok() != recorded,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            // Anything else is the filesystem declining to answer rather than
            // an answer. `modification_stamp` fails open for the same reason:
            // a session that started refusing edits because one lookup was
            // momentarily refused would be worse than one that lets the
            // write's own check decide.
            Err(_) => false,
        };
        if !changed {
            return;
        }

        let mut log = self.log.lock().expect("the read log is never poisoned");
        // Re-checked against the same stamp, because a read may have landed
        // while the stat above was running: that read saw the change, so
        // condemning what it recorded would refuse a file the model just
        // looked at.
        if log.read.get(path) != Some(&Seen::Read(recorded)) {
            return;
        }
        log.read.insert(path.to_owned(), Seen::Stale);
        log.unannounced.push(path.to_owned());
    }

    /// The files that went stale since this was last asked, and clears them.
    ///
    /// Draining is what makes the notice fire once per staleness episode
    /// rather than once per turn: the file stays `Seen::Stale` — so `write`
    /// and `edit` keep refusing it until it is read again — while the *telling*
    /// happens once. A file that is re-read and then changed again is a new
    /// episode, and is named again.
    pub fn take_stale(&self) -> Vec<PathBuf> {
        std::mem::take(
            &mut self
                .log
                .lock()
                .expect("the read log is never poisoned")
                .unannounced,
        )
    }

    /// Checks that `path` was read this session and has not changed on disk
    /// since.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Failed`] naming the remedy — read the file first,
    /// or read it again — because the message is what the model sees next.
    pub fn check_fresh(&self, path: &Path) -> Result<(), ToolError> {
        self.check_fresh_stat(path, modification_stamp(path))
    }

    /// The same check against a stamp the caller already has, under the same
    /// rule as [`FileTimes::record_stat`]: it must be an `fstat` on the
    /// descriptor about to be written, so what is judged fresh is the file
    /// that is about to be overwritten and not whatever the name resolves to
    /// a moment later.
    ///
    /// # Errors
    ///
    /// As [`FileTimes::check_fresh`].
    pub fn check_fresh_stat(
        &self,
        path: &Path,
        stamp: Option<SystemTime>,
    ) -> Result<(), ToolError> {
        let recorded = self
            .log
            .lock()
            .expect("the read log is never poisoned")
            .read
            .get(path)
            .copied();

        let stale = || {
            ToolError::Failed(format!(
                "{} changed on disk after it was read; read it again",
                path.display()
            ))
        };

        let recorded = match recorded {
            // Already condemned by the watcher, so this answers from the state
            // rather than from a fresh look: the stamp it was condemned on is
            // gone, and a file changed and changed back would otherwise pass
            // for one that never moved.
            Some(Seen::Stale) => return Err(stale()),
            Some(Seen::Read(recorded)) => recorded,
            None => {
                return Err(ToolError::Failed(format!(
                    "{} has not been read this session; read it first",
                    path.display()
                )));
            }
        };

        if stamp != recorded {
            return Err(stale());
        }

        Ok(())
    }
}

/// `path` spelled the way this platform spells one.
///
/// A tool's `filePath` is text a model wrote, and a model writes
/// `docs/guide.md` on every platform there is. Joining that onto a Windows
/// working directory gives `C:\project\docs/guide.md` — a path that *opens*,
/// because Windows accepts both separators, but that no Windows program would
/// ever print. These paths are printed: `read` echoes one back in its `<path>`
/// element, `edit` and `write` put one in the title a person reads, and the
/// golden differential compares every one of them against upstream, whose Node
/// `path.resolve` hands back a native spelling. A mixed one is a divergence
/// with no upside — nobody chose it, it just falls out of `join`.
///
/// Rebuilding from [`Path::components`] is the whole of it: the components come
/// back out joined with this platform's own separator. Windows treats `/` and
/// `\` alike when it parses, so both are read and one is written.
///
/// A no-op on unix, where `/` is the only separator there has ever been and a
/// `\` inside a name is part of the name.
fn native_path(path: PathBuf) -> PathBuf {
    path.components().collect()
}

/// Resolves `file_path` against `cwd` — never against the process cwd, so a
/// relative argument means what the call site meant, not what the engine's
/// own working directory happens to be.
///
/// Spelled the way this platform spells a path, because every caller echoes
/// it back to the model one way or another. See [`native_path`].
fn resolve(cwd: &Path, file_path: &str) -> PathBuf {
    let path = Path::new(file_path);
    let joined = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };

    native_path(joined)
}

/// [`resolve`], with `cwd` itself standing in when the call named no path at
/// all — the shape `glob` and `grep` take their optional search base through.
fn resolve_or_cwd(cwd: &Path, path: Option<&str>) -> PathBuf {
    path.map_or_else(|| cwd.to_owned(), |path| resolve(cwd, path))
}

/// `path` relative to `cwd` when it is under it, absolute otherwise — for a
/// title or one-line description a person can actually read.
fn display(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

/// The filesystem's modification stamp for `path`, or [`None`] where the
/// filesystem does not offer one — in which case recording and checking
/// compare as equal, failing open rather than refusing every edit.
fn modification_stamp(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

/// Whether `left` and `right` name the same file.
///
/// Both sides are canonicalized, so a link planted at an innocent name and a
/// `..` route that climbs back down onto the store are caught rather than
/// compared as text. Canonicalizing needs the path to exist and neither side is
/// guaranteed to — the store is absent until the first `ganja auth login` — so
/// a failure falls back to comparing what was written, made absolute. That
/// fallback does not resolve `..`, which is what a missing file costs: a file
/// that is not there has no contents to leak.
fn is_same_file(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => match (std::path::absolute(left), std::path::absolute(right)) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        },
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
