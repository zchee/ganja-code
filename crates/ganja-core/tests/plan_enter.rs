//! The plan-enter door through a real engine: the build agent asks whether to
//! plan first, and a Yes switches the session to plan at the turn boundary —
//! announced after the finish, durable on the row, applied at the next entry.
//!
//! Spec: none. `plan_enter` is **synthesized** (**D477**,
//! `plan-enter-synthesized`): upstream v1.18.13 publishes the permission
//! vocabulary and the model-facing description and wires no tool at all, so
//! what these drills pin is ganja's own contract — the mirror of the one
//! `plan_exit.rs` pins, under `.omc/plans/p7-plan-exit.md`'s two-phase
//! adoption. The ordering contract they lean on is that plan's:
//! **MessageFinished → AgentChanged (when pending) → slot release**, with
//! exactly one `agent_changed` frame per adoption.
//!
//! Only the drills that pin *enter's* own contract are here. The
//! exit-specific ones — the approval sentence's single ride, the
//! `SentencePending` states, the undo revocation — are `plan_exit.rs`'s and
//! `plan_exit_undo.rs`'s, because the enter door writes no sentence to ride
//! (**D477**): the plan agent's standing per-turn reminder already says what
//! it is.
//!
//! Nothing here mutates process-wide state: storage is handed to
//! [`Engine::persistent`] directly, and permissions are the in-memory
//! default. The MCP rebuild test shares the reference-server prerequisites
//! `mcp.rs` documents (`bun` plus the upstream checkout), and hard-fails
//! without them for the same reason.

use std::{path::Path, sync::Arc, time::Duration};

use futures::{StreamExt as _, stream::BoxStream};
use ganja_core::{
    Config, Engine, McpServers, McpStatus, Storage,
    agent::{BUILD_SWITCH_REMINDER, PLAN_REMINDER},
    permission::Permissions,
    protocol::{Command, Event, FinishReason, PartBody, QuestionId, ToolState},
    provider::{ChatRequest, Provider},
    tool::Registry,
};
use ganja_testkit::{ScriptedProvider, drain, tool_call};
use serde_json::json;

/// The approval sentence the *exit* door rides, spelled out so this suite can
/// assert it never appears: an enter switch has no sentence of its own, and a
/// build-side one leaking into a plan turn would be the misfire.
const APPROVAL: &str = "The plan has been approved, you can now edit files. Execute the plan";

/// Upstream's dismissal sentence, the failure text of a No.
const DISMISSED: &str = "The user dismissed this question";

/// How long any single event may take to arrive before a test gives up.
const PATIENCE: Duration = Duration::from_secs(30);

/// A step that calls `plan_enter` with its empty argument object.
fn enter_call() -> Vec<ganja_core::provider::ProviderEvent> {
    tool_call("plan_enter", json!({}))
}

/// An in-memory engine over `script`, with the builtin agents installed. The
/// session starts on `build`, which is the agent this door belongs to, so no
/// manual switch is needed to reach it.
fn engine_over(
    script: Vec<Vec<ganja_core::provider::ProviderEvent>>,
) -> (Engine, Arc<std::sync::Mutex<Vec<ChatRequest>>>) {
    let (provider, seen) = ScriptedProvider::new(script);
    let engine = Engine::new(
        provider as Arc<dyn Provider>,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
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

/// Reads until the plan-enter question is asked, handing back its id and
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

/// Sends `prompt`, answers the plan-enter question with `answer`, and returns
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
            Err(ganja_core::EngineError::Busy) => {
                tokio::time::sleep(Duration::from_millis(10)).await
            }
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
// The switch itself: boundary order, multiplicity, durability, delivery.
// ---------------------------------------------------------------------------

/// The whole happy path, with the ordering and multiplicity pins the mirror
/// door's own test makes: the finish comes first, then exactly one
/// `agent_changed`, then the engine admits the next turn; the row records
/// plan at the boundary; the next prompt runs as plan carrying `PLAN_REMINDER`
/// and neither of the build-side notices.
#[tokio::test]
async fn a_yes_answer_lands_the_switch_to_plan_when_the_turn_ends() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let (engine, requests) = persistent_over(
        vec![enter_call()],
        Storage::open(dir.path().join("storage")),
    )
    .await;
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    assert_eq!(
        engine.agent().as_deref(),
        Some("build"),
        "the enter door belongs to the agent a session starts on"
    );

    let seen = answered_turn(&engine, &mut events, "add a cache layer", "Yes").await;

    // The synthesized question, pinned where a person would read it (**D477**).
    let Some(Event::QuestionAsked { questions, .. }) = seen
        .iter()
        .find(|event| matches!(event, Event::QuestionAsked { .. }))
    else {
        panic!("the tool asks, got {seen:?}");
    };
    assert_eq!(questions.len(), 1);
    assert_eq!(
        questions[0].question,
        "Would you like to switch to the plan agent to research and design before implementing?"
    );
    assert_eq!(questions[0].header, "Plan Agent");

    // The call completed, and the turn finished before anything was
    // announced: no AgentChanged is in the drained turn.
    assert!(
        matches!(tool_part(&seen, "plan_enter"), ToolState::Completed { output, .. }
            if output == "User approved switching to plan agent. Wait for further instructions."),
        "the Yes completes the call"
    );
    assert_eq!(finish(&seen), FinishReason::Completed);
    assert_eq!(agent_changes(&seen), Vec::<(String, String)>::new());

    // The boundary: announcement right after the finish, row already durable.
    let (agent, model) = boundary_announcement(&mut events).await;
    assert_eq!(agent, "plan");
    assert_eq!(model, "recorder-model");
    let row = engine.current_session().expect("the session has a row");
    assert_eq!(row.agent.as_deref(), Some("plan"));

    // The next turn runs as plan and reads the planning notice — and neither
    // of the build side's, which would be the misfire this asserts against.
    send_when_idle(
        &engine,
        Command::SendPrompt {
            text: "what is the shape".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        },
    )
    .await;
    let second = drain(&mut events).await;
    assert_eq!(engine.agent().as_deref(), Some("plan"));
    assert_eq!(
        agent_changes(&second),
        Vec::<(String, String)>::new(),
        "the switch announces exactly once, at the boundary"
    );

    let requests = requests.lock().expect("the request log is never poisoned");
    let next = requests.last().expect("the second turn asked the provider");
    assert_eq!(parts_saying(next, PLAN_REMINDER), 1);
    assert_eq!(parts_saying(next, BUILD_SWITCH_REMINDER), 0);
    assert_eq!(parts_saying(next, APPROVAL), 0);
}

#[tokio::test]
async fn a_no_answer_leaves_the_session_building() {
    let (engine, requests) = engine_over(vec![enter_call()]);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    let seen = answered_turn(&engine, &mut events, "add a cache layer", "No").await;

    assert!(
        matches!(tool_part(&seen, "plan_enter"), ToolState::Error { error, .. }
            if error == DISMISSED),
        "a No reads as the dismissal sentence"
    );
    // A refusal is information, not a turn abort.
    assert_eq!(finish(&seen), FinishReason::Completed);
    assert_eq!(agent_changes(&seen), Vec::<(String, String)>::new());
    assert_eq!(engine.agent().as_deref(), Some("build"));

    // And the next prompt still builds: no switch was recorded anywhere.
    send_when_idle(
        &engine,
        Command::SendPrompt {
            text: "keep going".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        },
    )
    .await;
    let second = drain(&mut events).await;
    assert_eq!(agent_changes(&second), Vec::<(String, String)>::new());
    let requests = requests.lock().expect("the request log is never poisoned");
    let next = requests.last().expect("the second turn asked the provider");
    assert_eq!(parts_saying(next, PLAN_REMINDER), 0);
}

/// The two-phase seam, from the other side: the rules change at phase two, so
/// an edit the model tries in the same turn as its approved `plan_enter` is
/// still the *build* agent's edit and still runs. The enter door does not
/// tighten a turn already in flight; it decides the next one.
#[tokio::test]
async fn an_edit_in_the_turn_that_asked_is_still_a_build_agents_edit() {
    let (edit, edits) = ganja_testkit::RecorderTool::new("edit", "edit ran", "done");
    let (provider, _) = ScriptedProvider::new(vec![
        enter_call(),
        tool_call("edit", json!({ "filePath": "src/main.rs" })),
        ganja_testkit::says("done either way"),
    ]);
    let engine = Engine::new(
        provider as Arc<dyn Provider>,
        "recorder-model",
        Arc::new(Registry::new(vec![edit])),
        Permissions::default(),
    )
    .with_agents(ganja_testkit::agent_registry(&Config::default()));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    // Not `answered_turn`: build's `edit` is ask-by-default, so this turn
    // raises a permission dialog the plan agent's own turn never would, and
    // the drain has to answer it as well as the door's question.
    engine
        .send(Command::SendPrompt {
            text: "add a cache layer".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
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
    let seen = ganja_testkit::drain_allowing(&engine, &mut events).await;

    assert_eq!(
        edits.lock().expect("the call log").len(),
        1,
        "the edit ran: this turn is still build's"
    );
    assert!(
        matches!(tool_part(&seen, "edit"), ToolState::Completed { .. }),
        "and it completed rather than being refused"
    );

    // The switch still lands at the boundary, after the edit it did not stop.
    let (agent, _) = boundary_announcement(&mut events).await;
    assert_eq!(agent, "plan");
}

/// The person's later explicit choice outranks the earlier Yes, in this
/// direction too: a manual switch back to build discards the pending plan
/// switch whole (deviation: a-later-switch-outranks-a-yes).
#[tokio::test]
async fn a_manual_switch_after_yes_supersedes_the_plan_switch() {
    let (engine, requests) = engine_over(vec![enter_call()]);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    answered_turn(&engine, &mut events, "add a cache layer", "Yes").await;
    let (agent, _) = boundary_announcement(&mut events).await;
    assert_eq!(agent, "plan");

    send_when_idle(
        &engine,
        Command::SwitchAgent {
            name: "build".to_owned(),
        },
    )
    .await;
    match next_event(&mut events).await {
        Event::AgentChanged { agent, .. } => assert_eq!(agent, "build"),
        other => panic!("the manual supersede announces itself, got {other:?}"),
    }

    engine
        .send(Command::SendPrompt {
            text: "just do it".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("the switch left the engine idle");
    let second = drain(&mut events).await;

    assert_eq!(engine.agent().as_deref(), Some("build"));
    assert_eq!(agent_changes(&second), Vec::<(String, String)>::new());
    let requests = requests.lock().expect("the request log is never poisoned");
    let next = requests.last().expect("the build turn asked the provider");
    assert_eq!(parts_saying(next, PLAN_REMINDER), 0);
}

/// The boundary's durable half: a restart between the Yes and the next prompt
/// resumes as plan, because the row was written at the boundary rather than
/// at the entry that applies the switch in memory.
#[tokio::test]
async fn a_restart_between_yes_and_the_prompt_resumes_as_plan() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let storage = Storage::open(dir.path().join("storage"));

    let session = {
        let (engine, _) = persistent_over(vec![enter_call()], storage.clone()).await;
        let mut events = engine.subscribe().await.expect("the first subscriber wins");
        answered_turn(&engine, &mut events, "add a cache layer", "Yes").await;
        let (agent, _) = boundary_announcement(&mut events).await;
        assert_eq!(agent, "plan");

        engine.current_session().expect("the session has a row").id
    };

    // The restart: a fresh engine over the same store.
    let (provider, _) = ScriptedProvider::new(vec![ganja_testkit::says("planning")]);
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
        .expect("the switched session resumes");
    assert_eq!(
        engine.agent().as_deref(),
        Some("plan"),
        "the boundary's row write survives the restart"
    );
}

// ---------------------------------------------------------------------------
// Who may call it, and the rebuild the registration survives.
// ---------------------------------------------------------------------------

/// The permission half, end to end rather than in the rules alone: the plan
/// agent's call comes back as refusal *text* the model reads, not as an
/// absence it cannot see. Denied tools are not hidden here (standing), so the
/// tool is still in the offered set.
#[tokio::test]
async fn a_planning_session_reads_a_refusal_rather_than_a_missing_tool() {
    let (engine, requests) = engine_over(vec![enter_call()]);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SwitchAgent {
            name: "plan".to_owned(),
        })
        .await
        .expect("plan is a builtin primary agent");
    match next_event(&mut events).await {
        Event::AgentChanged { agent, .. } => assert_eq!(agent, "plan"),
        other => panic!("a manual switch announces itself, got {other:?}"),
    }

    engine
        .send(Command::SendPrompt {
            text: "plan it".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain(&mut events).await;

    assert!(
        matches!(tool_part(&seen, "plan_enter"), ToolState::Error { error, .. }
            if error.contains(r#""permission":"plan_enter""#)),
        "the refusal quotes the rule that made it"
    );
    assert_eq!(engine.agent().as_deref(), Some("plan"));
    assert_eq!(agent_changes(&seen), Vec::<(String, String)>::new());

    let requests = requests.lock().expect("the request log is never poisoned");
    let offered: Vec<&str> = requests
        .last()
        .expect("the turn asked the provider")
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert!(
        offered.contains(&"plan_enter"),
        "denied tools are not hidden: {offered:?}"
    );
}

/// The registration gate, in both directions: the door is absent from the
/// builtin surface the golden differential pins, present when the roster
/// holds a plan agent, and absent when it does not.
#[tokio::test]
async fn the_enter_door_is_registered_only_where_a_plan_agent_exists() {
    assert!(
        !Registry::with_builtins()
            .definitions()
            .iter()
            .any(|tool| tool.name == "plan_enter"),
        "the builtin surface must not move"
    );

    // A roster with plan in it: offered.
    let (engine, requests) = engine_over(vec![ganja_testkit::says("hello")]);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::SendPrompt {
            text: "hi".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;
    let offered: Vec<String> = requests
        .lock()
        .expect("the request log is never poisoned")
        .last()
        .expect("the turn asked the provider")
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect();
    assert!(
        offered.iter().any(|name| name == "plan_enter"),
        "{offered:?}"
    );

    // And a roster with plan disabled: nothing to switch to, nothing offered.
    let config: Config = serde_json::from_value(json!({
        "agent": { "plan": { "disable": true } }
    }))
    .expect("the fixture config is a config");
    let (provider, requests) = ScriptedProvider::new(vec![ganja_testkit::says("hello")]);
    let engine = Engine::new(
        provider as Arc<dyn Provider>,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_agents(ganja_testkit::agent_registry(&config));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::SendPrompt {
            text: "hi".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;
    let offered: Vec<String> = requests
        .lock()
        .expect("the request log is never poisoned")
        .last()
        .expect("the turn asked the provider")
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect();
    assert!(
        !offered.iter().any(|name| name == "plan_enter"),
        "presence is ability: {offered:?}"
    );
}

/// The rebuild clause, mirroring the exit door's: `plan_enter` is registered
/// inside the one rebuild chain every path shares, so it is **still present
/// after an MCP dial moves the tool surface**.
///
/// The dial is real — the reference `@modelcontextprotocol/sdk` server the
/// `mcp.rs` suite documents — because a failed dial bumps no generation and
/// would leave the rebuild untested. Prerequisites are `mcp.rs`'s, and
/// missing ones fail rather than skip, for the reason given there.
#[tokio::test]
async fn plan_enter_survives_the_mcp_dial_rebuild() {
    let checkout = std::env::var_os("GANJA_OPENCODE_DIR").map_or_else(
        || {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../.omc/reference/opencode-v1.18.13")
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
    // is over: a script queue is popped per *request*, and an asking turn
    // makes two.
    let (provider, requests) = ScriptedProvider::new(vec![enter_call()]);
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

    // Before the dial: offered, and callable enough to ask.
    let seen = answered_turn(&engine, &mut events, "add a cache layer", "No").await;
    assert!(
        matches!(tool_part(&seen, "plan_enter"), ToolState::Error { error, .. } if error == DISMISSED)
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
    // `plan_enter` survived beside them.
    script.push(enter_call());
    let second = answered_turn(&engine, &mut events, "still thinking", "No").await;
    assert!(
        matches!(tool_part(&second, "plan_enter"), ToolState::Error { error, .. } if error == DISMISSED),
        "plan_enter is still offered and still runs after the rebuild"
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
        offered.iter().any(|name| name == "plan_enter"),
        "got {offered:?}"
    );
    assert!(
        offered.iter().any(|name| name.starts_with("mcp__ref__")),
        "the rebuild really happened — the dialled server's tools are in the set: {offered:?}"
    );

    engine.shutdown_mcp().await;
}
