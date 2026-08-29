use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::{self, BoxStream};
use tokio_util::sync::CancellationToken;

use super::{
    Engine, EngineError, STALE_FILES, STORELESS, message_chars, send_message, stale_notice,
    subagent, teammate,
};
use crate::config::TeamlessSend;
use crate::permission::Permissions;
use crate::protocol::{
    Command, Event, FinishReason, Message, Part, PermissionReply, RevertScope, Role, Usage,
};
use crate::provider::fake::MODEL;
use crate::provider::{ChatRequest, FakeProvider, Provider, ProviderError, ProviderEvent};
use crate::storage::{self, SessionId, SessionInfo, Storage};
use crate::tool::{FileTimes, Registry};

/// How long a drain that should complete promptly is given before the
/// test calls it wedged. Generous against a loaded machine, and reached
/// only when delivery is broken — a green run never waits on it.
const DRAIN_PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);

/// An engine over `provider` with no tools and default rules, which is
/// all these tests need: they prove the turn lifecycle, not the loop.
fn bare(provider: Arc<dyn Provider>, model: &str) -> Engine {
    Engine::new(provider, model, Arc::new(Registry::new(Vec::new())), Permissions::default())
}

fn engine() -> Engine {
    bare(Arc::new(FakeProvider::new("one two", std::time::Duration::from_millis(1))), MODEL)
}

/// Records what it was asked and answers with a scripted stream.
struct ScriptedProvider {
    events: Vec<ProviderEvent>,
    failure: Option<ProviderError>,
    seen: Arc<Mutex<Vec<ChatRequest>>>,
}

impl ScriptedProvider {
    fn new(events: Vec<ProviderEvent>) -> Self {
        Self { events, failure: None, seen: Arc::default() }
    }

    fn failing(failure: ProviderError) -> Self {
        Self { events: Vec::new(), failure: Some(failure), seen: Arc::default() }
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        "scripted"
    }

    async fn stream(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        self.seen.lock().expect("the request log is never poisoned").push(request);

        match &self.failure {
            Some(failure) => Err(failure.clone()),
            None => Ok(stream::iter(self.events.clone()).boxed()),
        }
    }
}

/// Drains events until the turn finishes, returning everything seen.
async fn drain(events: &mut BoxStream<'static, Event>) -> Vec<Event> {
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
}

/// The text a transcript rebuilt from `events` alone would show.
fn replay(events: &[Event]) -> String {
    let mut messages: Vec<Message> = Vec::new();

    for event in events {
        match event {
                Event::MessageStarted {
                    session_id: _,
                    message,
                } => messages.push(message.clone()),
                Event::PartStarted {
                    session_id: _,
                    message_id,
                    part,
                } => {
                    if let Some(message) = messages.iter_mut().find(|it| it.id == *message_id) {
                        message.parts.push(part.clone());
                    }
                }
                Event::PartDelta {
                    session_id: _,
                    message_id,
                    part_id,
                    delta,
                } => {
                    if let Some(text) = messages
                        .iter_mut()
                        .find(|it| it.id == *message_id)
                        .and_then(|message| message.parts.iter_mut().find(|it| it.id == *part_id))
                        .and_then(crate::protocol::Part::as_text_mut)
                    {
                        text.push_str(delta);
                    }
                }
                Event::MessageFinished { .. }
                | Event::PartUpdated { .. }
                | Event::PermissionRequested { .. }
                | Event::SteerConsumed { .. }
                | Event::PermissionReplied { .. }
                | Event::QuestionAsked { .. }
                | Event::QuestionReplied { .. }
                | Event::QuestionRejected { .. }
                | Event::RevertChanged { .. }
                | Event::AgentChanged { .. }
                | Event::PermissionModeChanged { .. }
                | Event::CompactionProgress { .. }
                // No replayed text, permanently: a hold's whole point is
                // that nothing reached the transcript, and even a released
                // message arrives as a peer part on the steer lane — which
                // `Part::as_text` excludes — never as replayed text (D524).
                | Event::PeerHeld { .. }
                | Event::PeerHoldSettled { .. }
                // A receipt's model-facing half is its own batched
                // `<peer_receipt>` text part (D534); the event itself
                // carries nothing this replay collects.
                | Event::PeerReceipt { .. }
                | Event::EffortChanged { .. } => {}
            }
    }

    messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter_map(crate::protocol::Part::as_text)
        .collect()
}

#[tokio::test]
async fn a_turn_reports_both_messages_and_streams_the_reply_into_one_part() {
    let engine = engine();
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "hi".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    let seen = drain(&mut events).await;

    let Some(Event::MessageStarted { session_id: _, message: user }) = seen.first() else {
        panic!("a turn should open with the user's message, got {seen:?}");
    };
    assert_eq!(user.role, Role::User);
    assert_eq!(user.parts.first().and_then(|part| part.as_text()), Some("hi"));

    let Some(Event::MessageStarted { session_id: _, message: assistant }) = seen.get(1) else {
        panic!("the reply's envelope should follow, got {seen:?}");
    };
    assert_eq!(assistant.role, Role::Assistant);
    assert_eq!(assistant.model.as_deref(), Some(MODEL));
    assert!(assistant.parts.is_empty(), "the reply starts empty");

    assert_eq!(
        seen.iter()
            .filter(|event| matches!(
                event,
                Event::PartStarted { part, .. } if part.as_text().is_some()
            ))
            .count(),
        1,
        "streamed text belongs to one part, got {seen:?}"
    );
    assert_eq!(replay(&seen), "hione two");

    let Some(Event::MessageFinished { session_id: _, message_id, reason, usage, error, completed }) =
        seen.last()
    else {
        panic!("a turn always ends with a finish, got {seen:?}");
    };
    assert_eq!(*message_id, assistant.id);
    assert_eq!(*reason, FinishReason::Completed);
    assert_eq!(*usage, Some(Usage { input_tokens: 1, output_tokens: 2, ..Usage::default() }));
    assert!(error.is_none());
    assert!(*completed >= assistant.time.created);
}

#[tokio::test]
async fn a_second_turn_carries_the_first_one_in_its_request() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        ProviderEvent::TextDelta("sure".to_owned()),
        ProviderEvent::Finish(FinishReason::Completed),
    ]));
    let seen = Arc::clone(&provider.seen);
    let engine = bare(provider, "scripted-model");
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    for prompt in ["first", "second"] {
        engine
            .send(Command::SendPrompt {
                text: prompt.to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("an idle engine accepts a prompt");
        drain(&mut events).await;
    }

    let requests = seen.lock().expect("the request log is never poisoned");
    let first = requests.first().expect("the first turn asked the provider");
    assert_eq!(first.model, "scripted-model");
    assert!(first.system.is_none(), "an engine nobody configured asks without a system prompt");
    assert_eq!(first.messages.len(), 1, "the first turn has no history");

    let second = requests.get(1).expect("the second turn asked too");
    let transcript: Vec<(&str, Option<&str>)> = second
        .messages
        .iter()
        .map(|message| {
            (
                message.model.as_deref().unwrap_or("user"),
                // The first text part: an assistant message now opens
                // with a step marker before anything it said.
                message.parts.iter().find_map(crate::protocol::Part::as_text),
            )
        })
        .collect();
    assert_eq!(
        transcript,
        vec![("user", Some("first")), ("scripted-model", Some("sure")), ("user", Some("second")),],
        "the second turn should carry the first one"
    );
}

#[tokio::test]
async fn a_provider_that_cannot_answer_still_finishes_the_turn() {
    let engine = bare(
        Arc::new(ScriptedProvider::failing(ProviderError::Auth(
            "ANTHROPIC_API_KEY is unset".to_owned(),
        ))),
        "scripted-model",
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "hi".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    let seen = drain(&mut events).await;
    let Some(Event::MessageFinished { reason, error, .. }) = seen.last() else {
        panic!("a failed turn still finishes, got {seen:?}");
    };

    assert_eq!(*reason, FinishReason::Failed);
    assert!(
        error.as_deref().is_some_and(|error| error.contains("ANTHROPIC_API_KEY")),
        "the refusal should explain itself, got {error:?}"
    );

    engine
        .send(Command::SendPrompt {
            text: "again".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("a failed turn leaves the engine idle");
}

#[tokio::test]
async fn a_failed_turn_is_not_kept_as_context() {
    let provider = Arc::new(ScriptedProvider::failing(ProviderError::Transport(
        "connection reset".to_owned(),
    )));
    let seen = Arc::clone(&provider.seen);
    let engine = bare(provider, "scripted-model");
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    for prompt in ["first", "second"] {
        engine
            .send(Command::SendPrompt {
                text: prompt.to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("an idle engine accepts a prompt");
        drain(&mut events).await;
    }

    let requests = seen.lock().expect("the request log is never poisoned");
    let second = requests.get(1).expect("the second turn asked too");
    assert_eq!(
        second.messages.len(),
        2,
        "an empty reply should not enter the history, got {:?}",
        second.messages
    );
}

/// Every request a turn makes carries the configured prompt — including
/// the one that summarizes the conversation for compaction, which is what
/// keeps a compacted session from being summarized under instructions the
/// rest of it was never held under.
#[tokio::test]
async fn a_configured_system_prompt_reaches_the_agent_and_the_summarize_requests() {
    const SYSTEM: &str = "you are a canary";

    let provider = Arc::new(ScriptedProvider::new(vec![
        ProviderEvent::TextDelta("sure".to_owned()),
        ProviderEvent::Finish(FinishReason::Completed),
    ]));
    let seen = Arc::clone(&provider.seen);

    // A model the catalog knows, and a session already at its ceiling, so
    // the next turn compacts before it asks anything.
    let model = crate::catalog::default_model("anthropic")
        .expect("the catalog has a default for a provider this build ships");
    let window =
        crate::catalog::model(model).expect("the default model is in the catalog").context_window;

    let directory = tempfile::tempdir().expect("a temporary directory");
    let storage = Storage::open(directory.path().join("storage"));
    let session = SessionId::ascending();
    let info = SessionInfo {
        effort: None,
        id: session.clone(),
        version: storage::VERSION,
        // Pre-titled, so the title machinery stays out of a test that is
        // not about it and cannot spend a request of its own.
        title: Some("seeded".to_owned()),
        created: 1,
        updated: 2,
        usage: Usage::default(),
        context_tokens: window,
        summary: None,
        agent: None,
        model: None,
        activated_tools: std::collections::BTreeSet::new(),
        parent: None,
        revert: None,
    };
    storage.save_info(&info).expect("the seeded record writes");
    let earlier = Message::user("the objective");
    storage.save_message(&session, &earlier).expect("the seeded envelope writes");
    for part in &earlier.parts {
        storage.save_part(&session, &earlier.id, part).expect("the seeded part writes");
    }

    let engine = Engine::persistent(
        provider,
        model,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage,
    )
    .with_system_parts(Some(SYSTEM.to_owned()), None);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine.resume(&session).await.expect("the session loads");

    engine
        .send(Command::SendPrompt {
            text: "next".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    let requests = seen.lock().expect("the request log is never poisoned");
    assert_eq!(requests.len(), 2, "a compacting turn asks twice: summarize, then the model itself");
    assert!(
        requests[0].tools.is_empty(),
        "the summarize request is the toolless one, got {:?}",
        requests[0]
    );
    for request in requests.iter() {
        assert_eq!(request.system.as_deref(), Some(SYSTEM));
    }
}

/// The status bar's context meter polls this the way it polls `jobs()`:
/// the estimate is the stored measure compaction reads, and the window is
/// the catalog's — both visible without a turn in flight (**D469**).
#[tokio::test]
async fn the_context_estimate_reports_the_stored_measure_against_the_catalog_window() {
    let model = crate::catalog::default_model("anthropic")
        .expect("the catalog has a default for a provider this build ships");
    let window =
        crate::catalog::model(model).expect("the default model is in the catalog").context_window;

    let directory = tempfile::tempdir().expect("a temporary directory");
    let storage = Storage::open(directory.path().join("storage"));
    let session = SessionId::ascending();
    let info = SessionInfo {
        id: session.clone(),
        version: storage::VERSION,
        title: Some("seeded".to_owned()),
        created: 1,
        updated: 2,
        usage: Usage::default(),
        context_tokens: 1_234,
        summary: None,
        agent: None,
        model: None,
        effort: None,
        activated_tools: std::collections::BTreeSet::new(),
        parent: None,
        revert: None,
    };
    storage.save_info(&info).expect("the seeded record writes");

    let engine = Engine::persistent(
        Arc::new(FakeProvider::new("ok", std::time::Duration::from_millis(1))),
        model,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage,
    );

    let before = engine.context_estimate();
    assert_eq!(before.tokens, 0, "before a resume there is no session to have measured anything");
    assert_eq!(before.window, Some(window));

    engine.resume(&session).await.expect("the session loads");

    let after = engine.context_estimate();
    assert_eq!(after.tokens, 1_234, "the stored measure is what the bar shows");
    assert_eq!(after.window, Some(window));
}

/// A model the catalog does not know has no window to report — the same
/// honest absence that keeps such a session from ever auto-compacting.
#[tokio::test]
async fn the_context_estimate_has_no_window_for_an_uncataloged_model() {
    let estimate = engine().context_estimate();

    assert_eq!(estimate.tokens, 0, "an ephemeral engine stores no measure");
    assert_eq!(
        estimate.window, None,
        "only the catalog can size a window, and it does not know the fake model"
    );
}

/// An engine with something in every fixed category, for the breakdown
/// tests: a base prompt, a suffix carrying an instruction file and a
/// skills block spelled with the composer's own markers, and the builtin
/// tools.
fn furnished(model: &str) -> Engine {
    let suffix = "You are powered by the model named fake.\n<env>\n  Working directory: /\n</env>\
                      \nInstructions from: /project/AGENTS.md\nalways run the tests\
                      \nSkills provide specialized instructions and workflows for specific tasks.\n<available_skills>\n</available_skills>";

    Engine::new(
        Arc::new(FakeProvider::new("one two", std::time::Duration::from_millis(1))),
        model,
        Arc::new(Registry::with_builtins()),
        Permissions::default(),
    )
    .with_system_parts(Some("obey the tests".to_owned()), Some(suffix.to_owned()))
}

/// The grid's contract: the legend can only add up to the panel's total
/// because the accessor's categories add up to its own.
#[tokio::test]
async fn the_breakdown_categories_sum_to_the_total() {
    let breakdown = furnished(MODEL).context_breakdown().await;

    let summed = breakdown.system_prompt
        + breakdown.instructions
        + breakdown.tools_builtin
        + breakdown.tools_mcp
        + breakdown.skills
        + breakdown.conversation_user
        + breakdown.conversation_assistant;
    assert!(summed > 0, "the furnished engine fills categories");
    assert_eq!(summed, breakdown.total());
}

/// The counts ride the same walk that priced the tools, and the model id
/// is the engine's own: with only the builtins registered, the builtin
/// count is exactly the registry's roster and the MCP count is zero.
#[tokio::test]
async fn the_breakdown_counts_the_tools_the_same_walk_priced() {
    let breakdown = furnished(MODEL).context_breakdown().await;

    assert_eq!(
        breakdown.tools_builtin_count,
        Registry::with_builtins().definitions().len(),
        "every builtin the registry serves is counted once"
    );
    assert_eq!(breakdown.tools_mcp_count, 0, "no server is connected");
    assert_eq!(breakdown.model, MODEL, "the id the engine runs under");
}

/// AC4 undisturbed: the counts are metadata for the panel's detail
/// sections, so two breakdowns that differ only in them agree on every
/// token figure — the total and the free space sum nothing from a count.
#[test]
fn the_counts_are_metadata_and_move_no_token_figure() {
    use super::ContextBreakdown;

    let bare = ContextBreakdown {
        system_prompt: 1_000,
        tools_builtin: 2_000,
        tools_mcp: 500,
        window: Some(10_000),
        reserve: Some(1_000),
        ..ContextBreakdown::default()
    };
    let counted =
        ContextBreakdown { tools_builtin_count: 12, tools_mcp_count: 193, ..bare.clone() };

    assert_eq!(bare.total(), counted.total());
    assert_eq!(bare.free(), counted.free());
}

/// The free-space row is window − used − reserve, read off the exposed
/// reserve rather than re-derived from the compaction trigger.
#[tokio::test]
async fn free_space_is_the_window_minus_the_total_minus_the_reserve() {
    let model = crate::catalog::default_model("anthropic")
        .expect("the catalog has a default for a provider this build ships");
    let window =
        crate::catalog::model(model).expect("the default model is in the catalog").context_window;

    let engine = furnished(model);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::SendPrompt {
            text: "fill the conversation a little".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    let breakdown = engine.context_breakdown().await;
    assert_eq!(breakdown.window, Some(window));
    let reserve = breakdown.reserve.expect("a sized window has a reserve");
    assert!(reserve > 0, "the trigger holds a tenth back");
    assert_eq!(
        breakdown.free(),
        Some(window - breakdown.total() - reserve),
        "free space is what the window has left after the load and the reserve"
    );
}

/// Review changelog MAJOR 3's whole point: `/context` on a session that
/// has said nothing must still show what the first request would carry —
/// the fixed shares are computed on demand, not stashed by a turn that
/// never ran.
#[tokio::test]
async fn a_fresh_session_reports_system_and_tool_shares_and_no_conversation() {
    let breakdown = furnished(MODEL).context_breakdown().await;

    assert_eq!(breakdown.conversation_user, 0);
    assert_eq!(breakdown.conversation_assistant, 0);
    assert!(breakdown.system_prompt > 0, "{breakdown:?}");
    assert!(breakdown.instructions > 0, "{breakdown:?}");
    assert!(breakdown.skills > 0, "{breakdown:?}");
    assert!(breakdown.tools_builtin > 0, "{breakdown:?}");
    assert_eq!(breakdown.tools_mcp, 0, "no server is connected");
}

/// A standing conversation revert hides the anchor and everything after
/// it from the *next* request — `truncate_reverted` runs at the next
/// prompt — so a breakdown read in between must already leave those
/// messages out.
#[tokio::test]
async fn a_breakdown_right_after_a_revert_reflects_the_truncated_conversation() {
    let engine = engine();
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "the first prompt".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;
    let after_one_turn = engine.context_breakdown().await;

    engine
        .send(Command::SendPrompt {
            text: "the second prompt, which the revert takes back".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let second_turn = drain(&mut events).await;
    let anchor = second_turn
        .iter()
        .find_map(|event| match event {
            Event::MessageStarted { message, .. } if message.role == Role::User => {
                Some(message.id.clone())
            }
            _ => None,
        })
        .expect("the second turn opened with its user message");

    let after_two_turns = engine.context_breakdown().await;
    assert!(
        after_two_turns.conversation_user > after_one_turn.conversation_user,
        "the second turn grew the conversation"
    );

    engine
        .send(Command::RevertTo { message_id: anchor, scope: RevertScope::Conversation })
        .await
        .expect("a checkpoint that exists is revertable");

    let after_revert = engine.context_breakdown().await;
    assert_eq!(
        (after_revert.conversation_user, after_revert.conversation_assistant),
        (after_one_turn.conversation_user, after_one_turn.conversation_assistant),
        "what the revert hid is already left out"
    );
}

/// The same honest absence `context_estimate` reports: no catalog row, no
/// window, no reserve, no free-space figure — the dialog's degraded panel.
#[tokio::test]
async fn the_breakdown_has_no_window_for_an_uncataloged_model() {
    let breakdown = furnished(MODEL).context_breakdown().await;

    assert_eq!(breakdown.window, None);
    assert_eq!(breakdown.reserve, None);
    assert_eq!(breakdown.free(), None);
}

/// AC4's one-estimator claim, spelled honestly: `context_estimate` reads
/// the stored measure a finished request stamped — an *actual*, which no
/// on-demand estimate can be asserted equal to — so what "one estimator"
/// means, and what this pins, is the **convention**: the breakdown prices
/// characters exactly as the compaction fit guard does, four to a token,
/// and never through a second tokenizer.
#[tokio::test]
async fn the_breakdown_prices_by_the_compaction_estimators_own_convention() {
    let engine = engine();
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::SendPrompt {
            text: "x".repeat(400),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    let breakdown = engine.context_breakdown().await;
    assert_eq!(
        breakdown.conversation_user,
        crate::session::estimate_tokens(400),
        "four hundred characters are a hundred tokens under the shared convention"
    );
}

/// The successor to `a_second_subscriber_is_refused`, asserting the
/// contract that replaced the refusal: every subscriber has a queue of
/// its own, so a second one registered before the turn holds the same
/// transcript the first does, frame for frame.
#[tokio::test]
async fn a_second_subscriber_sees_the_same_events_the_first_does() {
    let engine = engine();
    let mut first = engine.subscribe().await.expect("the first subscriber claims the birth queue");
    let mut second =
        engine.subscribe().await.expect("a later subscriber registers a queue of its own");

    engine
        .send(Command::SendPrompt {
            text: "hi".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    // Bounded so a delivery that forgot one of the queues fails loudly
    // instead of waiting forever on a stream nothing feeds.
    let heard_first = tokio::time::timeout(DRAIN_PATIENCE, drain(&mut first))
        .await
        .expect("the first subscriber hears the whole turn");
    let heard_second = tokio::time::timeout(DRAIN_PATIENCE, drain(&mut second))
        .await
        .expect("the second subscriber hears the whole turn");

    assert!(
        matches!(heard_first.last(), Some(Event::MessageFinished { .. })),
        "a drained turn ends with its finish: {heard_first:?}"
    );
    assert_eq!(
        heard_first, heard_second,
        "two lossless subscribers of one turn hold the same transcript"
    );
}

/// Every event is addressed: it names the engine's current session, which
/// has a name even on an engine that stores nothing.
#[tokio::test]
async fn every_event_of_a_turn_carries_the_engines_session_id() {
    let engine = engine();
    let mut events = engine.subscribe().await.expect("the first subscriber claims the birth queue");

    let session = engine.session_id();
    assert!(
        crate::protocol::is_uuidv7(session.as_str()),
        "an ephemeral engine's session id is a bare UUIDv7 now that the \
             `ses_` prefix is gone: {session:?}"
    );

    engine
        .send(Command::SendPrompt {
            text: "hi".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain(&mut events).await;

    assert!(!seen.is_empty(), "a turn reports something");
    for event in &seen {
        assert_eq!(
            event.session_id(),
            &session,
            "every event of the turn names the engine's session: {event:?}"
        );
    }
}

/// `Command::NewSession` renames the engine before anything can be said
/// in the next conversation. Left stale, the second conversation's lazy
/// create would adopt the first one's id and `save_info` would upsert
/// over its row — so the pin is that two conversations on one persistent
/// engine store two distinct sessions, each addressed as itself.
#[tokio::test]
async fn two_conversations_on_one_engine_store_two_distinct_sessions() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let engine = Engine::persistent(
        Arc::new(FakeProvider::new("one two", std::time::Duration::from_millis(1))),
        MODEL,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        Storage::open(directory.path().join("storage")),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber");

    engine
        .send(Command::SendPrompt {
            text: "first".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let first_turn = drain(&mut events).await;
    let first = engine.current_session().expect("the first prompt created a session");
    assert_eq!(
        engine.session_id(),
        first.id,
        "the stored row adopted the id the engine was already using"
    );
    assert!(
        first_turn.iter().all(|event| event.session_id() == &first.id),
        "the first conversation's events name its session: {first_turn:?}"
    );

    engine.send(Command::NewSession).await.expect("an idle engine forgets its session");
    engine
        .send(Command::SendPrompt {
            text: "second".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("a fresh conversation accepts a prompt");
    let second_turn = drain(&mut events).await;
    let second = engine.current_session().expect("the second prompt created a session");

    assert_ne!(first.id, second.id, "a new conversation is a new session");
    assert!(
        second_turn.iter().all(|event| event.session_id() == &second.id),
        "the second conversation's events name its own session: {second_turn:?}"
    );

    let stored = engine.sessions().await.expect("the store lists");
    let ids: Vec<&SessionId> = stored.iter().map(|info| &info.id).collect();
    assert_eq!(stored.len(), 2, "two conversations, two rows: {ids:?}");
    assert!(
        ids.contains(&&first.id) && ids.contains(&&second.id),
        "and they are exactly the two the engine was on: {ids:?}"
    );
}

#[tokio::test]
async fn a_prompt_sent_mid_turn_is_refused() {
    let engine = engine();
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "first".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    assert!(matches!(events.next().await, Some(Event::MessageStarted { .. })));

    assert!(matches!(
        engine
            .send(Command::SendPrompt {
                text: "second".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await,
        Err(EngineError::Busy)
    ));
}

/// **D119.** Upstream aborts the running session and then reverts; here
/// the person at the terminal cancels first, so an undo is never something
/// that stopped work they were watching. Refused before anything else is
/// even looked at, which is why an engine with no snapshots still answers
/// `Busy` here rather than `NoSnapshots`.
#[tokio::test]
async fn an_undo_during_a_turn_is_refused_rather_than_stopping_it() {
    let engine = engine();
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "first".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    assert!(matches!(events.next().await, Some(Event::MessageStarted { .. })));

    assert!(matches!(engine.send(Command::Undo).await, Err(EngineError::Busy)));
    assert!(matches!(engine.send(Command::Redo).await, Err(EngineError::Busy)));
}

/// An engine that takes no snapshots says so rather than moving the
/// transcript: an undo that hid the messages and left every file where it
/// was would be an undo that only half happened, and nothing afterwards
/// could tell.
#[tokio::test]
async fn an_undo_without_snapshots_refuses_instead_of_half_happening() {
    let engine = engine();
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "first".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    assert!(matches!(engine.send(Command::Undo).await, Err(EngineError::NoSnapshots)));
    assert_eq!(
        engine.history.lock().await.len(),
        2,
        "a refused undo leaves the conversation exactly as it was"
    );
}

#[tokio::test]
async fn the_engine_accepts_a_prompt_again_once_the_turn_finished() {
    let engine = engine();
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "first".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    drain(&mut events).await;

    engine
        .send(Command::SendPrompt {
            text: "second".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("a finished turn leaves the engine idle");
}

#[tokio::test]
async fn cancelling_while_idle_does_nothing() {
    let engine = engine();
    let _events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(Command::CancelTurn).await.expect("an idle cancel is a no-op");
}

/// The context meter's half of the display-only invariant (bead `pwe`).
///
/// This one is not about what is *sent* but about what is *claimed to be
/// sent*: the meter reports how full the window is, and counting thinking
/// nothing carries would have it fill with words the model never receives
/// — a session told to compact by a measure of its own scratch paper. The
/// sealed half beside it *is* counted, because that one really does ride
/// the next request.
#[test]
fn readable_thinking_counts_nothing_toward_a_window_it_never_reaches() {
    let mut assistant = Message::assistant("claude-test");
    assistant.parts.push(Part::text("Hello!"));
    let (said, results) = message_chars(&assistant);

    assistant.parts.push(Part::reasoning_text("a".repeat(10_000)));
    assert_eq!(
        message_chars(&assistant),
        (said, results),
        "ten thousand characters of thinking moved the meter; nothing \
             sends them"
    );

    // And the contrast that keeps this from passing by measuring nothing:
    // sealed state is handed back, so it counts.
    assistant.parts.push(Part::reasoning("openai", "rs_1", Some("b".repeat(64))));
    assert_eq!(
        message_chars(&assistant),
        (said + 64, results),
        "the sealed half rides the next request and has to be measured"
    );
}

#[test]
fn the_stale_notice_names_its_files_the_way_the_model_would_ask_for_them() {
    let root = std::path::Path::new("/project");

    assert_eq!(stale_notice(&[], root), None, "nothing stale, nothing said");
    assert_eq!(
        stale_notice(
            &[
                PathBuf::from("/project/src/main.rs"),
                PathBuf::from("/project/README.md"),
                // A file the session read outside the project has no
                // relative form; naming it absolutely is what `read`
                // would take back.
                PathBuf::from("/etc/hosts"),
            ],
            root,
        )
        .as_deref(),
        Some(
            "The following files changed on disk after they were read in this session; \
                 re-read them before relying on their contents:\n\
                 - src/main.rs\n\
                 - README.md\n\
                 - /etc/hosts"
        )
    );
}

/// Marks `path` stale in `files` the way the watcher would: read, moved by
/// somebody else, noticed.
fn condemn(files: &FileTimes, path: &std::path::Path) {
    files.record(path);
    // Opened for writing because a stamp is metadata a handle must be
    // allowed to write: unix grants that with the file's own permissions,
    // Windows only through a handle that asked for write access.
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(std::time::SystemTime::UNIX_EPOCH))
        .expect("the fixture can move the stamp");
    files.note_change(path);
}

/// The text parts of the last user message in `request` — where a
/// reminder lands.
fn last_user_text(request: &ChatRequest) -> Vec<&str> {
    request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .expect("a request carries the user's message")
        .parts
        .iter()
        .filter_map(crate::protocol::Part::as_text)
        .collect()
}

/// **AC-23**, end to end: a command carrying a teammate's message becomes
/// a [`PartBody::Peer`] part on the user's own message, and reaches the
/// wire as §5.3's envelope inside that turn's text (**D495**).
///
/// The two halves are asserted in one test because each is worthless
/// without the other: a part nothing renders is a message the model was
/// never told, and an envelope nothing builds a part for is dead code.
///
/// The prompt's text is empty on purpose — a delivery turn is a turn whose
/// content *is* what the teammate said — which is also the case that pins
/// the empty text part being dropped rather than sent as a blank block.
#[tokio::test]
async fn a_teammates_message_reaches_the_wire_as_the_envelope() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        ProviderEvent::TextDelta("thanks".to_owned()),
        ProviderEvent::Finish(FinishReason::Completed),
    ]));
    let seen = Arc::clone(&provider.seen);
    let engine = bare(provider, "scripted-model");
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: String::new(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: vec![crate::protocol::team::PeerPayload::new(
                "w1",
                Some("picked up W2".to_owned()),
                None,
                "on the protocol",
            )],
        })
        .await
        .expect("an idle engine accepts a prompt");
    let announced = drain(&mut events).await;

    let Some(Event::MessageStarted { message: user, .. }) = announced.first() else {
        panic!("a turn opens with the user's message, got {announced:?}");
    };
    assert_eq!(
        user.parts.len(),
        1,
        "a delivery turn carries the teammate's words and no blank text part: {:?}",
        user.parts
    );
    assert!(
        matches!(
            &user.parts[0].body,
            crate::protocol::PartBody::Peer { from, body, .. }
                if from == "w1" && body == "on the protocol"
        ),
        "the payload became the part that says whose words these are: {:?}",
        user.parts
    );

    let requests = seen.lock().expect("the request log is never poisoned");
    let carried: Vec<&str> = requests
        .first()
        .expect("the turn asked the provider")
        .messages
        .last()
        .expect("a request carries the user's message")
        .parts
        .iter()
        .filter_map(crate::protocol::Part::as_text)
        .collect();
    assert_eq!(
        carried,
        vec![
            "<teammate-message teammate_id=\"w1\" summary=\"picked up W2\">\n\
                 on the protocol\n\
                 </teammate-message>"
        ],
        "the wire carries the envelope and nothing else"
    );
}

#[tokio::test]
async fn files_that_went_stale_are_named_to_the_model_once() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("notes.md");
    std::fs::write(&path, "one").expect("the fixture writes");

    let provider = Arc::new(ScriptedProvider::new(vec![
        ProviderEvent::TextDelta("sure".to_owned()),
        ProviderEvent::Finish(FinishReason::Completed),
    ]));
    let seen = Arc::clone(&provider.seen);
    let engine = bare(provider, "scripted-model");
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    condemn(&engine.files, &path);

    for prompt in ["first", "second"] {
        engine
            .send(Command::SendPrompt {
                text: prompt.to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("an idle engine accepts a prompt");
        drain(&mut events).await;
    }

    let requests = seen.lock().expect("the request log is never poisoned");
    let first = last_user_text(requests.first().expect("the first turn asked"));
    assert_eq!(first.first(), Some(&"first"), "the user's own text comes first: {first:?}");
    let notice = first.get(1).expect("the turn after the change carries the notice");
    assert!(notice.starts_with(STALE_FILES) && notice.contains("notes.md"), "got {notice:?}");

    assert_eq!(
        last_user_text(requests.get(1).expect("the second turn asked too")),
        vec!["second"],
        "one episode is told once; a later turn is not reminded again"
    );
}

/// A `!` passthrough asks the model nothing, so it is not a turn that can
/// carry a notice — and must not consume one on the way past.
#[tokio::test]
async fn a_passthrough_between_the_change_and_the_prompt_does_not_spend_the_notice() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let path = dir.path().join("notes.md");
    std::fs::write(&path, "one").expect("the fixture writes");

    let provider = Arc::new(ScriptedProvider::new(vec![
        ProviderEvent::TextDelta("sure".to_owned()),
        ProviderEvent::Finish(FinishReason::Completed),
    ]));
    let seen = Arc::clone(&provider.seen);
    let engine = bare(provider, "scripted-model");
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    condemn(&engine.files, &path);

    engine
        .send(Command::RunShell { command: "true".to_owned() })
        .await
        .expect("an idle engine accepts a passthrough");
    drain(&mut events).await;

    engine
        .send(Command::SendPrompt {
            text: "now what".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("a finished passthrough leaves the engine idle");
    drain(&mut events).await;

    let requests = seen.lock().expect("the request log is never poisoned");
    assert_eq!(requests.len(), 1, "a passthrough asks the provider nothing, got {requests:?}");
    let carried = last_user_text(&requests[0]);
    assert!(
        carried.iter().any(|text| text.starts_with(STALE_FILES) && text.contains("notes.md")),
        "the notice waited for the turn that could deliver it: {carried:?}"
    );
}

/// The effort rule's outer tier: the fake provider has no catalog rows,
/// so *any* name is refused with the no-catalog sentence — the same
/// posture that already denies such a session sizing and pricing — while
/// clearing asks for the state the session is already in and is accepted
/// and announced like any adoption.
#[tokio::test]
async fn an_effort_on_an_uncataloged_provider_is_refused_naming_the_catalog() {
    let engine = engine();
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    let refusal = engine
        .send(Command::SwitchEffort { effort: Some("max".to_owned()) })
        .await
        .expect_err("the fake provider has no catalog rows");
    assert!(matches!(refusal, EngineError::UncatalogedEffort { .. }), "got {refusal:?}");
    assert!(
        refusal.to_string().contains("not in the catalog"),
        "the refusal names the reason: {refusal}"
    );
    assert_eq!(engine.effort(), None, "a refused switch adopts nothing");

    engine
        .send(Command::SwitchEffort { effort: None })
        .await
        .expect("clearing needs no catalog: it asks for the state every session starts in");
    let event = events.next().await.expect("the adoption is announced");
    assert!(matches!(event, Event::EffortChanged { effort: None, .. }), "got {event:?}");
}

// ---- D474: the `/plugin` Reload seam ----

/// The skills half of the reload: swapping the base registry is what the
/// next turn is offered, task-tool riding and all — asserted through the
/// same private accessor the turn assembly reads.
#[test]
fn replacing_the_base_tools_is_what_the_next_turn_is_offered() {
    let engine = engine();
    assert!(
        engine.tools().get("read").is_none(),
        "the fixture starts with an empty registry, or the swap proves nothing"
    );

    engine.replace_base_tools(Arc::new(Registry::with_builtins()));

    assert!(
        engine.tools().get("read").is_some(),
        "the offered set is rebuilt from the replaced base"
    );
    assert!(
        engine.lent().get("read").is_some(),
        "the lent set a subagent is offered moves with it"
    );
}

/// The team's messaging tool is offered where a team exists, nowhere else,
/// and a reload of the base set does not drop it.
///
/// Three moves in one test because the claim is the difference between
/// them: an engine with no team must not offer `send_message` at all, or
/// the second move proves nothing; and the third is the reload seam
/// (**D474**), whose whole hazard is a tool that lives outside the shared
/// composition path and is quietly lost the first time the set is rebuilt.
#[test]
fn a_reload_of_the_base_tools_keeps_the_teams_messaging_tool() {
    assert!(
        engine().tools().get(send_message::ID).is_none(),
        "a session with no team has nobody to address"
    );

    let engine = engine().with_teammates(
        Arc::new(teammate::TeammateRegistry::new(
            ganja_team::TeamsRoot::new(std::path::PathBuf::from("/nonexistent/teams")),
            ganja_team::TeamName::parse("session-abcd1234").expect("a team name"),
            "session-abcd1234",
            std::path::PathBuf::from("/nonexistent/project"),
        )),
        crate::subagent::Backends::new(),
    );
    assert!(
        engine.tools().get(send_message::ID).is_some(),
        "a session with a team is offered the tool that addresses it"
    );

    engine.replace_base_tools(Arc::new(Registry::with_builtins()));

    assert!(
        engine.tools().get(send_message::ID).is_some(),
        "a reload rebuilds through the shared composition path, which offers it again"
    );
}

/// **U-2, D538.** An assembled map carrying no in-process entry gains the
/// engine's own.
///
/// Observed through the refusal, which is the one place the difference shows:
/// a session with no store answers a spawn on that surface with the storeless
/// sentence, where a map with no in-process entry at all would answer with the
/// absent-backend one. The point is that the frontend never supplies this
/// entry and never has to.
#[tokio::test]
async fn with_teammates_inserts_the_engines_own_in_process_entry() {
    let home = ganja_testkit::temp_dir();
    let engine = engine().with_teammates(
        Arc::new(teammate::TeammateRegistry::for_session(
            home.path(),
            "01998ad0-0000-7000-8000-000000000000",
            home.path(),
        )),
        crate::subagent::Backends::new(),
    );

    // Built here rather than out of `ganja_testkit`: that crate links its own
    // build of this one, so its `Caller` is a different type from the one this
    // method takes.
    #[derive(Debug)]
    struct Allow;

    #[async_trait::async_trait]
    impl crate::subagent::SpawnAsker for Allow {
        async fn ask(&self, _request: crate::subagent::SpawnAsk) -> PermissionReply {
            PermissionReply::Once
        }
    }

    let refused = engine
        .teammates()
        .expect("a session with a team has a door")
        .start(
            crate::tool::task::TeammateSpawn {
                name: "worker".to_owned(),
                backend: Some("in-process".to_owned()),
                agent_type: "general".to_owned(),
                prompt: "have a look at the parser".to_owned(),
            },
            &crate::subagent::Caller {
                model: MODEL.to_owned(),
                cwd: home.path().to_path_buf(),
                permissions: Arc::new(std::sync::Mutex::new(Permissions::default())),
                project_root: home.path().to_path_buf(),
            },
            &Allow,
        )
        .await
        .expect_err("this engine has no store to keep a teammate's transcript in");

    assert!(
        refused.reason.contains(STORELESS),
        "the entry the engine inserted is what answered: {refused:?}"
    );
    assert!(
        !refused.reason.contains("has no in-process backend"),
        "and not the refusal a map with no such entry would give: {refused:?}"
    );
}

/// A process that *is* a member is offered the messaging tool off its own
/// postbox, leads no team, and keeps the tool across a reload — the same
/// composition path as the lead's, entered through the other door.
///
/// The negative half is the same one the lead's test opens with, and it
/// is asserted again here rather than assumed: the whole claim is that
/// presence of the tool tracks presence of a postbox, and only that.
#[test]
fn a_member_engine_with_a_postbox_is_offered_send_message_and_leads_no_team() {
    assert!(
        engine().tools().get(send_message::ID).is_none(),
        "a session with no postbox has nobody to address"
    );

    let postbox = Arc::new(teammate::member::MemberPostbox::new(
        ganja_team::MemberName::parse("worker").expect("a member name"),
        ganja_team::TeamName::parse("session-abcd1234").expect("a team name"),
        ganja_team::TeamsRoot::new(std::path::PathBuf::from("/nonexistent/teams")),
    ));
    let engine = engine().with_postbox(postbox);
    assert!(
        engine.tools().get(send_message::ID).is_some(),
        "a member is offered the tool that addresses its team"
    );
    assert!(engine.teammates().is_none(), "and leads no team of its own");
    assert!(engine.teammate_dialogs().is_none(), "so no dialog channel is opened for it");

    engine.replace_base_tools(Arc::new(Registry::with_builtins()));

    assert!(
        engine.tools().get(send_message::ID).is_some(),
        "a reload rebuilds through the shared composition path, which offers it again"
    );
}

/// **D530**, as **D543** makes it reachable: a session whose team holds
/// nobody is offered the teamless-described tool — not the empty-roster
/// ("team of one") variant, which must not read alike. The two are told
/// apart by a live read of the registry, so this is the description a
/// shipped session that has spawned nobody really gets.
#[test]
fn a_session_leading_nobody_is_offered_send_message_with_the_teamless_description() {
    let home = ganja_testkit::temp_dir();
    let registry = crate::teammate::tests::registry(home.path());
    let engine = engine().with_teammates(registry, subagent::Backends::new());

    let tools = engine.tools();
    let tool = tools.get(send_message::ID).expect("a session leading nobody is offered it");
    assert!(
        !tool.description().contains("Teammates this session can address"),
        "no roster is claimed, unlike a team of one: {}",
        tool.description()
    );
    assert!(engine.teamless(), "a team nobody has joined is what teamless means (D543)");
}

/// **D543**'s reachability half, end to end at the composition seam: the
/// road-back sentence is absent while nothing is bound, and the first
/// refresh after a bind brings it in.
///
/// The bind is what a frontend does *after* assembly, so this is also the
/// pin on the memo carrying reachability: were the shape only roster and
/// solitude, the refresh below would see no change and the session would
/// describe itself as unanswerable for the rest of its life.
#[test]
fn a_bound_socket_adds_the_road_back_to_the_teamless_description() {
    let home = ganja_testkit::temp_dir();
    let registry = crate::teammate::tests::registry(home.path());
    let engine = engine().with_teammates(registry, subagent::Backends::new());
    let described = |engine: &Engine| {
        engine
            .tools()
            .get(send_message::ID)
            .expect("a session leading nobody is offered it")
            .description()
            .to_owned()
    };

    assert!(
        !described(&engine).contains("answer by addressing this one back"),
        "nothing is bound, so no road home is claimed: {}",
        described(&engine)
    );

    engine.set_peer_address(Some(std::path::Path::new("/tmp/ganja-501/0198c1a2.sock")));
    engine.refresh_team();

    assert!(
        described(&engine).contains("answer by addressing this one back"),
        "the first refresh after the bind names the road home: {}",
        described(&engine)
    );
}

/// **D543**'s other half at the same seam: an engine with **no team at
/// all** — a pane member, a fixture holding a bare postbox — is not
/// teamless, because leading nobody is a thing only a lead does.
#[test]
fn a_session_with_no_team_at_all_is_not_teamless() {
    let engine = engine();

    assert!(engine.teammates().is_none());
    assert!(!engine.teamless(), "no team is not a team of nobody");
}

/// **AC-40's engine-cell half (ADJ-2)**: `/rename` sets the self-name
/// cell a frontend writes its registration record from — the name other
/// sessions resolve this one by — and [`Engine::self_name`] answers it
/// back exactly as set. Since **D543** that cell stamps no wire identity
/// of its own: a send carries the session's team identity either way.
#[test]
fn set_self_name_moves_the_cell_a_registration_record_is_written_from() {
    let engine = engine();
    assert_eq!(
        engine.self_name(),
        crate::tool::registry::FALLBACK_NAME,
        "unseeded, the cell holds the same fallback D527's own sanitizer falls back to"
    );

    engine.set_self_name("fresh");

    assert_eq!(engine.self_name(), "fresh");
}

/// A session leading a team of **nobody**, over one script per step,
/// ready for a `send_message` call — D531/D543's fixture. Distinct from
/// this module's own single-script [`ScriptedProvider`] because a posture
/// test needs a different script for each of several turns.
///
/// The registry and the home come back with the engine because the
/// posture is read off that registry at every call (**D543**): a test
/// that wants the posture to move spawns into this handle, and the tree
/// its team file is written under has to outlive the engine.
struct Alone {
    engine: Engine,
    registry: Arc<teammate::TeammateRegistry>,
    /// Where the team file lands, and dropping it takes that tree with it.
    home: tempfile::TempDir,
}

fn teamless(
    scripts: Vec<Vec<ProviderEvent>>,
    rules: Vec<crate::permission::Rule>,
    posture: TeamlessSend,
) -> Alone {
    let (provider, _requests) = ganja_testkit::ScriptedProvider::new(scripts);
    let mut permissions = Permissions::default();
    permissions.set_baseline(rules);
    let home = ganja_testkit::temp_dir();
    let registry = crate::teammate::tests::registry(home.path());

    let engine = Engine::new(provider, MODEL, Arc::new(Registry::new(Vec::new())), permissions)
        .with_teammates(Arc::clone(&registry), subagent::Backends::new())
        .with_teamless_send(posture);

    Alone { engine, registry, home }
}

/// A `send_message` call to a name nobody answers to, one turn's worth.
fn send_call(to: &str, message: &str) -> Vec<ProviderEvent> {
    ganja_testkit::tool_call(send_message::ID, serde_json::json!({ "to": to, "message": message }))
}

async fn prompt(engine: &Engine, text: &str) {
    engine
        .send(Command::SendPrompt {
            text: text.to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
}

/// Reads until a `PermissionRequested` arrives, and answers its id —
/// this module's own `drain` cannot be used first, since the turn sits
/// in the dialog rather than finishing.
async fn until_requested(events: &mut BoxStream<'static, Event>) -> crate::protocol::PermissionId {
    loop {
        if let Event::PermissionRequested { id, .. } =
            events.next().await.expect("the stream outlives the turn")
        {
            return id;
        }
    }
}

/// **D531**: unasked by default, for a session that never named a
/// posture at all.
#[tokio::test]
async fn a_teamless_send_is_unasked_by_default() {
    let Alone { engine, .. } = teamless(
        vec![send_call("nobody", "hi"), ganja_testkit::says("done")],
        Vec::new(),
        TeamlessSend::Unasked,
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    prompt(&engine, "send it").await;
    let seen = drain(&mut events).await;

    assert!(
        !seen.iter().any(|event| matches!(event, Event::PermissionRequested { .. })),
        "a teamless session's default send is unasked: {seen:?}"
    );
}

/// **D531**: `teamless_send: "ask"` raises the ordinary permission
/// dialog, and a stored "always allow" answer silences the next one —
/// the computed default sits *beneath* every rule, never above it.
#[tokio::test]
async fn teamless_ask_raises_a_dialog_and_a_stored_always_answer_silences_the_next() {
    let Alone { engine, .. } = teamless(
        vec![
            send_call("nobody", "first"),
            ganja_testkit::says("first done"),
            send_call("nobody", "second"),
            ganja_testkit::says("second done"),
        ],
        Vec::new(),
        TeamlessSend::Ask,
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    prompt(&engine, "one").await;
    let waiting = until_requested(&mut events).await;
    engine
        .send(Command::ReplyPermission { id: waiting, reply: PermissionReply::Always })
        .await
        .expect("the dialog this turn raised is answerable");
    drain(&mut events).await;

    prompt(&engine, "two").await;
    let second = drain(&mut events).await;
    assert!(
        !second.iter().any(|event| matches!(event, Event::PermissionRequested { .. })),
        "a stored always answer outranks the handed-in default: {second:?}"
    );
}

/// **D531**: a deny rule still denies — the computed default sits
/// beneath every rule, never above one.
#[tokio::test]
async fn a_deny_rule_outranks_the_teamless_ask_default() {
    let Alone { engine, .. } = teamless(
        vec![send_call("nobody", "hi"), ganja_testkit::says("done")],
        vec![crate::permission::Rule {
            permission: send_message::ID.to_owned(),
            pattern: "*".to_owned(),
            action: crate::permission::Action::Deny,
        }],
        TeamlessSend::Ask,
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    prompt(&engine, "send it").await;
    let seen = drain(&mut events).await;

    assert!(
        !seen.iter().any(|event| matches!(event, Event::PermissionRequested { .. })),
        "a rule already answered this; nobody is asked: {seen:?}"
    );
}

/// **AC-35's mid-session clause, and AC-42**, driven through the
/// mechanism **D543** turned them into: the computed default is a read of
/// this session's own registry, so a teammate **joining** flips it back to
/// allow with the key still `ask` — in-team D498 stays byte-untouched —
/// and **retiring** that teammate leaves the session leading nobody
/// again, so asking resumes.
///
/// A real spawn and a real retire, rather than the two installer seams
/// D530 landed for this: `install_team`/`retire_team` were
/// production-callerless from the day they shipped and went with D543,
/// and what a shipped session really does to its team is spawn into it
/// and retire out of it. The posture therefore has to follow those, which
/// is the whole claim under test.
#[tokio::test]
async fn a_teammate_joining_stops_the_ask_and_retiring_it_resumes() {
    let Alone { engine, registry, home } = teamless(
        vec![
            send_call("nobody", "one"),
            ganja_testkit::says("one done"),
            send_call("nobody", "two"),
            ganja_testkit::says("two done"),
            send_call("nobody", "three"),
            ganja_testkit::says("three done"),
        ],
        Vec::new(),
        TeamlessSend::Ask,
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    // Leading nobody, ask key set: the first send is asked about.
    prompt(&engine, "one").await;
    let waiting = until_requested(&mut events).await;
    engine
        .send(Command::ReplyPermission { id: waiting, reply: PermissionReply::Once })
        .await
        .expect("the dialog this turn raised is answerable");
    drain(&mut events).await;

    // A teammate joins: the key still says `ask`, but a session that
    // holds somebody is D498's static ladder again, key regardless.
    registry
        .spawn(
            crate::teammate::tests::in_process(home.path()),
            crate::teammate::tests::request(
                "worker",
                crate::protocol::team::MemberBackend::InProcess,
                home.path(),
            ),
        )
        .await
        .expect("a teammate joins");

    prompt(&engine, "two").await;
    let second = drain(&mut events).await;
    assert!(
        !second.iter().any(|event| matches!(event, Event::PermissionRequested { .. })),
        "in-team D498 stays unasked regardless of the key: {second:?}"
    );

    // It is retired: the registry holds nobody again, and asking resumes.
    assert!(registry.retire("worker").await.expect("the retire lands"), "a member was retired");

    prompt(&engine, "three").await;
    let waiting = until_requested(&mut events).await;
    engine
        .send(Command::ReplyPermission { id: waiting, reply: PermissionReply::Once })
        .await
        .expect("the dialog this turn raised is answerable");
    let third = drain(&mut events).await;
    assert!(
        !third.iter().any(|event| matches!(
            event,
            Event::PermissionReplied { reply: PermissionReply::Reject, .. }
        )),
        "the send was not rejected: {third:?}"
    );
}

/// **D528**'s `NewSession` door: a pin this conversation made does not
/// survive a new one.
#[tokio::test]
async fn new_session_clears_the_identity_pin_map() {
    let engine = engine();
    engine.identity.pin("backend", "ses-far", "0198c1a2");
    assert!(engine.identity.pinned("backend").is_some());

    engine.new_session().await.expect("an idle engine accepts a new session");

    assert_eq!(engine.identity.pinned("backend"), None, "a new conversation has addressed nobody");
}

/// An engine with no store has no honest way to run an in-process
/// teammate — its transcript would be a root session nobody could resume —
/// so [`Storeless`] refuses the spawn by name while the pane surfaces stay
/// somebody else's to provide.
#[tokio::test]
async fn a_storeless_engine_refuses_an_in_process_teammate_by_name() {
    struct AllowSpawn;

    #[async_trait::async_trait]
    impl crate::subagent::SpawnAsker for AllowSpawn {
        async fn ask(
            &self,
            _request: crate::subagent::SpawnAsk,
        ) -> crate::protocol::PermissionReply {
            crate::protocol::PermissionReply::Once
        }
    }

    let home = tempfile::tempdir().expect("a temp teams root");
    let engine = engine().with_teammates(
        Arc::new(teammate::TeammateRegistry::new(
            ganja_team::TeamsRoot::new(home.path().join("teams")),
            ganja_team::TeamName::parse("session-abcd1234").expect("a team name"),
            "session-abcd1234",
            home.path().join("project"),
        )),
        crate::subagent::Backends::new(),
    );

    let refused = engine
        .teammates()
        .expect("the engine leads a team")
        .start(
            crate::tool::task::TeammateSpawn {
                name: "w1".to_owned(),
                backend: Some("in-process".to_owned()),
                agent_type: "general".to_owned(),
                prompt: "hello".to_owned(),
            },
            &crate::subagent::Caller {
                model: MODEL.to_owned(),
                cwd: home.path().join("project"),
                permissions: Arc::new(std::sync::Mutex::new(
                    crate::permission::Permissions::default(),
                )),
                project_root: home.path().join("project"),
            },
            &AllowSpawn,
        )
        .await
        .expect_err("a storeless engine cannot keep a teammate's transcript");
    assert!(
        refused.reason.contains(super::STORELESS),
        "the refusal names the missing store: {}",
        refused.reason
    );
}

/// One limit, written twice, pinned equal.
///
/// `ganja-tool` may not name `ganja-protocol` — its internal dependency
/// list is exactly the permission crate — so §5.3's cap on a summary is
/// declared on both sides of that boundary. This crate is the only one
/// that sees both, which is what makes the pin its debt.
#[test]
fn the_summary_cap_the_tool_enforces_is_the_one_the_wire_declares() {
    assert_eq!(
        send_message::SUMMARY_CAP,
        crate::protocol::team::DISPLAY_FIELD_CAP,
        "a summary capped at one number and rendered against another is a summary cut twice"
    );
}

/// The hooks half of the reload: an install lands for the next fire, and
/// [`None`] uninstalls rather than leaving the old table standing.
#[test]
fn replacing_the_hooks_installs_for_the_next_fire_and_none_uninstalls() {
    let engine = engine();
    assert!(engine.hooks().is_none(), "the fixture starts hookless");

    let table = std::collections::BTreeMap::from([(
        "Stop".to_owned(),
        vec![crate::config::HookMatcher {
            matcher: None,
            hooks: vec![crate::config::HookHandler::Command(crate::config::HookCommand {
                command: "true".to_owned(),
                timeout: None,
            })],
        }],
    )]);
    let hooks = crate::hook::Hooks::new(&table, &PathBuf::from("."))
        .expect("one Stop handler is a hooks table");
    engine.replace_hooks(Some(hooks));
    assert!(
        engine.hooks().is_some_and(|hooks| hooks.fires(crate::hook::HookEvent::Stop)),
        "the swapped-in table is the one the next fire reads"
    );

    engine.replace_hooks(None);
    assert!(
        engine.hooks().is_none(),
        "a reload that found no hooks leaves an engine that does no hook work"
    );
}

/// The prompt half of the reload: the replaced closure is recomposed on
/// the spot, so the suffix the next request carries already reflects it.
#[test]
fn replacing_the_environment_recomposes_the_suffix_immediately() {
    let engine = engine();
    assert_eq!(engine.environment_half(), None);

    engine.replace_environment(|model| Some(format!("environment for {model}")));

    assert_eq!(
        engine.environment_half().as_deref(),
        Some(format!("environment for {MODEL}").as_str()),
        "the swap recomposes now rather than waiting for a model switch"
    );
}

// ---------------------------------------------------------------------
// The peer address seam and the facts it feeds (**D532**)
// ---------------------------------------------------------------------

/// The one seam sets both readings of one fact, and clears both together:
/// there is no way to end up advertising a reply address with no marker, or
/// a marker with no address.
#[test]
fn the_peer_address_seam_sets_and_clears_both_readings_at_once() {
    let engine = engine();
    assert_eq!(engine.peer_address(), None, "an unbound session has neither");

    let socket = std::path::Path::new("/tmp/ganja-501/0198c1a2.sock");
    engine.set_peer_address(Some(socket));
    assert_eq!(
        engine.peer_address(),
        Some((socket.to_path_buf(), "0198c1a2".to_owned())),
        "one call, both readings, off one path"
    );

    engine.set_peer_address(None);
    assert_eq!(engine.peer_address(), None, "and one call takes both away again");
}

/// A path with no readable stem records **nothing** rather than half of the
/// fact: a marker guessed from a nameless path would be the invention this
/// design refuses everywhere else.
#[test]
fn a_path_with_no_stem_records_nothing_at_all() {
    let engine = engine();

    engine.set_peer_address(Some(std::path::Path::new("/")));

    assert_eq!(engine.peer_address(), None);
}

/// The facts value reads the engine's **cells**, so a mode switch, a bind and
/// an inbound chain all reach the next send rather than a copy taken when the
/// postbox was built.
#[test]
fn the_peer_facts_read_the_cells_rather_than_a_snapshot() {
    use crate::subagent::SenderMode;

    let engine = engine();
    let facts = engine.peer_facts();

    assert_eq!(facts.sender_mode(), Some(SenderMode::Prompting));
    assert_eq!(facts.reply_to(), None);
    assert!(facts.hop_chain().is_empty());

    *engine.permission_mode.lock().expect("the permission mode is never poisoned") =
        crate::protocol::PermissionMode::Bypass;
    let socket = std::path::Path::new("/tmp/ganja-501/0198c1a2.sock");
    engine.set_peer_address(Some(socket));

    assert_eq!(
        facts.sender_mode(),
        Some(SenderMode::Bypass),
        "the class is read live, through the same call the receiver's own door makes"
    );
    assert_eq!(facts.reply_to(), Some(socket.to_path_buf()));
    assert_eq!(facts.hop_chain(), vec!["0198c1a2".to_owned()]);
}

/// The sender cap truncates **oldest-first**, which is the clause no door in
/// this build can reach: the receiver's own chain check drops anything past
/// 28 entries, so a chain can only ever *inherit* 28 and the 33rd entry
/// cannot arrive that way. Asserted over the facts value, where the chain can
/// be as long as the arithmetic needs.
#[test]
fn the_sender_cap_drops_the_oldest_entries_first() {
    use crate::subagent::MAX_HOP_CHAIN_ENTRIES;

    let engine = engine();
    let inherited: Vec<String> = (0..40).map(|index| format!("0198c{index:03}")).collect();
    *engine.inbound_chain.lock().expect("the inbound chain cell is never poisoned") =
        inherited.clone();
    engine.set_peer_address(Some(std::path::Path::new("/tmp/ganja-501/0198ffff.sock")));

    let carried = engine.peer_facts().hop_chain();

    assert_eq!(carried.len(), MAX_HOP_CHAIN_ENTRIES, "the cap bounds it");
    assert_eq!(
        carried[0],
        inherited[40 - (MAX_HOP_CHAIN_ENTRIES - 1)],
        "what survives is the newest, oldest-first being what goes"
    );
    assert_eq!(
        carried[MAX_HOP_CHAIN_ENTRIES - 1],
        "0198ffff",
        "and this session is still the last entry"
    );
}
