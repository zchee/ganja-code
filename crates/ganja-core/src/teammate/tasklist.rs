//! The team's shared task list, as the four tools reach it.
//!
//! The engine's half of [`ganja_tool::tasklist`]'s seam: the tools decide what
//! the model asked for, and this decides which documents move. Nothing here is
//! ported from either upstream — opencode has no teams, and Claude Code keeps
//! its list inside its own process — so the specification is this port's own,
//! and the store beneath is [`ganja_team::task`]'s.
//!
//! # Why the seam is here at all
//!
//! `ganja-tool`'s internal dependency list is asserted to be exactly
//! `ganja-permission`, so it cannot name the crate that owns the documents'
//! format — which is the same reason `send_message` reaches a mailbox through
//! [`crate::tool::team::Postbox`]. What crosses is values; what stays down
//! here is a directory, a lock protocol and a counter.
//!
//! # Synchronous store, asynchronous seam
//!
//! [`ganja_team::task::Store`] is deliberately synchronous — a task document
//! is a sub-second read-modify-write on a small file — and its own doc says
//! whoever calls it from inside a turn wraps it in `spawn_blocking`. That is
//! what every method below does: a lock held by another process is a wait, and
//! a wait on the runtime's own thread would be a whole session's render loop
//! waiting on somebody else's write.
//!
//! # The identity is bound here, once
//!
//! [`TeamTasks::new`](crate::teammate::tasklist::TeamTasks::new) takes the name this session writes comments under and
//! keeps it, so no call can choose one. It is the same value the session's
//! postbox stamps a message with, and it is bound for that seam's reason: a
//! `from` argument would be a fact about what a model typed.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use ganja_team::task::{
    Comment as StoredComment, NewTask, Store, Task, TaskError, TaskId, TaskStatus, TaskSummary,
    Update,
};
use ganja_team::{TeamName, TeamsRoot, record};
use ganja_tool::tasklist::{
    Change, Comment, Draft, Owner, Record, Status, Summary, TaskFailure, TaskList,
};

/// What a failure that is nobody's refusal reads as, ahead of the store's own
/// account of it.
///
/// The store's refusals — an id that is not an id, a task nobody filed, a
/// claim somebody already won — are sentences already, and are carried through
/// as they are. A lock that would not open or a directory that would not be
/// written is not a refusal at all, and saying so is what keeps the model from
/// reading "already claimed" and "the disk is full" in the same voice.
const UNREACHABLE: &str = "The team's task list could not be read or written";

/// What a blocking call whose answer never came back reads as. Unreachable
/// short of the runtime tearing down under a call, and said in words rather
/// than unwrapped.
///
/// It does not claim the change landed nowhere, because it may have: a
/// runtime shutting down loses the answer of a call that had already written
/// its document, and only a read can settle which happened.
const INTERRUPTED: &str = "The team's task list did not answer, so whether the change landed is unknown. Read the task back before deciding.";

/// One team's shared task list, and the name this session acts on it under.
pub struct TeamTasks {
    /// The documents themselves.
    store: Store,
    /// Who a comment written through this list is by — bound here, never an
    /// argument.
    identity: Arc<str>,
}

impl std::fmt::Debug for TeamTasks {
    /// Which list and which member, never what a task says: a description, a
    /// metadata value and a comment are content, and content stays out of a
    /// `{:?}` for the reason it stays out of a log.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TeamTasks")
            .field("tasks", &self.store.dir().display())
            .field("as", &self.identity)
            .finish()
    }
}

impl TeamTasks {
    /// The list kept in `dir`, acted on as `identity`.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>, identity: impl Into<Arc<str>>) -> Self {
        Self { store: Store::new(dir), identity: identity.into() }
    }

    /// The list `team` keeps under `root`, acted on as `identity` — the one
    /// call a holder of a teams root makes.
    #[must_use]
    pub fn of(root: &TeamsRoot, team: &TeamName, identity: impl Into<Arc<str>>) -> Self {
        Self::new(root.tasks_dir(team), identity)
    }

    /// Runs one store call off the runtime's own threads.
    async fn blocking<T, F>(&self, work: F) -> Result<T, TaskFailure>
    where
        F: FnOnce(Store) -> Result<T, TaskError> + Send + 'static,
        T: Send + 'static,
    {
        let store = self.store.clone();
        match tokio::task::spawn_blocking(move || work(store)).await {
            Ok(answer) => answer.map_err(refusal),
            Err(error) => {
                tracing::warn!(
                    tasks = %self.store.dir().display(),
                    %error,
                    "a task list call did not finish",
                );

                Err(TaskFailure { reason: INTERRUPTED.to_owned() })
            }
        }
    }
}

/// A store failure as the one sentence the model reads next.
///
/// The refusals are the store's own constants, rendered with the id they are
/// about — including the holder's name a lost claim needs, which is the whole
/// answer that call is asking for. Everything else is machinery the model
/// cannot act on beyond retrying, and is labelled as such.
fn refusal(error: TaskError) -> TaskFailure {
    let reason = match &error {
        TaskError::Shape { .. }
        | TaskError::NoSuchTask { .. }
        | TaskError::AlreadyOwned { .. }
        | TaskError::CounterExhausted
        | TaskError::SchemaInvalid { .. }
        // The cap renders the number it refused and the number it allows, and
        // the model's next move is to split the call — which is an act, so it
        // is the store's own sentence rather than machinery.
        | TaskError::TooManyCounterparts { .. }
        // A name that is not a document is one the model can go and look at,
        // which is why it is rendered whole rather than as machinery.
        | TaskError::NotADocument { .. } => error.to_string(),
        TaskError::Lock(_) | TaskError::Io(_) | TaskError::Json(_) => {
            format!("{UNREACHABLE}: {error}")
        }
    };

    TaskFailure { reason }
}

/// An id as the store reads them, or the refusal it earns.
fn id_of(id: &str) -> Result<TaskId, TaskFailure> {
    TaskId::parse(id).map_err(refusal)
}

/// Every id in `ids`, or the first refusal one of them earns.
fn ids_of(ids: &[String]) -> Result<Vec<TaskId>, TaskFailure> {
    ids.iter().map(|id| id_of(id)).collect()
}

/// The seam's status in the store's vocabulary.
const fn status_of(status: Status) -> TaskStatus {
    match status {
        Status::Pending => TaskStatus::Pending,
        Status::InProgress => TaskStatus::InProgress,
        Status::Completed => TaskStatus::Completed,
    }
}

/// The store's status in the seam's.
const fn status_from(status: TaskStatus) -> Status {
    match status {
        TaskStatus::Pending => Status::Pending,
        TaskStatus::InProgress => Status::InProgress,
        TaskStatus::Completed => Status::Completed,
    }
}

/// A stored task as the model reads it back.
///
/// The passthrough map a document carries is deliberately **not** rendered: a
/// key this build has never heard of survives a rewrite because the store
/// keeps it in position, and showing it to the model would be this build
/// claiming to understand somebody else's field.
fn record_of(task: Task) -> Record {
    Record {
        id: task.id.to_string(),
        subject: task.subject,
        description: task.description,
        active_form: task.active_form,
        status: status_from(task.status),
        owner: task.owner,
        blocks: task.blocks.iter().map(TaskId::to_string).collect(),
        blocked_by: task.blocked_by.iter().map(TaskId::to_string).collect(),
        metadata: task.metadata.into_iter().collect(),
        comments: task
            .comments
            .into_iter()
            .map(|comment| Comment { from: comment.from, at: comment.at, text: comment.text })
            .collect(),
    }
}

/// A stored summary as a listing shows it.
fn summary_of(summary: TaskSummary) -> Summary {
    Summary {
        id: summary.id.to_string(),
        subject: summary.subject,
        status: status_from(summary.status),
        owner: summary.owner,
        blocked_by: summary.blocked_by.iter().map(TaskId::to_string).collect(),
    }
}

#[async_trait]
impl TaskList for TeamTasks {
    async fn create(&self, draft: Draft) -> Result<Record, TaskFailure> {
        let new = NewTask {
            subject: draft.subject,
            description: draft.description,
            active_form: draft.active_form,
            metadata: draft.metadata.into_iter().collect(),
        };

        self.blocking(move |store| store.create(new)).await.map(record_of)
    }

    /// Every id read first, then ownership, then everything else.
    ///
    /// The order is the contract: a claim is the one part of a call that can
    /// be **refused**, and a call whose claim failed must leave the task
    /// exactly as it was — a teammate that lost the race and had nonetheless
    /// marked the task in progress would have told the whole team a lie about
    /// who is doing the work. So the claim goes first and a refusal returns
    /// before anything else is written; the second write is skipped entirely
    /// when the owner was the whole of what moved.
    ///
    /// Which is why the blocker ids are read *before* the claim rather than
    /// where they are used: they are the other way a call can be refused, and
    /// a claim taken and then refused for a malformed id would leave exactly
    /// the task this contract exists to prevent.
    async fn update(&self, id: &str, mut change: Change) -> Result<Record, TaskFailure> {
        let id = id_of(id)?;
        let add_blocks = ids_of(&change.add_blocks)?;
        let add_blocked_by = ids_of(&change.add_blocked_by)?;

        let mut claimed = None;
        if let Some(owner) = change.owner.take() {
            claimed = Some(match owner {
                Owner::Claim(owner) => self.blocking(move |store| store.claim(&id, &owner)).await?,
                Owner::Release => {
                    let release = Update { owner: Some(String::new()), ..Update::default() };
                    self.blocking(move |store| store.update(&id, release)).await?
                }
            });
        }
        if let Some(task) = claimed.filter(|_| change.is_only_ownership()) {
            return Ok(record_of(task));
        }

        let update = Update {
            status: change.status.map(status_of),
            subject: change.subject,
            description: change.description,
            active_form: change.active_form,
            // Never here: the owner was settled above, by the door that can
            // refuse. Passing it again would be a second write of a value
            // already written, through the door that cannot.
            owner: None,
            metadata: change.metadata.into_iter().collect(),
            add_blocks,
            add_blocked_by,
            add_comment: change.add_comment.map(|text| {
                StoredComment::new(self.identity.to_string(), text, record::now_iso8601())
            }),
        };

        self.blocking(move |store| store.update(&id, update)).await.map(record_of)
    }

    async fn delete(&self, id: &str) -> Result<(), TaskFailure> {
        let id = id_of(id)?;

        self.blocking(move |store| store.delete(&id)).await
    }

    async fn list(&self) -> Result<Vec<Summary>, TaskFailure> {
        let listed = self.blocking(move |store| store.list()).await?;

        Ok(listed.into_iter().map(summary_of).collect())
    }

    async fn get(&self, id: &str) -> Result<Record, TaskFailure> {
        let id = id_of(id)?;

        self.blocking(move |store| store.get(&id)).await.map(record_of)
    }
}

#[cfg(test)]
#[path = "tasklist_tests.rs"]
mod tests;
