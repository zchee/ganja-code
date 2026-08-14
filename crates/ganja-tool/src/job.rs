//! What a `bash` call needs to run a command beside the turn that started it,
//! instead of inside it.
//!
//! Spec: Claude Code's `run_in_background`/`BashOutput`/`KillShell` (2.1.x).
//! Upstream opencode has no equivalent — see `crates/ganja-core/src/job.rs`'s
//! module doc and **D454** (`background-execution-is-a-claude-port`) — so
//! nothing here ports a TypeScript file; the shape below is this port's own
//! reading of the observed contract.
//!
//! # What is here and what is not
//!
//! Tracking a background job is not something a tool knows how to do: an
//! engine-owned registry, a ring buffer, a spill file, and the process itself
//! are `ganja-core`'s vocabulary, and a tool that named them would be a tool
//! the engine cannot be assembled without. So the tracking is somebody
//! else's, reached through [`Jobs`] — the exact seam [`crate::task::Subagents`]
//! draws for the `task` tool — and what stays here is the schema, the
//! arguments, and the bytes the model finally reads.

use std::fmt;

use async_trait::async_trait;
use tokio::process::Child;

/// Where a background job stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Still running.
    Running,
    /// Ended on its own.
    Exited {
        /// Its exit code, when the platform reported one.
        code: Option<i32>,
    },
    /// Ended by a `kill_shell` call, or by the engine shutting down.
    Killed,
}

/// One background job's identity and where it stands right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobStatus {
    /// The id it was registered under, named in every reply about it.
    pub id: String,
    /// The command it is running, for a listing.
    pub command: String,
    /// Where it stands as of the moment this was read.
    pub state: State,
}

/// What a poll of a background job answers with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobRead {
    /// Output produced since the last poll of this job (or since it started,
    /// for the first), decoded lossily like every other tool's output.
    pub chunk: String,
    /// Where the job stands as of this read.
    pub status: JobStatus,
}

/// What all three background tools say when the context they were called in
/// carries no [`Jobs`] handle.
///
/// One sentence, spelled once: a person meeting this through `bash`,
/// `bash_output` or `kill_shell` is meeting the same fact, and three copies
/// of it were three sentences waiting to drift. Here rather than in any of
/// the three because this is the module that owns the handle they all ask
/// for. Not reachable through the engine, which wires one into every
/// [`crate::ToolCtx`] it builds.
pub(crate) const NO_JOBS: &str = "background shells are not available in this context";

/// Why a call against a background job did not produce what it asked for.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum JobsError {
    /// Nothing this registry knows about goes by that id.
    #[error("no background shell with id {0}")]
    NotFound(String),
}

/// Tracks and runs the background jobs a `bash` call registers, on
/// `bash_output`'s and `kill_shell`'s behalf as well as `bash`'s own.
///
/// Deliberately says nothing about *how*: an engine-owned registry, a
/// process, a ring buffer are the engine's vocabulary, and a tool that named
/// them would be a tool the engine cannot be assembled without.
///
/// [`fmt::Debug`] is required because [`crate::ToolCtx`] derives it, and an
/// implementation is expected to render a summary rather than every job's
/// buffered bytes.
#[async_trait]
pub trait Jobs: fmt::Debug + Send + Sync {
    /// Registers `child` — already spawned, already in its own process group
    /// — as a new background job labelled `command`, and returns the id it
    /// was assigned and its status the instant registration completes.
    /// Always [`State::Running`]: a [`Child`] that exists already started.
    async fn start(&self, command: String, child: Child) -> JobStatus;

    /// Output `bash_id` has produced since the last call for that id — or
    /// since it started, for the first — and its status as of now.
    ///
    /// # Errors
    ///
    /// [`JobsError::NotFound`] when `bash_id` names no job this registry
    /// knows.
    async fn output(&self, bash_id: &str) -> Result<JobRead, JobsError>;

    /// Ends `bash_id`'s whole process tree and reports its terminal status.
    /// A `bash_id` that has already exited or was already killed answers
    /// with its existing terminal status rather than an error: killing
    /// something already dead is not a failure.
    ///
    /// # Errors
    ///
    /// [`JobsError::NotFound`] when `bash_id` names no job this registry
    /// knows.
    async fn kill(&self, bash_id: &str) -> Result<JobStatus, JobsError>;

    /// Every job this registry knows about, for a status display. Cheap and
    /// synchronous, because a status bar polls it every tick.
    fn list(&self) -> Vec<JobStatus>;
}
