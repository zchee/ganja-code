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
//! [`COUNTER`], read-and-bumped under its own hold, and **a standing counter
//! never issues an id twice**: deleting task 3 leaves a gap where 3 was, and
//! the next create still gets 4. **The documents on disk are read on every
//! create all the same**, and the id issued is one past whichever of the two
//! is higher — a counter can be *behind* the directory it counts as easily as
//! it can be missing from it. The rename that files a document is not fsynced
//! — `mailbox::write_atomically` says why, in a doc this module cannot link
//! to, that function being crate-private — so a power loss can leave
//! `4.json` on disk beside a counter that still says 3; a team directory
//! restored from a copy carries whatever the counter held when the copy was
//! taken. A counter trusted on its own there would issue 4 a second time and
//! the create would rename straight over a live task, which is the one
//! outcome this whole scheme exists to prevent — so a create pays one
//! `read_dir`, the price the rebuild path already paid. What no repair can
//! recover is the ids of tasks deleted above the highest survivor — the
//! counter was the only record those were ever issued — and a `blockedBy`
//! still naming one of them then points at new work. That is pinned by test
//! rather than left to be rediscovered.
//!
//! # What a delete leaves behind
//!
//! [`Store::delete`] removes one document and touches no other, so deleting A
//! strands every edge that named it: B keeps a `blockedBy` pointing at an id
//! nothing is filed under, and [`Store::update`]'s missing-counterpart refusal
//! means `add_blocked_by: [A]` cannot repair it either, A being exactly the id
//! that refusal is about. Recorded as a follow-up rather than closed here.
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
//! For the same reason a *name* in that directory is not yet a document: the
//! writer that planted it need not have been a task list at all. So nothing
//! here reads a path it has not first stamped — a regular file, within
//! [`MAX_DOCUMENT_BYTES`] — because following a symlink would read somebody
//! else's file into a team's list and opening a FIFO would park the reader for
//! good, on a listing a lead's own render loop polls.
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

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::{fmt, fs, io};

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::Value;

use crate::lock::{self, LockError};
use crate::mailbox::{unreported, write_atomically};
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

/// How many digits [`ID_MAX`] has — derived from it so the two cannot drift,
/// and a constant so a parse allocates nothing to learn it.
const ID_DIGITS: usize = ID_MAX.ilog10() as usize + 1;

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

/// Why an update naming a crowd of counterparts was refused.
pub const REFUSED_TOO_MANY_COUNTERPARTS: &str =
    "an update may wire only so many other tasks at once";

/// Why a document was left out of a listing, when it would not decode.
///
/// Deliberately says nothing more, and the reason is
/// [`crate::mailbox`]'s `DROPPED_UNDECODABLE`: a decoder's own message can
/// quote the value it choked on, and what a task says is content.
pub const DROPPED_UNDECODABLE: &str = "the document is not a task this build can read";

/// Why a document was left out of a listing, when it could not be read at all.
pub const DROPPED_UNREADABLE: &str = "the document could not be read";

/// Why a document was left out of a listing, when it is not a file this module
/// will read at all.
pub const DROPPED_NOT_A_DOCUMENT: &str = "the name is not a regular file of a size a task is";

/// Why a name in the tasks directory was not read.
///
/// Names the bound in words rather than in bytes, because this is a sentence a
/// person reads in a log; [`MAX_DOCUMENT_BYTES`] is the number it is about,
/// and the two are one decision that moves together.
pub const REFUSED_NOT_A_DOCUMENT: &str =
    "a task document is a regular file no larger than a megabyte";

/// The most a task document may weigh before this module declines to read it.
///
/// The bound is about the **reader**, not about how large a document could
/// conceivably grow: every listing reads every document, and a listing is
/// polled for as long as somebody is watching the list. A subject, a
/// description, a metadata map and a comment thread — all of them prose
/// written one tool call at a time — come nowhere near it; a megabyte is on
/// the order of ten thousand hundred-byte comments on a single task. What it
/// stops is a same-uid writer planting a name this store would then read
/// without end, `/dev/zero` being the shortest way to say it.
pub const MAX_DOCUMENT_BYTES: u64 = 1024 * 1024;

/// How many other tasks one [`Store::update`] may wire at once.
///
/// The bound is about **how long the first hold is held**, not about how many
/// blockers a task may end up carrying. An update takes every document's hold
/// before it reads any, and a hold a peer already has costs the whole ladder —
/// ≈655 ms — before it is even reported, so sixteen contended counterparts
/// would hold the first document for over ten seconds. Ten seconds is
/// [`lock::STALE`]: a peer would break a hold this process is still standing
/// in, on the protocol's premise that a hold is one sub-second
/// read-modify-write. Eight keeps that worst case near five seconds and is
/// already past what a dependency list a person reads ever holds — a task
/// wired to nine others in one call is a plan, not a task. Nothing stops a
/// caller from adding more one call at a time; what is refused is doing it
/// under a single hold.
pub const MAX_COUNTERPARTS: usize = 8;

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
    /// tasks — and returned rather than wrapped, because a standing counter
    /// never issues an id twice, and wrapping to 1 would.
    #[error("{REFUSED_COUNTER_EXHAUSTED}")]
    CounterExhausted,
    /// A passthrough map carries a key the shape itself declares.
    #[error("{REFUSED_SCHEMA}: {}", issues.join("; "))]
    SchemaInvalid {
        /// One sentence per offending key.
        issues: Vec<String>,
    },
    /// An update named more counterparts than [`MAX_COUNTERPARTS`].
    ///
    /// Carries the count rather than the ids: what the caller has to change is
    /// how many it named, and the ids are its own arguments read back.
    #[error("{REFUSED_TOO_MANY_COUNTERPARTS}: {named} named, {MAX_COUNTERPARTS} at most")]
    TooManyCounterparts {
        /// How many distinct other tasks the update named.
        named: usize,
    },
    /// The path is not a regular file, or is larger than any task document.
    ///
    /// A path rather than an id, because the whole point of it is that what is
    /// there is not a task: naming the file is what tells whoever reads this
    /// which name to go and look at.
    #[error("{REFUSED_NOT_A_DOCUMENT}: {}", path.display())]
    NotADocument {
        /// The name that is not a document.
        path: PathBuf,
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
        if id.is_empty() || id.len() > ID_DIGITS {
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
    ///
    /// Each id named here is also written from the other end — this task lands
    /// in that one's `blockedBy` — so the pair cannot disagree about the edge.
    /// An id nothing is filed under refuses the whole update; see
    /// [`Store::update`].
    pub add_blocks: Vec<TaskId>,
    /// Ids to add to `blockedBy`, skipping any already there.
    ///
    /// Mirrored the same way: this task lands in each named task's `blocks`.
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
    /// [`TaskError::CounterExhausted`] at the end of the id space, and
    /// whatever the locks or the filesystem returned. A draft carries no
    /// passthrough map and no comment, so the schema check every write runs
    /// has nothing here it could refuse.
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
                    // Once per damage per process, on the mailbox's memory:
                    // this listing is polled for as long as somebody is
                    // watching it, and a document that stays broken must not
                    // become the log.
                    if unreported((&self.dir, id, error.to_string())) {
                        tracing::warn!(
                            tasks = %self.dir.display(),
                            %id,
                            reason = dropped(&error),
                            "a task document would not read and was left out of the list",
                        );
                    }

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
    /// # A dependency is written on both tasks
    ///
    /// `add_blocks` and `add_blocked_by` each name the *other* task, and both
    /// documents move: A blocking B **is** B blocked by A, and a list that
    /// recorded only the side the call named would keep calling B free — a
    /// listing renders `blockedBy`, and free-to-pick-up is what that field
    /// answers. So an update carrying either list takes every document's hold
    /// — the task's and each counterpart's — **lowest id first**, and reads
    /// them all before it writes any.
    ///
    /// The order is the whole of what makes it deadlock-free: two updates
    /// wiring A and B in opposite directions would otherwise each hold what
    /// the other waits for, and the in-process half of a hold parks on a
    /// condvar with no timeout by design ([`lock::acquire`]'s own hazard).
    /// A counterpart named twice, or a task named as its own counterpart, is
    /// **one** hold rather than two, for the same reason: re-entering a hold
    /// this thread already holds never returns. The dedupe is on the id, and
    /// two *different* ids can name one file — a counterpart planted as a
    /// symlink resolves onto its target, which is the very path
    /// [`lock::acquire_unseeded`] keys on — so every path in the set is
    /// stamped the way every read here stamps one **before any hold is taken**,
    /// and a name that is not a regular document refuses the call
    /// with [`TaskError::NotADocument`]. That narrows the window rather than
    /// closing it, as every read's stamp does: a swap between the stamp and
    /// the hold still collapses two ids onto one key, and that half parks with
    /// no timeout — which is why the test that plants one bounds itself. A
    /// hard link needs no answer here: it
    /// canonicalizes to itself, so it is two keys rather than one, and a write
    /// through either name renames a fresh file over that name rather than
    /// through it.
    ///
    /// At most [`MAX_COUNTERPARTS`] of them, counted after the dedupe and
    /// refused before the first hold, because the first document's hold is
    /// held while every other one is taken — that constant's own doc has the
    /// arithmetic.
    ///
    /// A counterpart that does not exist **refuses the whole call** with
    /// [`TaskError::NoSuchTask`], before a byte is written. Recording the half
    /// that can be recorded is the failure this door exists to close: an edge
    /// naming a task nobody filed is the dangling `blockedBy` this module's
    /// counter note calls out, and a model that mistyped an id is better
    /// served reading that than by a list that quietly disagrees with itself.
    ///
    /// Every refusal a write could raise is raised **before the first
    /// rename**: the schema check a write makes is made over every document up
    /// front, so a counterpart carrying a shadowing passthrough key leaves
    /// the whole list as it was rather than half-written. What is left is
    /// narrower and is stated rather than glossed — the writes are **not** one
    /// transaction and cannot be, this store having no journal, so a crash or
    /// an IO failure *between* two of them leaves the edge on one side. The
    /// next update naming that pair repairs it, since both sides are appended
    /// without duplication.
    ///
    /// # Errors
    ///
    /// [`TaskError::NoSuchTask`] when nothing is filed under the id or under
    /// one an edge names, [`TaskError::NotADocument`] when one of those names
    /// is not a document this module reads, [`TaskError::TooManyCounterparts`]
    /// past [`MAX_COUNTERPARTS`], [`TaskError::SchemaInvalid`] when any
    /// document this call would write carries a passthrough key its own shape
    /// declares — the comment being appended is the reachable one; the
    /// metadata merge is a nested map and cannot shadow a top-level key — and
    /// whatever the locks or the filesystem returned.
    pub fn update(&self, id: &TaskId, update: Update) -> Result<Task, TaskError> {
        // Every document this call writes, sorted and deduplicated: the order
        // is the deadlock-free one and the dedupe is what keeps a self-edge
        // from re-entering a hold this thread already has.
        let mut held = vec![*id];
        held.extend(&update.add_blocks);
        held.extend(&update.add_blocked_by);
        held.sort_unstable();
        held.dedup();

        // The task itself is in the set and is not a counterpart of its own.
        let named = held.len() - 1;
        if named > MAX_COUNTERPARTS {
            return Err(TaskError::TooManyCounterparts { named });
        }

        // Before the first hold, because the hold is what a planted symlink
        // turns into a wait nothing ends: `acquire_unseeded` canonicalizes its
        // target, so two ids that are one file are one lock key, and taking it
        // twice on this thread parks on a condvar with no timeout.
        for filed in &held {
            stamped(&self.path_of(filed))?;
        }

        let _holds = held
            .iter()
            .map(|filed| self.hold(&self.path_of(filed), filed))
            .collect::<Result<Vec<lock::Guard>, TaskError>>()?;

        // Read every document before writing any, so an edge naming a task
        // nobody filed refuses the call rather than half-applying it.
        let mut documents = Vec::with_capacity(held.len());
        for filed in &held {
            let path = self.path_of(filed);
            let task = read(&path)?.ok_or(TaskError::NoSuchTask { id: *filed })?;
            documents.push((path, task));
        }

        // The same edges from the other end, taken before `apply` consumes
        // them: what this task blocks is blocked by this task, and what blocks
        // it is what it is blocked by.
        let blocks = update.add_blocks.clone();
        let blocked_by = update.add_blocked_by.clone();
        apply(&mut documents[at(&held, id)?].1, update);
        for counterpart in &blocks {
            extend_unique(&mut documents[at(&held, counterpart)?].1.blocked_by, [*id]);
        }
        for counterpart in &blocked_by {
            extend_unique(&mut documents[at(&held, counterpart)?].1.blocks, [*id]);
        }

        // Every check a write would make, made before the first one runs: a
        // refusal raised halfway through would leave the documents ahead of it
        // renamed into place with the rest of the edge missing.
        for (_, task) in &documents {
            unshadowed(task)?;
        }
        for (path, task) in &documents {
            write(path, task)?;
        }

        let task = documents
            .into_iter()
            .find_map(|(_, task)| (task.id == *id).then_some(task))
            .ok_or(TaskError::NoSuchTask { id: *id })?;
        tracing::debug!(
            tasks = %self.dir.display(),
            %id,
            status = task.status.as_str(),
            edges = blocks.len() + blocked_by.len(),
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
    ///
    /// **The directory is read on every create**, not only when the counter is
    /// missing, and the id issued is one past whichever of the two is higher —
    /// the module note above says why a counter that parses cleanly can still
    /// be behind the documents it counts, and what a create that trusted it
    /// would rename over. The cost is one `read_dir` per create, which is what
    /// the rebuild path already spent and what [`Store::list`] spends on every
    /// poll.
    fn issue_id(&self) -> Result<TaskId, TaskError> {
        let path = self.counter_path();
        let _hold = lock::acquire_unseeded(&path)?;

        let highest = self.highest_id()?;
        let issued = self.last_issued(&path, highest)?.max(highest);
        // `last_issued` bounds `issued` at `ID_MAX`, well short of `u64::MAX`.
        let mut next = issued + 1;
        // Documents set the issue point; a name that is not one — planted, or a
        // document too damaged to read — is stepped over, never renamed over:
        // counting such names would let one spelt like the last id there is
        // refuse every create from then on, and issuing under one would put a
        // fresh task where something already stands. Only an absent name is
        // free.
        while next <= ID_MAX && !matches!(stamped(&self.path_of(&TaskId(next))), Ok(false)) {
            next += 1;
        }
        if next > ID_MAX {
            return Err(TaskError::CounterExhausted);
        }
        write_atomically(&path, next.to_string().as_bytes())?;

        Ok(TaskId(next))
    }

    /// The highest id the counter says it has issued, or `highest` when it
    /// cannot say.
    ///
    /// A counter that is **not there** is a list nobody has created a task in
    /// yet — or one whose counter somebody removed. Both are answered from the
    /// documents on disk rather than from zero, because starting over at 1
    /// would hand a fresh task the id of one that still exists and quietly
    /// merge two pieces of work. A counter that is there and unreadable is the
    /// same repair with a line about it: the alternative is a list that cannot
    /// be added to until somebody deletes a file by hand. So is one that is
    /// not a regular file at all — the write that repairs it renames a fresh
    /// file **over the name**, and a rename follows nothing.
    ///
    /// `highest` is passed in rather than read here so that a create walks the
    /// directory exactly once, whichever of these arms it takes.
    fn last_issued(&self, path: &Path, highest: u64) -> Result<u64, TaskError> {
        let issued = match read_guarded(path) {
            Ok(Some(text)) => text.trim().parse::<u64>().ok().filter(|issued| *issued <= ID_MAX),
            Ok(None) => return Ok(highest),
            Err(TaskError::NotADocument { .. }) => None,
            Err(error) => return Err(error),
        };
        let Some(issued) = issued else {
            tracing::warn!(
                counter = %path.display(),
                highest,
                "the task counter would not read and was rebuilt from the documents on disk",
            );

            return Ok(highest);
        };

        Ok(issued)
    }

    /// The highest id any **document** in the directory is filed under, or
    /// zero. A name alone moves nothing: a planted `<ID_MAX>.json` with no
    /// document behind it would otherwise push the issue point past the end
    /// of the id space and refuse every create from then on, with no repair
    /// short of deleting it by hand. What the read guard would not read as a
    /// document is not one here either — an absent, damaged or oversized name
    /// is skipped — and only an I/O failure is an error.
    fn highest_id(&self) -> Result<u64, TaskError> {
        let mut highest = 0;
        for id in self.ids()? {
            match read(&self.path_of(&id)) {
                Ok(Some(_)) => highest = highest.max(id.number()),
                Ok(None) | Err(TaskError::NotADocument { .. } | TaskError::Json(_)) => {}
                Err(error) => return Err(error),
            }
        }

        Ok(highest)
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
        TaskError::NotADocument { .. } => DROPPED_NOT_A_DOCUMENT,
        _ => DROPPED_UNREADABLE,
    }
}

/// One task document, or [`None`] when there is no such file.
fn read(path: &Path) -> Result<Option<Task>, TaskError> {
    let Some(text) = read_guarded(path)? else { return Ok(None) };

    Ok(Some(serde_json::from_str(&text)?))
}

/// The bytes at `path`, when what is there is something this module will read.
///
/// The tasks directory is one another process of this user's can write into —
/// that is what makes the list shared — so a **name** in it is not yet a
/// document, and the guard is what stands between a planted one and a reader.
/// A symlink would redirect this read into somebody else's file; a FIFO would
/// make it never return, and `/dev/zero` would make it never stop, on a
/// listing that is polled for as long as anybody is watching the list. So
/// [`stamped`] decides — a regular file, within [`MAX_DOCUMENT_BYTES`] — and
/// anything else is [`TaskError::NotADocument`], which [`Store::list`] reports
/// and skips exactly as it does a document that will not decode.
///
/// **The window is narrowed rather than closed**, and it is worth being exact
/// about which half: a writer that swaps a regular file for a FIFO between
/// this stamp and the open below can still park the reader. Closing that needs
/// the open itself to carry `O_NOFOLLOW | O_NONBLOCK`, which nothing this
/// crate ships can spell: `libc` is a dev-dependency here, taken on so a test
/// can plant a real FIFO, and putting it in the shipped closure would be this
/// crate's first system call outside `std`. What is here refuses every name a
/// planted document can already be, and bounds the read on the open file,
/// where no rename can reach it.
fn read_guarded(path: &Path) -> Result<Option<String>, TaskError> {
    let refuse = || TaskError::NotADocument { path: path.to_owned() };
    if !stamped(path)? {
        return Ok(None);
    }

    let file = match fs::File::open(path) {
        Ok(file) => file,
        // Deleted between the stamp and the open: a race with a winner and no
        // loser, which is how `Store::list` already reads a missing document.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };

    // One byte past the bound, so a file that grew after its stamp is refused
    // on what was actually read rather than on what was promised.
    let mut text = String::new();
    file.take(MAX_DOCUMENT_BYTES + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(refuse());
    }

    Ok(Some(text))
}

/// Whether a document stands at `path` at all, on the link's own stamp.
///
/// The tasks directory is one another process of this user's can write into —
/// that is what makes the list shared — so a **name** in it is not yet a
/// document. `symlink_metadata` reports on the link rather than on its target,
/// which is the whole point: [`fs::metadata`] would describe whatever a
/// planted symlink points at, and then whoever asked would read it.
///
/// `false` is nothing there, which every caller reads as an absent task rather
/// than as damage; [`TaskError::NotADocument`] is a name that is there and is
/// not something this module will touch.
fn stamped(path: &Path) -> Result<bool, TaskError> {
    let stamp = match fs::symlink_metadata(path) {
        Ok(stamp) => stamp,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !stamp.is_file() || stamp.len() > MAX_DOCUMENT_BYTES {
        return Err(TaskError::NotADocument { path: path.to_owned() });
    }

    Ok(true)
}

/// The schema check [`write`] makes, on its own so a caller writing several
/// documents can make every one of them before the first rename.
///
/// It is [`crate::mailbox::write_bounded`]'s check, for the same reason: a
/// passthrough map holding a key the shape also declares would emit that key
/// twice, and a reader taking the last one would read something the writer
/// never meant. Unreachable from a document read off disk — serde refuses a
/// duplicate declared key outright, so the flatten map never receives one —
/// and checked anyway, because hand-building a record (the comment an update
/// appends is one) is the way to get there and the cost of being wrong is a
/// corrupt shared file.
fn unshadowed(task: &Task) -> Result<(), TaskError> {
    let mut issues = shadowed(&task.extra, &TASK_KEYS);
    for comment in &task.comments {
        issues.extend(shadowed(&comment.extra, &COMMENT_KEYS));
    }
    if issues.is_empty() { Ok(()) } else { Err(TaskError::SchemaInvalid { issues }) }
}

/// One task document, encoded the way every other document in this crate is
/// and landed atomically.
fn write(path: &Path, task: &Task) -> Result<(), TaskError> {
    unshadowed(task)?;
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

/// Where the document filed under `id` sits among the ones an update holds.
///
/// The hold set is sorted, so this is a search rather than a scan. Fallible
/// rather than an `expect`: every id asked for here was put into that set by
/// the same call, and if that ever stops being true a refused update is a
/// better answer than a panicking store.
fn at(held: &[TaskId], id: &TaskId) -> Result<usize, TaskError> {
    held.binary_search(id).map_err(|_| TaskError::NoSuchTask { id: *id })
}

/// Appends the ids that are not already there, keeping the order they arrived
/// in.
///
/// Quadratic, and deliberately: a task's blocker list is a handful of ids, and
/// a `HashSet` here would cost an allocation to save comparisons nobody can
/// measure. Takes anything that yields ids so that a mirrored edge — always
/// exactly one — costs no `Vec` to append.
fn extend_unique(held: &mut Vec<TaskId>, added: impl IntoIterator<Item = TaskId>) {
    for id in added {
        if !held.contains(&id) {
            held.push(id);
        }
    }
}

#[cfg(test)]
#[path = "task_tests.rs"]
mod tests;
