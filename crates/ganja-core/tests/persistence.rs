//! Persistent sessions, end to end: write-through, crash resume, the session
//! request/response API, auto-title and auto-compaction.
//!
//! Every test owns a [`Storage`] under a temporary directory handed straight
//! to [`Engine::persistent`], so nothing here reads the environment for a
//! path and nothing can touch a real user's sessions. The suite does assume
//! `GANJA_FAKE_TITLE` is **unset** — the frozen default — and deliberately
//! never mutates process environment, because this binary's tests run on
//! parallel threads.
//!
//! Providers are either the real [`FakeProvider`] (deterministic word-count
//! usage) or [`LaneProvider`], a scripted provider modeled on `engine.rs`'s
//! test provider that additionally *claims* a provider id: the title and
//! compaction paths key on `Provider::id`, so claiming `"anthropic"`
//! exercises the catalog lookups and claiming `"fake"` proves the no-request
//! title rule while still exposing a request log.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use futures::{
    StreamExt as _,
    stream::{self, BoxStream},
};
use ganja_core::{
    Engine, EngineError, Message, Part, PartBody, PartId, Permissions, Registry, Role, SessionId,
    SessionInfo, Storage, ToolState, Usage,
    protocol::{Command, Event, FinishReason},
    provider::{ChatRequest, FakeProvider, Provider, ProviderError, ProviderEvent, fake},
    storage,
};
use tokio_util::sync::CancellationToken;

/// A store rooted in a directory that vanishes with the test. The directory
/// handle is returned because dropping it deletes the tree.
fn store() -> (tempfile::TempDir, Storage) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let storage = Storage::open(dir.path().join("storage"));

    (dir, storage)
}

/// A persistent engine with no tools and default rules — these tests prove
/// persistence, not the tool loop.
fn persistent(provider: Arc<dyn Provider>, model: &str, storage: Storage) -> Engine {
    Engine::persistent(
        provider,
        model,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage,
    )
}

/// Answers request *n* from script entry *n* (the last entry repeating) and
/// records every request it was asked, under whatever provider id it claims.
struct LaneProvider {
    id: &'static str,
    turns: Vec<Result<Vec<ProviderEvent>, ProviderError>>,
    seen: Arc<Mutex<Vec<ChatRequest>>>,
    served: AtomicUsize,
}

impl LaneProvider {
    fn new(id: &'static str, turns: Vec<Result<Vec<ProviderEvent>, ProviderError>>) -> Arc<Self> {
        assert!(!turns.is_empty(), "a lane script needs at least one turn");

        Arc::new(Self {
            id,
            turns,
            seen: Arc::default(),
            served: AtomicUsize::new(0),
        })
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.seen
            .lock()
            .expect("the request log is never poisoned")
            .clone()
    }
}

#[async_trait]
impl Provider for LaneProvider {
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

        let index = self.served.fetch_add(1, Ordering::SeqCst);
        let turn = self
            .turns
            .get(index)
            .or_else(|| self.turns.last())
            .expect("the constructor refused an empty script");

        match turn {
            Ok(events) => Ok(stream::iter(events.clone()).boxed()),
            Err(error) => Err(error.clone()),
        }
    }
}

/// A provider that says when it was asked and answers only when it is let go.
///
/// What it buys is a moment the test can *hold*: the turn is open, the request
/// is in flight, and nothing has finished — which is the only place a
/// write-through claim can honestly be checked. A claim checked after the turn
/// ends cannot tell a store that writes as it goes from one that writes at the
/// end.
///
/// It claims `"fake"` so the title path asks it nothing: a title request would
/// be a second call, and the second call would block on the same latch.
struct HeldProvider {
    /// A permit per request the provider has received.
    asked: tokio::sync::Semaphore,
    /// A permit per request the test has allowed to answer.
    released: tokio::sync::Semaphore,
    /// What it answers with, once let go.
    reply: Vec<ProviderEvent>,
}

impl HeldProvider {
    fn new(reply: Vec<ProviderEvent>) -> Arc<Self> {
        Arc::new(Self {
            asked: tokio::sync::Semaphore::new(0),
            released: tokio::sync::Semaphore::new(0),
            reply,
        })
    }

    /// Waits until the provider has been asked, or fails loudly instead of
    /// hanging the suite.
    async fn asked(&self) {
        tokio::time::timeout(Duration::from_secs(30), self.asked.acquire())
            .await
            .expect("the provider should have been asked within the deadline")
            .expect("the latch is never closed")
            .forget();
    }

    /// Lets the pending request answer.
    fn release(&self) {
        self.released.add_permits(1);
    }
}

#[async_trait]
impl Provider for HeldProvider {
    fn id(&self) -> &str {
        "fake"
    }

    async fn stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        self.asked.add_permits(1);
        self.released
            .acquire()
            .await
            .expect("the latch is never closed")
            .forget();

        Ok(stream::iter(self.reply.clone()).boxed())
    }
}

/// One completed reply: `text` in a single fragment, then usage claiming
/// `input_tokens`, then a completed finish.
fn reply(text: &str, input_tokens: u64) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::TextDelta(text.to_owned()),
        ProviderEvent::Usage(Usage {
            input_tokens,
            output_tokens: 7,
            ..Usage::default()
        }),
        ProviderEvent::Finish(FinishReason::Completed),
    ]
}

/// Drains events until the turn's finish, or fails loudly instead of
/// hanging the suite.
async fn drain(events: &mut BoxStream<'static, Event>) -> Vec<Event> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut seen = Vec::new();
        loop {
            let Some(event) = events.next().await else {
                return seen;
            };
            let finished = matches!(event, Event::MessageFinished { .. });
            seen.push(event);
            if finished {
                return seen;
            }
        }
    })
    .await
    .expect("a turn should finish within the deadline")
}

/// Polls `check` until it yields, for the detached title task's writes.
async fn eventually<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    for _ in 0..500 {
        if let Some(value) = check() {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    panic!("{what} did not happen within five seconds");
}

/// The text a message carries, all text parts concatenated.
fn text_of(message: &Message) -> String {
    message.parts.iter().filter_map(Part::as_text).collect()
}

/// Every text a request carries, flattened, for content assertions.
fn request_text(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .map(text_of)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The stored info for `id`, read back through the same API the picker uses.
fn stored_info(storage: &Storage, id: &SessionId) -> SessionInfo {
    storage
        .load_info(id)
        .expect("the store is readable")
        .expect("the session exists on disk")
}

#[tokio::test]
async fn a_turn_on_a_persistent_engine_reaches_the_disk_as_it_streamed() {
    let (_dir, storage) = store();
    let engine = persistent(
        Arc::new(FakeProvider::new("one two", Duration::from_millis(1))),
        fake::MODEL,
        storage.clone(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "hi disk".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    let info = engine
        .current_session()
        .expect("the first prompt creates a session");
    let on_disk = stored_info(&storage, &info.id);
    assert_eq!(on_disk.version, storage::VERSION);
    assert_eq!(
        on_disk.usage,
        Usage {
            input_tokens: 2,
            output_tokens: 2,
            ..Usage::default()
        },
        "the fake reports word counts, and the session should have summed them"
    );
    assert_eq!(
        on_disk.context_tokens, 2,
        "context_tokens is the last request's reported input"
    );
    assert_eq!(
        on_disk.title.as_deref(),
        Some("hi disk"),
        "a fake session takes the fallback title, already on disk at finish"
    );

    let transcript = storage
        .load_transcript(&info.id)
        .expect("the transcript loads");
    assert_eq!(
        transcript.len(),
        2,
        "one prompt, one reply: {transcript:#?}"
    );

    let user = &transcript[0];
    assert_eq!(user.role, Role::User);
    assert_eq!(text_of(user), "hi disk");
    assert!(
        user.time.completed.is_some(),
        "a user message is born complete"
    );

    let assistant = &transcript[1];
    assert_eq!(assistant.role, Role::Assistant);
    assert_eq!(assistant.model.as_deref(), Some(fake::MODEL));
    assert!(
        assistant.time.completed.is_some(),
        "the finish rewrite stamps the envelope"
    );
    assert!(
        assistant.usage.is_some(),
        "the finish rewrite carries what the turn spent"
    );
    assert_eq!(
        text_of(assistant),
        "one two",
        "the streamed text and the stored text are the same text"
    );
}

#[tokio::test]
async fn a_prompt_is_on_disk_before_the_provider_is_asked_rather_than_when_the_turn_ends() {
    let (_dir, storage) = store();
    let provider = HeldProvider::new(reply("understood", 3));
    let engine = persistent(
        Arc::clone(&provider) as Arc<dyn Provider>,
        "canned",
        storage.clone(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "hold the line".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    // The one moment worth asserting in: the request is in flight and the turn
    // has finished nothing. Everything read below is what a `kill -9` here
    // would have left behind.
    provider.asked().await;

    let info = engine
        .current_session()
        .expect("the first prompt creates a session");
    let mid_turn = storage
        .load_transcript(&info.id)
        .expect("the transcript loads");
    assert_eq!(mid_turn[0].role, Role::User);
    assert_eq!(
        text_of(&mid_turn[0]),
        "hold the line",
        "the prompt's own part must be stored with it, not held until the turn ends"
    );
    assert!(
        mid_turn
            .iter()
            .all(|message| message.role == Role::User || message.time.completed.is_none()),
        "the turn is genuinely open here — nothing may be stored as finished: {mid_turn:#?}"
    );

    provider.release();
    drain(&mut events).await;

    let finished = storage
        .load_transcript(&info.id)
        .expect("the transcript loads");
    assert_eq!(finished.len(), 2, "{finished:#?}");
    assert_eq!(text_of(&finished[1]), "understood");
}

#[tokio::test]
async fn a_crash_resumes_with_the_prompt_kept_and_open_calls_closed() {
    let (_dir, storage) = store();

    // The disk state a kill -9 mid-stream leaves behind: the user's prompt
    // whole, the assistant envelope never completed, a partial text part, a
    // tool call still Running and another still Pending.
    let sid = SessionId::ascending();
    storage
        .save_info(&ganja_testkit::seeded_session_info(sid.clone(), 9))
        .expect("the seeded info writes");

    let user = Message::user("please read x");
    ganja_testkit::seed_message(&storage, &sid, &user);

    let mut aborted = Message::assistant("canned");
    aborted.parts.push(Part::text("I was about to"));
    aborted.parts.push(Part {
        id: PartId::ascending(),
        body: PartBody::Tool {
            call_id: "call_9".to_owned(),
            tool: "read".to_owned(),
            state: ToolState::Running {
                input: serde_json::json!({"path": "x.rs"}),
                metadata: serde_json::Value::Null,
                started: 5,
            },
        },
    });
    aborted.parts.push(Part {
        id: PartId::ascending(),
        body: PartBody::Tool {
            call_id: "call_10".to_owned(),
            tool: "glob".to_owned(),
            state: ToolState::Pending,
        },
    });
    assert!(
        aborted.time.completed.is_none(),
        "the crash marker is the seed"
    );
    ganja_testkit::seed_message(&storage, &sid, &aborted);

    let provider = LaneProvider::new("scripted", vec![Ok(reply("understood", 42))]);
    let engine = persistent(
        Arc::clone(&provider) as Arc<dyn Provider>,
        "canned",
        storage.clone(),
    );

    let transcript = engine.resume(&sid).await.expect("the session resumes");
    assert_eq!(transcript.len(), 2, "{transcript:#?}");

    let resumed = &transcript[1];
    assert!(
        resumed.time.completed.is_none(),
        "an aborted envelope stays aborted; nothing invents a completion"
    );
    assert_eq!(
        text_of(resumed),
        "I was about to",
        "the partial text survives exactly as far as it streamed"
    );

    let states: Vec<&ToolState> = resumed
        .parts
        .iter()
        .filter_map(|part| match &part.body {
            PartBody::Tool { state, .. } => Some(state),
            _ => None,
        })
        .collect();
    assert_eq!(states.len(), 2);
    let ToolState::Error {
        input,
        error,
        started,
        completed,
    } = states[0]
    else {
        panic!(
            "a Running call closes as Error on load, got {:?}",
            states[0]
        );
    };
    assert_eq!(
        input,
        &serde_json::json!({"path": "x.rs"}),
        "the stored input is kept"
    );
    assert!(
        error.contains("interrupted"),
        "the error should explain the interruption, got {error:?}"
    );
    assert_eq!(started, completed, "both stamps are the load's");
    assert!(
        matches!(states[1], ToolState::Error { .. }),
        "a Pending call closes too, got {:?}",
        states[1]
    );

    // The closure is persisted, not just returned: a second load sees it.
    let reloaded = storage
        .load_transcript(&sid)
        .expect("the transcript reloads");
    assert!(
        reloaded[1].parts.iter().all(|part| match &part.body {
            PartBody::Tool { state, .. } => matches!(state, ToolState::Error { .. }),
            _ => true,
        }),
        "the closures must be on disk: {reloaded:#?}"
    );

    // And the installed history answers every opened call on the next
    // request: the aborted message rides along with its closed calls.
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::SendPrompt {
            text: "continue".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("a resumed engine accepts a prompt");
    drain(&mut events).await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 1, "a pre-titled session asks for no title");
    let carried = &requests[0].messages;
    assert_eq!(
        carried.len(),
        3,
        "prompt, aborted reply, new prompt: {carried:#?}"
    );
    assert!(
        carried[1].parts.iter().all(|part| match &part.body {
            PartBody::Tool { state, .. } => matches!(state, ToolState::Error { .. }),
            _ => true,
        }),
        "every opened call must carry a result into the next request"
    );
}

#[tokio::test]
async fn session_operations_know_when_they_cannot_run() {
    // An in-memory engine keeps no sessions and says so.
    let ephemeral = Engine::new(
        Arc::new(FakeProvider::new("one", Duration::from_millis(1))),
        fake::MODEL,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    );
    assert!(
        matches!(ephemeral.sessions().await, Err(EngineError::Ephemeral)),
        "listing sessions on Engine::new should refuse as ephemeral"
    );
    assert!(
        matches!(
            ephemeral.resume(&SessionId::ascending()).await,
            Err(EngineError::Ephemeral)
        ),
        "resuming on Engine::new should refuse as ephemeral"
    );
    assert!(ephemeral.current_session().is_none());

    // An unknown id is not found, and says which id.
    let (_dir, storage) = store();
    let engine = persistent(
        Arc::new(FakeProvider::new(
            "one two three four five six seven eight nine ten",
            Duration::from_millis(30),
        )),
        fake::MODEL,
        storage,
    );
    let unknown = SessionId::ascending();
    assert!(
        matches!(
            engine.resume(&unknown).await,
            Err(EngineError::SessionNotFound { ref id }) if *id == unknown
        ),
        "an id the store never held should be SessionNotFound"
    );

    // Mid-stream, resume is refused as busy: the turn in flight is writing
    // into the session it started on.
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::SendPrompt {
            text: "stream for a while".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    assert!(
        matches!(events.next().await, Some(Event::MessageStarted { .. })),
        "the turn should have started streaming"
    );
    assert!(
        matches!(
            engine.resume(&SessionId::ascending()).await,
            Err(EngineError::Busy)
        ),
        "resume mid-turn should be Busy"
    );

    engine
        .send(Command::CancelTurn)
        .await
        .expect("a streaming turn accepts a cancel");
    drain(&mut events).await;
}

#[tokio::test]
async fn usage_and_the_context_measure_survive_a_restart() {
    let (_dir, storage) = store();
    let first = persistent(
        Arc::new(FakeProvider::new("one two three", Duration::from_millis(1))),
        fake::MODEL,
        storage.clone(),
    );
    let mut events = first.subscribe().await.expect("the first subscriber wins");

    // The fake reports input = words in the request, output = fragments, so
    // every number below is arithmetic, not coincidence.
    for prompt in ["alpha beta", "gamma"] {
        first
            .send(Command::SendPrompt {
                text: prompt.to_owned(),
                mentions: Vec::new(),
            })
            .await
            .expect("an idle engine accepts a prompt");
        drain(&mut events).await;
    }

    let sid = first
        .current_session()
        .expect("the first prompt created a session")
        .id;
    let after_two = stored_info(&storage, &sid);
    assert_eq!(
        after_two.usage,
        Usage {
            input_tokens: 8,
            output_tokens: 6,
            ..Usage::default()
        },
        "turn one spent 2 in / 3 out, turn two 6 in / 3 out, summed"
    );
    assert_eq!(
        after_two.context_tokens, 6,
        "the measure is the last request's input, not the sum"
    );
    drop(events);
    drop(first);

    // A fresh process: same store, new engine, resumed session.
    let second = persistent(
        Arc::new(FakeProvider::new("one two three", Duration::from_millis(1))),
        fake::MODEL,
        storage.clone(),
    );
    let listed = second.sessions().await.expect("the store lists");
    assert!(
        listed.iter().any(|info| info.id == sid),
        "the session should be in the listing: {listed:#?}"
    );

    let transcript = second.resume(&sid).await.expect("the session resumes");
    assert_eq!(transcript.len(), 4, "two turns stored: {transcript:#?}");
    let resumed = second
        .current_session()
        .expect("resume installs the session");
    assert_eq!(resumed.usage, after_two.usage);
    assert_eq!(resumed.context_tokens, 6);

    let mut events = second.subscribe().await.expect("the first subscriber wins");
    second
        .send(Command::SendPrompt {
            text: "delta".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("a resumed engine accepts a prompt");
    drain(&mut events).await;

    let after_three = stored_info(&storage, &sid);
    assert_eq!(
        after_three.usage,
        Usage {
            input_tokens: 18,
            output_tokens: 9,
            ..Usage::default()
        },
        "the resumed window carries both old turns, so turn three costs 10 in"
    );
    assert_eq!(after_three.context_tokens, 10);
}

#[tokio::test]
async fn an_over_budget_session_is_summarized_before_the_turn() {
    let (_dir, storage) = store();

    // claude-haiku-4-5's window is 200k; 180k stored is exactly the 90% line.
    let sid = SessionId::ascending();
    storage
        .save_info(&ganja_testkit::seeded_session_info(sid.clone(), 180_000))
        .expect("the seeded info writes");
    let old_user = Message::user("the old objective was pruning the catalog");
    ganja_testkit::seed_message(&storage, &sid, &old_user);
    let mut old_reply = Message::assistant("claude-haiku-4-5");
    old_reply.parts.push(Part::text("we removed three rows"));
    old_reply.complete();
    ganja_testkit::seed_message(&storage, &sid, &old_reply);

    let provider = LaneProvider::new(
        "anthropic",
        vec![
            Ok(reply("Summary of the early work.", 111)),
            Ok(reply("Continuing now.", 222)),
        ],
    );
    let engine = persistent(
        Arc::clone(&provider) as Arc<dyn Provider>,
        "claude-haiku-4-5",
        storage.clone(),
    );
    engine.resume(&sid).await.expect("the session resumes");

    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::SendPrompt {
            text: "next step please".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain(&mut events).await;

    // The first thing the frontend hears is the summary, arriving complete:
    // the frozen protocol has no way to close a message that is not the
    // turn's, so it is never announced half-grown.
    let Some(Event::MessageStarted { message: summary }) = seen.first() else {
        panic!("the summary should open the event stream, got {seen:#?}");
    };
    assert_eq!(summary.role, Role::Assistant);
    assert!(
        summary.time.completed.is_some(),
        "the summary arrives already complete"
    );
    assert_eq!(text_of(summary), "Summary of the early work.");

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "one summarize request, then the turn: {requests:#?}"
    );

    let summarize = &requests[0];
    assert!(
        summarize.tools.is_empty(),
        "the summarize request offers no tools"
    );
    assert_eq!(
        summarize.messages.len(),
        1,
        "core's shape: one user message carrying everything"
    );
    assert_eq!(summarize.messages[0].role, Role::User);
    let prompt = request_text(summarize);
    assert!(
        prompt.contains("Create a new anchored summary"),
        "no prior summary means the create instruction: {prompt}"
    );
    assert!(
        prompt.contains("## Objective"),
        "the ported template rides along: {prompt}"
    );
    assert!(
        prompt.contains("[User]: the old objective was pruning the catalog")
            && prompt.contains("[Assistant]: we removed three rows"),
        "the window is serialized into the prompt: {prompt}"
    );

    let turn = &requests[1];
    assert_eq!(
        turn.messages.len(),
        2,
        "the post-compact request is exactly [summary, new prompt]: {:#?}",
        turn.messages
    );
    assert_eq!(turn.messages[0].role, Role::Assistant);
    assert_eq!(text_of(&turn.messages[0]), "Summary of the early work.");
    assert_eq!(turn.messages[0].id, summary.id);
    assert_eq!(text_of(&turn.messages[1]), "next step please");
    let post_compact = request_text(turn);
    assert!(
        !post_compact.contains("old objective") && !post_compact.contains("three rows"),
        "the old window must not ride along after compaction: {post_compact}"
    );

    let info = stored_info(&storage, &sid);
    assert_eq!(
        info.summary.as_ref(),
        Some(&summary.id),
        "the window pointer names the summary message"
    );
    assert_eq!(
        info.context_tokens, 222,
        "the last request's input replaces the over-budget measure"
    );
    // Compaction is spend the user is on the hook for, so the session's
    // running total has to carry the summarize request too — not just the
    // turn that followed it. The script bills 111 and 222 input, 7 output
    // each, against a seeded total of zero.
    assert_eq!(
        info.usage,
        Usage {
            input_tokens: 333,
            output_tokens: 14,
            ..Usage::default()
        },
        "the summarize request's own tokens belong in the session's usage"
    );
    let transcript = storage
        .load_transcript(&sid)
        .expect("the transcript reloads");
    assert!(
        transcript
            .iter()
            .any(|message| message.id == summary.id && message.time.completed.is_some()),
        "the summary is persisted, complete: {transcript:#?}"
    );
}

#[tokio::test]
async fn a_cancel_during_compaction_leaves_the_window_uninstalled() {
    let (_dir, storage) = store();

    let sid = SessionId::ascending();
    storage
        .save_info(&ganja_testkit::seeded_session_info(sid.clone(), 180_000))
        .expect("the seeded info writes");
    let old_user = Message::user("seed prompt");
    ganja_testkit::seed_message(&storage, &sid, &old_user);
    let mut old_reply = Message::assistant("claude-haiku-4-5");
    old_reply.parts.push(Part::text("seed reply"));
    old_reply.complete();
    ganja_testkit::seed_message(&storage, &sid, &old_reply);

    // Forty fragments at 25ms give the cancel a full second of summarize to
    // land inside.
    let engine = persistent(
        Arc::new(FakeProvider::new(
            &"word ".repeat(40),
            Duration::from_millis(25),
        )),
        "claude-haiku-4-5",
        storage.clone(),
    );
    engine.resume(&sid).await.expect("the session resumes");

    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::SendPrompt {
            text: "cancel me".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    tokio::time::sleep(Duration::from_millis(150)).await;
    engine
        .send(Command::CancelTurn)
        .await
        .expect("a compacting turn accepts a cancel");

    let seen = drain(&mut events).await;
    let Some(Event::MessageFinished { reason, .. }) = seen.first() else {
        panic!(
            "a cancel during compaction ends in a clean finish and nothing \
             else was announced first, got {seen:#?}"
        );
    };
    assert_eq!(*reason, FinishReason::Cancelled);

    let info = stored_info(&storage, &sid);
    assert!(
        info.summary.is_none(),
        "no half-installed window: the pointer stays unset"
    );
    let transcript = storage
        .load_transcript(&sid)
        .expect("the transcript reloads");
    assert_eq!(
        transcript.len(),
        2,
        "the cancelled prompt never entered the transcript: {transcript:#?}"
    );

    engine
        .send(Command::SendPrompt {
            text: "still alive?".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("a cancelled turn leaves the engine idle");
    engine
        .send(Command::CancelTurn)
        .await
        .expect("cleanup cancel");
    drain(&mut events).await;
}

#[tokio::test]
async fn the_fake_provider_titles_from_the_prompt_without_a_request() {
    let (_dir, storage) = store();

    // The provider claims the fake id, so the title rule applies — and its
    // request log proves no title request ever happens.
    let provider = LaneProvider::new("fake", vec![Ok(reply("canned words", 3))]);
    let engine = persistent(
        Arc::clone(&provider) as Arc<dyn Provider>,
        "canned",
        storage.clone(),
    );

    // Forty-nine ASCII characters, then multibyte: character fifty lands
    // inside the Japanese run, so a byte-indexed clip would panic or tear.
    let prompt = format!("{}日本語のタイトル境界テスト", "x".repeat(49));
    let expected: String = prompt.chars().take(50).collect();

    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::SendPrompt {
            text: prompt.clone(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    let sid = engine
        .current_session()
        .expect("the first prompt created a session")
        .id;
    assert_eq!(
        stored_info(&storage, &sid).title.as_deref(),
        Some(expected.as_str()),
        "the fallback title is the clipped first prompt, on disk at finish"
    );

    // No detached task exists on this path, so a short grace period is
    // enough to catch one that should not have been spawned.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        provider.requests().len(),
        1,
        "the turn's request is the only request; a title call would be a \
         second entry"
    );
}

#[tokio::test]
async fn a_real_provider_titles_through_its_cheapest_stablemate() {
    let (_dir, storage) = store();

    let provider = LaneProvider::new(
        "anthropic",
        vec![
            Ok(reply("sure thing", 10)),
            Ok(reply("Fixing the flux capacitor", 5)),
        ],
    );
    let engine = persistent(
        Arc::clone(&provider) as Arc<dyn Provider>,
        "claude-sonnet-5",
        storage.clone(),
    );

    let prompt = "fix the flux capacitor please";
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::SendPrompt {
            text: prompt.to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    let sid = engine
        .current_session()
        .expect("the first prompt created a session")
        .id;

    let title_request =
        eventually("the title request", || provider.requests().get(1).cloned()).await;
    assert!(
        title_request.tools.is_empty(),
        "the title request offers no tools"
    );
    assert_eq!(
        title_request.model, "claude-haiku-4-5",
        "the cheapest anthropic catalog entry by input price"
    );
    assert!(
        title_request
            .system
            .as_deref()
            .is_some_and(|system| system.contains("title generator")),
        "the ported title prompt rides as the system prompt"
    );
    assert_eq!(title_request.messages.len(), 2);
    assert!(
        text_of(&title_request.messages[0]).starts_with("Generate a title for this conversation:"),
        "upstream's instruction opens the request"
    );
    assert_eq!(
        text_of(&title_request.messages[1]),
        prompt,
        "the session's first user message is the context"
    );

    let titled = eventually("the title write", || stored_info(&storage, &sid).title).await;
    assert_eq!(titled, "Fixing the flux capacitor");

    // Title bookkeeping stays out of the session's accounting: the measure
    // and the spend belong to the conversation, not to metadata calls.
    let info = stored_info(&storage, &sid);
    assert_eq!(info.context_tokens, 10);
    assert_eq!(info.usage.input_tokens, 10);
}

#[tokio::test]
async fn a_failed_title_request_falls_back_to_the_prompt() {
    let (_dir, storage) = store();

    let provider = LaneProvider::new(
        "anthropic",
        vec![
            Ok(reply("done", 4)),
            Err(ProviderError::Transport("boom".to_owned())),
        ],
    );
    let engine = persistent(
        Arc::clone(&provider) as Arc<dyn Provider>,
        "claude-sonnet-5",
        storage.clone(),
    );

    let prompt = "please refactor the parser module";
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::SendPrompt {
            text: prompt.to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    let sid = engine
        .current_session()
        .expect("the first prompt created a session")
        .id;
    let titled = eventually("the fallback title write", || {
        stored_info(&storage, &sid).title
    })
    .await;
    assert_eq!(
        titled, prompt,
        "a dead title request falls back to the clipped first prompt"
    );
}

#[tokio::test]
async fn an_unsummarizable_history_skips_compaction_instead_of_failing() {
    let (_dir, storage) = store();

    // Over the 90% trigger AND too big to summarize: the serialized history
    // alone estimates past the whole context window minus the summary's
    // reserved output, so the fit guard must refuse to even ask.
    let sid = SessionId::ascending();
    storage
        .save_info(&ganja_testkit::seeded_session_info(sid.clone(), 180_000))
        .expect("the seeded info writes");
    let old_user = Message::user("start");
    ganja_testkit::seed_message(&storage, &sid, &old_user);
    let mut huge = Message::assistant("claude-haiku-4-5");
    huge.parts.push(Part::text("y".repeat(800_000)));
    huge.complete();
    ganja_testkit::seed_message(&storage, &sid, &huge);

    let provider = LaneProvider::new("anthropic", vec![Ok(reply("fine", 5))]);
    let engine = persistent(
        Arc::clone(&provider) as Arc<dyn Provider>,
        "claude-haiku-4-5",
        storage.clone(),
    );
    engine.resume(&sid).await.expect("the session resumes");

    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::SendPrompt {
            text: "go".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        1,
        "no summarize request may be sent for a prompt that cannot fit"
    );
    assert_eq!(
        requests[0].messages.len(),
        3,
        "the turn proceeds uncompacted, oversized window and all"
    );
    assert!(
        stored_info(&storage, &sid).summary.is_none(),
        "skipping the summarize leaves no window pointer behind"
    );
}

/// Regression for the P3 finding: the turn slot used to be released before
/// `MessageFinished` was queued, so a prompt accepted in that window could
/// interleave its opening events ahead of the previous turn's finish. The
/// multi-thread flavor is the point — on a single thread the race cannot be
/// observed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finish_is_never_overtaken_by_the_next_turns_events() {
    const TURNS: usize = 25;

    let (_dir, storage) = store();
    let engine = Arc::new(persistent(
        Arc::new(FakeProvider::new("one two", Duration::ZERO)),
        fake::MODEL,
        storage,
    ));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    // Hammers the boundary: a prompt is offered the instant the engine will
    // take one, which before the fix was the gap between slot release and
    // the finish event reaching the queue.
    let pusher = tokio::spawn({
        let engine = Arc::clone(&engine);
        async move {
            let mut accepted = 0usize;
            while accepted < TURNS {
                match engine
                    .send(Command::SendPrompt {
                        text: format!("turn {accepted}"),
                        mentions: Vec::new(),
                    })
                    .await
                {
                    Ok(()) => accepted += 1,
                    Err(EngineError::Busy) => tokio::task::yield_now().await,
                    Err(other) => panic!("only Busy is a legal refusal at the boundary: {other}"),
                }
            }
        }
    });

    let mut open_turns = 0usize;
    let mut finished = 0usize;
    while finished < TURNS {
        let event = tokio::time::timeout(Duration::from_secs(30), events.next())
            .await
            .expect("the stream keeps moving")
            .expect("the engine outlives the drain");
        match &event {
            Event::MessageStarted { message } if message.role == Role::User => {
                open_turns += 1;
                assert!(
                    open_turns <= 1,
                    "a turn opened before the previous finish was delivered"
                );
            }
            Event::MessageFinished { .. } => {
                assert!(open_turns > 0, "a finish arrived with no turn open");
                open_turns -= 1;
                finished += 1;
            }
            _ => {}
        }
    }

    pusher.await.expect("the prompt pusher finishes cleanly");
}
