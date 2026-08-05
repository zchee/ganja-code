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
use ganja_tool::task::{Delegated, Delegation, Subagents, Unanswered};
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
