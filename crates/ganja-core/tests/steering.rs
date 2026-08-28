//! Steering: a message typed while a turn is running joins **that** turn.
//!
//! Spec: no upstream port. Upstream v1.18.22 has a mid-turn join
//! (`session/prompt.ts:1052-1071` with `effect/runner.ts:115-137`) whose whole
//! contract is implicit — persist a message and hope the running loop re-reads
//! it — and this build refused that shape in favour of a named command with a
//! correlation id and a typed refusal (**D450**). What is pinned here is
//! therefore ganja's own contract, mirroring Codex `codex-rs`'s
//! `session/input_queue.rs` + `session/inject.rs` design rather than any
//! TypeScript:
//!
//! - a steer is drained at a **step boundary** — after the tool results are
//!   in, before the request that carries them — and again **before the turn
//!   finishes**, so a message that arrives during the model's last request
//!   continues the turn instead of vanishing;
//! - every outcome is typed: consumed (`Event::SteerConsumed`), refused
//!   (`EngineError::NotStreaming`), or left unconsumed for a frontend to
//!   re-own;
//! - a cancelled turn drains nothing;
//! - a steered message is an ordinary user message on disk, so a resumed
//!   session replays exactly what the live turn asked.
//!
//! The one-turn-at-a-time contract is untouched throughout: the turn stays
//! singular, and `EngineError::Busy` still refuses a second prompt. The tests
//! that pin *that* live in `engine.rs` and `agent_loop.rs` and are not
//! restated here.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::{self, BoxStream};
use ganja_core::permission::Permissions;
use ganja_core::protocol::{
    Command, Event, FinishReason, Mention, Message, PartBody, PermissionId, PermissionReply, Role,
    SessionId,
};
use ganja_core::provider::{ChatRequest, Provider, ProviderError, ProviderEvent};
use ganja_core::tool::Registry;
use ganja_core::{Engine, EngineError, Storage};
use ganja_testkit::{RecorderTool, ScriptedProvider, drain, says};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// A tool-call script fragment.
fn call(id: &str, tool: &str, json: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ToolCallStart { id: id.to_owned(), name: tool.to_owned() },
        ProviderEvent::ToolCallDelta { id: id.to_owned(), json: json.to_owned() },
        ProviderEvent::ToolCallEnd { id: id.to_owned() },
    ]
}

/// A provider that answers from a script and **pauses** before answering the
/// request whose index is `gate_at`, until the test lets it go.
///
/// The seam a steer needs: a scripted stream answers instantly, so there is no
/// other way to land a message inside the model's *last* request — the one the
/// finish path drains after. Everything else about it is
/// [`ScriptedProvider`]'s shape.
struct GatedProvider {
    scripts: Mutex<VecDeque<Vec<ProviderEvent>>>,
    seen: Arc<Mutex<Vec<ChatRequest>>>,
    gate_at: usize,
    entered: mpsc::Sender<()>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
}

/// A [`GatedProvider`] and the three handles a test drives it by.
struct Gated {
    provider: Arc<GatedProvider>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    /// Fires when the gated request arrives.
    arrived: mpsc::Receiver<()>,
    /// Lets that request through.
    release: oneshot::Sender<()>,
}

impl Gated {
    /// Builds the provider and its three handles: the log it records into, the
    /// signal that the gated request arrived, and the release that lets it go.
    fn new(scripts: Vec<Vec<ProviderEvent>>, gate_at: usize) -> Self {
        let (entered, arrived) = mpsc::channel(4);
        let (open, release) = oneshot::channel();
        let seen: Arc<Mutex<Vec<ChatRequest>>> = Arc::default();

        Self {
            provider: Arc::new(GatedProvider {
                scripts: Mutex::new(scripts.into()),
                seen: Arc::clone(&seen),
                gate_at,
                entered,
                release: Mutex::new(Some(release)),
            }),
            requests: seen,
            arrived,
            release: open,
        }
    }
}

#[async_trait]
impl Provider for GatedProvider {
    fn id(&self) -> &str {
        "gated"
    }

    async fn stream(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let index = {
            let mut seen = self.seen.lock().expect("the request log is never poisoned");
            seen.push(request);

            seen.len() - 1
        };

        if index == self.gate_at {
            let _ = self.entered.send(()).await;
            // Taken out of the lock before the await: nothing holds a
            // std::sync guard across a suspension point.
            let waiting = self.release.lock().expect("the release is never poisoned").take();
            if let Some(waiting) = waiting {
                let _ = waiting.await;
            }
        }

        let script = self
            .scripts
            .lock()
            .expect("the scripts are never poisoned")
            .pop_front()
            .unwrap_or_else(|| vec![ProviderEvent::Finish(FinishReason::Completed)]);

        Ok(stream::iter(script).boxed())
    }
}

/// A registry holding one gated tool.
///
/// Named `shell` because the builtin rules put that one in front of the user
/// by default: a permission dialog is the deterministic hold point these tests
/// need mid-turn, and reaching it through the default ruleset means no test
/// here has to invent one (the idiom `tests/agent_loop.rs` already uses).
fn gated_tool() -> Arc<Registry> {
    let (tool, _calls) = RecorderTool::new("shell", "shell ran", "found it");

    Arc::new(Registry::new(vec![tool]))
}

fn prompt(text: &str) -> Command {
    Command::SendPrompt {
        text: text.to_owned(),
        mentions: Vec::new(),
        skills: Vec::new(),
        session_mentions: Vec::new(),
        peers: Vec::new(),
    }
}

fn steer(id: &str, text: &str) -> Command {
    Command::Steer {
        id: id.to_owned(),
        text: text.to_owned(),
        mentions: Vec::new(),
        skills: Vec::new(),
        session_mentions: Vec::new(),
        peers: Vec::new(),
    }
}

/// One line per event, in the vocabulary these tests assert order in.
fn shape(event: &Event) -> String {
    match event {
        Event::MessageStarted { message, .. } => match message.role {
            Role::User => format!(
                "started:user:{}",
                message
                    .parts
                    .iter()
                    .find_map(ganja_core::protocol::Part::as_text)
                    .unwrap_or_default()
            ),
            Role::Assistant => "started:assistant".to_owned(),
        },
        Event::SteerConsumed { id, .. } => format!("steer_consumed:{id}"),
        Event::PartStarted { part, .. } => match &part.body {
            PartBody::Tool { call_id, .. } => format!("part:tool:{call_id}"),
            PartBody::StepStart => "part:step_start".to_owned(),
            PartBody::Text { .. } => "part:text".to_owned(),
            _ => "part:other".to_owned(),
        },
        Event::PartUpdated { part, .. } => match &part.body {
            PartBody::Tool { call_id, .. } => format!("updated:{call_id}"),
            _ => "updated:other".to_owned(),
        },
        Event::PermissionRequested { .. } => "perm_requested".to_owned(),
        Event::PermissionReplied { .. } => "perm_replied".to_owned(),
        Event::MessageFinished { reason, .. } => format!("finished:{reason:?}"),
        other => format!("other:{other:?}"),
    }
}

/// Drains until a permission request arrives, so a test can act while the turn
/// is provably mid-flight and blocked.
async fn until_permission(events: &mut BoxStream<'static, Event>) -> (PermissionId, Vec<Event>) {
    let mut seen = Vec::new();

    loop {
        let event =
            events.next().await.expect("a permission request should arrive before the stream ends");
        seen.push(event.clone());

        if let Event::PermissionRequested { id, .. } = event {
            return (id, seen);
        }
        assert!(
            !matches!(event, Event::MessageFinished { .. }),
            "the turn finished without asking; events so far: {seen:?}"
        );
    }
}

/// The user text of every message in `request`, in order — what the model was
/// actually told, and in what order it was told it.
fn user_text(request: &ChatRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .map(|message| {
            message
                .parts
                .iter()
                .filter_map(ganja_core::protocol::Part::as_text)
                .collect::<Vec<_>>()
                .join("")
        })
        .collect()
}

/// **F4, drain at the step boundary.** A steer that lands while a tool call is
/// waiting on the user joins the turn the moment that call resolves: its
/// `SteerConsumed` precedes its own `MessageStarted`, both land after the
/// tool's parts, and the request that follows carries it.
#[tokio::test]
async fn a_steer_joins_the_running_turn_at_its_next_step_boundary() {
    let mut first = vec![ProviderEvent::TextDelta("looking".to_owned())];
    first.extend(call("call_1", "shell", r#"{"key":"a"}"#));
    first.push(ProviderEvent::Finish(FinishReason::Completed));

    let (provider, requests) =
        ScriptedProvider::strict("recorder", vec![first, says("done"), says("really done")]);
    let engine = Engine::new(provider, "scripted-model", gated_tool(), Permissions::default());
    let mut events = engine.subscribe().await.expect("a subscription");

    engine.send(prompt("go")).await.expect("the prompt starts");
    let (permission, mut seen) = until_permission(&mut events).await;

    // Typed while the dialog is open: the message waits for the boundary the
    // dialog is holding, and touches nothing about the dialog itself.
    engine
        .send(steer("steer-1", "actually, stop after this"))
        .await
        .expect("a steer reaches a running turn");
    engine
        .send(Command::ReplyPermission { id: permission, reply: PermissionReply::Once })
        .await
        .expect("the reply is never refused");

    seen.extend(drain(&mut events).await);
    let shapes: Vec<String> = seen.iter().map(shape).collect();

    let consumed = shapes
        .iter()
        .position(|line| line == "steer_consumed:steer-1")
        .unwrap_or_else(|| panic!("the steer should be consumed; got {shapes:?}"));
    assert_eq!(
        shapes.get(consumed + 1).map(String::as_str),
        Some("started:user:actually, stop after this"),
        "the id is announced immediately before the message it names: {shapes:?}"
    );
    let replied =
        shapes.iter().position(|line| line == "perm_replied").expect("the dialog was answered");
    assert!(consumed > replied, "the drain is at the boundary the dialog was holding: {shapes:?}");
    assert_eq!(
        shapes.iter().filter(|line| line.starts_with("finished:")).count(),
        1,
        "the turn stays singular: {shapes:?}"
    );

    let requests = requests.lock().expect("the request log is never poisoned");
    assert_eq!(
        requests.len(),
        2,
        "the tool round bought the second request, and the steer rides it"
    );
    assert_eq!(
        user_text(&requests[1]),
        ["go", "actually, stop after this"],
        "the steered message rides after the prompt it corrects"
    );
    assert_eq!(
        requests[1].messages.iter().map(|message| message.role).collect::<Vec<_>>(),
        [Role::User, Role::Assistant, Role::User],
        "and after the reply it interrupted, which is where its stored id sorts"
    );
}

/// **F4, pre-mortem #1 — the steer that vanished.** The model's last request
/// carries no tool calls, so nothing would bring the loop round again; the
/// finish path checks the mailbox first and the turn continues instead of
/// ending.
#[tokio::test]
async fn a_steer_arriving_before_the_last_answer_continues_the_turn() {
    // Gated on request 0: the steer lands while the model's first — and, but
    // for the steer, only — request is in flight.
    let Gated { provider, requests, mut arrived, release } =
        Gated::new(vec![says("all done"), says("and now really done")], 0);
    let engine = Engine::new(
        provider,
        "gated-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("a subscription");

    engine.send(prompt("go")).await.expect("the prompt starts");
    arrived.recv().await.expect("the request reaches the gate");

    engine.send(steer("steer-1", "one more thing")).await.expect("a steer reaches a running turn");
    let _ = release.send(());

    let seen = drain(&mut events).await;
    let shapes: Vec<String> = seen.iter().map(shape).collect();

    assert!(
        shapes.contains(&"steer_consumed:steer-1".to_owned()),
        "a turn that would otherwise have ended took the message first: {shapes:?}"
    );
    let consumed =
        shapes.iter().position(|line| line == "steer_consumed:steer-1").expect("checked above");
    let finished =
        shapes.iter().position(|line| line.starts_with("finished:")).expect("the turn ends");
    assert!(consumed < finished, "the finish tail is not reached while a steer waits: {shapes:?}");
    assert_eq!(
        shapes.iter().filter(|line| line.starts_with("finished:")).count(),
        1,
        "and it is still one turn: {shapes:?}"
    );

    let requests = requests.lock().expect("the request log is never poisoned");
    assert_eq!(requests.len(), 2, "the waiting steer bought a second request");
    assert_eq!(user_text(&requests[1]), ["go", "one more thing"]);
}

/// **F4, cancel.** A cancelled turn drains nothing: the message is never
/// announced, never persisted and never answered, which is what leaves it for
/// the frontend's fallback lane to re-own.
#[tokio::test]
async fn a_cancelled_turn_leaves_its_steers_unconsumed() {
    let mut first = vec![ProviderEvent::TextDelta("looking".to_owned())];
    first.extend(call("call_1", "shell", r#"{"key":"a"}"#));
    first.push(ProviderEvent::Finish(FinishReason::Completed));

    let (provider, requests) = ScriptedProvider::named("recorder", vec![first, says("done")]);
    let engine = Engine::new(provider, "scripted-model", gated_tool(), Permissions::default());
    let mut events = engine.subscribe().await.expect("a subscription");

    engine.send(prompt("go")).await.expect("the prompt starts");
    let (_permission, mut seen) = until_permission(&mut events).await;

    engine.send(steer("steer-1", "never mind")).await.expect("a steer reaches a running turn");
    engine.send(Command::CancelTurn).await.expect("a cancel is never refused");

    seen.extend(drain(&mut events).await);
    let shapes: Vec<String> = seen.iter().map(shape).collect();

    assert!(
        !shapes.iter().any(|line| line.starts_with("steer_consumed")),
        "a cancelled turn consumes nothing: {shapes:?}"
    );
    assert!(
        !shapes.iter().any(|line| line == "started:user:never mind"),
        "and puts nothing in the transcript: {shapes:?}"
    );
    assert_eq!(shapes.last().map(String::as_str), Some("finished:Cancelled"), "{shapes:?}");
    assert_eq!(requests.lock().expect("the request log").len(), 1, "no request ever carried it");
}

/// **F4, the other race.** A steer that arrives with nothing streaming is
/// refused by type rather than quietly promoted to a turn of its own: which of
/// the two a message is belongs to whoever typed it.
#[tokio::test]
async fn a_steer_with_nothing_streaming_is_refused_as_not_streaming() {
    let (provider, _requests) = ScriptedProvider::new(vec![says("hi")]);
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("a subscription");

    let refused = engine.send(steer("steer-1", "nobody is listening")).await;
    assert!(matches!(refused, Err(EngineError::NotStreaming)), "got {refused:?}");

    // And it started nothing: the engine is still idle, and the next prompt is
    // the session's first turn.
    engine.send(prompt("go")).await.expect("the prompt starts");
    let seen = drain(&mut events).await;
    let shapes: Vec<String> = seen.iter().map(shape).collect();
    assert_eq!(
        shapes.iter().filter(|line| line.starts_with("started:user")).count(),
        1,
        "the refused steer is in no transcript: {shapes:?}"
    );
}

/// **F4, read-at-send.** A steer's mentions are references, resolved when the
/// request that carries them is built — so a file edited between the steer and
/// the boundary reaches the model as it is *then*, exactly as a prompt's
/// mention does (`lib.rs`'s `PartBody::File`).
#[tokio::test]
async fn a_steered_mention_is_read_when_the_request_carrying_it_is_built() {
    let root = tempfile::tempdir().expect("a temporary directory");
    let note = root.path().join("note.txt");
    std::fs::write(&note, "the old contents").expect("the fixture writes");

    let mut first = vec![ProviderEvent::TextDelta("looking".to_owned())];
    first.extend(call("call_1", "shell", r#"{"key":"a"}"#));
    first.push(ProviderEvent::Finish(FinishReason::Completed));

    let (provider, requests) =
        ScriptedProvider::strict("recorder", vec![first, says("done"), says("really done")]);
    let engine = Engine::new(provider, "scripted-model", gated_tool(), Permissions::default());
    let mut events = engine.subscribe().await.expect("a subscription");

    engine.send(prompt("go")).await.expect("the prompt starts");
    let (permission, _seen) = until_permission(&mut events).await;

    // Absolute, as every mention fixture in this suite is: the file lives in a
    // temporary directory rather than in whatever checkout is running the
    // tests (`tests/mentions.rs` says the same).
    let named = note.to_string_lossy().into_owned();
    engine
        .send(Command::Steer {
            id: "steer-1".to_owned(),
            text: format!("read @{named}"),
            mentions: vec![Mention { path: named.clone(), ..Default::default() }],
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("a steer reaches a running turn");
    // Written after the steer and before the boundary that drains it: what
    // the model sees has to be this, not what was there when it was typed.
    std::fs::write(&note, "the new contents").expect("the fixture rewrites");

    engine
        .send(Command::ReplyPermission { id: permission, reply: PermissionReply::Once })
        .await
        .expect("the reply is never refused");
    drain(&mut events).await;

    let requests = requests.lock().expect("the request log is never poisoned");
    // A text mention is inlined into the message that carried it, so what the
    // model was told is the user text of the request that followed the drain.
    let carried = user_text(&requests[1]).join("\n");
    assert!(
        carried.contains("the new contents"),
        "the mention is read when the request is built: {carried}"
    );
    assert!(!carried.contains("the old contents"), "and not when the steer was typed: {carried}");
    assert!(
        requests[1].messages.iter().all(|message| message
            .parts
            .iter()
            .all(|part| !matches!(part.body, PartBody::File { .. }))),
        "a resolved mention leaves no unresolved reference behind"
    );
}

/// **F4, resume.** A steered message is an ordinary user message on disk: it
/// sorts after the reply it interrupted, survives the process, and the request
/// a resumed session builds carries it in the same place the live turn did.
#[tokio::test]
async fn a_steered_message_replays_as_an_ordinary_user_message_on_resume() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let storage = Storage::open(directory.path().join("storage"));

    let mut first = vec![ProviderEvent::TextDelta("looking".to_owned())];
    first.extend(call("call_1", "shell", r#"{"key":"a"}"#));
    first.push(ProviderEvent::Finish(FinishReason::Completed));

    let (provider, requests) = ScriptedProvider::named(
        "recorder",
        vec![first, says("done"), says("carrying on"), says("and again")],
    );
    let engine = Engine::persistent(
        Arc::clone(&provider) as Arc<dyn Provider>,
        "scripted-model",
        gated_tool(),
        Permissions::default(),
        storage.clone(),
    );
    let mut events = engine.subscribe().await.expect("a subscription");

    engine.send(prompt("go")).await.expect("the prompt starts");
    let (permission, _seen) = until_permission(&mut events).await;
    engine.send(steer("steer-1", "and this too")).await.expect("a steer reaches a running turn");
    engine
        .send(Command::ReplyPermission { id: permission, reply: PermissionReply::Once })
        .await
        .expect("the reply is never refused");
    drain(&mut events).await;

    let session: SessionId = storage
        .list_sessions()
        .expect("the store lists its sessions")
        .first()
        .expect("the turn created one")
        .id
        .clone();
    let stored = storage.load_transcript(&session).expect("the transcript reads back");
    assert_eq!(
        stored.iter().map(|message| message.role).collect::<Vec<_>>(),
        [Role::User, Role::Assistant, Role::User],
        "the steer sorts after the reply it interrupted"
    );
    assert_eq!(
        stored
            .last()
            .and_then(|message| message.parts.first())
            .and_then(ganja_core::protocol::Part::as_text),
        Some("and this too")
    );

    // A fresh engine over the same store, resuming that session: the request
    // its next prompt builds carries the steered message exactly where the
    // live turn carried it.
    let resumed = Engine::persistent(
        provider as Arc<dyn Provider>,
        "scripted-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage,
    );
    let mut resumed_events = resumed.subscribe().await.expect("a subscription");
    let replayed: Vec<Message> = resumed.resume(&session).await.expect("the session resumes");
    assert_eq!(
        replayed.iter().map(|message| message.role).collect::<Vec<_>>(),
        [Role::User, Role::Assistant, Role::User]
    );

    resumed.send(prompt("next")).await.expect("the prompt starts");
    drain(&mut resumed_events).await;

    let requests = requests.lock().expect("the request log is never poisoned");
    let last = requests.last().expect("the resumed turn asked");
    assert_eq!(
        user_text(last),
        ["go", "and this too", "next"],
        "a resumed session replays what the live turn asked"
    );
}

/// **F4, two at once.** Two steers waiting at the same boundary are drained in
/// arrival order, each announced before its own message, and both reach the
/// request that follows.
#[tokio::test]
async fn two_steers_waiting_at_one_boundary_drain_in_the_order_they_arrived() {
    let mut first = vec![ProviderEvent::TextDelta("looking".to_owned())];
    first.extend(call("call_1", "shell", r#"{"key":"a"}"#));
    first.push(ProviderEvent::Finish(FinishReason::Completed));

    let (provider, requests) =
        ScriptedProvider::named("recorder", vec![first, says("done"), says("really done")]);
    let engine = Engine::new(provider, "scripted-model", gated_tool(), Permissions::default());
    let mut events = engine.subscribe().await.expect("a subscription");

    engine.send(prompt("go")).await.expect("the prompt starts");
    let (permission, mut seen) = until_permission(&mut events).await;

    engine
        .send(steer("steer-1", "first correction"))
        .await
        .expect("a steer reaches a running turn");
    engine
        .send(steer("steer-2", "second correction"))
        .await
        .expect("a steer reaches a running turn");
    engine
        .send(Command::ReplyPermission { id: permission, reply: PermissionReply::Once })
        .await
        .expect("the reply is never refused");

    seen.extend(drain(&mut events).await);
    let shapes: Vec<String> = seen.iter().map(shape).collect();
    let steering: Vec<&str> = shapes
        .iter()
        .filter(|line| line.starts_with("steer_consumed") || line.starts_with("started:user"))
        .map(String::as_str)
        .collect();

    assert_eq!(
        steering,
        [
            "started:user:go",
            "steer_consumed:steer-1",
            "started:user:first correction",
            "steer_consumed:steer-2",
            "started:user:second correction",
        ],
        "{shapes:?}"
    );

    let requests = requests.lock().expect("the request log is never poisoned");
    assert_eq!(user_text(&requests[1]), ["go", "first correction", "second correction"]);
}

/// **F4, the Busy contract holds.** Steering adds a lane; it takes none away.
/// A `SendPrompt` while a turn runs is refused exactly as it always was.
#[tokio::test]
async fn steering_leaves_the_busy_refusal_where_it_was() {
    let mut first = vec![ProviderEvent::TextDelta("looking".to_owned())];
    first.extend(call("call_1", "shell", r#"{"key":"a"}"#));
    first.push(ProviderEvent::Finish(FinishReason::Completed));

    let (provider, _requests) = ScriptedProvider::named("recorder", vec![first, says("done")]);
    let engine = Engine::new(provider, "scripted-model", gated_tool(), Permissions::default());
    let mut events = engine.subscribe().await.expect("a subscription");

    engine.send(prompt("go")).await.expect("the prompt starts");
    let (permission, _seen) = until_permission(&mut events).await;

    let refused = engine.send(prompt("a second turn")).await;
    assert!(matches!(refused, Err(EngineError::Busy)), "got {refused:?}");

    engine
        .send(Command::ReplyPermission { id: permission, reply: PermissionReply::Once })
        .await
        .expect("the reply is never refused");
    drain(&mut events).await;
}
