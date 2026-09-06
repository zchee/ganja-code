//! Two [`Provider`] doubles, each recording every request it was asked, and
//! differing only in how the answer is chosen.
//!
//! [`ScriptedProvider`] answers with the next entry from a fixed script.
//! Every scripted-turn integration suite in `ganja-core/tests` rebuilds this
//! shape under a different name — `Recorder`, `StepProvider`, `Scripted` —
//! popping a script, handing it back as a stream, logging the request. What
//! actually varies between them is a handful of values, not the shape
//! itself: the provider id it answers to, and what it does once the script
//! runs dry. Both are parameters here rather than forks.
//!
//! [`Director`] answers by what it was asked instead, which a queue cannot
//! do the moment two engines share one provider. The suites that need it say
//! so in their own module docs; the reason it exists at all is on the type.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::{self, BoxStream};
use ganja_core::provider::{ChatRequest, Provider, ProviderError, ProviderEvent};
use ganja_protocol::{FinishReason, PartBody, ToolState};
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
    /// Whether this double claims to carry binary attachments. On by default —
    /// a recorder that refused them could never record the base64 path — and
    /// off for suites pinning the engine's degradation to text.
    attachments: bool,
}

impl ScriptedProvider {
    /// A provider answering to `"recorder"`, completing once its script runs
    /// out — the shape most scripted-turn suites want.
    ///
    /// ```
    /// use ganja_core::provider::ProviderEvent;
    ///
    /// let (provider, requests) =
    ///     ganja_testkit::ScriptedProvider::new(vec![ganja_testkit::says("hello")]);
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

    /// A recorder whose wire carries no binary attachments — the shape a
    /// suite pinning the engine's degradation-to-text path asks in.
    pub fn text_only(
        scripts: Vec<Vec<ProviderEvent>>,
    ) -> (Arc<Self>, Arc<Mutex<Vec<ChatRequest>>>) {
        let (provider, seen) = Self::build("recorder", scripts, OnExhausted::Complete);
        let mut provider = Arc::into_inner(provider).expect("the pair was just built");
        provider.attachments = false;

        (Arc::new(provider), seen)
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
                attachments: true,
            }),
            seen,
        )
    }

    /// Appends a step to the end of the script, for a test whose next answer
    /// depends on what an earlier turn produced.
    pub fn push(&self, script: Vec<ProviderEvent>) {
        self.scripts.lock().expect("the scripts are never poisoned").push_back(script);
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        self.id
    }

    fn accepts_attachment(&self, _mime: &str) -> bool {
        self.attachments
    }

    async fn stream(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        self.seen.lock().expect("the request log is never poisoned").push(request);

        let next = self.scripts.lock().expect("the scripts are never poisoned").pop_front();

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
    vec![ProviderEvent::TextDelta(text.to_owned()), ProviderEvent::Finish(FinishReason::Completed)]
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
        ProviderEvent::ToolCallStart { id: "call".to_owned(), name: tool.to_owned() },
        ProviderEvent::ToolCallDelta { id: "call".to_owned(), json: args.to_string() },
        ProviderEvent::ToolCallEnd { id: "call".to_owned() },
        ProviderEvent::Finish(FinishReason::Completed),
    ]
}

/// Answers each request by **what it was asked**, and records every request
/// it was asked in the handle returned alongside it.
///
/// [`ScriptedProvider`] is a FIFO, which is exactly the wrong shape the
/// moment two engines share one provider: a lead and its in-process teammate
/// both take turns of their own, a persistent engine asks for a title beside
/// them, and a queue would hand one conversation's answer to whichever engine
/// reached it first. Keyed on the conversation instead — read with
/// [`transcript`] — every request is answered with the step that conversation
/// is at, and a toolless request (a title) is answered as one.
///
/// What varies between suites is only the keying, so it arrives as a closure
/// rather than as another enum of scripts.
///
/// ```
/// let (provider, requests) = ganja_testkit::Director::answering(|request| {
///     if ganja_testkit::transcript(request).contains("ping") {
///         ganja_testkit::says("pong")
///     } else {
///         ganja_testkit::says("who is this")
///     }
/// });
/// assert!(requests.lock().unwrap().is_empty(), "nothing asked yet");
/// let _ = provider;
/// ```
pub struct Director {
    answer: Keying,
    seen: Arc<Mutex<Vec<ChatRequest>>>,
}

/// How a [`Director`] chooses its answer: the conversation in, the step that
/// conversation is at out.
type Keying = Box<dyn Fn(&ChatRequest) -> Vec<ProviderEvent> + Send + Sync>;

impl Director {
    /// A provider answering to `"recorder"` — [`ScriptedProvider::new`]'s own
    /// name, since a suite that keys on the conversation is not also keying on
    /// the provider's id — that hands every request to `answer`.
    pub fn answering(
        answer: impl Fn(&ChatRequest) -> Vec<ProviderEvent> + Send + Sync + 'static,
    ) -> (Arc<Self>, Arc<Mutex<Vec<ChatRequest>>>) {
        let seen: Arc<Mutex<Vec<ChatRequest>>> = Arc::default();
        (Arc::new(Self { answer: Box::new(answer), seen: Arc::clone(&seen) }), seen)
    }
}

#[async_trait]
impl Provider for Director {
    fn id(&self) -> &str {
        "recorder"
    }

    async fn stream(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let script = (self.answer)(&request);
        self.seen.lock().expect("the request log is never poisoned").push(request);

        Ok(stream::iter(script).boxed())
    }
}

/// Everything a request's conversation says: the text of every part, and what
/// every finished tool call answered.
///
/// The tool output is what makes this readable as a conversation rather than
/// as a prompt: a [`Director`] deciding what step a conversation is at reads
/// what the previous step *produced*, which for a tool call is its result and
/// never its text.
pub fn transcript(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| &message.parts)
        .map(|part| match &part.body {
            PartBody::Tool { state: ToolState::Completed { output, .. }, .. } => output.clone(),
            PartBody::Tool { state: ToolState::Error { error, .. }, .. } => error.clone(),
            _ => part.as_text().unwrap_or_default().to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}
