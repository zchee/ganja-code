//! The plan-exit handover through a real engine: a Yes switches the session
//! to build at the turn boundary — announced after the finish, durable on the
//! row, applied at the next entry — and everything that could misfire around
//! that seam is pinned here by name.
//!
//! Spec: upstream `packages/opencode/src/tool/plan.ts`, under ganja's
//! two-phase adoption (`.omc/plans/p7-plan-exit.md`). The ordering contract
//! every test leans on: **MessageFinished → AgentChanged (when pending) →
//! slot release**, with exactly one `agent_changed` frame per adoption.
//!
//! Nothing here mutates process-wide state: storage is handed to
//! [`Engine::persistent`] directly, and permissions are the in-memory
//! default. The undo revocation drill lives in `plan_exit_undo.rs` instead,
//! because snapshots resolve their repository through `XDG_DATA_HOME` and an
//! env-mutating test gets a binary of its own. The MCP rebuild test shares
//! the reference-server prerequisites `mcp.rs` documents (`bun` plus the
//! upstream checkout), and hard-fails without them for the same reason.

use std::{path::Path, sync::Arc, time::Duration};

use futures::{StreamExt as _, stream::BoxStream};
use ganja_core::{
    Config, Engine, EngineError, McpServers, McpStatus, Storage,
    agent::{BUILD_SWITCH_REMINDER, PLAN_REMINDER},
    permission::Permissions,
    protocol::{Command, Event, FinishReason, PartBody, QuestionId, ToolState},
    provider::{ChatRequest, Provider},
    tool::{Registry, Tool},
};
use ganja_testkit::{BlockingTool, RecorderTool, ScriptedProvider, drain, says, tool_call};
use serde_json::json;

/// What the first build prompt after a Yes reads, spelled out rather than
/// imported: the sentence is a wire-visible promise, and a test that read it
/// off the constant under test would pass however the constant drifted.
const APPROVAL: &str = "The plan has been approved, you can now edit files. Execute the plan";

/// Upstream's dismissal sentence, the failure text of a No.
const DISMISSED: &str = "The user dismissed this question";

/// How long any single event may take to arrive before a test gives up.
const PATIENCE: Duration = Duration::from_secs(30);

/// A step that calls `plan_exit` with its empty argument object.
fn exit_call() -> Vec<ganja_core::provider::ProviderEvent> {
    tool_call("plan_exit", json!({}))
}

/// An in-memory engine over `script`, holding `tools` beside whatever
/// `install` adds, with the builtin agents installed.
fn engine_over(
    script: Vec<Vec<ganja_core::provider::ProviderEvent>>,
    tools: Vec<Arc<dyn Tool>>,
) -> (Engine, Arc<std::sync::Mutex<Vec<ChatRequest>>>) {
    let (provider, seen) = ScriptedProvider::new(script);
    let engine = Engine::new(
        provider as Arc<dyn Provider>,
        "recorder-model",
        Arc::new(Registry::new(tools)),
        Permissions::default(),
    )
    .with_agents(ganja_testkit::agent_registry(&Config::default()));

    (engine, seen)
}

/// A persistent engine over `script`, resumed onto a pre-titled seeded
/// session so the title machinery never spends a scripted request.
async fn persistent_over(
    script: Vec<Vec<ganja_core::provider::ProviderEvent>>,
    storage: Storage,
) -> (Engine, Arc<std::sync::Mutex<Vec<ChatRequest>>>) {
    let session = ganja_testkit::seed_session(&storage, 0);
    let (provider, seen) = ScriptedProvider::new(script);
    let engine = Engine::persistent(
        provider as Arc<dyn Provider>,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage,
    )
    .with_agents(ganja_testkit::agent_registry(&Config::default()));
    engine
        .resume(&session)
        .await
        .expect("the seeded session resumes");

    (engine, seen)
}

/// The next event, or a panic naming the wait that wedged.
async fn next_event(events: &mut BoxStream<'static, Event>) -> Event {
    tokio::time::timeout(PATIENCE, events.next())
        .await
        .expect("an event should arrive before the patience runs out")
        .expect("the stream should not end mid-test")
}

/// Switches the session to plan and consumes the `AgentChanged` the manual
/// path announces, so later counts see only the events of the drill itself.
async fn switch_to_plan(engine: &Engine, events: &mut BoxStream<'static, Event>) {
    engine
        .send(Command::SwitchAgent {
            name: "plan".to_owned(),
        })
        .await
        .expect("plan is a builtin primary agent");
    match next_event(events).await {
        Event::AgentChanged { agent, .. } => assert_eq!(agent, "plan"),
        other => panic!("a manual switch announces itself, got {other:?}"),
    }
}

/// Reads until the plan-exit question is asked, handing back its id and
/// everything seen on the way.
async fn until_question(events: &mut BoxStream<'static, Event>) -> (QuestionId, Vec<Event>) {
    let mut seen = Vec::new();
    loop {
        let event = next_event(events).await;
        let asked = match &event {
            Event::QuestionAsked { id, .. } => Some(id.clone()),
            _ => None,
        };
        seen.push(event);
        if let Some(id) = asked {
            return (id, seen);
        }
    }
}

/// Sends `prompt`, answers the plan-exit question with `answer`, and returns
/// everything seen through the turn's finish. Whatever the boundary announces
/// after the finish is left on the stream for the caller to read.
async fn answered_turn(
    engine: &Engine,
    events: &mut BoxStream<'static, Event>,
    prompt: &str,
    answer: &str,
) -> Vec<Event> {
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
    let (id, mut seen) = until_question(events).await;
    engine
        .send(Command::ReplyQuestion {
            id,
            answers: vec![vec![answer.to_owned()]],
        })
        .await
        .expect("a reply is never refused");
    seen.extend(drain(events).await);

    seen
}

/// The boundary's announcement, which must be the very next event after a
/// finish that follows a Yes.
async fn boundary_announcement(events: &mut BoxStream<'static, Event>) -> (String, String) {
    match next_event(events).await {
        Event::AgentChanged { agent, model, .. } => (agent, model),
        other => panic!("the boundary announces right after the finish, got {other:?}"),
    }
}

/// Sends `command` once the boundary has actually released the slot: the
/// announcement is queued *before* the release, so receipt says nothing about
/// idleness and a raced send may still read Busy for a moment.
async fn send_when_idle(engine: &Engine, command: Command) {
    for _ in 0..500 {
        match engine.send(command.clone()).await {
            Ok(()) => return,
            Err(EngineError::Busy) => tokio::time::sleep(Duration::from_millis(10)).await,
            Err(other) => panic!("the command was refused for a non-Busy reason: {other}"),
        }
    }
    panic!("the engine never went idle");
}

/// How many synthetic parts of `request` say exactly `text`.
fn parts_saying(request: &ChatRequest, text: &str) -> usize {
    request
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter_map(ganja_core::protocol::Part::as_text)
        .filter(|part| *part == text)
        .count()
}

/// Every `AgentChanged` in `seen`, as `(agent, model)` pairs.
fn agent_changes(seen: &[Event]) -> Vec<(String, String)> {
    seen.iter()
        .filter_map(|event| match event {
            Event::AgentChanged { agent, model, .. } => Some((agent.clone(), model.clone())),
            _ => None,
        })
        .collect()
}

/// The final state of the tool part named `tool`.
fn tool_part(seen: &[Event], tool: &str) -> ToolState {
    seen.iter()
        .rev()
        .find_map(|event| match event {
            Event::PartUpdated { part, .. } => match &part.body {
                PartBody::Tool {
                    tool: named, state, ..
                } if named == tool => Some(state.clone()),
                _ => None,
            },
            _ => None,
        })
        .unwrap_or_else(|| panic!("the turn produced no part for {tool}"))
}

/// Why the turn ended.
fn finish(seen: &[Event]) -> FinishReason {
    match seen.last() {
        Some(Event::MessageFinished { reason, .. }) => *reason,
        other => panic!("a turn always ends with a finish, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The approval itself: boundary order, multiplicity, durability, delivery.
// ---------------------------------------------------------------------------

/// The whole happy path, with the ordering and multiplicity pins: the finish
/// comes first, then exactly one `agent_changed`, then the engine admits the
/// next turn; the row records build at the boundary; the next prompt runs as
/// build carrying `BUILD_SWITCH_REMINDER` and the approval sentence exactly
/// once each.
#[tokio::test]
async fn a_yes_answer_lands_the_switch_when_the_turn_ends() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let (engine, requests) =
        persistent_over(vec![exit_call()], Storage::open(dir.path().join("storage"))).await;
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    switch_to_plan(&engine, &mut events).await;

    let seen = answered_turn(&engine, &mut events, "here is the plan", "Yes").await;

    // The question is upstream's, verbatim but for the plan-file clause.
    let Some(Event::QuestionAsked { questions, .. }) = seen
        .iter()
        .find(|event| matches!(event, Event::QuestionAsked { .. }))
    else {
        panic!("the tool asks, got {seen:?}");
    };
    assert_eq!(questions.len(), 1);
    assert_eq!(
        questions[0].question,
        "The plan is complete. Would you like to switch to the build agent and start implementing?"
    );
    assert_eq!(questions[0].header, "Build Agent");

    // The call completed with upstream's output, and the turn finished before
    // anything was announced: no AgentChanged is in the drained turn.
    assert!(
        matches!(tool_part(&seen, "plan_exit"), ToolState::Completed { output, .. }
            if output == "User approved switching to build agent. Wait for further instructions."),
        "the Yes completes the call"
    );
    assert_eq!(finish(&seen), FinishReason::Completed);
    assert_eq!(agent_changes(&seen), Vec::<(String, String)>::new());

    // The boundary: announcement right after the finish, row already durable.
    let (agent, model) = boundary_announcement(&mut events).await;
    assert_eq!(agent, "build");
    assert_eq!(model, "recorder-model");
    let row = engine.current_session().expect("the session has a row");
    assert_eq!(row.agent.as_deref(), Some("build"));

    // The next turn runs as build and reads both notices exactly once.
    send_when_idle(
        &engine,
        Command::SendPrompt {
            text: "go ahead".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        },
    )
    .await;
    let second = drain(&mut events).await;
    assert_eq!(engine.agent().as_deref(), Some("build"));
    assert_eq!(
        agent_changes(&second),
        Vec::<(String, String)>::new(),
        "the approval announces exactly once, at the boundary"
    );

    let requests = requests.lock().expect("the request log is never poisoned");
    let next = requests.last().expect("the second turn asked the provider");
    assert_eq!(parts_saying(next, BUILD_SWITCH_REMINDER), 1);
    assert_eq!(parts_saying(next, APPROVAL), 1);
    assert_eq!(parts_saying(next, PLAN_REMINDER), 0);
}

#[tokio::test]
async fn a_no_answer_leaves_the_session_planning() {
    let (engine, requests) = engine_over(vec![exit_call()], Vec::new());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    switch_to_plan(&engine, &mut events).await;

    let seen = answered_turn(&engine, &mut events, "here is the plan", "No").await;

    assert!(
        matches!(tool_part(&seen, "plan_exit"), ToolState::Error { error, .. }
            if error == DISMISSED),
        "a No reads as the dismissal sentence"
    );
    // A refusal is information, not a turn abort.
    assert_eq!(finish(&seen), FinishReason::Completed);
    assert_eq!(agent_changes(&seen), Vec::<(String, String)>::new());
    assert_eq!(engine.agent().as_deref(), Some("plan"));

    // And the next prompt still plans: no switch was recorded anywhere.
    send_when_idle(
        &engine,
        Command::SendPrompt {
            text: "keep refining".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        },
    )
    .await;
    let second = drain(&mut events).await;
    assert_eq!(agent_changes(&second), Vec::<(String, String)>::new());
    let requests = requests.lock().expect("the request log is never poisoned");
    let next = requests.last().expect("the second turn asked the provider");
    assert_eq!(parts_saying(next, PLAN_REMINDER), 1);
    assert_eq!(parts_saying(next, APPROVAL), 0);
}

/// The rules change only at phase two, matching upstream's turn-end
/// semantics: an edit the model tries in the same turn as its approved
/// `plan_exit` is still the planning agent's edit, and still refused.
#[tokio::test]
async fn an_edit_in_the_turn_that_asked_is_still_a_plan_agents_edit() {
    let (edit, edits) = RecorderTool::new("edit", "edit ran", "done");
    let (engine, _) = {
        let (provider, seen) = ScriptedProvider::new(vec![
            exit_call(),
            tool_call("edit", json!({ "filePath": "src/main.rs" })),
            says("done either way"),
        ]);
        (
            Engine::new(
                provider as Arc<dyn Provider>,
                "recorder-model",
                Arc::new(Registry::new(vec![edit])),
                Permissions::default(),
            )
            .with_agents(ganja_testkit::agent_registry(&Config::default())),
            seen,
        )
    };
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    switch_to_plan(&engine, &mut events).await;

    let seen = answered_turn(&engine, &mut events, "here is the plan", "Yes").await;

    assert!(
        edits.lock().expect("the call log").is_empty(),
        "the edit must never have run"
    );
    assert!(
        matches!(tool_part(&seen, "edit"), ToolState::Error { error, .. }
            if error.contains(r#""permission":"edit""#)),
        "the refusal quotes the planning agent's rule"
    );

    // The switch still lands — refusing the premature edit and honouring the
    // approval are two separate answers.
    let (agent, _) = boundary_announcement(&mut events).await;
    assert_eq!(agent, "build");
}

/// Cancel converges through `run_turn`'s one tail, so a Yes already recorded
/// survives the turn being cancelled afterwards — there is no second site for
/// the boundary to drift out of.
#[tokio::test]
async fn a_cancel_after_yes_still_switches() {
    let (entered, mut entry) = tokio::sync::mpsc::channel(1);
    let block = BlockingTool::with_entry_signal("block", "blocks until cancelled", entered);
    let (engine, _) = {
        let (provider, seen) =
            ScriptedProvider::new(vec![exit_call(), tool_call("block", json!({}))]);
        (
            Engine::new(
                provider as Arc<dyn Provider>,
                "recorder-model",
                Arc::new(Registry::new(vec![block])),
                Permissions::default(),
            )
            .with_agents(ganja_testkit::agent_registry(&Config::default())),
            seen,
        )
    };
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    switch_to_plan(&engine, &mut events).await;

    engine
        .send(Command::SendPrompt {
            text: "here is the plan".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let (id, _) = until_question(&mut events).await;
    engine
        .send(Command::ReplyQuestion {
            id,
            answers: vec![vec!["Yes".to_owned()]],
        })
        .await
        .expect("a reply is never refused");

    // The next scripted call is already blocking before the cancel lands, so
    // the cancel interrupts a turn whose Yes is recorded.
    tokio::time::timeout(PATIENCE, entry.recv())
        .await
        .expect("the blocking call starts")
        .expect("the entry signal arrives");
    engine
        .send(Command::CancelTurn)
        .await
        .expect("a cancel is never refused");

    let seen = drain(&mut events).await;
    assert_eq!(finish(&seen), FinishReason::Cancelled);

    let (agent, _) = boundary_announcement(&mut events).await;
    assert_eq!(agent, "build");
    send_when_idle(
        &engine,
        Command::SendPrompt {
            text: "go ahead".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        },
    )
    .await;
    drain(&mut events).await;
    assert_eq!(engine.agent().as_deref(), Some("build"));
}

// ---------------------------------------------------------------------------
// Supersession and the sentence's one ride.
// ---------------------------------------------------------------------------

/// The person's later explicit choice outranks the earlier Yes: a manual
/// switch back to plan discards the pending approval whole, so no approval
/// sentence can ever land beside `PLAN_REMINDER` in a plan turn.
#[tokio::test]
async fn a_manual_switch_after_yes_supersedes_the_approval() {
    let (engine, requests) = engine_over(vec![exit_call()], Vec::new());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    switch_to_plan(&engine, &mut events).await;

    answered_turn(&engine, &mut events, "here is the plan", "Yes").await;
    let (agent, _) = boundary_announcement(&mut events).await;
    assert_eq!(agent, "build");

    send_when_idle(
        &engine,
        Command::SwitchAgent {
            name: "plan".to_owned(),
        },
    )
    .await;
    match next_event(&mut events).await {
        Event::AgentChanged { agent, .. } => assert_eq!(agent, "plan"),
        other => panic!("the manual supersede announces itself, got {other:?}"),
    }

    engine
        .send(Command::SendPrompt {
            text: "keep planning".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("the switch left the engine idle");
    let second = drain(&mut events).await;

    assert_eq!(engine.agent().as_deref(), Some("plan"));
    assert_eq!(agent_changes(&second), Vec::<(String, String)>::new());
    let requests = requests.lock().expect("the request log is never poisoned");
    let next = requests.last().expect("the plan turn asked the provider");
    assert_eq!(parts_saying(next, PLAN_REMINDER), 1);
    assert_eq!(parts_saying(next, APPROVAL), 0);
    assert_eq!(parts_saying(next, BUILD_SWITCH_REMINDER), 0);
}

/// The boundary's durable half, and the owned sentence loss: a restart
/// between the Yes and the next prompt resumes as build — the row was
/// written at the boundary — while the request-time sentence is gone
/// (deviation family: approval-rides-the-request).
#[tokio::test]
async fn a_restart_between_yes_and_the_prompt_resumes_as_build_without_the_sentence() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let storage = Storage::open(dir.path().join("storage"));

    let session = {
        let (engine, _) = persistent_over(vec![exit_call()], storage.clone()).await;
        let mut events = engine.subscribe().await.expect("the first subscriber wins");
        switch_to_plan(&engine, &mut events).await;
        answered_turn(&engine, &mut events, "here is the plan", "Yes").await;
        let (agent, _) = boundary_announcement(&mut events).await;
        assert_eq!(agent, "build");

        engine.current_session().expect("the session has a row").id
    };

    // The restart: a fresh engine over the same store.
    let (provider, requests) = ScriptedProvider::new(vec![says("executing")]);
    let engine = Engine::persistent(
        provider as Arc<dyn Provider>,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage,
    )
    .with_agents(ganja_testkit::agent_registry(&Config::default()));
    engine
        .resume(&session)
        .await
        .expect("the approved session resumes");
    assert_eq!(
        engine.agent().as_deref(),
        Some("build"),
        "the boundary's row write survives the restart"
    );

    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::SendPrompt {
            text: "go ahead".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("a resumed engine accepts a prompt");
    drain(&mut events).await;

    let requests = requests.lock().expect("the request log is never poisoned");
    let next = requests
        .last()
        .expect("the resumed turn asked the provider");
    assert_eq!(
        parts_saying(next, APPROVAL),
        0,
        "the sentence did not survive"
    );
    assert_eq!(parts_saying(next, BUILD_SWITCH_REMINDER), 0);
}

#[tokio::test]
async fn the_approval_sentence_rides_one_request_and_never_returns() {
    let (engine, requests) = engine_over(vec![exit_call()], Vec::new());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    switch_to_plan(&engine, &mut events).await;

    answered_turn(&engine, &mut events, "here is the plan", "Yes").await;
    boundary_announcement(&mut events).await;

    for prompt in ["go ahead", "and then keep going"] {
        send_when_idle(
            &engine,
            Command::SendPrompt {
                text: prompt.to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            },
        )
        .await;
        drain(&mut events).await;
    }

    let requests = requests.lock().expect("the request log is never poisoned");
    let counts: Vec<usize> = requests
        .iter()
        .map(|request| parts_saying(request, APPROVAL))
        .collect();
    assert_eq!(
        counts.iter().sum::<usize>(),
        1,
        "the sentence rides exactly one request, got {counts:?}"
    );
    assert_eq!(
        parts_saying(requests.last().expect("the third turn asked"), APPROVAL),
        0,
        "and never returns"
    );
}

#[tokio::test]
async fn a_manual_switch_mid_turn_is_still_refused_busy() {
    let (engine, _) = engine_over(vec![exit_call()], Vec::new());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    switch_to_plan(&engine, &mut events).await;

    engine
        .send(Command::SendPrompt {
            text: "here is the plan".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let (id, _) = until_question(&mut events).await;

    // The question is open, so the turn is in flight and a switch must wait.
    let refused = engine
        .send(Command::SwitchAgent {
            name: "build".to_owned(),
        })
        .await
        .expect_err("a mid-turn switch is refused");
    assert!(matches!(refused, EngineError::Busy), "got {refused:?}");

    engine
        .send(Command::RejectQuestion { id })
        .await
        .expect("a rejection is never refused");
    let seen = drain(&mut events).await;
    assert_eq!(finish(&seen), FinishReason::Completed);
    assert_eq!(engine.agent().as_deref(), Some("plan"));
}

// ---------------------------------------------------------------------------
// The SentencePending rides: shell turns, model switches, new sessions.
// ---------------------------------------------------------------------------

/// The state the four-state cell exists to name: a `!` turn between the Yes
/// and the first asking prompt applies the switch but cannot deliver a
/// reminder, so the sentence keeps riding for the prompt that can.
#[tokio::test]
async fn a_yes_then_a_shell_turn_keeps_the_sentence_for_the_next_asking_prompt() {
    let (engine, requests) = engine_over(vec![exit_call()], Vec::new());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    switch_to_plan(&engine, &mut events).await;

    answered_turn(&engine, &mut events, "here is the plan", "Yes").await;
    boundary_announcement(&mut events).await;

    let asked_before = requests
        .lock()
        .expect("the request log is never poisoned")
        .len();
    send_when_idle(
        &engine,
        Command::RunShell {
            command: "echo ok".to_owned(),
        },
    )
    .await;
    drain(&mut events).await;
    assert_eq!(
        requests
            .lock()
            .expect("the request log is never poisoned")
            .len(),
        asked_before,
        "a shell turn asks the model nothing"
    );
    // The shell turn ran the switch's phase two.
    assert_eq!(engine.agent().as_deref(), Some("build"));

    send_when_idle(
        &engine,
        Command::SendPrompt {
            text: "go ahead".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        },
    )
    .await;
    drain(&mut events).await;

    let requests = requests.lock().expect("the request log is never poisoned");
    let next = requests.last().expect("the asking turn asked the provider");
    assert_eq!(parts_saying(next, APPROVAL), 1, "the sentence waited");
    assert_eq!(parts_saying(next, BUILD_SWITCH_REMINDER), 1);
}

/// The stale-row clobber the `switch_model` policy row exists to prevent:
/// without apply-first, that entry's own selection write would put the stale
/// plan agent back over the row's build.
#[tokio::test]
async fn a_yes_then_a_model_switch_keeps_the_row_on_build() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let (engine, requests) =
        persistent_over(vec![exit_call()], Storage::open(dir.path().join("storage"))).await;
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    switch_to_plan(&engine, &mut events).await;

    answered_turn(&engine, &mut events, "here is the plan", "Yes").await;
    boundary_announcement(&mut events).await;

    send_when_idle(
        &engine,
        Command::SwitchModel {
            model: "recorder-two".to_owned(),
        },
    )
    .await;

    let row = engine.current_session().expect("the session has a row");
    assert_eq!(
        row.agent.as_deref(),
        Some("build"),
        "the row stays on build"
    );
    assert_eq!(row.model.as_deref(), Some("recorder-two"));
    assert_eq!(engine.agent().as_deref(), Some("build"));

    // The sentence was not the model switch's to spend.
    send_when_idle(
        &engine,
        Command::SendPrompt {
            text: "go ahead".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        },
    )
    .await;
    drain(&mut events).await;
    let requests = requests.lock().expect("the request log is never poisoned");
    let next = requests.last().expect("the asking turn asked the provider");
    assert_eq!(parts_saying(next, APPROVAL), 1);
}

/// A `NewSession` clears the pending approval with the conversation it
/// belonged to: the next conversation starts clean, planning, with no
/// sentence — while the old session's row keeps the build the boundary wrote.
#[tokio::test]
async fn a_yes_then_a_new_session_starts_clean_with_no_sentence() {
    let (engine, requests) = engine_over(vec![exit_call()], Vec::new());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    switch_to_plan(&engine, &mut events).await;

    answered_turn(&engine, &mut events, "here is the plan", "Yes").await;
    boundary_announcement(&mut events).await;

    send_when_idle(&engine, Command::NewSession).await;
    engine
        .send(Command::SendPrompt {
            text: "a fresh start".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("a new session accepts a prompt");
    let seen = drain(&mut events).await;

    // The Yes belonged to the conversation that is over: the in-memory
    // selection was never applied, so the new conversation still plans.
    assert_eq!(engine.agent().as_deref(), Some("plan"));
    assert_eq!(agent_changes(&seen), Vec::<(String, String)>::new());
    let requests = requests.lock().expect("the request log is never poisoned");
    let next = requests.last().expect("the fresh turn asked the provider");
    assert_eq!(parts_saying(next, APPROVAL), 0);
    assert_eq!(parts_saying(next, BUILD_SWITCH_REMINDER), 0);
    assert_eq!(parts_saying(next, PLAN_REMINDER), 1);
}

// ---------------------------------------------------------------------------
// The manual path's announcement, and the rebuild the registration survives.
// ---------------------------------------------------------------------------

/// Engine-level `AgentChanged` on the manual path, exactly once: a `/agents`
/// switch is visible to every subscriber, not only the frontend that issued
/// it.
#[tokio::test]
async fn a_manual_switch_announces_itself_on_the_event_stream() {
    let (engine, _) = engine_over(vec![says("hello")], Vec::new());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SwitchAgent {
            name: "plan".to_owned(),
        })
        .await
        .expect("plan is a builtin primary agent");
    match next_event(&mut events).await {
        Event::AgentChanged {
            agent,
            model,
            session_id,
        } => {
            assert_eq!(agent, "plan");
            assert_eq!(model, "recorder-model");
            assert_eq!(session_id, engine.session_id());
        }
        other => panic!("the switch announces itself, got {other:?}"),
    }

    // Exactly once: the whole next turn carries no second frame.
    engine
        .send(Command::SendPrompt {
            text: "hi".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("the switch left the engine idle");
    let seen = drain(&mut events).await;
    assert_eq!(agent_changes(&seen), Vec::<(String, String)>::new());
}

/// Success criterion 1's rebuild clause: `plan_exit` is absent from the
/// builtin surface the golden differential pins, present in the set `install`
/// rebuilds, and **still present after an MCP dial moves the tool surface**,
/// because it is registered inside the one rebuild chain every path shares.
///
/// The dial is real — the reference `@modelcontextprotocol/sdk` server the
/// `mcp.rs` suite documents — because a failed dial bumps no generation and
/// would leave the rebuild untested. Prerequisites are `mcp.rs`'s, and
/// missing ones fail rather than skip, for the reason given there.
#[tokio::test]
async fn plan_exit_survives_the_mcp_dial_rebuild() {
    assert!(
        !Registry::with_builtins()
            .definitions()
            .iter()
            .any(|tool| tool.name == "plan_exit"),
        "the builtin surface must not move"
    );

    let checkout = std::env::var_os("GANJA_OPENCODE_DIR").map_or_else(
        || {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../.omc/reference/opencode-v1.18.22")
                .to_owned()
        },
        std::path::PathBuf::from,
    );
    let sdk = [
        checkout.join("packages/opencode/node_modules/@modelcontextprotocol/sdk"),
        checkout.join("node_modules/@modelcontextprotocol/sdk"),
    ]
    .into_iter()
    .find(|candidate| candidate.join("dist/esm/server/index.js").is_file())
    .expect("the upstream checkout with an installed @modelcontextprotocol/sdk is a prerequisite");
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp/reference-server.mjs");
    assert!(script.is_file(), "{} is missing", script.display());

    let config: Config = serde_json::from_value(json!({
        "mcp": {
            "ref": {
                "type": "local",
                "command": ["bun", script.to_str().expect("the fixture path is UTF-8")],
                "environment": { "GANJA_MCP_SDK_DIR": sdk.to_str().expect("the SDK path is UTF-8") },
            }
        }
    }))
    .expect("the fixture config is a config");

    // One scripted call per turn, the second pushed only once the first turn
    // is over: a script queue is popped per *request*, and an approval turn
    // makes two — the second `exit_call` pre-queued would be consumed by turn
    // one's closing request and hang it on a question nobody answers.
    let (provider, requests) = ScriptedProvider::new(vec![exit_call()]);
    let script = Arc::clone(&provider);
    let servers = McpServers::new(config.mcp.clone(), Path::new("."));
    let engine = Engine::new(
        provider as Arc<dyn Provider>,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_agents(ganja_testkit::agent_registry(&Config::default()))
    .with_mcp(Arc::clone(&servers));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    switch_to_plan(&engine, &mut events).await;

    // Before the dial: offered, and callable enough to ask.
    let seen = answered_turn(&engine, &mut events, "here is the plan", "No").await;
    assert!(
        matches!(tool_part(&seen, "plan_exit"), ToolState::Error { error, .. } if error == DISMISSED)
    );

    // The dial. Waited for, because a test that raced it would test the race.
    engine.connect_mcp();
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let status = engine.mcp_status();
        if status
            .get("ref")
            .is_some_and(|status| matches!(status, McpStatus::Connected))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the reference server never connected: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // After the rebuild the dial forces: the lent tools arrived, and
    // `plan_exit` survived beside them.
    script.push(exit_call());
    let second = answered_turn(&engine, &mut events, "still the plan", "No").await;
    assert!(
        matches!(tool_part(&second, "plan_exit"), ToolState::Error { error, .. } if error == DISMISSED),
        "plan_exit is still offered and still runs after the rebuild"
    );
    let offered: Vec<String> = {
        let requests = requests.lock().expect("the request log is never poisoned");
        requests
            .last()
            .expect("the second turn asked the provider")
            .tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect()
    };
    assert!(
        offered.iter().any(|name| name == "plan_exit"),
        "got {offered:?}"
    );
    assert!(
        offered.iter().any(|name| name.starts_with("mcp__ref__")),
        "the rebuild really happened — the dialled server's tools are in the set: {offered:?}"
    );

    engine.shutdown_mcp().await;
}
