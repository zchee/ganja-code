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

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Tool, ToolCtx, ToolError, ToolOutput};

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
never reused. Give it an owner, wire what it waits on, or move it along with \
task_update.";

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

`owner` claims or releases it. A non-empty owner claims the task for that \
member and is refused when somebody already holds it, which is what makes \
claiming safe to race: two teammates reaching for one task produce one owner \
and one refusal, never two members doing one piece of work. The empty string \
releases a task, which is how work is taken back from a member that has \
stopped. Reassigning is therefore two calls — release it, then claim it for \
whoever takes it next.

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
kept beside it, and every comment written on it.";

/// What an empty list reads as. A sentence rather than nothing, so a model
/// that asked can tell an empty list from a tool that answered nothing.
const EMPTY: &str = "The team's task list is empty.";

/// What a released task is listed as, where a name would be.
const UNOWNED: &str = "unowned";

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
    /// Sorted rather than as the call spelled it, exactly as [`Draft`]'s is:
    /// a key this merge introduces lands alphabetically, and one the task
    /// already carries keeps where it already is.
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
pub struct TaskListTool;

/// Reads one whole task off the team's shared list.
#[derive(Debug, Default)]
pub struct TaskGetTool;

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
        std::sync::Arc::new(TaskListTool),
        std::sync::Arc::new(TaskGetTool),
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

/// A call titled by the task it names, or by the tool alone where the
/// arguments named none.
///
/// The title is what a transcript row and a permission dialog are headed
/// with, so a call that arrived without a `task_id` must not read as the tool's
/// name and a trailing space.
fn describing(tool: &str, args: &serde_json::Value) -> String {
    match args.get("task_id").and_then(serde_json::Value::as_str) {
        Some(id) => format!("{tool} {id}"),
        None => tool.to_owned(),
    }
}

/// One task as a listing shows it: what it is, where it is, who has it, and
/// what it is waiting on.
///
/// The fields rather than a [`Summary`], so a whole [`Record`] renders as one
/// too without being copied into one first.
fn summary_line(
    id: &str,
    subject: &str,
    status: Status,
    owner: &str,
    blocked_by: &[String],
) -> String {
    let owner = if owner.is_empty() { UNOWNED.to_owned() } else { format!("owner {owner}") };
    let blocked = if blocked_by.is_empty() {
        String::new()
    } else {
        format!(", blocked by {}", blocked_by.join(", "))
    };

    format!("{id} [{status}] {owner}{blocked} — {subject}")
}

/// A whole record as its own summary, for the one-line answer an update reads
/// back as.
fn line_of(record: &Record) -> String {
    summary_line(&record.id, &record.subject, record.status, &record.owner, &record.blocked_by)
}

/// A record as the structured extra a frontend may render richer than text.
fn carried(record: &Record) -> serde_json::Value {
    serde_json::json!({ "task": record })
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
        let subject = args.get("subject").and_then(serde_json::Value::as_str).unwrap_or_default();

        format!("{CREATE_ID} {subject}")
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
        describing(UPDATE_ID, args)
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
                    "Task {id} is off the list for good, and its id will not be issued again."
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
        owner: args.owner.map(|owner| {
            if owner.trim().is_empty() { Owner::Release } else { Owner::Claim(owner) }
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
            output,
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
        describing(GET_ID, args)
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

        Ok(ToolOutput { title: format!("task {}", record.id), output, metadata: carried(&record) })
    }
}

#[cfg(test)]
#[path = "tasklist_tests.rs"]
mod tests;
