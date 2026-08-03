//! Tools the agent loop can execute on the model's behalf.
//!
//! Spec: upstream `packages/opencode/src/tool/` — `tool.ts` for the contract,
//! `registry.ts` for the set. Each tool lives in its own module beside this
//! one, and descriptions are ported from upstream's `*.txt` prompt files
//! (MIT, attributed in `THIRD_PARTY_NOTICES.md`).

pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;
pub mod shell;
pub mod todo;
pub mod truncate;
pub mod webfetch;
pub mod write;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
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
        let modified = modification_stamp(path);
        self.read
            .lock()
            .expect("the read log is never poisoned")
            .insert(path.to_owned(), modified);
    }

    /// Checks that `path` was read this session and has not changed on disk
    /// since.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Failed`] naming the remedy — read the file first,
    /// or read it again — because the message is what the model sees next.
    pub fn check_fresh(&self, path: &Path) -> Result<(), ToolError> {
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

        if modification_stamp(path) != recorded {
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{FileTimes, Registry, ToolError};

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
}
