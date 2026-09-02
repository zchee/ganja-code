//! The four tools a team drives its shared list with: `task_create`,
//! `task_update`, `task_list` and `task_get`.
//!
//! **Neither upstream has a counterpart to port.** opencode has no teams, so
//! it has nothing for two agents to divide between them; Claude Code has a
//! task list, but it keeps it inside its own process. What is taken from the
//! latter is the *semantics* a model is already trained on — the status
//! vocabulary, an owner that is empty until somebody claims it, a metadata map
//! that merges, comments that only ever grow. Every sentence below is ganja's
//! own, written from this port's own behavior specification.
//!
//! # Not the `task` tool (**glossary**)
//!
//! A **task** here is an entry on a team's shared list. The tool that starts a
//! subagent or a teammate is [`crate::task`], always spelled *the task tool*,
//! and the two never mean each other. The modules are named apart for the same
//! reason: `task.rs` is the spawn door, `tasklist.rs` is the list.
//!
//! # What is here and what is not
//!
//! Keeping a list is not something a tool knows how to do: the documents live
//! in a team's own directory, under a lock protocol two processes share, and
//! neither the directory nor the protocol is this layer's vocabulary — this
//! crate's internal dependency list is asserted to be exactly
//! `ganja-permission`, so it may not name the crate that owns the format
//! either. So the keeping is somebody else's, reached through [`TaskList`](crate::tasklist::TaskList),
//! and what stays here is the tool's own half: the arguments, the schemas, the
//! rendering the model reads, and which call each argument becomes.
//!
//! Three of those mappings are decided here rather than beyond the seam,
//! because each is about what the model *asked for* rather than about how a
//! document is written:
//!
//! - a `status` of `deleted` is not a status at all, it is a removal
//!   ([`TaskList::delete`](crate::tasklist::TaskList::delete)) — the list carries three states and a deleted task
//!   is gone rather than tombstoned;
//! - an `owner` decides between claiming and releasing ([`Owner`](crate::tasklist::Owner)), because a
//!   claim is the one operation that can be refused and a release never is;
//! - a comment's author is **never** an argument. It is bound where the seam
//!   is built, exactly as [`crate::team::Postbox`]'s sender is and for that
//!   rule's reason: a `from` parameter would be a fact about what the caller
//!   typed, and the caller is a model whose arguments could put somebody
//!   else's name on what it said.
//!
//! # They run unasked
//!
//! None of the four is in
//! [`ASK_BY_DEFAULT`](ganja_permission::permission::ASK_BY_DEFAULT), which is
//! the same answer `todowrite` gets and for the same reason: what changes is a
//! list this session's own team keeps, inside a directory this user already
//! owns, and whatever a member goes on to *do* about a task is gated by that
//! member's own rules. The permission that matters was answered at the spawn
//! that made the team.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::list_sessions::neutralize;
use crate::{Tool, ToolCtx, ToolError, ToolOutput, truncate};

/// The id of the tool that files a task, which is also its permission key.
pub const CREATE_ID: &str = "task_create";

/// The id of the tool that changes one, which is also its permission key.
pub const UPDATE_ID: &str = "task_update";

/// The id of the tool that lists them, which is also its permission key.
pub const LIST_ID: &str = "task_list";

/// The id of the tool that reads one whole, which is also its permission key.
pub const GET_ID: &str = "task_get";

/// What a call reads where this build offered a task tool with no list behind
/// it.
///
/// Reachable only in the window between a teammate's tools being lent and its
/// list being installed — the engine lends the four with the rest of a
/// teammate's roster and the spawn installs the list a moment later — which is
/// [`crate::task`]'s own `NO_TEAM` precedent. `pub` for that module's reason
/// too: a session that cannot reach a list should say so in one sentence
/// wherever the answer is formed.
///
/// So it names a fault rather than something to do about it: nothing the model
/// can say next would install the list, and telling it to start a teammate
/// would send it after a condition that is not the one it hit.
pub const NO_LIST: &str = "This build offered the task tools without a shared list behind them, which is a wiring fault rather than anything this conversation can settle. Nothing was read or written; carry on without the list and say that it was unreachable.";

/// What a call reads when it asked to remove a task and change it at once.
///
/// Refused rather than half-applied: a removal leaves nothing for a new
/// subject to be a subject of, and silently dropping the rest would be the
/// model believing it wrote something that was never written.
const DELETE_WITH_CHANGES: &str = "Removing a task is the whole of what that call does, so nothing else may travel with `status: \"deleted\"`. Send the removal on its own.";

/// What the model is told `task_create` is for.
const CREATE_DESCRIPTION: &str = "\
File a task on the shared list this session's team coordinates through. Every \
member of the team reads the same list, so a task filed here is work anybody \
on it can pick up.

`subject` is the imperative one-liner a member chooses the task by. \
`description` is everything somebody picking it up needs, written to be read \
on its own: a teammate reads the task, not this conversation. `active_form` \
is the present-continuous wording to show while the task is being worked on, \
where there is one worth showing. `metadata` carries whatever else the team \
needs kept beside the task.

A new task is pending and belongs to nobody, and its id is issued in order and \
not reissued while this team's counter stands: removing a task leaves a gap \
where it was rather than freeing its id. Give it an owner, wire what it waits \
on, or move it along with task_update.";

/// How many other tasks one `task_update` call may wire, which is the number
/// `UPDATE_DESCRIPTION` spells in words.
///
/// A **mirror**, never the bound itself: the cap belongs to the store, where
/// the documents and their locks are, and this crate's internal dependency
/// list is asserted to be exactly `ganja-permission`, so it may not name that
/// constant to read it. What keeps the two spellings one decision is
/// `ganja-core`, the crate that sees both: its seam tests assert this equals
/// `ganja_team::task::MAX_COUNTERPARTS` and that the seam accepts eight
/// counterparts and refuses nine, so raising one number alone reddens.
///
/// The description spells it in words and cannot read this constant — a
/// `&str` const cannot format one without a crate this workspace does not
/// have — so the prose is pinned to this number by a test of this module's
/// own rather than derived from it.
pub const MAX_COUNTERPARTS: usize = 8;

/// What the model is told `task_update` is for.
const UPDATE_DESCRIPTION: &str = "\
Change one task on the team's shared list. This is the only door that changes \
one.

Name it in `task_id` and pass only what should move; everything left out stays \
as it is. `status` moves it along: pending, in_progress, completed, or \
deleted, which removes the task permanently and cannot travel with any other \
change.

`owner` claims or releases it, and may name any member rather than only \
yourself: handing work to whoever will do it is a lead's job, and a team is \
one trust domain. A non-empty owner claims the task for that member and is \
refused when somebody already holds it, which is what makes claiming safe to \
race: two teammates reaching for one task produce one owner and one refusal, \
never two members doing one piece of work. That holds unless a claim somehow \
takes more than ten seconds to write: a hold that old is broken by the next \
claimant taking it, age being the only thing anybody checks, and the two then \
write the owner without ever seeing each other and both believe they won. A \
claim is one small write, so it is a bound worth knowing rather than one to \
work around. Whoever already holds a task counts as somebody, including you: \
claiming a task that is already yours is refused like anybody else's claim on \
it, and the refusal takes the whole call with it, so a status or a comment \
sent beside your own name is not written either. Once a task is yours, leave \
`owner` out of later calls about it. The empty string releases a task, which \
is how work is taken back from a member that has stopped. Reassigning is \
therefore two calls — release it, then claim it for whoever takes it next.

`metadata` merges into what is already there, and a null value removes its \
key. `add_blocks` and `add_blocked_by` add dependencies and remove none, and \
each dependency is recorded on both tasks — what this one blocks is blocked \
by it — so an id naming no task refuses the whole call rather than leaving \
half an edge behind. For that reason one call wires at most eight other tasks \
between the two lists, and naming more refuses rather than truncates; wire a \
longer list a few calls at a time. `add_comment` appends one comment; who \
wrote it is this session's own identity rather than anything a call can \
choose.";

/// What the model is told `task_list` is for.
const LIST_DESCRIPTION: &str = "\
The team's shared task list, lowest id first: each task's id, subject, status, \
owner and what it is blocked by.

Work that is free to pick up is a task that is pending, owned by nobody and \
blocked by nothing. Read one in full — its description, its metadata and its \
comments — with task_get.";

/// What the model is told `task_get` is for.
const GET_DESCRIPTION: &str = "\
One task from the team's shared list, whole: everything task_list summarizes, \
plus the description somebody picking it up works from, the metadata the team \
kept beside it, and every comment written on it.

The record reads back in the spelling it is kept in, which is not the spelling \
task_update takes: what is answered as `activeForm` is written as \
`active_form`, and what is answered as `blockedBy` is added to with \
`add_blocked_by`.";

/// What an empty list reads as. A sentence rather than nothing, so a model
/// that asked can tell an empty list from a tool that answered nothing.
const EMPTY: &str = "The team's task list is empty.";

/// What a released task is listed as, where a name would be.
///
/// `pub` because the model's listing is not the only surface that renders an
/// empty owner: `ganja-tui`'s Tasks section says the same word in the same
/// column, and a second spelling of it would let the two drift into disagreeing
/// about what an unclaimed task looks like. This is the one, and the frontend
/// imports it.
pub const UNOWNED: &str = "unowned";

/// Where a task is in its life, as the list keeps it.
///
/// Three states, because removing a task is not a fourth: the document is
/// gone. What the model may *ask for* is one wider — see the `status` argument of `task_update`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Nobody has started it. What a create makes.
    Pending,
    /// Somebody is on it now.
    InProgress,
    /// Done.
    Completed,
}

impl Status {
    /// The status as the model reads it, which is also how it is spelled on
    /// disk.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One thing somebody said about a task.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Comment {
    /// Who wrote it — a member name, which the list bound rather than read off
    /// a call.
    pub from: String,
    /// When, in the list's own spelling.
    pub at: String,
    /// What they said.
    pub text: String,
}

/// One whole task, as the model reads it back.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    /// Which task.
    pub id: String,
    /// Its imperative one-liner.
    pub subject: String,
    /// Everything somebody picking it up needs.
    pub description: String,
    /// The present-continuous form, where there is one.
    pub active_form: Option<String>,
    /// Where it is.
    pub status: Status,
    /// Who holds it, empty when nobody does.
    pub owner: String,
    /// What it holds up.
    pub blocks: Vec<String>,
    /// What holds it up.
    pub blocked_by: Vec<String>,
    /// Whatever the team keeps beside it.
    pub metadata: serde_json::Map<String, serde_json::Value>,
    /// Everything said about it, oldest first.
    pub comments: Vec<Comment>,
}

/// What a listing shows: enough to choose a task, never enough to do it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    /// Which task.
    pub id: String,
    /// Its one-liner.
    pub subject: String,
    /// Where it is.
    pub status: Status,
    /// Who holds it, empty when nobody does.
    pub owner: String,
    /// What holds it up.
    pub blocked_by: Vec<String>,
}

impl Summary {
    /// This row as a listing shows it.
    fn line(&self) -> String {
        summary_line(&self.id, &self.subject, self.status, &self.owner, &self.blocked_by)
    }
}

/// A task somebody wants filed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Draft {
    /// The imperative one-liner.
    pub subject: String,
    /// Everything somebody picking it up needs.
    pub description: String,
    /// The present-continuous form, when the call named one.
    pub active_form: Option<String>,
    /// Whatever the call wants carried beside the task.
    ///
    /// Sorted rather than as the call spelled it: this map is a `BTreeMap`,
    /// so a key filed here lands in alphabetical order among the others. Only
    /// new keys are placed — a key the document already holds keeps the
    /// position the store gave it.
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

/// What a call asks to happen to a task's owner.
///
/// Two operations rather than one assignment, because only one of them can be
/// refused: claiming is the race a whole team runs, and releasing is a lead
/// taking work back from a member that stopped.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Owner {
    /// Take the task for this member, or be refused because somebody holds it.
    Claim(String),
    /// Leave it to whoever picks it up next.
    Release,
}

/// What one `task_update` call changes.
///
/// Every field is "leave it alone" by default, which is what makes one tool
/// the whole mutation door: a call moves what it names and nothing else.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Change {
    /// Move it to another status. Never [`Status`]'s missing fourth — a
    /// removal is [`TaskList::delete`], decided before this value is built.
    pub status: Option<Status>,
    /// Reword the one-liner.
    pub subject: Option<String>,
    /// Rewrite the description.
    pub description: Option<String>,
    /// Set the present-continuous form.
    pub active_form: Option<String>,
    /// Claim it, or let it go.
    pub owner: Option<Owner>,
    /// Keys to merge, a null value removing its key.
    ///
    /// Sorted rather than as the call spelled it, exactly as [`Draft`]'s is —
    /// but a merge lands in a document that already has an order of its own,
    /// so what this sorting decides is narrower than [`Draft`]'s: a key this
    /// merge introduces lands **after every key the task already carries**,
    /// alphabetically among the keys of this call. A key the task already
    /// carries keeps the position it already had.
    pub metadata: serde_json::Map<String, serde_json::Value>,
    /// Ids to add to what this task holds up.
    ///
    /// Recorded on both tasks by whatever keeps the list, which is why an id
    /// nothing is filed under refuses the call: half an edge would leave a
    /// listing calling the other task free.
    pub add_blocks: Vec<String>,
    /// Ids to add to what holds this task up, recorded on both tasks the same
    /// way.
    pub add_blocked_by: Vec<String>,
    /// One comment to append, as its text alone: the author is the list's to
    /// stamp.
    pub add_comment: Option<String>,
}

impl Change {
    /// Whether this change moves nothing but the owner.
    ///
    /// What lets a claim that is the whole call be one write rather than two:
    /// the implementation behind the seam settles the owner first, and asks
    /// this before writing the document a second time for nothing.
    #[must_use]
    pub fn is_only_ownership(&self) -> bool {
        self.status.is_none()
            && self.subject.is_none()
            && self.description.is_none()
            && self.active_form.is_none()
            && self.metadata.is_empty()
            && self.add_blocks.is_empty()
            && self.add_blocked_by.is_empty()
            && self.add_comment.is_none()
    }

    /// Whether this change moves anything at all — the question a removal
    /// asks, since a removal may travel with nothing.
    #[must_use]
    pub fn moves_nothing(&self) -> bool {
        self.owner.is_none() && self.is_only_ownership()
    }
}

/// Why a call did not change the list, in the one sentence the model reads and
/// acts on next.
///
/// One sentence rather than a kind, for [`crate::task::NotSpawned`]'s reason:
/// every refusal is the list's — an id that is not an id, a task nobody filed,
/// a claim somebody already won, a directory that would not open — and a kind
/// enumerated here would be this crate holding half a vocabulary whose other
/// half it cannot see.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskFailure {
    /// What went wrong, as the model reads it.
    pub reason: String,
}

/// Keeps one team's shared task list on a tool call's behalf.
///
/// Deliberately says nothing about *how*: a team directory, a document per
/// task, a lock two processes share and a counter that issues ids are the
/// engine's vocabulary, and a tool that named them would be a tool the engine
/// cannot be assembled without.
///
/// # The author is bound at construction, never passed
///
/// No method here takes a `from`, and that is a mechanism rather than a
/// preference — [`crate::team::Postbox`]'s, restated for the surface that
/// records who said what. An implementation carries the caller's identity as a
/// field, set once when the list is built for a particular engine, so a
/// teammate's comments can only ever be written under that teammate's name.
///
/// [`std::fmt::Debug`] is required because [`ToolCtx`] derives it, and an
/// implementation is expected to render which list and which member it speaks
/// for rather than the machinery behind them.
#[async_trait]
pub trait TaskList: std::fmt::Debug + Send + Sync {
    /// Files `draft` and answers with the task it became.
    ///
    /// # Errors
    ///
    /// [`TaskFailure`], carrying the one sentence the model reads next.
    async fn create(&self, draft: Draft) -> Result<Record, TaskFailure>;

    /// Applies `change` to the task `id` names and answers with what it now
    /// is.
    ///
    /// # Errors
    ///
    /// [`TaskFailure`], carrying the one sentence the model reads next —
    /// including the refusal a claim earns when somebody already holds the
    /// task, which names the holder, and the one an edge earns when it names
    /// a task nothing is filed under.
    async fn update(&self, id: &str, change: Change) -> Result<Record, TaskFailure>;

    /// Removes the task `id` names, permanently.
    ///
    /// # Errors
    ///
    /// [`TaskFailure`], carrying the one sentence the model reads next.
    async fn delete(&self, id: &str) -> Result<(), TaskFailure>;

    /// Every task on the list, lowest id first.
    ///
    /// # Errors
    ///
    /// [`TaskFailure`], carrying the one sentence the model reads next.
    async fn list(&self) -> Result<Vec<Summary>, TaskFailure>;

    /// One whole task, comments and all.
    ///
    /// # Errors
    ///
    /// [`TaskFailure`], carrying the one sentence the model reads next.
    async fn get(&self, id: &str) -> Result<Record, TaskFailure>;
}

/// The status a call may ask for, which is [`Status`] plus the one that is not
/// a status at all.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
enum StatusArgument {
    /// Nobody has started it.
    Pending,
    /// Somebody is on it now.
    InProgress,
    /// Done.
    Completed,
    /// Remove the task permanently.
    Deleted,
}

/// What the model passes to `task_create`.
#[derive(Debug, Deserialize, JsonSchema)]
struct CreateArgs {
    /// The imperative one-liner this task is chosen by
    subject: String,
    /// Everything somebody picking the task up needs, readable on its own
    description: String,
    /// The present-continuous wording to show while the task is worked on
    #[serde(default)]
    active_form: Option<String>,
    /// Anything else the team needs kept beside the task
    #[serde(default)]
    metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

/// What the model passes to `task_update`.
#[derive(Debug, Deserialize, JsonSchema)]
struct UpdateArgs {
    /// The id of the task to change, as task_create or task_list reported it
    task_id: String,
    /// Where the task now is: pending, in_progress, completed, or deleted,
    /// which removes it permanently and may travel with nothing else
    #[serde(default)]
    status: Option<StatusArgument>,
    /// A new imperative one-liner
    #[serde(default)]
    subject: Option<String>,
    /// A new description
    #[serde(default)]
    description: Option<String>,
    /// A new present-continuous wording
    #[serde(default)]
    active_form: Option<String>,
    /// The member to claim the task for, or the empty string to release it
    #[serde(default)]
    owner: Option<String>,
    /// Keys to merge into the task's metadata; a null value removes its key
    #[serde(default)]
    metadata: Option<serde_json::Map<String, serde_json::Value>>,
    /// Ids of tasks this one holds up
    #[serde(default)]
    add_blocks: Option<Vec<String>>,
    /// Ids of tasks that hold this one up
    #[serde(default)]
    add_blocked_by: Option<Vec<String>>,
    /// A comment to append; it is recorded under this session's own identity
    #[serde(default)]
    add_comment: Option<String>,
}

/// What the model passes to `task_list`. Nothing: the list is the team's, and
/// there is one of it.
#[derive(Debug, Deserialize, JsonSchema)]
struct ListArgs {}

/// What the model passes to `task_get`.
#[derive(Debug, Deserialize, JsonSchema)]
struct GetArgs {
    /// The id of the task to read, as task_create or task_list reported it
    task_id: String,
}

/// Files a task on the team's shared list.
#[derive(Debug, Default)]
pub struct TaskCreateTool;

/// Changes one task on the team's shared list.
#[derive(Debug, Default)]
pub struct TaskUpdateTool;

/// Lists the team's shared task list.
#[derive(Debug, Default)]
pub struct TaskListTool {
    /// Where a clamped listing spills its whole text, when a caller named a
    /// directory rather than leaving [`budgeted`] to resolve one per call.
    ///
    /// Only a test ever sets this, through `TaskListTool::spilling_into` —
    /// gated `#[cfg(test)]`, so there is no item here to link. The seam is
    /// `shell.rs`'s (`ShellTool::spill_dir`) and exists for its reason: a test
    /// spilling into the resolved data directory would fill a real person's
    /// `~/.local/share` with fixtures, which `tests/AGENTS.md` forbids in as
    /// many words — and one that merely avoided naming a directory would pass
    /// on the pathless notice a machine with no writable candidate answers
    /// with, never proving a file was written at all. Every other build leaves
    /// it empty and the location is resolved per call.
    spill_dir: Option<PathBuf>,
}

/// Reads one whole task off the team's shared list.
#[derive(Debug, Default)]
pub struct TaskGetTool {
    /// Where a clamped record spills its whole text. See
    /// `TaskListTool::spill_dir`, which is the same seam on the other reading
    /// door.
    spill_dir: Option<PathBuf>,
}

impl TaskListTool {
    /// Spills into `dir` rather than the resolved data directory. See
    /// `TaskListTool::spill_dir`.
    #[cfg(test)]
    fn spilling_into(dir: &Path) -> Self {
        Self { spill_dir: Some(dir.to_owned()) }
    }
}

impl TaskGetTool {
    /// Spills into `dir` rather than the resolved data directory. See
    /// `TaskListTool::spill_dir`.
    #[cfg(test)]
    fn spilling_into(dir: &Path) -> Self {
        Self { spill_dir: Some(dir.to_owned()) }
    }
}

/// The four, as one registration.
///
/// A function rather than four constructors at each call site: every surface
/// that offers one of these offers all four — a list you may write but not
/// read would be a list nobody can work from — so the set is spelled once and
/// a fifth tool joins it in one place.
#[must_use]
pub fn tools() -> Vec<std::sync::Arc<dyn Tool>> {
    vec![
        std::sync::Arc::new(TaskCreateTool),
        std::sync::Arc::new(TaskUpdateTool),
        std::sync::Arc::new(TaskListTool::default()),
        std::sync::Arc::new(TaskGetTool::default()),
    ]
}

/// The list this call runs against, or the sentence a build with none answers.
fn list_of(ctx: &ToolCtx) -> Result<&dyn TaskList, ToolError> {
    ctx.tasks.as_deref().ok_or_else(|| ToolError::Failed(NO_LIST.to_owned()))
}

/// A refusal from beyond the seam, as the failed call the model reads next.
///
/// A refusal is **information**, not control flow: it comes back as a failed
/// call whose sentence the model acts on, exactly as an unknown agent type
/// does in [`crate::task`]. Nothing here ends a turn.
fn refused(failure: TaskFailure) -> ToolError {
    ToolError::Failed(failure.reason)
}

/// A call titled by the argument that says which task it is about, or by the
/// tool alone where the arguments carried none.
///
/// The title is what a transcript row and a permission dialog are headed with,
/// so a call that arrived without that argument must not read as the tool's
/// name and a trailing space. **Absent and empty are the same case** — a
/// schema a model fills field by field produces `""` as readily as it omits
/// the key, and both leave nothing to title the row with — which is why this
/// asks what the argument amounts to rather than whether it is there.
///
/// `names` because the two tools that title this way do not name a task with
/// the same key: `task_create` has no id to give and titles on its subject,
/// which is the one thing it does have.
///
/// Cut at [`crate::shell::DESCRIBE_LIMIT`], through the same
/// [`crate::shell::shorten`] a long shell command is echoed by: a subject is
/// free text another member wrote and is under no length bound anywhere, so a
/// subject the size of a document would otherwise be a transcript row and a
/// permission heading the size of a document.
fn describing(tool: &str, args: &serde_json::Value, names: &str) -> String {
    match args.get(names).and_then(serde_json::Value::as_str).map(str::trim) {
        Some(named) if !named.is_empty() => {
            format!("{tool} {}", crate::shell::shorten(named, crate::shell::DESCRIBE_LIMIT))
        }
        _ => tool.to_owned(),
    }
}

/// One task as a listing shows it: what it is, where it is, who has it, and
/// what it is waiting on.
///
/// The fields rather than a [`Summary`], so a whole [`Record`] renders as one
/// too without being copied into one first.
///
/// # The subject and the owner are somebody else's words
///
/// **One task to a line, and a line is what a reader counts tasks by** — so a
/// subject carrying a newline would not be a subject with a newline in it, it
/// would be two rows, the second of them a task nobody filed, written by
/// whichever member typed it. That is the shared list's own shape used against
/// the members reading it, and it is answered exactly where
/// [`crate::list_sessions`] answers it on the registry's self-written names:
/// [`neutralize`] drops the control characters and the two brackets that could
/// pass for structure, and caps the result. The id, the status and the blocker
/// ids are the store's own and are not run through it — they are digits and a
/// closed vocabulary, not anything a member typed.
fn summary_line(
    id: &str,
    subject: &str,
    status: Status,
    owner: &str,
    blocked_by: &[String],
) -> String {
    let owner =
        if owner.is_empty() { UNOWNED.to_owned() } else { format!("owner {}", neutralize(owner)) };
    let blocked = if blocked_by.is_empty() {
        String::new()
    } else {
        format!(", blocked by {}", blocked_by.join(", "))
    };

    format!("{id} [{status}] {owner}{blocked} — {}", neutralize(subject))
}

/// A whole record as its own summary, for the one-line answer an update reads
/// back as.
fn line_of(record: &Record) -> String {
    summary_line(&record.id, &record.subject, record.status, &record.owner, &record.blocked_by)
}

/// A record as the structured extra the answer carries beside its text: what
/// the transcript part persists, and what a `PostToolUse` hook is handed.
///
/// It is deliberately **not** what [`budgeted`] clamps — a hook reading a task
/// it was told about wants the task, and neither reader spends a context
/// window on it.
fn carried(record: &Record) -> serde_json::Value {
    serde_json::json!({ "task": record })
}

/// A reading answer, cut to what a tool result may carry.
///
/// Nothing between these tools and the documents bounds a description or a
/// comment thread — the store's only bound is on a whole document, and it is
/// a megabyte — so a single `task_get` could otherwise spend a whole context
/// window, and a listing grows one row per task the team ever filed. This is
/// the budget [`crate::read`], [`crate::websearch`] and [`crate::webfetch`]
/// answer through and for their reason, spill file included: the model is told
/// what it did not see and where the rest of it went.
///
/// Only the two *reading* doors need it. What a create or an update answers
/// with is one summary line it built itself.
///
/// `spill` is where the overflow file goes when the caller named a directory,
/// and is [`None`] in every shipped build — the shape `shell.rs`'s
/// `open_spill` takes, for its reason (`TaskListTool::spill_dir`).
fn budgeted(output: &str, spill: Option<&Path>) -> String {
    match spill {
        Some(dir) => truncate::clamp_with(output, dir).text,
        None => truncate::clamp(output).text,
    }
}

#[async_trait]
impl Tool for TaskCreateTool {
    fn id(&self) -> &str {
        CREATE_ID
    }

    fn description(&self) -> &str {
        CREATE_DESCRIPTION
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(CreateArgs)
    }

    fn describe(&self, args: &serde_json::Value) -> String {
        describing(CREATE_ID, args, "subject")
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let tasks = list_of(ctx)?;
        let args: CreateArgs = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;

        let record = tasks
            .create(Draft {
                subject: args.subject,
                description: args.description,
                active_form: args.active_form,
                metadata: args.metadata.unwrap_or_default(),
            })
            .await
            .map_err(refused)?;

        Ok(ToolOutput {
            title: format!("task {} filed", record.id),
            output: format!("Filed as task {}. {}", record.id, line_of(&record)),
            metadata: carried(&record),
        })
    }
}

#[async_trait]
impl Tool for TaskUpdateTool {
    fn id(&self) -> &str {
        UPDATE_ID
    }

    fn description(&self) -> &str {
        UPDATE_DESCRIPTION
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(UpdateArgs)
    }

    fn describe(&self, args: &serde_json::Value) -> String {
        describing(UPDATE_ID, args, "task_id")
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let tasks = list_of(ctx)?;
        let args: UpdateArgs = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;

        let id = args.task_id.clone();
        let removing = args.status == Some(StatusArgument::Deleted);
        let change = change_of(args);
        if removing {
            // The removal is the whole call or it is refused: see
            // [`DELETE_WITH_CHANGES`].
            if !change.moves_nothing() {
                return Err(ToolError::Failed(DELETE_WITH_CHANGES.to_owned()));
            }
            tasks.delete(&id).await.map_err(refused)?;

            return Ok(ToolOutput {
                title: format!("task {id} removed"),
                output: format!(
                    "Task {id} is off the list for good, and its id will not be issued again \
                     while this team's counter stands."
                ),
                metadata: serde_json::json!({ "task_id": id, "deleted": true }),
            });
        }

        let record = tasks.update(&id, change).await.map_err(refused)?;

        Ok(ToolOutput {
            title: format!("task {} updated", record.id),
            output: line_of(&record),
            metadata: carried(&record),
        })
    }
}

/// Everything a `task_update` call asks for except which task, and except the
/// removal that is decided before this runs.
///
/// The owner mapping lives here rather than beyond the seam because it is
/// about what the model asked for: a name is a claim, and nothing — the empty
/// string, or whitespace that is no name — is a release.
fn change_of(args: UpdateArgs) -> Change {
    Change {
        status: match args.status {
            Some(StatusArgument::Pending) => Some(Status::Pending),
            Some(StatusArgument::InProgress) => Some(Status::InProgress),
            Some(StatusArgument::Completed) => Some(Status::Completed),
            Some(StatusArgument::Deleted) | None => None,
        },
        subject: args.subject,
        description: args.description,
        active_form: args.active_form,
        // Trimmed at the claim, because the trim already decided the branch: an
        // owner that is nothing but whitespace is a release, so an owner with
        // whitespace around it is that member and not a second one that merely
        // renders like them. A padded name stored as it arrived would be a
        // holder no later claim or release could name back.
        owner: args.owner.map(|owner| {
            let owner = owner.trim();

            if owner.is_empty() { Owner::Release } else { Owner::Claim(owner.to_owned()) }
        }),
        metadata: args.metadata.unwrap_or_default(),
        // An explicit `null` is the same as the argument being absent, the
        // shape `metadata` already has: a model filling every field of a
        // schema it was shown nulls the ones it is not using.
        add_blocks: args.add_blocks.unwrap_or_default(),
        add_blocked_by: args.add_blocked_by.unwrap_or_default(),
        add_comment: args.add_comment,
    }
}

#[async_trait]
impl Tool for TaskListTool {
    fn id(&self) -> &str {
        LIST_ID
    }

    fn description(&self) -> &str {
        LIST_DESCRIPTION
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(ListArgs)
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let tasks = list_of(ctx)?;
        // Parsed rather than ignored so that a payload which is not an
        // arguments object at all is refused here. An invented key is dropped
        // in silence, which is what every other tool in this crate does with
        // one.
        let _: ListArgs = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;

        let summaries = tasks.list().await.map_err(refused)?;
        let output = if summaries.is_empty() {
            EMPTY.to_owned()
        } else {
            summaries.iter().map(Summary::line).collect::<Vec<_>>().join("\n")
        };

        Ok(ToolOutput {
            title: title_of(summaries.len()),
            output: budgeted(&output, self.spill_dir.as_deref()),
            metadata: serde_json::json!({ "tasks": summaries }),
        })
    }
}

/// How a listing is titled: the work it found.
fn title_of(count: usize) -> String {
    match count {
        0 => "no tasks".to_owned(),
        1 => "1 task".to_owned(),
        many => format!("{many} tasks"),
    }
}

#[async_trait]
impl Tool for TaskGetTool {
    fn id(&self) -> &str {
        GET_ID
    }

    fn description(&self) -> &str {
        GET_DESCRIPTION
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(GetArgs)
    }

    fn describe(&self, args: &serde_json::Value) -> String {
        describing(GET_ID, args, "task_id")
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let tasks = list_of(ctx)?;
        let args: GetArgs = serde_json::from_value(args)
            .map_err(|error| ToolError::InvalidArgs(error.to_string()))?;

        let record = tasks.get(&args.task_id).await.map_err(refused)?;
        // Whole, as JSON: what a listing leaves out is the half somebody
        // picking the task up works from, and a rendering that summarized it
        // again would be the second listing nobody asked for.
        let output =
            serde_json::to_string_pretty(&record).expect("a task record is JSON by construction");

        Ok(ToolOutput {
            title: format!("task {}", record.id),
            output: budgeted(&output, self.spill_dir.as_deref()),
            metadata: carried(&record),
        })
    }
}

#[cfg(test)]
#[path = "tasklist_tests.rs"]
mod tests;
