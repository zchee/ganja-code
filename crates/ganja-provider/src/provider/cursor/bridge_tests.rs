//! The drops and the no-drops (**AC-18**), the recovery and its cap
//! (**D553**, AC-14 through AC-15), driven through the real pause and the
//! real resume.
//!
//! Nothing here fakes the machinery it is testing: every case builds a fold
//! over in-memory channels — the `replay` precedent — hands it a real
//! [`HeldRuns`] table, and drives it with the same `run` a live turn uses. Two
//! `stream()` calls share one table, which is what makes "a `stream()` never
//! drops a held run it does not key to" a thing a test can watch happen. The
//! cases that need to see the Run a recovery *opens* go through
//! `Provider::stream` against `tests::serve_run`'s loopback, and read back
//! the run request that actually went out.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use buffa::Message as _;
use futures::StreamExt as _;
use futures::channel::mpsc;
use tokio_util::sync::CancellationToken;

use super::{HeldRuns, Key, Reason, Resolution};
use crate::protocol::{FinishReason, Message, MessageId, Part, PartBody, ToolState};
use crate::provider::cursor::tests::{
    Answered, Served, end_stream, finished, framed, opening, roster, sent_so_far, serve_run, text,
    turn_ended,
};
use crate::provider::cursor::{Bridge, CursorProvider, Duplex, connect, history, proto};
use crate::provider::{ChatRequest, CredentialSource, Provider as _, ProviderEvent};

/// The same request one step later, with the assistant's tool part on it —
/// what the engine hands back after running the call.
fn answered(request: &ChatRequest, call_id: &str, state: ToolState) -> ChatRequest {
    let mut resumed = request.clone();
    let mut reply = Message::assistant(&request.model);
    reply.parts.push(Part {
        id: crate::protocol::PartId::ascending(),
        body: PartBody::Tool { call_id: call_id.to_owned(), tool: "read".to_owned(), state },
    });
    resumed.messages.push(reply);

    resumed
}

/// A frame carrying the server calling the declared tool.
fn called(id: u32, tool_call_id: &str) -> Vec<u8> {
    exec(id, |exec| {
        exec.mcp_args = buffa::MessageField::some(
            proto::McpArgs::default()
                .with_name("read")
                .with_tool_name("read")
                .with_tool_call_id(tool_call_id)
                .with_provider_identifier("ganja"),
        );
    })
}

/// A frame carrying one exec, `fill` having chosen its kind and arguments.
fn exec(id: u32, fill: impl FnOnce(&mut proto::ExecRequest)) -> Vec<u8> {
    let mut asked = proto::ExecRequest {
        id: Some(id),
        exec_id: Some(format!("exec-{id}")),
        ..Default::default()
    };
    fill(&mut asked);
    let message = proto::ServerMessage {
        exec_request: buffa::MessageField::some(asked),
        ..Default::default()
    };

    connect::envelope(&message.encode_to_vec())
}

/// A turn paused on one bridged call, with its channels left open so the test
/// can watch what happens to the held Run.
struct Paused {
    request: ChatRequest,
    cancel: CancellationToken,
    /// Kept alive so the response body does not end under the held fold.
    _body: mpsc::UnboundedSender<Result<Vec<u8>, Infallible>>,
    /// What the wire answered on the request body, so a test can watch the
    /// heartbeats.
    answered: Answered,
    events: Vec<ProviderEvent>,
}

/// Runs one turn up to its pause and returns everything it left behind.
async fn pause(model: &str, held: &Arc<HeldRuns>, call_id: &str) -> Paused {
    pause_with(opening(model), held, call_id).await
}

/// The same for a request of the caller's — a recovered turn's, whose
/// opening message and so whose key is one an earlier pause already used.
async fn pause_with(request: ChatRequest, held: &Arc<HeldRuns>, call_id: &str) -> Paused {
    let key = Key::of(&request).expect("a request with a message keys");
    let cancel = CancellationToken::new();

    let (body, chunks) = mpsc::unbounded::<Result<Vec<u8>, Infallible>>();
    let (answers, answered) = mpsc::unbounded();
    let stream = crate::provider::cursor::events(
        chunks,
        cancel.clone(),
        Duplex::for_tests(answers, roster()),
        Some(Bridge::new(Arc::clone(held), key)),
    );

    body.unbounded_send(Ok(called(1, call_id))).expect("the body is open");
    let events: Vec<ProviderEvent> =
        tokio::time::timeout(Duration::from_secs(10), stream.collect())
            .await
            .expect("a bridged exec pauses the stream rather than hanging it");

    Paused { request, cancel, _body: body, answered, events }
}

/// The events a pause commits to: the call, then the finish that ends the step.
fn calls(events: &[ProviderEvent], call_id: &str) {
    assert_eq!(
        events,
        [
            ProviderEvent::ToolCallStart { id: call_id.to_owned(), name: "read".to_owned() },
            ProviderEvent::ToolCallDelta { id: call_id.to_owned(), json: "{}".to_owned() },
            ProviderEvent::ToolCallEnd { id: call_id.to_owned() },
            ProviderEvent::Finish(FinishReason::Completed),
        ],
        "the exec arrives whole, so one delta carries the whole argument object"
    );
}

fn completed(output: &str) -> ToolState {
    ToolState::Completed {
        input: serde_json::json!({}),
        output: output.to_owned(),
        title: "read".to_owned(),
        metadata: serde_json::json!({}),
        started: 0,
        completed: 0,
    }
}

/// The shape the whole feature rests on: a declared tool called mid-stream
/// becomes a tool call, the step ends, and the **same** Run is read on when
/// the result arrives — with the answer going out on the body that was never
/// closed.
#[tokio::test]
async fn a_bridged_call_pauses_the_run_and_the_result_resumes_it() {
    let held = Arc::new(HeldRuns::default());
    let mut paused = pause("auto", &held, "call-1").await;
    calls(&paused.events, "call-1");

    let resumed = answered(&paused.request, "call-1", completed("the file's contents"));
    assert!(
        matches!(held.resolve(&resumed), Resolution::Resume(_)),
        "the result keys the held run, and the answers go out as it is resolved"
    );

    // The answer the server was waiting for, on the body the pause left open.
    let sent = sent_so_far(&mut paused.answered);
    let result = sent
        .iter()
        .find_map(|message| message.exec_response.as_option())
        .expect("the tool's result went out on the exec channel");
    assert_eq!(result.id, Some(1), "the id the server minted comes back");
    assert_eq!(
        result.mcp_result.as_option().and_then(|result| result.success.as_option()).map(
            |success| success.content[0]
                .text
                .as_option()
                .and_then(|text| text.text.as_deref())
                .unwrap_or_default()
                .to_owned()
        ),
        Some("the file's contents".to_owned())
    );
    assert!(
        sent.iter().any(|message| message
            .exec_control
            .as_option()
            .is_some_and(|control| control.stream_close.is_set())),
        "and then the close that ends the exec"
    );
    drop(paused.cancel);
}

/// **Drop 1.** The turn that paused was cancelled, so nobody is left to answer
/// the Run it was holding.
#[tokio::test]
async fn a_cancelled_turn_drops_the_run_it_was_holding() {
    let held = Arc::new(HeldRuns::default());
    let paused = pause("auto", &held, "call-1").await;

    paused.cancel.cancel();
    settled(&held, &paused.request).await;

    let resumed = answered(&paused.request, "call-1", completed("late"));
    assert!(
        matches!(held.resolve(&resumed), Resolution::Recover(why) if why.contains("cancelled")),
        "a resume against a cancelled bridge is recovered on a fresh run, and the log line says \
         why"
    );
}

/// **Drop 2**, the guard: a differently-keyed request carrying *another* held
/// Run's results. Root turns are serial and each subagent keys to its own
/// opening message, so the engine does not produce this state today — the test
/// constructs it, and the rule is kept for a future that does.
#[tokio::test]
async fn a_request_carrying_another_held_runs_results_evicts_it() {
    let held = Arc::new(HeldRuns::default());
    let first = pause("auto", &held, "call-1").await;
    let second = pause("gpt-5.3-codex", &held, "call-2").await;

    // Keyed to the second turn, carrying the first turn's result.
    let confused = answered(&second.request, "call-1", completed("somebody else's answer"));
    assert!(
        matches!(held.resolve(&confused), Resolution::Fresh),
        "a request whose own run is still waiting opens a fresh one and keeps it held"
    );

    let resumed = answered(&first.request, "call-1", completed("its own answer"));
    assert!(
        matches!(held.resolve(&resumed), Resolution::Recover(why)
            if why.contains("differently-keyed")),
        "the run whose results turned up elsewhere is gone, the recovery says why"
    );

    let still_held = answered(&second.request, "call-2", completed("its own answer"));
    assert!(
        matches!(held.resolve(&still_held), Resolution::Resume(_)),
        "and the run this request did key to was never touched"
    );
}

/// **Drop 3**, and the case nothing else covers: a turn that pauses and then
/// simply never comes back. Without the bound the Run would hold a task and a
/// socket for the life of the process.
#[tokio::test(start_paused = true)]
async fn a_run_nobody_resumes_is_dropped_at_the_idle_bound() {
    let held = Arc::new(HeldRuns::default());
    let paused = pause("auto", &held, "call-1").await;

    // Just inside the bound: still there, still beating.
    tokio::time::sleep(super::IDLE_BOUND - Duration::from_secs(1)).await;
    let early = answered(&paused.request, "call-1", completed("in time"));
    assert!(
        matches!(held.resolve(&early), Resolution::Resume(_)),
        "a resume inside the bound is still a resume"
    );

    // And past it, on a second run held the same way.
    let paused = pause("auto", &held, "call-2").await;
    tokio::time::sleep(super::IDLE_BOUND + Duration::from_secs(1)).await;

    let late = answered(&paused.request, "call-2", completed("too late"));
    assert!(
        matches!(held.resolve(&late), Resolution::Recover(why) if why.contains("idle bound")),
        "past the bound the run is gone, and the resume recovers naming the bound"
    );
}

/// **Drop 4.** The request body closed under a held Run, so the heartbeat the
/// keeper writes has nowhere to go and no answer ever could either.
///
/// The keeper is the only thing watching that channel while a Run is held —
/// nothing else writes to it between the pause and the resume — so the *beat*
/// failing is how this build learns the body is gone.
#[tokio::test(start_paused = true)]
async fn a_body_that_closes_under_a_held_run_drops_it_and_the_resume_recovers_over_history() {
    let held = Arc::new(HeldRuns::default());
    let paused = pause("auto", &held, "call-1").await;

    // The far end of the request body goes away, which is what a server
    // hanging up on the Run looks like from here.
    drop(paused.answered);
    tokio::time::sleep(super::HEARTBEAT + Duration::from_secs(1)).await;
    settled(&held, &paused.request).await;

    let resumed = answered(&paused.request, "call-1", completed("nowhere to go"));
    assert!(
        matches!(held.resolve(&resumed), Resolution::Recover(why)
            if why.contains("closed the request body")),
        "a resume against a closed body recovers naming which of the four reasons it was"
    );
}

/// The same closure a beat ahead of the keeper: the entry is still in the
/// table, so the resume finds it, its answers cannot be delivered, and the
/// turn is **recovered** on a fresh Run over the composed history — under
/// `resume_action`, since the request's newest message is the assistant's —
/// rather than read on into a Run nobody is generating into.
///
/// Driven through `CursorProvider::stream` against a loopback, so the Run
/// the recovery opens is the one the wire really built and sent.
#[tokio::test]
async fn a_resume_whose_body_closed_under_it_reopens_a_run_rather_than_reading_on() {
    let served = serve_run(finished("Reopened."), false).await;
    let provider = provider_at(&served);
    let paused = pause("auto", &provider.held, "call-1").await;

    // Closed, and resolved before the keeper's next beat notices — which is
    // the window in which a resume can find an entry it cannot settle.
    drop(paused.answered);
    let resumed = answered(&paused.request, "call-1", completed("the file's contents"));

    let events = through_provider(&provider, resumed.clone()).await;
    assert_eq!(
        events,
        vec![
            ProviderEvent::TextDelta("Reopened.".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ],
        "the turn read on from a fresh run rather than failing: {events:?}"
    );
    assert_resumed_over(served, &resumed, "the file's contents").await;
}

/// The provider a recovery is driven through: the loopback `served` answers
/// on, with a credential that is a value rather than a store.
fn provider_at(served: &Served) -> CursorProvider {
    CursorProvider::at(
        &served.base_url,
        CredentialSource::key("at-bridge-canary").expect("a non-blank token"),
    )
    .expect("loopback may carry a token")
}

/// One turn through `Provider::stream`, collected — bounded, because a
/// recovery that hung would otherwise hang the suite.
async fn through_provider(provider: &CursorProvider, request: ChatRequest) -> Vec<ProviderEvent> {
    tokio::time::timeout(Duration::from_secs(10), async {
        provider
            .stream(request, CancellationToken::new())
            .await
            .expect("the turn opens on the loopback")
            .collect()
            .await
    })
    .await
    .expect("the turn ends rather than hanging")
}

/// What a recovered Run's run request has to say: `resume_action`, and a
/// state whose root ids are `request`'s own composition — the map the Run
/// answers gets from — with a root entry carrying `output`, the text the
/// tool answered. Takes the endpoint, since reading the one request it
/// recorded consumes it.
async fn assert_resumed_over(served: Served, request: &ChatRequest, output: &str) {
    let opened = served.opened.await.expect("the recovered run went out");
    assert!(
        opened.action.as_option().is_some_and(|action| action.resume_action.is_set()),
        "a recovery resumes over the composed state rather than sending a user message: \
         {opened:?}"
    );
    let composed = history::compose(request);
    let state = opened.conversation_state.as_option().expect("a composed state");
    assert_eq!(
        state.root_prompt_messages_json, composed.root,
        "the ids on the wire are the composition's"
    );
    assert!(
        composed
            .root
            .iter()
            .any(|id| String::from_utf8_lossy(&composed.blobs[id]).contains(output)),
        "what the tool answered rides the state into the reopened run"
    );
}

/// The heartbeat that makes a hold survivable at all: while a Run is held,
/// something has to keep the exchange alive, and a live 25-second hold on this
/// ping alone was measured before this was built.
#[tokio::test(start_paused = true)]
async fn a_held_run_beats_on_the_body_it_left_open() {
    let held = Arc::new(HeldRuns::default());
    let mut paused = pause("auto", &held, "call-1").await;

    tokio::time::sleep(super::HEARTBEAT * 5 + Duration::from_secs(1)).await;

    let beats = sent_so_far(&mut paused.answered)
        .into_iter()
        .filter(|message| message.client_heartbeat.is_set())
        .count();

    assert!(beats >= 4, "a held run beats on the body it left open, and beat {beats} times");
}

/// **No-drop 1.** A title or summary one-shot — `turn_start == 0`, no tools,
/// no results — keys to nothing and must leave a held Run exactly where it is.
/// The engine runs one of these on the same wire at the end of every turn.
#[tokio::test]
async fn a_one_shot_turn_leaves_a_held_run_alone() {
    let held = Arc::new(HeldRuns::default());
    let paused = pause("auto", &held, "call-1").await;

    let one_shot = ChatRequest {
        messages: vec![Message::user("summarize this conversation")],
        tools: Vec::new(),
        ..opening("auto")
    };
    assert!(matches!(held.resolve(&one_shot), Resolution::Fresh));

    let resumed = answered(&paused.request, "call-1", completed("still here"));
    assert!(matches!(held.resolve(&resumed), Resolution::Resume(_)), "the held run survived");
}

/// **No-drop 2.** A `task` child turn — its own opening message, its own key,
/// up to `agents.concurrency` of them at once on the same `Arc<dyn Provider>`.
#[tokio::test]
async fn a_subagent_turn_leaves_its_parents_held_run_alone() {
    let held = Arc::new(HeldRuns::default());
    let paused = pause("auto", &held, "call-1").await;

    let child = opening("auto");
    assert_ne!(
        Key::of(&child),
        Key::of(&paused.request),
        "a child turn opens with a message of its own, which is what keys it elsewhere"
    );
    assert!(matches!(held.resolve(&child), Resolution::Fresh));

    let resumed = answered(&paused.request, "call-1", completed("still here"));
    assert!(matches!(held.resolve(&resumed), Resolution::Resume(_)));
}

/// **No-drop 3.** The compaction summary: a turn of its own, a fresh
/// `[summary, prompt]` with newly minted ids, so it keys to nothing.
#[tokio::test]
async fn the_compaction_summary_leaves_a_held_run_alone() {
    let held = Arc::new(HeldRuns::default());
    let paused = pause("auto", &held, "call-1").await;

    let compaction = ChatRequest {
        messages: vec![Message::user("<summary>…</summary>"), Message::user("carry on")],
        ..opening("auto")
    };
    assert!(matches!(held.resolve(&compaction), Resolution::Fresh));

    let resumed = answered(&paused.request, "call-1", completed("still here"));
    assert!(matches!(held.resolve(&resumed), Resolution::Resume(_)));
}

/// **No-drop 4.** An ordinary new turn keys to no held run and opens a fresh
/// one, which is the common case and must cost the held run nothing.
#[tokio::test]
async fn a_request_keying_to_nothing_opens_a_fresh_run() {
    let held = Arc::new(HeldRuns::default());
    let paused = pause("auto", &held, "call-1").await;

    let next = opening("auto");
    assert!(matches!(held.resolve(&next), Resolution::Fresh));

    let resumed = answered(&paused.request, "call-1", completed("still here"));
    assert!(matches!(held.resolve(&resumed), Resolution::Resume(_)));
}

/// **No-drop 5** (and **AC-14c**, the table half). A keyed hit whose
/// confirmation *fails* — the request keys to the held Run but carries no
/// finished result for the exec it is waiting on. A fresh Run, and the held
/// one is left exactly as it was: dropping it would throw away a bridge that
/// is still perfectly good, and the fresh Run's composed history names the
/// call as unanswered rather than inventing an answer (`cursor::tests` reads
/// that run request back).
#[tokio::test]
async fn a_keyed_hit_whose_results_have_not_arrived_leaves_the_run_held() {
    let held = Arc::new(HeldRuns::default());
    let paused = pause("auto", &held, "call-1").await;
    let key = Key::of(&paused.request).expect("a request with a message keys");

    // Same key, but the call is still pending.
    let pending = answered(&paused.request, "call-1", ToolState::Pending { input: None });
    assert!(matches!(held.resolve(&pending), Resolution::Fresh));
    assert!(held.holds(&key), "a fresh run, and the held one is still in the table");

    // And the same key with a result for a call this run never asked for.
    let elsewhere = answered(&paused.request, "call-9", completed("not ours"));
    assert!(matches!(held.resolve(&elsewhere), Resolution::Fresh));
    assert!(held.holds(&key));

    let resumed = answered(&paused.request, "call-1", completed("at last"));
    assert!(
        matches!(held.resolve(&resumed), Resolution::Resume(_)),
        "the held run was still there for the resume that did carry its result"
    );
}

/// A resume against a bridge that was never held at all is a recovery too —
/// the ring has no reason to name, so the sentence carries none — and the
/// fresh Run it opens carries the composed history, which is what makes
/// reopening honest where it once quietly forgot what the tool answered.
#[tokio::test]
async fn a_resume_against_a_bridge_that_never_existed_reopens_a_run_over_the_composed_history() {
    let held = Arc::new(HeldRuns::default());
    let request = opening("auto");
    let resumed = answered(&request, "call-1", completed("an answer to nothing"));

    let Resolution::Recover(why) = held.resolve(&resumed) else {
        panic!("a request carrying tool results resumes something or recovers");
    };
    assert!(
        why.contains("gone; reopening a run over the composed conversation"),
        "no reason clause for a drop the ring never saw, and the recovery is named: {why}"
    );
    assert_eq!(
        history::compose(&resumed).action,
        history::Action::Resume,
        "and the run that recovery opens goes out under `resume_action`, by the request's shape"
    );
}

/// A request with no messages at all keys to nothing and cannot be resumed —
/// which is the honest answer rather than a panic on an empty list.
#[test]
fn a_request_with_no_messages_keys_to_nothing() {
    let empty =
        ChatRequest { messages: Vec::new(), turn_start: 7, tools: Vec::new(), ..opening("auto") };

    assert_eq!(Key::of(&empty), None);
}

/// `turn_start` is a `pub` field on a `pub` struct, so its value is a caller's:
/// one past the end names no message, and the key clamps rather than slices.
#[test]
fn a_turn_start_past_the_end_is_clamped_rather_than_sliced() {
    let mut request = opening("auto");
    request.turn_start = 99;

    assert_eq!(
        Key::of(&request),
        Key::of(&opening_with(request.messages[0].id.clone())),
        "the last message is the far end of the clamp"
    );
}

/// A request whose opening message is `id`.
fn opening_with(id: MessageId) -> ChatRequest {
    let mut request = opening("auto");
    request.messages[0].id = id;
    request
}

/// **AC-14.** The reason a recovery names is the reason its bridge actually
/// went, which is what makes the log line worth reading: four reasons, four
/// distinct clauses, and a resume keyed to a Run dropped for each one
/// recovers with that reason's clause in its sentence.
#[tokio::test]
async fn every_drop_reason_has_a_sentence_of_its_own_and_the_recovery_names_it() {
    let reasons = [Reason::Cancelled, Reason::Idle, Reason::Closed, Reason::Evicted];
    let spelled: Vec<&str> = reasons.iter().map(Reason::spelled).collect();
    let mut unique = spelled.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), spelled.len(), "four reasons, four sentences: {spelled:?}");

    let held = Arc::new(HeldRuns::default());
    for (index, reason) in reasons.into_iter().enumerate() {
        let call_id = format!("call-{index}");
        let paused = pause("auto", &held, &call_id).await;
        let key = Key::of(&paused.request).expect("a request with a message keys");
        let clause = reason.spelled();
        held.drop_run(&key, reason);

        let resumed = answered(&paused.request, &call_id, completed("after the drop"));
        let Resolution::Recover(why) = held.resolve(&resumed) else {
            panic!("a resume keyed to a dropped run recovers");
        };
        assert!(why.contains(clause), "the recovery names its drop: {why:?} lacks {clause:?}");
    }
}

/// Waits for the keeper task to notice what the test just did.
///
/// The drop happens in a task of its own — a child of nothing a turn owns —
/// so a test that asserted immediately would be racing it. Watched through
/// the table alone, never through `resolve`: a poll that resolved a request
/// carrying results would spend the one recovery the cap allows.
async fn settled(held: &Arc<HeldRuns>, request: &ChatRequest) {
    let key = Key::of(request).expect("a request with a message keys");
    for _ in 0..100 {
        if !held.holds(&key) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    panic!("the keeper never dropped the held run");
}

/// One frame at a time is the worst case for a batch, and one chunk carrying
/// two execs is the other: the server issues concurrent execs — two
/// `grep_args` within 5 ms on a recorded run — and both must ride one step
/// rather than pausing twice.
#[tokio::test]
async fn a_batch_the_server_sent_together_rides_one_step() {
    let held = Arc::new(HeldRuns::default());
    let request = opening("auto");
    let key = Key::of(&request).expect("a request with a message keys");
    let cancel = CancellationToken::new();

    let mut batch = called(1, "call-1");
    batch.extend(called(2, "call-2"));

    // The body stays open, the way a real one does while the server waits for
    // the answers: a response stream that *ended* here would be a bridge that
    // died mid-batch, which is a different case and a failed turn.
    let (body, chunks) = mpsc::unbounded::<Result<Vec<u8>, Infallible>>();
    let (answers, _answered) = mpsc::unbounded();
    let stream = crate::provider::cursor::events(
        chunks,
        cancel,
        Duplex::for_tests(answers, roster()),
        Some(Bridge::new(Arc::clone(&held), key)),
    );
    body.unbounded_send(Ok(batch)).expect("the body is open");

    let events: Vec<ProviderEvent> =
        tokio::time::timeout(Duration::from_secs(10), stream.collect())
            .await
            .expect("the batch pauses once");
    let started: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ToolCallStart { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();

    assert_eq!(started, vec!["call-1", "call-2"], "both calls, one step");
    assert_eq!(
        events.last(),
        Some(&ProviderEvent::Finish(FinishReason::Completed)),
        "and one finish"
    );
}

/// **M2.** A terminal event arriving while execs are still gathering ends the
/// step without a hold: the server decided not to wait, so there is nothing
/// left to hold open and nothing a later request could resume. The bridged
/// call reaches the engine as nothing — no `ToolCallStart` — while a refusal
/// that was due in the same chunk still goes out, because a refusal is
/// answered the moment it decodes and a bridge is answered on a resume that
/// will never come.
#[tokio::test]
async fn a_run_that_ends_while_execs_are_gathering_never_holds() {
    let held = Arc::new(HeldRuns::default());
    let request = opening("auto");
    let key = Key::of(&request).expect("a request with a message keys");

    let (body, chunks) = mpsc::unbounded::<Result<Vec<u8>, Infallible>>();
    let (answers, mut answered) = mpsc::unbounded();
    let stream = crate::provider::cursor::events(
        chunks,
        CancellationToken::new(),
        Duplex::for_tests(answers, roster()),
        Some(Bridge::new(Arc::clone(&held), key.clone())),
    );

    // One chunk: the bridgeable call, a refusable delete, then the turn's end
    // and the verdict — all cut and mapped before the gather window can close.
    let mut chunk = called(1, "call-1");
    chunk.extend(exec(2, |exec| {
        exec.delete_args = buffa::MessageField::some(proto::DeleteArgs::default().with_path("/f"));
    }));
    chunk.extend(framed(turn_ended()));
    chunk.extend(end_stream("{}"));
    body.unbounded_send(Ok(chunk)).expect("the body is open");
    drop(body);

    let events: Vec<ProviderEvent> =
        tokio::time::timeout(Duration::from_secs(10), stream.collect())
            .await
            .expect("a turn the server ended ends here too");
    assert!(
        !events.iter().any(|event| matches!(event, ProviderEvent::ToolCallStart { .. })),
        "a call the server did not wait for never reaches the engine: {events:?}"
    );
    assert_eq!(events.last(), Some(&ProviderEvent::Finish(FinishReason::Completed)));
    assert!(!held.holds(&key), "nothing was held: there is no run left to resume");

    let sent = sent_so_far(&mut answered);
    assert_eq!(sent.len(), 2, "the delete's refusal and its close, and nothing for the call");
    assert!(
        sent[0]
            .exec_response
            .as_option()
            .is_some_and(|response| response.id == Some(2) && response.delete_result.is_set()),
        "the refusal that was due still went out: {sent:?}"
    );
}

/// **M3, the restart.** A second exec inside the gather window restarts it,
/// and both ride one step. The restart is what is pinned, and it is pinned by
/// the clock rather than by the batch alone: under paused time the pause lands
/// exactly on the deadline that fired, so a window that had *not* restarted
/// would end this step one half-window earlier.
#[tokio::test(start_paused = true)]
async fn a_second_exec_inside_the_window_restarts_it_and_rides_the_same_step() {
    let Reading { body, _answered, driver } = reading();
    let opened = tokio::time::Instant::now();

    body.unbounded_send(Ok(called(1, "call-1"))).expect("the body is open");
    tokio::time::sleep(super::super::GATHER_WINDOW / 2).await;
    body.unbounded_send(Ok(called(2, "call-2"))).expect("the body is open");

    let events = paused_step(driver).await;
    assert_eq!(
        opened.elapsed(),
        super::super::GATHER_WINDOW + super::super::GATHER_WINDOW / 2,
        "the window restarted on the second exec"
    );
    assert_eq!(started(&events), vec!["call-1", "call-2"], "both calls, one step");
}

/// **M3, the clause.** Text does not extend the window: a chatty server cannot
/// postpone the step it is waiting on. Same clock, opposite reading — the
/// pause lands on the *first* exec's deadline, a half-window after the text.
#[tokio::test(start_paused = true)]
async fn a_text_frame_inside_the_window_does_not_extend_it() {
    let Reading { body, _answered, driver } = reading();
    let opened = tokio::time::Instant::now();

    body.unbounded_send(Ok(called(1, "call-1"))).expect("the body is open");
    tokio::time::sleep(super::super::GATHER_WINDOW / 2).await;
    body.unbounded_send(Ok(framed(text("meanwhile…")))).expect("the body is open");

    let events = paused_step(driver).await;
    assert_eq!(opened.elapsed(), super::super::GATHER_WINDOW, "text moved nothing");
    assert_eq!(started(&events), vec!["call-1"]);
    assert!(
        events.contains(&ProviderEvent::TextDelta("meanwhile…".to_owned())),
        "and the text still rode the step it arrived in: {events:?}"
    );
}

/// **M3, the boundary.** An exec arriving after the window closed rides the
/// next step: the first pause committed to one call, and the second call is
/// read only once the first has been answered and the same Run resumed.
#[tokio::test(start_paused = true)]
async fn an_exec_after_the_window_closed_rides_the_next_step() {
    let held = Arc::new(HeldRuns::default());
    let request = opening("auto");
    let key = Key::of(&request).expect("a request with a message keys");
    let cancel = CancellationToken::new();

    let (body, chunks) = mpsc::unbounded::<Result<Vec<u8>, Infallible>>();
    let (answers, _answered) = mpsc::unbounded();
    let stream = crate::provider::cursor::events(
        chunks,
        cancel.clone(),
        Duplex::for_tests(answers, roster()),
        Some(Bridge::new(Arc::clone(&held), key.clone())),
    );
    let driver = tokio::spawn(stream.collect::<Vec<ProviderEvent>>());

    body.unbounded_send(Ok(called(1, "call-1"))).expect("the body is open");
    tokio::time::sleep(super::super::GATHER_WINDOW * 2).await;
    body.unbounded_send(Ok(called(2, "call-2"))).expect("the body is open");

    let first = paused_step(driver).await;
    assert_eq!(started(&first), vec!["call-1"], "the window had closed before the second call");

    let resumed = answered(&request, "call-1", completed("done"));
    let Resolution::Resume(fold) = held.resolve(&resumed) else {
        panic!("the result keys the held run");
    };
    let second: Vec<ProviderEvent> =
        crate::provider::cursor::run(*fold, cancel, Some(Bridge::new(Arc::clone(&held), key)))
            .collect()
            .await;
    assert_eq!(started(&second), vec!["call-2"], "the second call is the next step's");
}

/// A bridging fold over an open body, driven in a task of its own so the
/// gather clock ticks while the test is sleeping.
struct Reading {
    body: mpsc::UnboundedSender<Result<Vec<u8>, Infallible>>,
    /// Kept alive so the answer channel stays open under the fold.
    _answered: Answered,
    driver: tokio::task::JoinHandle<Vec<ProviderEvent>>,
}

fn reading() -> Reading {
    let held = Arc::new(HeldRuns::default());
    let request = opening("auto");
    let key = Key::of(&request).expect("a request with a message keys");

    let (body, chunks) = mpsc::unbounded::<Result<Vec<u8>, Infallible>>();
    let (answers, answered) = mpsc::unbounded();
    let stream = crate::provider::cursor::events(
        chunks,
        CancellationToken::new(),
        Duplex::for_tests(answers, roster()),
        Some(Bridge::new(Arc::clone(&held), key)),
    );

    Reading { body, _answered: answered, driver: tokio::spawn(stream.collect()) }
}

/// The events a driven fold committed to when it paused — bounded, so a
/// window that never closes is a failure rather than a hang under a clock
/// nothing else advances.
async fn paused_step(driver: tokio::task::JoinHandle<Vec<ProviderEvent>>) -> Vec<ProviderEvent> {
    tokio::time::timeout(Duration::from_secs(10), driver)
        .await
        .expect("the gather window closes")
        .expect("the fold pauses")
}

/// The call ids a step started, in order.
fn started(events: &[ProviderEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ToolCallStart { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect()
}

/// **M4.** A server that called without minting a `tool_call_id` still gets
/// an answer, under an id this wire minted: the engine needs one, and an empty
/// one would collide with the next empty one. The minted id is what the tool
/// part carries and what the resume is matched by — and the answer still goes
/// out under the *exec's* own numeric id, which is the only key the server
/// reads.
#[tokio::test]
async fn a_call_without_a_tool_call_id_is_bridged_under_a_minted_one() {
    let held = Arc::new(HeldRuns::default());
    let request = opening("auto");
    let key = Key::of(&request).expect("a request with a message keys");

    let (body, chunks) = mpsc::unbounded::<Result<Vec<u8>, Infallible>>();
    let (answers, mut replies) = mpsc::unbounded();
    let stream = crate::provider::cursor::events(
        chunks,
        CancellationToken::new(),
        Duplex::for_tests(answers, roster()),
        Some(Bridge::new(Arc::clone(&held), key)),
    );
    body.unbounded_send(Ok(exec(1, |exec| {
        exec.mcp_args = buffa::MessageField::some(
            proto::McpArgs::default()
                .with_name("read")
                .with_tool_name("read")
                .with_provider_identifier("ganja"),
        );
    })))
    .expect("the body is open");

    let events: Vec<ProviderEvent> =
        tokio::time::timeout(Duration::from_secs(10), stream.collect())
            .await
            .expect("a bridged exec pauses the stream");
    let [minted] = started(&events).try_into().expect("one call");
    assert_eq!(minted.len(), 36, "a v4 uuid in the recorded client's spelling: {minted}");

    let resumed = answered(&request, minted, completed("under the minted id"));
    assert!(
        matches!(held.resolve(&resumed), Resolution::Resume(_)),
        "the minted id is what the resume is matched by"
    );

    let sent = sent_so_far(&mut replies);
    let result = sent
        .iter()
        .find_map(|message| message.exec_response.as_option())
        .expect("the result went out on the exec channel");
    assert_eq!(result.id, Some(1), "answered under the exec's own id, the one the server keyed");
    assert!(result.mcp_result.as_option().is_some_and(|result| result.success.is_set()));
}

/// **M6, now AC-15's opening half.** The recovery reaches the engine through
/// `Provider::stream` and not only through `resolve`: a turn resuming a
/// cancelled bridge opens a fresh Run under `resume_action` whose state
/// carries what the tool answered, and publishes no failure.
#[tokio::test]
async fn a_resume_against_a_dropped_bridge_recovers_through_the_provider_with_a_resume_action() {
    let served = serve_run(finished("Recovered."), false).await;
    let provider = provider_at(&served);
    let paused = pause("auto", &provider.held, "call-1").await;

    paused.cancel.cancel();
    settled(&provider.held, &paused.request).await;

    let resumed = answered(&paused.request, "call-1", completed("late"));
    let events = through_provider(&provider, resumed.clone()).await;
    assert!(
        !events.iter().any(|event| matches!(event, ProviderEvent::Failed(_))),
        "a recovery is not a failed turn: {events:?}"
    );
    assert_eq!(events.last(), Some(&ProviderEvent::Finish(FinishReason::Completed)));
    assert_resumed_over(served, &resumed, "late").await;
}

/// **AC-15.** A recovered Run is a Run like any other: held again by its own
/// next bridged exec under the **same** key, and resumed by that exec's
/// result — `Resume`, never `Failed`. The cap counts reopenings, not
/// continuations, so a recovered turn goes on exactly as an unrecovered one
/// does.
#[tokio::test]
async fn a_recovered_run_is_held_again_under_the_same_key_and_resumes_rather_than_failing() {
    // The reopened Run's server calls the declared tool again and waits.
    let served = serve_run(called(2, "call-2"), true).await;
    let provider = provider_at(&served);
    let paused = pause("auto", &provider.held, "call-1").await;
    let key = Key::of(&paused.request).expect("a request with a message keys");

    paused.cancel.cancel();
    settled(&provider.held, &paused.request).await;

    let resumed = answered(&paused.request, "call-1", completed("the file's contents"));
    let events = through_provider(&provider, resumed.clone()).await;
    assert!(
        !events.iter().any(|event| matches!(event, ProviderEvent::Failed(_))),
        "the recovery opened a run and paused on its exec: {events:?}"
    );
    assert_eq!(started(&events), vec!["call-2"], "the reopened run's own call reached the engine");
    assert_resumed_over(served, &resumed, "the file's contents").await;

    assert_eq!(
        Key::of(&resumed),
        Some(key.clone()),
        "a recovered run keys as the turn it reopened"
    );
    assert!(provider.held.holds(&key), "and it is held under that key");

    let again = answered(&resumed, "call-2", completed("the second file's contents"));
    assert!(
        matches!(provider.held.resolve(&again), Resolution::Resume(_)),
        "its result resumes the recovered run — the cap never stands in front of a resume"
    );
}

/// **AC-14b**, the cap in the shipped shape: recover → the reopened Run is
/// held again under the same key → dropped again, for a *different* reason,
/// so the ring's newest entry for the key is the second drop's → the next
/// keyed resume is refused by name, carrying that second reason. A different
/// turn's recovery is unaffected.
#[tokio::test(start_paused = true)]
async fn a_second_recovery_of_one_turn_is_refused_by_name_and_another_turns_is_not() {
    let held = Arc::new(HeldRuns::default());
    let first = pause("auto", &held, "call-1").await;
    let key = Key::of(&first.request).expect("a request with a message keys");
    first.cancel.cancel();
    settled(&held, &first.request).await;

    let resumed = answered(&first.request, "call-1", completed("once"));
    assert!(
        matches!(held.resolve(&resumed), Resolution::Recover(why) if why.contains("cancelled")),
        "the first recovery, naming the cancel"
    );

    // The recovered Run — the same opening message, so the same key — is held
    // again by its next exec and dropped again, this time at the idle bound.
    let again = pause_with(resumed, &held, "call-2").await;
    assert_eq!(Key::of(&again.request), Some(key.clone()));
    tokio::time::sleep(super::IDLE_BOUND + Duration::from_secs(1)).await;
    settled(&held, &again.request).await;

    let twice = answered(&again.request, "call-2", completed("twice"));
    let Resolution::Failed(sentence) = held.resolve(&twice) else {
        panic!("a second recovery of one turn is refused");
    };
    assert!(sentence.contains("already reopened once"), "the cap is named: {sentence}");
    assert!(
        sentence.contains("idle bound"),
        "with the second drop's reason, which a count kept in the ring would have lost: \
         {sentence}"
    );
    assert!(!held.holds(&key), "nothing is left held under the capped key");

    let other = pause("auto", &held, "call-3").await;
    other.cancel.cancel();
    settled(&held, &other.request).await;
    let elsewhere = answered(&other.request, "call-3", completed("elsewhere"));
    assert!(
        matches!(held.resolve(&elsewhere), Resolution::Recover(_)),
        "another turn's first recovery is its own"
    );
}

/// **AC-14b**, the immediate variant: no intervening hold, the same request
/// resolved twice. The second answer is the cap's, and the ring's reason —
/// the only drop the key ever had — still rides it.
#[tokio::test]
async fn a_second_resume_straight_after_a_recovery_is_refused_by_name() {
    let held = Arc::new(HeldRuns::default());
    let paused = pause("auto", &held, "call-1").await;
    paused.cancel.cancel();
    settled(&held, &paused.request).await;

    let resumed = answered(&paused.request, "call-1", completed("once"));
    assert!(matches!(held.resolve(&resumed), Resolution::Recover(_)));

    let Resolution::Failed(sentence) = held.resolve(&resumed) else {
        panic!("the same key is not reopened twice");
    };
    assert!(sentence.contains("already reopened once"), "{sentence}");
    assert!(
        sentence.contains("cancelled"),
        "the drop's reason still rides the refusal: {sentence}"
    );
}

/// **AC-14b**, through the provider: a capped resume is a failed stream
/// naming the cap, and opens no Run — the endpoint here is one nothing can
/// connect to, so an attempted fresh Run would have been a transport error
/// rather than this sentence.
#[tokio::test]
async fn a_capped_resume_fails_the_turn_through_the_provider_and_opens_no_run() {
    let provider = CursorProvider::at(
        "http://127.0.0.1:9",
        CredentialSource::key("at-bridge-canary").expect("a non-blank token"),
    )
    .expect("loopback may carry a token");
    let paused = pause("auto", &provider.held, "call-1").await;
    paused.cancel.cancel();
    settled(&provider.held, &paused.request).await;

    let resumed = answered(&paused.request, "call-1", completed("once"));
    assert!(
        matches!(provider.held.resolve(&resumed), Resolution::Recover(_)),
        "the one recovery, spent on the table"
    );

    let events = through_provider(&provider, resumed).await;
    assert!(
        matches!(
            events.as_slice(),
            [ProviderEvent::Failed(error)] if error.to_string().contains("already reopened once")
        ),
        "the turn fails naming the cap, without a socket: {events:?}"
    );
}

/// A turn whose messages carry no user message at all still keys, because the
/// opening message is an index rather than a role — and a recovered Run, the
/// request that reopened it having the same opening message, keys exactly as
/// the turn it reopened did.
#[test]
fn the_key_is_the_opening_messages_id_and_the_model_and_a_recovered_run_keys_the_same() {
    let request = opening("auto");
    let mut elsewhere = request.clone();
    elsewhere.model = "gpt-5.3-codex".to_owned();

    assert_ne!(
        Key::of(&request),
        Key::of(&elsewhere),
        "two models are two runs, whatever the conversation"
    );

    let mut later = request.clone();
    later.messages.push(Message::user("and another thing"));
    assert_eq!(
        Key::of(&request),
        Key::of(&later),
        "a turn that grew is the same turn, which is what makes a resume find its own run"
    );

    let recovered = answered(&request, "call-1", completed("what the tool answered"));
    let continued = answered(&recovered, "call-2", completed("and the next"));
    assert_eq!(
        Key::of(&recovered),
        Key::of(&request),
        "the request that reopens a run keys as the run it reopens"
    );
    assert_eq!(
        Key::of(&continued),
        Key::of(&request),
        "and so does every later step of the recovered turn"
    );
}
