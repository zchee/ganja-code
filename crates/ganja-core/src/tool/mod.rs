//! Tools the agent loop can execute on the model's behalf.
//!
//! Spec: upstream `packages/opencode/src/tool/` — `tool.ts` for the contract,
//! `registry.ts` for the set. Each tool lives in its own module beside this
//! one, and descriptions are ported from upstream's `*.txt` prompt files
//! (MIT, attributed in `THIRD_PARTY_NOTICES.md`).

/// Anchored file I/O, shared by the two tools that write. Not public: it is
/// how `write` and `edit` reach the disk, not something a frontend or a
/// third-party tool has any business addressing files through.
mod anchor;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;
pub mod shell;
pub mod task;
pub mod todo;
pub mod truncate;
pub mod webfetch;
pub mod write;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::SystemTime,
};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

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
    /// What a call needs to run a whole second agent loop, which only
    /// [`task::TaskTool`] does.
    ///
    /// [`None`] on every turn that has no agents to spawn — and on every
    /// *child* turn, which is the entire depth guard stated a second way. Its
    /// fields are private and it has no public constructor, so a frontend
    /// building a [`ToolCtx`] of its own can only ever pass [`None`].
    pub spawn: Option<task::Spawn>,
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
    fn id(&self) -> &'static str;

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
            Arc::new(todo::TodoWriteTool::new()),
            Arc::new(webfetch::WebfetchTool),
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

    /// The same set without the tool named `id`.
    ///
    /// A subagent's registry is this build's minus `task`, which is how the
    /// depth limit is enforced: the tool is not refused, it is not offered.
    #[must_use]
    pub fn without(&self, id: &str) -> Self {
        Self {
            tools: self
                .tools
                .iter()
                .filter(|tool| tool.id() != id)
                .map(Arc::clone)
                .collect(),
        }
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

/// Which files were read this session, and the modification stamp each had.
///
/// `write` and `edit` refuse to touch an existing file the model has not
/// read, or one that changed on disk after the read — upstream's
/// read-before-write rule — and this is where the reads are recorded.
#[derive(Debug, Default)]
pub struct FileTimes {
    read: Mutex<HashMap<PathBuf, Option<SystemTime>>>,
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
        self.read
            .lock()
            .expect("the read log is never poisoned")
            .insert(path.to_owned(), stamp);
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
        self.read
            .lock()
            .expect("the read log is never poisoned")
            .clear();
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
            .read
            .lock()
            .expect("the read log is never poisoned")
            .get(path)
            .copied();

        let Some(recorded) = recorded else {
            return Err(ToolError::Failed(format!(
                "{} has not been read this session; read it first",
                path.display()
            )));
        };

        if stamp != recorded {
            return Err(ToolError::Failed(format!(
                "{} changed on disk after it was read; read it again",
                path.display()
            )));
        }

        Ok(())
    }
}

/// The filesystem's modification stamp for `path`, or [`None`] where the
/// filesystem does not offer one — in which case recording and checking
/// compare as equal, failing open rather than refusing every edit.
fn modification_stamp(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

/// Where ganja keeps its own credentials, or [`None`] when this machine has no
/// home directory to resolve a store against — in which case there is nothing
/// here to protect.
///
/// Resolved once per process: the store cannot move while ganja runs, a guard
/// that could be pointed somewhere harmless by setting an environment variable
/// mid-run would not be worth much, and `grep` would otherwise re-derive the
/// path for every file it walks past.
fn credential_store() -> Option<&'static Path> {
    static STORE: OnceLock<Option<PathBuf>> = OnceLock::new();

    STORE
        .get_or_init(|| crate::auth::store_path().ok())
        .as_deref()
}

/// Whether `path` is ganja's credential store.
///
/// `read` and `grep` run without asking — that is what makes them usable — and
/// both take a path the model chose, so without this a model acting on
/// instructions it read in a file or a fetched page could put this machine's
/// provider API keys straight into the transcript that is sent to a provider.
///
/// Only ganja's own store is guarded. Which *other* files hold secrets is a
/// question only the user can answer, and a built-in half-answer would read as
/// a promise this cannot keep.
pub(crate) fn is_credential_store(path: &Path) -> bool {
    credential_store().is_some_and(|store| is_same_file(path, store))
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
mod tests {
    use std::sync::Arc;

    use super::{
        FileTimes, Registry, ToolError, credential_store, is_credential_store, is_same_file,
    };

    #[test]
    fn the_registry_finds_a_tool_by_id_and_misses_unknown_names() {
        let registry = Registry::with_builtins();

        let read = registry.get("read").expect("read ships in every build");
        assert_eq!(read.id(), "read");
        assert!(registry.get("no-such-tool").is_none());
    }

    #[test]
    fn a_file_must_be_read_before_it_may_be_touched() {
        let times = FileTimes::default();
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one").expect("the fixture writes");

        let refused = times
            .check_fresh(&path)
            .expect_err("an unread file is refused");
        assert!(
            matches!(&refused, ToolError::Failed(message) if message.contains("read it first")),
            "got {refused:?}"
        );

        times.record(&path);
        times
            .check_fresh(&path)
            .expect("a freshly read file is fresh");
    }

    #[test]
    fn a_file_changed_after_its_read_goes_stale() {
        let times = FileTimes::default();
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one").expect("the fixture writes");

        times.record(&path);
        // Filesystem stamps can be coarse; force one that differs.
        let stale = std::time::SystemTime::UNIX_EPOCH;
        std::fs::File::open(&path)
            .and_then(|file| file.set_modified(stale))
            .expect("the fixture can move the stamp");

        let refused = times
            .check_fresh(&path)
            .expect_err("a changed file is refused");
        assert!(
            matches!(&refused, ToolError::Failed(message) if message.contains("read it again")),
            "got {refused:?}"
        );

        times.record(&path);
        times.check_fresh(&path).expect("re-reading repairs it");
    }

    #[test]
    fn the_file_log_is_shared_by_clone_not_copied() {
        let times = Arc::new(FileTimes::default());
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one").expect("the fixture writes");

        Arc::clone(&times).record(&path);
        times
            .check_fresh(&path)
            .expect("both handles see the same log");
    }

    #[test]
    fn the_credential_store_guard_answers_for_the_store_and_not_for_a_namesake() {
        let store = credential_store().expect("this machine has a home directory");

        assert!(
            is_credential_store(store),
            "the guard has to recognize the store it exists to protect"
        );

        let dir = tempfile::tempdir().expect("a scratch directory");
        let namesake = dir.path().join("auth.json");
        std::fs::write(&namesake, "{}").expect("the fixture writes");

        assert!(
            !is_credential_store(&namesake),
            "the guard is about which file this is, not what it is called"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_planted_at_an_innocent_name_is_still_the_file_it_points_at() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let target = dir.path().join("auth.json");
        std::fs::write(&target, "{}").expect("the fixture writes");
        let planted = dir.path().join("notes.json");
        std::os::unix::fs::symlink(&target, &planted).expect("the link plants");

        assert!(
            is_same_file(&planted, &target),
            "a link is the file it points at, whatever it is called"
        );
    }

    #[test]
    fn a_route_that_climbs_out_and_back_down_lands_on_the_same_file() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let target = dir.path().join("auth.json");
        std::fs::write(&target, "{}").expect("the fixture writes");
        let nested = dir.path().join("one").join("two");
        std::fs::create_dir_all(&nested).expect("the fixture nests");

        let climbed = nested.join("..").join("..").join("auth.json");

        assert!(
            is_same_file(&climbed, &target),
            "{} should resolve onto {}",
            climbed.display(),
            target.display()
        );
    }

    #[test]
    fn paths_that_cannot_be_canonicalized_are_compared_as_written() {
        // Canonicalizing needs the file to be there, and the store is not until
        // the first login: what is left to compare is the paths themselves.
        let dir = tempfile::tempdir().expect("a scratch directory");
        let absent = dir.path().join("ganja").join("auth.json");
        let present = dir.path().join("auth.json");
        std::fs::write(&present, "{}").expect("the fixture writes");

        assert!(is_same_file(&absent, &absent));
        assert!(is_same_file(&dir.path().join("./ganja/auth.json"), &absent));
        assert!(!is_same_file(
            &dir.path().join("ganja").join("other.json"),
            &absent
        ));
        assert!(
            !is_same_file(&present, &absent),
            "a file that exists is not one that does not"
        );
    }
}
