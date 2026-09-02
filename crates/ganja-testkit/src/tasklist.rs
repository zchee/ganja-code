//! A [`TaskList`] double for suites that need a shared list to answer with
//! something fixed, and to say how often it was asked.
//!
//! `ganja-tool`'s own tests hold a fuller double, but it is private to that
//! crate's test module, so the first suite outside it that needed a list
//! rewrote one. This is that rewrite, moved to where the second one will
//! find it.

use std::sync::Mutex;

use async_trait::async_trait;
use ganja_tool::tasklist::{Change, Draft, Record, Status, Summary, TaskFailure, TaskList};

/// A shared list that answers with what it was built over, and counts how
/// often it was asked.
///
/// Only [`TaskList::list`] is implemented. Everything reaching a list through
/// this double so far only ever reads it, and the four write arms are left
/// [`unreachable!`] rather than improvised: a double that quietly answered a
/// call no test makes would be answering for the real list. A suite that needs
/// one of them fills that arm in here rather than forking the double.
#[derive(Debug)]
pub struct StaticTasks {
    listed: Vec<Summary>,
    failure: Option<String>,
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
        Self { listed, failure: None, reads: Mutex::new(0) }
    }

    /// A list that refuses to open, answering every read with `reason` — the
    /// team directory that is not there, or is not readable.
    #[must_use]
    pub fn failing(reason: &str) -> Self {
        Self { listed: Vec::new(), failure: Some(reason.to_owned()), reads: Mutex::new(0) }
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
        match &self.failure {
            Some(reason) => Err(TaskFailure { reason: reason.clone() }),
            None => Ok(self.listed.clone()),
        }
    }

    async fn get(&self, _id: &str) -> Result<Record, TaskFailure> {
        unreachable!("this double only ever reads")
    }
}

/// One task, as a store would have summarized it: `id` is also its subject's
/// distinguishing half, so a listing can be read back by eye.
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
    Summary {
        id: id.to_owned(),
        subject: format!("task {id}"),
        status,
        owner: owner.to_owned(),
        blocked_by: Vec::new(),
    }
}
