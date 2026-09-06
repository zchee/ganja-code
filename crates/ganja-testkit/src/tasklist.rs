//! A [`TaskList`] double for suites that need a shared list to answer with
//! something fixed, and to say how often it was asked.
//!
//! `ganja-tool`'s own tests hold a fuller double, but it is private to that
//! crate's test module, so the first suite outside it that needed a list
//! rewrote one. This is that rewrite, moved to where the second one will
//! find it.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use ganja_tool::tasklist::{Change, Draft, Record, Status, Summary, TaskFailure, TaskList};

/// A shared list that answers with what it was built over, and counts how
/// often it was asked — or stops answering, which is the other thing a store
/// does.
///
/// Only [`TaskList::list`] is implemented. Everything reaching a list through
/// this double so far only ever reads it, and the four write arms are left
/// [`unreachable!`] rather than improvised: a double that quietly answered a
/// call no test makes would be answering for the real list. A suite that needs
/// one of them fills that arm in here rather than forking the double.
///
/// The three ways a read ends are all here, because a suite that wants one of
/// them usually wants the one before it too: an answer ([`StaticTasks::new`]),
/// a refusal ([`StaticTasks::failing`]) and a read that never comes back
/// ([`StaticTasks::stalling`]) — with [`StaticTasks::answering_once`] for the
/// case that needs a good read *before* the stop, since a section that goes
/// on drawing, or stops drawing, is only legible against what it drew first.
#[derive(Debug)]
pub struct StaticTasks {
    listed: Vec<Summary>,
    failure: Option<String>,
    /// Whether the answer above is given only once, everything after it
    /// hanging or refusing by the same rule the first read would have.
    once: bool,
    /// Whether the one answer has been given, when `once` says there is one.
    answered: AtomicBool,
    reads: Mutex<usize>,
}

impl StaticTasks {
    /// A list that answers with `listed`, lowest id first if that is how the
    /// caller built it — the double preserves the order it was given rather
    /// than sorting, so a test can pin what the real store's order becomes.
    ///
    /// ```
    /// let tasks = ganja_testkit::StaticTasks::new(Vec::new());
    /// assert_eq!(tasks.reads(), 0, "nothing has asked yet");
    /// ```
    #[must_use]
    pub fn new(listed: Vec<Summary>) -> Self {
        Self {
            listed,
            failure: None,
            once: false,
            answered: AtomicBool::new(false),
            reads: Mutex::new(0),
        }
    }

    /// A list that answers the **first** read with `listed` and never answers
    /// again: the reads after it hang, or — with [`StaticTasks::failing`]'s
    /// reason set on top — refuse.
    ///
    /// ```
    /// let tasks = ganja_testkit::StaticTasks::answering_once(Vec::new());
    /// assert_eq!(tasks.reads(), 0, "nothing has asked yet");
    /// ```
    #[must_use]
    pub fn answering_once(listed: Vec<Summary>) -> Self {
        Self { once: true, ..Self::new(listed) }
    }

    /// A list whose reads never come back at all, from the very first one —
    /// the planted FIFO, the wedged lock, the filesystem that stopped
    /// answering.
    ///
    /// `pending` rather than a long sleep: a deadline a test can outwait is
    /// not the failure this stands for.
    ///
    /// ```
    /// let tasks = ganja_testkit::StaticTasks::stalling();
    /// assert_eq!(tasks.reads(), 0, "nothing has asked yet");
    /// ```
    #[must_use]
    pub fn stalling() -> Self {
        Self { once: true, answered: AtomicBool::new(true), ..Self::new(Vec::new()) }
    }

    /// A list that refuses to open, answering every read with `reason` — the
    /// team directory that is not there, or is not readable.
    #[must_use]
    pub fn failing(reason: &str) -> Self {
        Self { failure: Some(reason.to_owned()), ..Self::new(Vec::new()) }
    }

    /// The same refusal, on the list this was built as rather than on an
    /// empty one.
    ///
    /// Set on top of [`StaticTasks::answering_once`] it refuses only the reads
    /// after the first: the directory removed under a running session, which
    /// is the composition [`StaticTasks::failing`] cannot express, since that
    /// one refuses the very first read too.
    #[must_use]
    pub fn then_failing(self, reason: &str) -> Self {
        Self { failure: Some(reason.to_owned()), ..self }
    }

    /// How many times this list has been read.
    #[must_use]
    pub fn reads(&self) -> usize {
        *self.reads.lock().expect("the counter is never poisoned")
    }
}

#[async_trait]
impl TaskList for StaticTasks {
    async fn create(&self, _draft: Draft) -> Result<Record, TaskFailure> {
        unreachable!("this double only ever reads")
    }

    async fn update(&self, _id: &str, _change: Change) -> Result<Record, TaskFailure> {
        unreachable!("this double only ever reads")
    }

    async fn delete(&self, _id: &str) -> Result<(), TaskFailure> {
        unreachable!("this double only ever reads")
    }

    async fn list(&self) -> Result<Vec<Summary>, TaskFailure> {
        *self.reads.lock().expect("the counter is never poisoned") += 1;
        // The one answer, where there is only one: `swap` rather than a
        // load-then-store, so two reads racing cannot both be the first.
        if self.once && !self.answered.swap(true, Ordering::SeqCst) {
            return Ok(self.listed.clone());
        }
        match (&self.failure, self.once) {
            (Some(reason), _) => Err(TaskFailure { reason: reason.clone() }),
            // The read that does not come back, which is what a list that
            // has said its one thing does next.
            (None, true) => std::future::pending().await,
            (None, false) => Ok(self.listed.clone()),
        }
    }

    async fn get(&self, _id: &str) -> Result<Record, TaskFailure> {
        unreachable!("this double only ever reads")
    }
}

/// One task, as a store's listing hands it over: every field a [`Summary`]
/// has, spelled out.
///
/// The one builder, because a fixture task is one shape and four spellings of
/// it are four places for a field a `Summary` grows to be forgotten.
/// [`task_summary`] is this with the two fields a caller that does not care
/// about them filled in.
///
/// ```
/// use ganja_tool::tasklist::Status;
///
/// let task = ganja_testkit::task("2", Status::InProgress, "w1", "Wire it", &["1"]);
/// assert_eq!(task.subject, "Wire it");
/// assert_eq!(task.blocked_by, ["1"], "and it waits on what it was given");
/// ```
#[must_use]
pub fn task(id: &str, status: Status, owner: &str, subject: &str, blocked_by: &[&str]) -> Summary {
    Summary {
        id: id.to_owned(),
        subject: subject.to_owned(),
        status,
        owner: owner.to_owned(),
        blocked_by: blocked_by.iter().map(|id| (*id).to_owned()).collect(),
    }
}

/// One task whose subject is derived from its `id`, so a listing can be read
/// back by eye — for the suites that are about the listing rather than about
/// any one task's text.
///
/// ```
/// use ganja_tool::tasklist::Status;
///
/// let task = ganja_testkit::task_summary("1", Status::Pending, "");
/// assert_eq!(task.subject, "task 1");
/// assert!(task.blocked_by.is_empty(), "a fixture task waits on nothing");
/// ```
#[must_use]
pub fn task_summary(id: &str, status: Status, owner: &str) -> Summary {
    task(id, status, owner, &format!("task {id}"), &[])
}
