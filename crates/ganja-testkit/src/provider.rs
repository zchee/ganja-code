//! A [`Provider`] double that answers each request with the next entry from
//! a fixed script, and records every request it was asked.
//!
//! Every scripted-turn integration suite in `ganja-core/tests` rebuilds this
//! shape under a different name — `Recorder`, `StepProvider`, `Scripted` —
//! popping a script, handing it back as a stream, logging the request. What
//! actually varies between them is a handful of values, not the shape
//! itself: the provider id it answers to, and what it does once the script
//! runs dry. Both are parameters here rather than forks.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use futures::{
    StreamExt as _,
    stream::{self, BoxStream},
};
use ganja_core::provider::{ChatRequest, Provider, ProviderError, ProviderEvent};
use ganja_protocol::FinishReason;
use tokio_util::sync::CancellationToken;

/// What a [`ScriptedProvider`] does once its script queue runs dry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnExhausted {
    /// Answer with a single completed finish, forever — for suites where a
    /// script only needs to cover the turns a test actually inspects.
    #[default]
    Complete,
    /// Panic — for suites that pin an exact request count and want a script
    /// gap to fail loudly rather than be quietly papered over.
    Panic,
}

/// Answers each request with the next entry from its script, and records
/// every request it was asked in the handle returned alongside it.
pub struct ScriptedProvider {
    id: &'static str,
    scripts: Mutex<VecDeque<Vec<ProviderEvent>>>,
    seen: Arc<Mutex<Vec<ChatRequest>>>,
    on_exhausted: OnExhausted,
}

impl ScriptedProvider {
    /// A provider answering to `"recorder"`, completing once its script runs
    /// out — the shape most scripted-turn suites want.
    ///
    /// ```
    /// use ganja_core::provider::ProviderEvent;
    ///
    /// let (provider, requests) = ganja_testkit::ScriptedProvider::new(vec![
    ///     ganja_testkit::says("hello"),
    /// ]);
    /// assert!(requests.lock().unwrap().is_empty(), "nothing asked yet");
    /// let _ = provider;
    /// ```
    pub fn new(scripts: Vec<Vec<ProviderEvent>>) -> (Arc<Self>, Arc<Mutex<Vec<ChatRequest>>>) {
        Self::build("recorder", scripts, OnExhausted::Complete)
    }

    /// The same, answering to `id` — for suites where the provider's own
    /// name is itself under test (a catalog lookup, a title rule keyed on
    /// it).
    pub fn named(
        id: &'static str,
        scripts: Vec<Vec<ProviderEvent>>,
    ) -> (Arc<Self>, Arc<Mutex<Vec<ChatRequest>>>) {
        Self::build(id, scripts, OnExhausted::Complete)
    }

    /// A provider that panics rather than improvise once its script runs
    /// out — for suites that pin an exact request count and want a script
    /// gap to fail loudly instead of silently returning a bare completion.
    pub fn strict(
        id: &'static str,
        scripts: Vec<Vec<ProviderEvent>>,
    ) -> (Arc<Self>, Arc<Mutex<Vec<ChatRequest>>>) {
        Self::build(id, scripts, OnExhausted::Panic)
    }

    fn build(
        id: &'static str,
        scripts: Vec<Vec<ProviderEvent>>,
        on_exhausted: OnExhausted,
    ) -> (Arc<Self>, Arc<Mutex<Vec<ChatRequest>>>) {
        let seen: Arc<Mutex<Vec<ChatRequest>>> = Arc::default();
        (
            Arc::new(Self {
                id,
                scripts: Mutex::new(scripts.into()),
                seen: Arc::clone(&seen),
                on_exhausted,
            }),
            seen,
        )
    }

    /// Appends a step to the end of the script, for a test whose next answer
    /// depends on what an earlier turn produced.
    pub fn push(&self, script: Vec<ProviderEvent>) {
        self.scripts
            .lock()
            .expect("the scripts are never poisoned")
            .push_back(script);
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        self.id
    }

    async fn stream(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        self.seen
            .lock()
            .expect("the request log is never poisoned")
            .push(request);

        let next = self
            .scripts
            .lock()
            .expect("the scripts are never poisoned")
            .pop_front();

        let script = match (next, self.on_exhausted) {
            (Some(script), _) => script,
            (None, OnExhausted::Complete) => vec![ProviderEvent::Finish(FinishReason::Completed)],
            (None, OnExhausted::Panic) => panic!("the script has a step for every request"),
        };

        Ok(stream::iter(script).boxed())
    }
}

/// A step that says `text` and stops: one text fragment, then a completed
/// finish. The shape every scripted suite's "and the model signs off" turn
/// takes.
///
/// ```
/// use ganja_core::provider::ProviderEvent;
///
/// let script = ganja_testkit::says("done");
/// assert!(matches!(script[0], ProviderEvent::TextDelta(_)));
/// assert!(matches!(script[1], ProviderEvent::Finish(_)));
/// ```
pub fn says(text: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::TextDelta(text.to_owned()),
        ProviderEvent::Finish(FinishReason::Completed),
    ]
}

/// A step that calls `tool` with `args`, under the fixed id `"call"`, and
/// stops.
///
/// The id is fixed rather than derived from `tool` because nothing in these
/// suites keys a lookup off it — parts are found by tool name, not call id —
/// so a fixed id is one fewer thing a script has to spell out.
///
/// ```
/// use ganja_core::provider::ProviderEvent;
///
/// let script = ganja_testkit::tool_call("read", serde_json::json!({"path": "x"}));
/// assert!(matches!(script[0], ProviderEvent::ToolCallStart { .. }));
/// assert!(matches!(script.last(), Some(ProviderEvent::Finish(_))));
/// ```
pub fn tool_call(tool: &str, args: serde_json::Value) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ToolCallStart {
            id: "call".to_owned(),
            name: tool.to_owned(),
        },
        ProviderEvent::ToolCallDelta {
            id: "call".to_owned(),
            json: args.to_string(),
        },
        ProviderEvent::ToolCallEnd {
            id: "call".to_owned(),
        },
        ProviderEvent::Finish(FinishReason::Completed),
    ]
}
