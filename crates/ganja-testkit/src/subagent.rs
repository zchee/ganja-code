//! A subagent double: the seam a `task` call delegates through, with no
//! engine on the other side of it.
//!
//! The real implementation of [`Subagents`] is an agent loop — a provider, an
//! agent roster, a permission tier, a second turn. A test about what the *tool*
//! does with an answer needs none of that, and paying for it would mean
//! scripting a whole child conversation to assert on four lines of rendering.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use ganja_tool::task::{
    Delegated, Delegation, NotSpawned, Subagents, TeammateSpawn, Teammated, Unanswered,
};
use tokio_util::sync::CancellationToken;

/// Answers each delegation from a queued script, and records what it was asked.
///
/// ```
/// let (_subagents, asked) = ganja_testkit::ScriptedSubagents::new(Vec::new());
/// assert!(asked.lock().unwrap().is_empty(), "nothing has delegated yet");
/// ```
#[derive(Debug)]
pub struct ScriptedSubagents {
    answers: Mutex<VecDeque<Result<Delegated, Unanswered>>>,
    asked: Arc<Mutex<Vec<Delegation>>>,
}

impl ScriptedSubagents {
    /// A double answering with `answers`, in order, alongside the log of what
    /// it was asked to run.
    #[must_use]
    pub fn new(
        answers: Vec<Result<Delegated, Unanswered>>,
    ) -> (Arc<Self>, Arc<Mutex<Vec<Delegation>>>) {
        let asked: Arc<Mutex<Vec<Delegation>>> = Arc::default();
        (
            Arc::new(Self {
                answers: Mutex::new(answers.into()),
                asked: Arc::clone(&asked),
            }),
            asked,
        )
    }
}

#[async_trait]
impl Subagents for ScriptedSubagents {
    async fn delegate(
        &self,
        request: Delegation,
        _cancel: CancellationToken,
    ) -> Result<Delegated, Unanswered> {
        self.asked
            .lock()
            .expect("the delegation log is never poisoned")
            .push(request);

        self.answers
            .lock()
            .expect("the script is never poisoned")
            .pop_front()
            .expect("the script has an answer for every delegation it is asked for")
    }
}

/// Records what the `task` tool's teammate door asked to start, and answers
/// every spawn with one canned [`Teammated`].
///
/// A delegation is refused (`Unanswered::Unknown`): a suite driving the
/// teammate door has no child conversation to script, and a delegation
/// reaching this double is that suite's own bug.
///
/// ```
/// use ganja_tool::task::Teammated;
///
/// let spawner = ganja_testkit::RecordingSpawner::new(Teammated {
///     name: "w3".to_owned(),
///     agent_id: "w3@session-abcd1234".to_owned(),
///     backend: "in-process".to_owned(),
///     note: "it reads this through its mailbox".to_owned(),
/// });
/// assert!(spawner.started().is_empty(), "nothing has spawned yet");
/// ```
#[derive(Debug)]
pub struct RecordingSpawner {
    answer: Teammated,
    started: Mutex<Vec<TeammateSpawn>>,
}

impl RecordingSpawner {
    /// A double answering every spawn with `answer`.
    #[must_use]
    pub fn new(answer: Teammated) -> Arc<Self> {
        Arc::new(Self {
            answer,
            started: Mutex::new(Vec::new()),
        })
    }

    /// Every spawn recorded so far, in call order.
    #[must_use]
    pub fn started(&self) -> Vec<TeammateSpawn> {
        self.started
            .lock()
            .expect("the spawn log is never poisoned")
            .clone()
    }
}

#[async_trait]
impl Subagents for RecordingSpawner {
    async fn delegate(
        &self,
        _request: Delegation,
        _cancel: CancellationToken,
    ) -> Result<Delegated, Unanswered> {
        Err(Unanswered::Unknown)
    }

    async fn spawn_teammate(&self, request: TeammateSpawn) -> Result<Teammated, NotSpawned> {
        self.started
            .lock()
            .expect("the spawn log is never poisoned")
            .push(request);

        Ok(self.answer.clone())
    }
}
