//! The shared list a team coordinates through: one document per task, and the
//! counter that issues their ids.
//!
//! **Neither upstream has a counterpart to port.** opencode has no teams, no
//! teammates and therefore nothing for two agents to divide between them.
//! Claude Code has a task list, but it keeps it inside a `claude` process
//! rather than in the teams directory — which is why a `claude` teammate
//! drives its own list and cannot see this one. What is taken from it is the
//! *semantics* a model is already trained on: the status vocabulary, an owner
//! that is empty until somebody claims it, a free metadata map, and comments
//! that only ever grow. Every byte of the format below is ganja's own.
//!
//! # Why a document per task
//!
//! Because claiming is the operation that matters. A single `tasks.json`
//! holding an array would put every create, every status change and every
//! claim behind one lock, so two teammates picking up two *different* tasks
//! would queue behind each other — and a claim would rewrite the whole list to
//! change one field. A document per task makes a claim a hold on the one
//! document being contended, and makes a create touch nothing that already
//! exists. The cost is that [`Store::list`] reads a directory instead of a
//! file, which is the cheaper half of the trade at any size a team's list
//! reaches.
//!
//! # Why a counter document
//!
//! Ids are sequential strings — `"1"`, `"2"`, … — because that is the shape a
//! model expects, and a directory listing cannot issue the next one without a
//! race: two creates that both read "the highest is 3" both write `4.json` and
//! one of them silently loses its task. So the next id comes from
//! [`COUNTER`], read-and-bumped under its own hold, and **an id is never
//! reused**: deleting task 3 leaves a gap where 3 was, and the next create
//! still gets 4. A counter that has gone missing is rebuilt from the highest
//! id on disk rather than restarted at 1, so losing the file costs a gap
//! rather than a collision.
//!
//! # What is somebody else's, and what is not
//!
//! These documents live under a team directory a real `claude` process may be
//! sharing (D-1), in a `tasks/` subdirectory of ganja's own — the plan's
//! decision 6. That is deliberately the *only* liberty taken with a shared
//! directory: a ganja-only subdirectory beside Claude's documents, never a
//! ganja-only key inside one of them. The passthrough posture the rest of this
//! crate uses is kept all the same — every shape here carries a
//! `#[serde(flatten)] extra`, so a key written by a build this one has never
//! met survives a rewrite in the position it arrived in.
//!
//! The lock is [`crate::lock`]'s, unchanged: the same `mkdir` protocol, the
//! same mtime staleness, the same ladder. A task document takes
//! [`lock::acquire_unseeded`] rather than [`lock::acquire`] for the reason the
//! team file does — there is no empty state to seed a task document with, and
//! a create is a write to a path that is not there yet.
//!
//! # Content and addressing
//!
//! An id, a status and an owner are addressing; a subject is the label every
//! listing renders. A description, a comment's text and whatever a model put
//! in `metadata` are **content**, and this module treats them the way the rest
//! of the crate treats a message body: never in a log line, and rendered as a
//! size or a key set by the [`Debug`] implementations below.

use std::path::{Path, PathBuf};
use std::{fmt, fs, io};

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::Value;

use crate::lock::{self, LockError};
use crate::mailbox::write_atomically;
use crate::record::{Redacted, document, shadowed};

/// The subdirectory a team's task documents live in (`<team dir>/tasks`).
///
/// Spelled here rather than only in [`crate::TeamsRoot::tasks_dir`] so a
/// caller holding a bare path can name the same directory without restating
/// the string.
pub const TASKS_DIR: &str = "tasks";

/// The document that issues ids, beside the tasks it issues them for.
///
/// Deliberately extension-less: it is not a task, and a `*.json` name would
/// put it in the way of every listing that walks the directory looking for
/// them.
pub const COUNTER: &str = "counter";

/// What a task document's name ends with.
const DOCUMENT_SUFFIX: &str = ".json";

/// The largest id the grammar admits — nineteen digits, which is every value a
/// [`u64`] counter can reach without growing a twentieth.
const ID_MAX: u64 = 9_999_999_999_999_999_999;

/// Why an id was refused.
pub const REFUSED_ID_SHAPE: &str =
    "a task id is 1 to 19 decimal digits, starts at 1 and carries no leading zero";

/// Why an operation found nothing to act on.
pub const REFUSED_NO_SUCH_TASK: &str = "no task is filed under this id";

/// Why a claim was refused.
pub const REFUSED_ALREADY_OWNED: &str = "this task is already claimed";

/// Why a create could not be given an id.
pub const REFUSED_COUNTER_EXHAUSTED: &str = "every id the grammar admits has been issued";

/// Why a write was refused before it touched the directory.
pub const REFUSED_SCHEMA: &str = "a task document does not match the task schema";

/// Why a document was left out of a listing, when it would not decode.
///
/// Deliberately says nothing more, and the reason is
/// [`crate::mailbox`]'s `DROPPED_UNDECODABLE`: a decoder's own message can
/// quote the value it choked on, and what a task says is content.
pub const DROPPED_UNDECODABLE: &str = "the document is not a task this build can read";

/// Why a document was left out of a listing, when it could not be read at all.
pub const DROPPED_UNREADABLE: &str = "the document could not be read";

/// A refusal or a failure on the way to a task document.
///
/// Every variant names an **id or a path** and nothing else. What a task
/// *says* — its description, a comment, a metadata value — is content, and
/// stays out of errors for the reason it stays out of logs.
#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    /// The id is not [`REFUSED_ID_SHAPE`]'s grammar.
    #[error("{REFUSED_ID_SHAPE}: {id:?}")]
    Shape {
        /// What was offered as an id.
        id: String,
    },
    /// Nothing is filed under this id — never created, or deleted since.
    #[error("{REFUSED_NO_SUCH_TASK}: {id}")]
    NoSuchTask {
        /// The id nothing answered to.
        id: TaskId,
    },
    /// A claim arrived at a task somebody already owns.
    ///
    /// Carries the owner, because the loser of a race needs to know who won it
    /// — that is the whole answer a refused claim is asking for.
    #[error("{REFUSED_ALREADY_OWNED}: {id} belongs to {owner:?}")]
    AlreadyOwned {
        /// The task that was already claimed.
        id: TaskId,
        /// Who holds it.
        owner: String,
    },
    /// The counter reached the last id the grammar admits.
    ///
    /// Unreachable in practice — a team would have to create ten quintillion
    /// tasks — and returned rather than wrapped, because reusing an id is the
    /// one thing this module promises not to do.
    #[error("{REFUSED_COUNTER_EXHAUSTED}")]
    CounterExhausted,
    /// A passthrough map carries a key the shape itself declares.
    #[error("{REFUSED_SCHEMA}: {}", issues.join("; "))]
    SchemaInvalid {
        /// One sentence per offending key.
        issues: Vec<String>,
    },
    /// The lock could not be taken.
    #[error(transparent)]
    Lock(#[from] LockError),
    /// The filesystem refused.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// A document would not encode, or would not decode.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Every key a [`Task`] emits, and so the ones its passthrough map may not
/// carry.
///
/// Hand-written beside the struct and tied to it by
/// `the_task_key_list_is_exactly_what_a_task_serializes`, which is what keeps
/// a field added later from being governed by neither.
const TASK_KEYS: [&str; 10] = [
    "id",
    "subject",
    "description",
    "activeForm",
    "status",
    "owner",
    "blocks",
    "blockedBy",
    "metadata",
    "comments",
];

/// Every key a [`Comment`] emits.
const COMMENT_KEYS: [&str; 3] = ["from", "at", "text"];

/// A task's id: a decimal number, spelled as a string on disk.
///
/// A number rather than a validated string, and that buys the property the
/// name grammar in [`crate::team`] has to work for: **an id cannot spell a
/// path component that escapes anything.** There is no `.`, no `/` and no
/// leading dash to be read as a flag in a decimal integer, so joining one onto
/// the tasks directory is safe by construction rather than by a check
/// somewhere below.
///
/// The ordering is numeric, which is the whole reason it is not a string: a
/// listing sorted as text puts `"10"` before `"9"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId(u64);

impl TaskId {
    /// Accepts an id, or refuses it.
    ///
    /// # Errors
    ///
    /// [`TaskError::Shape`] when the text is not [`REFUSED_ID_SHAPE`]'s
    /// grammar. A leading zero is refused rather than trimmed, so one task has
    /// exactly one spelling and `01.json` can never sit beside `1.json`.
    pub fn parse(id: &str) -> Result<Self, TaskError> {
        let refuse = || TaskError::Shape { id: id.to_owned() };
        if id.is_empty() || id.len() > ID_MAX.to_string().len() {
            return Err(refuse());
        }
        let mut digits = id.chars();
        match digits.next() {
            Some(first) if first.is_ascii_digit() && first != '0' => {}
            _ => return Err(refuse()),
        }
        if !digits.all(|digit| digit.is_ascii_digit()) {
            return Err(refuse());
        }

        id.parse().map(Self).map_err(|_| refuse())
    }

    /// The id as the number it is, for whoever needs to compare or count.
    #[must_use]
    pub fn number(self) -> u64 {
        self.0
    }

    /// The document this id is filed in, under `dir`.
    fn document_in(self, dir: &Path) -> PathBuf {
        dir.join(format!("{}{DOCUMENT_SUFFIX}", self.0))
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// A string on the wire, because that is what a task id is everywhere a model
/// meets one.
impl Serialize for TaskId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

/// The same grammar on the way in, so a document holding an id this build
/// would never issue is a document this build declines to read rather than one
/// it half-understands. The file's own name is what a store addresses by, so a
/// document that fails here is reported and skipped rather than fatal — see
/// [`Store::list`].
impl<'de> Deserialize<'de> for TaskId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;

        Self::parse(&text).map_err(de::Error::custom)
    }
}

/// Where a task is in its life.
///
/// Three states rather than four: **deleting is not a status**, it is
/// [`Store::delete`] removing the document. A tombstone would be a row every
/// listing has to filter and every count has to remember to exclude, for a
/// list that is regenerated per team task rather than audited.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Nobody has started it. What a create makes.
    #[default]
    Pending,
    /// Somebody is on it now.
    InProgress,
    /// Done.
    Completed,
}

impl TaskStatus {
    /// The status as it is spelled on disk and to a model.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One thing somebody said about a task.
///
/// Append-only by construction: [`Update`] can add one and nothing anywhere
/// can edit or remove one, because the value of a comment thread is that it is
/// what was actually said.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Comment {
    /// Who wrote it — a member name, an address rather than content.
    pub from: String,
    /// When, in [`crate::record::now_iso8601`]'s spelling.
    pub at: String,
    /// What they said.
    pub text: String,
    /// Everything this build has never heard of, kept in position.
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

impl Comment {
    /// A comment ready to be appended.
    ///
    /// The timestamp is handed in rather than read off the clock, for the
    /// reason [`crate::MailboxMessage::new`]'s is: a value a test can pin is a
    /// value a test can assert on. [`crate::record::now_iso8601`] is the clock
    /// for callers that want it.
    #[must_use]
    pub fn new(from: impl Into<String>, text: impl Into<String>, at: impl Into<String>) -> Self {
        Self { from: from.into(), at: at.into(), text: text.into(), extra: IndexMap::new() }
    }
}

/// Renders everything except what was said, which is rendered as its size.
impl fmt::Debug for Comment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Comment")
            .field("from", &self.from)
            .field("at", &self.at)
            .field("text", &Redacted(Some(&self.text)))
            .field("extra", &Keys(&self.extra))
            .finish()
    }
}

/// A task at rest.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    /// Which task this is.
    pub id: TaskId,
    /// The imperative one-liner a listing shows.
    pub subject: String,
    /// Everything somebody picking this up needs, without asking.
    #[serde(default)]
    pub description: String,
    /// The present-continuous form a status line renders while it runs
    /// ("wiring the parser"), when the caller supplied one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
    /// Where it is in its life.
    #[serde(default)]
    pub status: TaskStatus,
    /// Who holds it. **Empty means unowned**, which is what makes a claim a
    /// test on one field rather than on the presence of a key.
    #[serde(default)]
    pub owner: String,
    /// Tasks this one holds up.
    #[serde(default)]
    pub blocks: Vec<TaskId>,
    /// Tasks that hold this one up.
    #[serde(default)]
    pub blocked_by: Vec<TaskId>,
    /// A free map for whatever a team needs to carry that the schema does not
    /// name. Merged rather than replaced on update, and a null deletes a key.
    #[serde(default)]
    pub metadata: IndexMap<String, Value>,
    /// What has been said about it, oldest first.
    #[serde(default)]
    pub comments: Vec<Comment>,
    /// Everything this build has never heard of, kept in position.
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

impl Task {
    /// What a listing shows about this task.
    #[must_use]
    pub fn summary(&self) -> TaskSummary {
        TaskSummary {
            id: self.id,
            subject: self.subject.clone(),
            status: self.status,
            owner: self.owner.clone(),
            blocked_by: self.blocked_by.clone(),
        }
    }

    /// Whether anybody holds it.
    #[must_use]
    pub fn is_owned(&self) -> bool {
        !self.owner.is_empty()
    }
}

/// Renders the addressing, and the content as sizes and key sets.
impl fmt::Debug for Task {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Task")
            .field("id", &self.id)
            .field("subject", &self.subject)
            .field("description", &Redacted(Some(&self.description)))
            .field("active_form", &self.active_form)
            .field("status", &self.status)
            .field("owner", &self.owner)
            .field("blocks", &self.blocks)
            .field("blocked_by", &self.blocked_by)
            .field("metadata", &Keys(&self.metadata))
            .field("comments", &self.comments)
            .field("extra", &Keys(&self.extra))
            .finish()
    }
}

/// What a listing shows: enough to choose a task, never enough to do it.
#[derive(Clone, Debug, PartialEq)]
pub struct TaskSummary {
    /// Which task.
    pub id: TaskId,
    /// Its one-liner.
    pub subject: String,
    /// Where it is.
    pub status: TaskStatus,
    /// Who holds it, empty when nobody does.
    pub owner: String,
    /// What holds it up.
    pub blocked_by: Vec<TaskId>,
}

/// A task somebody wants created.
///
/// A struct rather than six arguments, because the shape grows: `blockedBy` is
/// wired by [`Update`] today and would be a seventh parameter the day a caller
/// wants it at creation.
#[derive(Clone, Default, PartialEq)]
pub struct NewTask {
    /// The imperative one-liner.
    pub subject: String,
    /// Everything somebody picking it up needs.
    pub description: String,
    /// The present-continuous form, when there is one.
    pub active_form: Option<String>,
    /// Whatever the team needs carried.
    pub metadata: IndexMap<String, Value>,
}

impl NewTask {
    /// A pending, unowned, unblocked task with nothing said about it yet.
    #[must_use]
    pub fn new(subject: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            description: description.into(),
            active_form: None,
            metadata: IndexMap::new(),
        }
    }
}

/// Renders the subject, and the rest as sizes and key sets.
impl fmt::Debug for NewTask {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NewTask")
            .field("subject", &self.subject)
            .field("description", &Redacted(Some(&self.description)))
            .field("active_form", &self.active_form)
            .field("metadata", &Keys(&self.metadata))
            .finish()
    }
}

/// What one [`Store::update`] changes.
///
/// Every field is "leave it alone" by default, which is what makes this one
/// door rather than eight: a caller sets what it means to change and nothing
/// else moves. The two list fields and the comment are **add-only** — there is
/// no door here that removes a blocker or edits what somebody said.
#[derive(Clone, Default, PartialEq)]
pub struct Update {
    /// Move it to another status.
    pub status: Option<TaskStatus>,
    /// Reword the one-liner.
    pub subject: Option<String>,
    /// Rewrite the description.
    pub description: Option<String>,
    /// Set the present-continuous form.
    pub active_form: Option<String>,
    /// Set or clear the owner. An empty string releases the task, which is the
    /// door a lead reassigning a dead member's work goes through — [`claim`]
    /// is the door that refuses.
    ///
    /// [`claim`]: Store::claim
    pub owner: Option<String>,
    /// Keys to merge into the metadata map. A [`Value::Null`] **deletes** its
    /// key; anything else sets it, keeping the position an existing key
    /// already had. An empty map changes nothing.
    pub metadata: IndexMap<String, Value>,
    /// Ids to add to `blocks`, skipping any already there.
    pub add_blocks: Vec<TaskId>,
    /// Ids to add to `blockedBy`, skipping any already there.
    pub add_blocked_by: Vec<TaskId>,
    /// One comment to append.
    pub add_comment: Option<Comment>,
}

/// Renders the structure, and the content as sizes and key sets.
impl fmt::Debug for Update {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Update")
            .field("status", &self.status)
            .field("subject", &self.subject)
            .field("description", &Redacted(self.description.as_deref()))
            .field("active_form", &self.active_form)
            .field("owner", &self.owner)
            .field("metadata", &Keys(&self.metadata))
            .field("add_blocks", &self.add_blocks)
            .field("add_blocked_by", &self.add_blocked_by)
            .field("add_comment", &self.add_comment)
            .finish()
    }
}

/// A map rendered as the keys it holds.
///
/// A metadata value is whatever a model put there, so it is content by the
/// same argument a description is; the key set is structure, and structure is
/// what makes a `{:?}` worth having.
struct Keys<'a>(&'a IndexMap<String, Value>);

impl fmt::Debug for Keys<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_set().entries(self.0.keys()).finish()
    }
}

/// One team's task list, as the directory it lives in.
///
/// The directory is a value somebody handed over, exactly as
/// [`TeamsRoot`](crate::TeamsRoot) is: nothing here reads an environment
/// variable or asks a config where a home is. `Store::new(root.tasks_dir(&
/// team))` is the whole of how a caller with a root gets one.
///
/// **Synchronous, like the rest of the crate.** Whoever calls it from inside a
/// turn wraps it in `spawn_blocking`.
///
/// ```
/// use ganja_team::task::{NewTask, Store, TaskStatus, Update};
/// use ganja_team::{TeamName, TeamsRoot};
///
/// let home = tempfile::tempdir()?;
/// let root = TeamsRoot::new(home.path().join("teams"));
/// let team = TeamName::parse("session-224cbeab")?;
/// let tasks = Store::new(root.tasks_dir(&team));
///
/// let task = tasks.create(NewTask::new("port the parser", "start from the spec"))?;
/// assert_eq!(task.id.to_string(), "1");
/// assert!(!task.is_owned());
///
/// // A claim is what a teammate does before it starts, and only one can.
/// tasks.claim(&task.id, "worker-1")?;
/// assert!(tasks.claim(&task.id, "worker-2").is_err());
///
/// let done = tasks
///     .update(&task.id, Update { status: Some(TaskStatus::Completed), ..Update::default() })?;
/// assert_eq!(done.status, TaskStatus::Completed);
/// assert_eq!(done.owner, "worker-1");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Store {
    /// The `tasks/` directory itself.
    dir: PathBuf,
}

impl Store {
    /// The list kept in `dir`, which is made on the first create.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory this store is.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where a task's document is, whether or not it is there.
    #[must_use]
    pub fn path_of(&self, id: &TaskId) -> PathBuf {
        id.document_in(&self.dir)
    }

    /// Where the id counter is.
    #[must_use]
    pub fn counter_path(&self) -> PathBuf {
        self.dir.join(COUNTER)
    }

    /// Files a new task and answers with it.
    ///
    /// Two holds, taken one after the other and never nested: the counter's,
    /// long enough to issue an id, and then the new document's. Nesting them
    /// would make the counter the lock every create queues on for as long as a
    /// document write takes, for no property — the id is already unique when
    /// the counter's hold releases, so nobody else can be writing that
    /// document.
    ///
    /// # Errors
    ///
    /// [`TaskError::CounterExhausted`] at the end of the id space,
    /// [`TaskError::SchemaInvalid`] when the draft's metadata would shadow a
    /// declared key, and whatever the locks or the filesystem returned.
    pub fn create(&self, draft: NewTask) -> Result<Task, TaskError> {
        fs::create_dir_all(&self.dir)?;
        let id = self.issue_id()?;
        let task = Task {
            id,
            subject: draft.subject,
            description: draft.description,
            active_form: draft.active_form,
            status: TaskStatus::Pending,
            owner: String::new(),
            blocks: Vec::new(),
            blocked_by: Vec::new(),
            metadata: draft.metadata,
            comments: Vec::new(),
            extra: IndexMap::new(),
        };

        let path = self.path_of(&id);
        let _hold = lock::acquire_unseeded(&path)?;
        write(&path, &task)?;
        tracing::debug!(tasks = %self.dir.display(), %id, "a task joined the list");

        Ok(task)
    }

    /// One whole task, comments and all.
    ///
    /// Takes no lock, and does not need one: every write here lands through a
    /// rename, so a reader sees one whole document or the one before it.
    ///
    /// # Errors
    ///
    /// [`TaskError::NoSuchTask`] when nothing is filed under the id, and
    /// whatever reading or decoding the document returned.
    pub fn get(&self, id: &TaskId) -> Result<Task, TaskError> {
        read(&self.path_of(id))?.ok_or(TaskError::NoSuchTask { id: *id })
    }

    /// Every task in the list, lowest id first.
    ///
    /// A document that will not read is **dropped and reported**, never fatal:
    /// one damaged file must not take a team's whole list with it. The report
    /// is a log line naming the directory, the id and which *kind* of failure
    /// it was — never the decoder's own sentence, which can quote the value it
    /// choked on.
    ///
    /// # Errors
    ///
    /// Whatever reading the directory returned. A directory that is not there
    /// yet is an empty list rather than an error: a team that has created no
    /// task is exactly that case.
    pub fn list(&self) -> Result<Vec<TaskSummary>, TaskError> {
        let mut summaries: Vec<TaskSummary> = self
            .ids()?
            .into_iter()
            .filter_map(|id| match read(&self.path_of(&id)) {
                Ok(Some(task)) => Some(task.summary()),
                // Deleted between the listing and the read, which is a race
                // with a winner and no loser.
                Ok(None) => None,
                Err(error) => {
                    tracing::warn!(
                        tasks = %self.dir.display(),
                        %id,
                        reason = dropped(&error),
                        "a task document would not read and was left out of the list",
                    );

                    None
                }
            })
            .collect();
        summaries.sort_by_key(|summary| summary.id);

        Ok(summaries)
    }

    /// Applies `update` to one task and answers with what it now is.
    ///
    /// The whole read-modify-write happens under the task's own hold, so two
    /// updates to one task cannot interleave into a document holding half of
    /// each.
    ///
    /// # Errors
    ///
    /// [`TaskError::NoSuchTask`] when nothing is filed under the id,
    /// [`TaskError::SchemaInvalid`] when the merge would shadow a declared
    /// key, and whatever the lock or the filesystem returned.
    pub fn update(&self, id: &TaskId, update: Update) -> Result<Task, TaskError> {
        let path = self.path_of(id);
        let _hold = self.hold(&path, id)?;

        let mut task = read(&path)?.ok_or(TaskError::NoSuchTask { id: *id })?;
        apply(&mut task, update);
        write(&path, &task)?;
        tracing::debug!(
            tasks = %self.dir.display(),
            %id,
            status = task.status.as_str(),
            "a task changed",
        );

        Ok(task)
    }

    /// Claims a task for `owner`, or refuses because somebody else has it.
    ///
    /// **This is the operation the file-per-task layout exists for.** Read,
    /// test and write all happen under one hold, so two processes that both
    /// find the task unowned cannot both write their own name into it: the
    /// second one takes the lock *after* the first one released it, reads the
    /// owner the first one wrote, and is refused.
    ///
    /// The refusal is unconditional on a non-empty owner, including when that
    /// owner is the claimant itself — a caller re-claiming its own task is
    /// answered by [`TaskError::AlreadyOwned`] naming itself, which is
    /// information rather than a wrong answer. Releasing a task is
    /// [`Store::update`] with an empty owner, which is the door a lead
    /// reassigning a dead member's work goes through.
    ///
    /// # Errors
    ///
    /// [`TaskError::NoSuchTask`] when nothing is filed under the id,
    /// [`TaskError::AlreadyOwned`] when somebody holds it, and whatever the
    /// lock or the filesystem returned.
    pub fn claim(&self, id: &TaskId, owner: &str) -> Result<Task, TaskError> {
        let path = self.path_of(id);
        let _hold = self.hold(&path, id)?;

        let mut task = read(&path)?.ok_or(TaskError::NoSuchTask { id: *id })?;
        if task.is_owned() {
            return Err(TaskError::AlreadyOwned { id: *id, owner: task.owner });
        }
        task.owner = owner.to_owned();
        write(&path, &task)?;
        tracing::debug!(tasks = %self.dir.display(), %id, owner, "a task was claimed");

        Ok(task)
    }

    /// Removes a task permanently.
    ///
    /// Under the task's own hold, so a delete cannot land in the middle of
    /// somebody's read-modify-write; the hold's own directory is removed after
    /// the document, by the guard's [`Drop`].
    ///
    /// The id is **not** returned to the counter — see this module's own note
    /// on why a gap is the right outcome.
    ///
    /// # Errors
    ///
    /// [`TaskError::NoSuchTask`] when nothing is filed under the id, and
    /// whatever the lock or the filesystem returned.
    pub fn delete(&self, id: &TaskId) -> Result<(), TaskError> {
        let path = self.path_of(id);
        let _hold = self.hold(&path, id)?;

        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(TaskError::NoSuchTask { id: *id });
            }
            Err(error) => return Err(error.into()),
        }
        tracing::debug!(tasks = %self.dir.display(), %id, "a task was deleted");

        Ok(())
    }

    /// Takes a task document's lock, reading an absent *directory* as an
    /// absent task.
    ///
    /// [`lock::acquire_unseeded`] names the lock from the target's parent when
    /// the target is not there, so a store whose directory has never been
    /// created answers `ENOENT` from that `realpath` rather than from the
    /// document. There is exactly one thing that can mean — no task is filed
    /// under this id — so it is said in those words instead of as a path
    /// error.
    fn hold(&self, path: &Path, id: &TaskId) -> Result<lock::Guard, TaskError> {
        match lock::acquire_unseeded(path) {
            Ok(hold) => Ok(hold),
            Err(LockError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                Err(TaskError::NoSuchTask { id: *id })
            }
            Err(error) => Err(error.into()),
        }
    }

    /// The next id, under the counter's own hold.
    ///
    /// The hold covers the read as well as the write, which is the whole point:
    /// two creates that both read "3" would both write "4" and both file
    /// `4.json`, and one of those tasks would silently never have existed.
    fn issue_id(&self) -> Result<TaskId, TaskError> {
        let path = self.counter_path();
        let _hold = lock::acquire_unseeded(&path)?;

        let issued = self.last_issued(&path)?;
        let next = issued.saturating_add(1);
        if next > ID_MAX {
            return Err(TaskError::CounterExhausted);
        }
        write_atomically(&path, next.to_string().as_bytes())?;

        Ok(TaskId(next))
    }

    /// The highest id the counter says it has issued.
    ///
    /// A counter that is **not there** is a list nobody has created a task in
    /// yet — or one whose counter somebody removed. Both are answered from the
    /// documents on disk rather than from zero, because starting over at 1
    /// would hand a fresh task the id of one that still exists and quietly
    /// merge two pieces of work. A counter that is there and unreadable is the
    /// same repair with a line about it: the alternative is a list that cannot
    /// be added to until somebody deletes a file by hand.
    fn last_issued(&self, path: &Path) -> Result<u64, TaskError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return self.highest_id(),
            Err(error) => return Err(error.into()),
        };
        match text.trim().parse::<u64>() {
            Ok(issued) if issued <= ID_MAX => Ok(issued),
            _ => {
                let highest = self.highest_id()?;
                tracing::warn!(
                    counter = %path.display(),
                    highest,
                    "the task counter would not read and was rebuilt from the documents on disk",
                );

                Ok(highest)
            }
        }
    }

    /// The highest id any document in the directory is filed under, or zero.
    fn highest_id(&self) -> Result<u64, TaskError> {
        Ok(self.ids()?.into_iter().map(TaskId::number).max().unwrap_or(0))
    }

    /// Every id the directory holds a document for, in whatever order the
    /// filesystem answered in.
    ///
    /// A name that is not `<digits>.json` is not a task and is skipped without
    /// comment: [`COUNTER`] is one such name by design, and a lock directory
    /// beside a document is another.
    fn ids(&self) -> Result<Vec<TaskId>, TaskError> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };

        let mut ids = Vec::new();
        for entry in entries {
            let name = entry?.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(stem) = name.strip_suffix(DOCUMENT_SUFFIX) else { continue };
            if let Ok(id) = TaskId::parse(stem) {
                ids.push(id);
            }
        }

        Ok(ids)
    }
}

/// Which sentence a dropped document is reported with.
///
/// A fixed string per kind rather than the error's own rendering: a
/// `serde_json` message quotes the value it failed on, and a task's words are
/// content by the same argument a message body is.
fn dropped(error: &TaskError) -> &'static str {
    match error {
        TaskError::Json(_) => DROPPED_UNDECODABLE,
        _ => DROPPED_UNREADABLE,
    }
}

/// One task document, or [`None`] when there is no such file.
fn read(path: &Path) -> Result<Option<Task>, TaskError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };

    Ok(Some(serde_json::from_str(&text)?))
}

/// One task document, encoded the way every other document in this crate is
/// and landed atomically.
///
/// The shadow check is [`crate::mailbox::write_bounded`]'s, for the same
/// reason: a passthrough map holding a key the shape also declares would emit
/// that key twice, and a reader taking the last one would read something the
/// writer never meant. Unreachable from a document read off disk — a declared
/// key is captured by its field before the flatten map sees it — and checked
/// anyway, because hand-building a record is the one way to get there and the
/// cost of being wrong is a corrupt shared file.
fn write(path: &Path, task: &Task) -> Result<(), TaskError> {
    let mut issues = shadowed(&task.extra, &TASK_KEYS);
    for comment in &task.comments {
        issues.extend(shadowed(&comment.extra, &COMMENT_KEYS));
    }
    if !issues.is_empty() {
        return Err(TaskError::SchemaInvalid { issues });
    }
    write_atomically(path, document(task)?.as_bytes())?;

    Ok(())
}

/// [`Update`]'s semantics, in one place so the store's door and its tests
/// cannot disagree about them.
fn apply(task: &mut Task, update: Update) {
    let Update {
        status,
        subject,
        description,
        active_form,
        owner,
        metadata,
        add_blocks,
        add_blocked_by,
        add_comment,
    } = update;

    if let Some(status) = status {
        task.status = status;
    }
    if let Some(subject) = subject {
        task.subject = subject;
    }
    if let Some(description) = description {
        task.description = description;
    }
    if let Some(active_form) = active_form {
        task.active_form = Some(active_form);
    }
    if let Some(owner) = owner {
        task.owner = owner;
    }
    for (key, value) in metadata {
        if value.is_null() {
            // `shift_remove` rather than `swap_remove`: a metadata map's order
            // is the order its keys arrived in, and swapping the tail into a
            // hole would reorder the rest of it on every delete.
            task.metadata.shift_remove(&key);
        } else {
            // `insert` keeps an existing key where it already was, which is
            // what makes a merge a merge rather than a reshuffle.
            task.metadata.insert(key, value);
        }
    }
    extend_unique(&mut task.blocks, add_blocks);
    extend_unique(&mut task.blocked_by, add_blocked_by);
    if let Some(comment) = add_comment {
        task.comments.push(comment);
    }
}

/// Appends the ids that are not already there, keeping the order they arrived
/// in.
///
/// Quadratic, and deliberately: a task's blocker list is a handful of ids, and
/// a `HashSet` here would cost an allocation to save comparisons nobody can
/// measure.
fn extend_unique(held: &mut Vec<TaskId>, added: Vec<TaskId>) {
    for id in added {
        if !held.contains(&id) {
            held.push(id);
        }
    }
}

#[cfg(test)]
#[path = "task_tests.rs"]
mod tests;
