//! Proves the agent loop end to end: a turn spans as many model requests as
//! its tool calls demand, every call is gated, executed and answered in
//! order, and the event stream tells the whole story.
//!
//! Providers and tools here are test doubles scripted per request, because
//! the loop under test is the engine's, not theirs.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::{self, BoxStream};
use ganja_core::permission::{Decision, Permissions};
use ganja_core::protocol::{
    Command, Event, FinishReason, PartBody, PermissionId, PermissionMode, PermissionReply, Role,
    ToolState, Usage,
};
use ganja_core::provider::{COMPOSING, ChatRequest, Provider, ProviderError, ProviderEvent};
use ganja_core::tool::{Registry, Tool, ToolCtx, ToolError, ToolOutput};
use ganja_core::{Engine, EngineError, Storage};
use ganja_testkit::{BlockingTool, RecorderTool, ScriptedProvider, drain};
use tokio_util::sync::CancellationToken;

/// The rejection text the model reads, pinned to upstream
/// `packages/core/src/v1/permission.ts`.
const REJECTED: &str = "The user rejected permission to use this specific tool call.";

/// The invalid-call prefix, pinned to upstream `tool/invalid.ts`.
const INVALID_PREFIX: &str = "The arguments provided to the tool are invalid:";

/// Fails every invocation with a message the model is meant to read.
struct FailingTool;

#[async_trait]
impl Tool for FailingTool {
    fn id(&self) -> &str {
        "lookup"
    }

    fn description(&self) -> &str {
        "fails on purpose"
    }

    fn schema(&self) -> schemars::Schema {
        ganja_testkit::placeholder_schema()
    }

    async fn run(&self, _args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Failed("the index is corrupt".to_owned()))
    }
}

/// A tool-call script fragment: start, arguments in two pieces, end.
fn call(id: &str, tool: &str, json: &str) -> Vec<ProviderEvent> {
    let (head, tail) = json.split_at(json.len() / 2);

    vec![
        ProviderEvent::ToolCallStart { id: id.to_owned(), name: tool.to_owned() },
        ProviderEvent::ToolCallDelta { id: id.to_owned(), json: head.to_owned() },
        ProviderEvent::ToolCallDelta { id: id.to_owned(), json: tail.to_owned() },
        ProviderEvent::ToolCallEnd { id: id.to_owned() },
    ]
}

fn usage(input: u64, output: u64) -> Usage {
    Usage { input_tokens: input, output_tokens: output, ..Usage::default() }
}

/// One line per event, carrying exactly what the order tests pin.
fn shape(event: &Event) -> String {
    fn state_tag(state: &ToolState) -> &'static str {
        match state {
            ToolState::Pending { .. } => "pending",
            ToolState::Running { .. } => "running",
            ToolState::Completed { .. } => "completed",
            ToolState::Error { .. } => "error",
        }
    }

    match event {
        Event::MessageStarted { session_id: _, message } => match message.role {
            Role::User => "started:user".to_owned(),
            Role::Assistant => "started:assistant".to_owned(),
        },
        Event::PartStarted { part, .. } => match &part.body {
            PartBody::Text { .. } => "part:text".to_owned(),
            PartBody::File { path, .. } => format!("part:file:{path}"),
            PartBody::StepStart => "part:step_start".to_owned(),
            PartBody::StepFinish { usage } => {
                format!("part:step_finish:{}/{}", usage.input_tokens, usage.output_tokens)
            }
            PartBody::Tool { call_id, state, .. } => {
                format!("part:tool_{}:{call_id}", state_tag(state))
            }
            PartBody::Patch { files, .. } => format!("part:patch:{}", files.join(",")),
            // The item id and never the state: a transcript line quoting a
            // provider's sealed bytes is unreadable, and this file's whole
            // output is meant to be read.
            PartBody::Reasoning { item, .. } => format!("part:reasoning:{item}"),
            // Opened empty and grown by deltas, the way a text part is, so the
            // line names the kind and the deltas carry the words.
            PartBody::ReasoningText { .. } => "part:reasoning_text".to_owned(),
            // Finished when it arrives, so the line names what ran rather
            // than a state it will pass through.
            PartBody::ServerTool { tool, .. } => format!("part:server_tool:{tool}"),
            // Whole when it arrives too, and named by its sender: which
            // teammate said a thing is the fact an order test would be
            // written about, where the words themselves are prose.
            PartBody::Peer { from, .. } => format!("part:peer:{from}"),
        },
        Event::PartDelta { delta, .. } => format!("delta:{delta}"),
        Event::PartUpdated { part, .. } => match &part.body {
            PartBody::Tool { call_id, state, .. } => {
                format!("updated:{}:{call_id}", state_tag(state))
            }
            _ => "updated:other".to_owned(),
        },
        Event::PermissionRequested { tool, .. } => format!("perm_requested:{tool}"),
        Event::PermissionReplied { reply, .. } => format!(
            "perm_replied:{}",
            match reply {
                PermissionReply::Once => "once",
                PermissionReply::Always => "always",
                PermissionReply::Reject => "reject",
            }
        ),
        Event::SteerConsumed { id, .. } => format!("steer_consumed:{id}"),
        Event::CompactionProgress { tokens, .. } => format!("compaction:{tokens}"),
        Event::QuestionAsked { questions, .. } => format!("question_asked:{}", questions.len()),
        Event::QuestionReplied { answers, .. } => format!("question_replied:{}", answers.len()),
        Event::QuestionRejected { .. } => "question_rejected".to_owned(),
        Event::MessageFinished { reason, .. } => format!(
            "finished:{}",
            match reason {
                FinishReason::Completed => "completed",
                FinishReason::Cancelled => "cancelled",
                FinishReason::Failed => "failed",
            }
        ),
        Event::RevertChanged { revert, .. } => format!(
            "revert:{}",
            match revert {
                Some(revert) => revert.message_id.as_str(),
                None => "cleared",
            }
        ),
        Event::AgentChanged { agent, .. } => format!("agent_changed:{agent}"),
        Event::EffortChanged { effort, .. } => {
            format!("effort_changed:{}", effort.as_deref().unwrap_or("default"))
        }
        Event::PermissionModeChanged { mode, .. } => format!(
            "permission_mode_changed:{}",
            match mode {
                PermissionMode::Ask => "ask",
                PermissionMode::Bypass => "bypass",
            }
        ),
        // Named with their payloads so an order test that meets one fails
        // readably. Nothing in this suite feeds the admission gate — no test
        // here installs a team, and only a lead session can hold — so a hold
        // event in this binary is itself the finding.
        Event::PeerHeld { id, cause, .. } => format!("peer_held:{}:{cause:?}", id.as_str()),
        Event::PeerHoldSettled { id, outcome, .. } => {
            format!("peer_hold_settled:{}:{outcome:?}", id.as_str())
        }
        // Same reasoning as the two arms above: nothing in this suite sends
        // a `uds:` message either, so a receipt event here is itself the
        // finding, named with its payload rather than swallowed.
        Event::PeerReceipt { id, status, to, .. } => {
            format!("peer_receipt:{}:{status:?}:{to}", id.as_str())
        }
    }
}

/// Drains until a permission request arrives, returning its id and everything
/// seen so far.
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
            "the turn finished without asking, events so far: {seen:?}"
        );
    }
}

/// The tool parts of the last message in `request`, which is where the reply
/// so far — and every call's result — travels back to the model.
fn tool_states(request: &ChatRequest) -> Vec<(&str, &ToolState)> {
    request
        .messages
        .last()
        .expect("the request carries messages")
        .parts
        .iter()
        .filter_map(|part| match &part.body {
            PartBody::Tool { call_id, state, .. } => Some((call_id.as_str(), state)),
            _ => None,
        })
        .collect()
}

fn prompt() -> Command {
    Command::SendPrompt {
        text: "go".to_owned(),
        mentions: Vec::new(),
        skills: Vec::new(),
        session_mentions: Vec::new(),
        peers: Vec::new(),
    }
}

/// The loop advertises `Registry::definitions()` on every request, so the
/// builtin set has to produce them without panicking — this is what makes it
/// safe for a frontend to construct its engine over `with_builtins`.
#[test]
fn the_builtin_registry_advertises_every_tool() {
    let definitions = Registry::with_builtins().definitions();
    let names: Vec<&str> = definitions.iter().map(|definition| definition.name.as_str()).collect();

    assert_eq!(
        names,
        [
            "read",
            "edit",
            "write",
            "glob",
            "grep",
            "bash",
            "todowrite",
            "webfetch",
            "websearch",
            "skill",
            "question",
            "bash_output",
            "kill_shell",
        ]
    );
    assert!(
        definitions
            .iter()
            .all(|definition| !definition.description.is_empty() && definition.schema.is_object()),
        "every builtin describes itself to the model"
    );
}

#[tokio::test]
async fn a_turn_spans_steps_until_a_request_ends_without_tool_calls() {
    let mut step_one = vec![ProviderEvent::TextDelta("Let me look. ".to_owned())];
    step_one.extend(call("call_1", "lookup", r#"{"key":"a"}"#));
    step_one.extend(call("call_2", "lookup", r#"{"key":"b"}"#));
    step_one.push(ProviderEvent::Usage(usage(3, 5)));
    step_one.push(ProviderEvent::Finish(FinishReason::Completed));

    let step_two = vec![
        ProviderEvent::TextDelta("done".to_owned()),
        ProviderEvent::Usage(usage(7, 11)),
        ProviderEvent::Finish(FinishReason::Completed),
    ];

    let (provider, seen_requests) =
        ScriptedProvider::strict("step-scripted", vec![step_one, step_two]);
    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "found it");
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    let seen = drain(&mut events).await;

    let shapes: Vec<String> = seen.iter().map(shape).collect();
    assert_eq!(
        shapes,
        vec![
            "started:user",
            "started:assistant",
            "part:step_start",
            "part:text",
            "delta:Let me look. ",
            "part:tool_pending:call_1",
            // The moment a call's arguments finish streaming its part renames
            // them, still pending: what will run is on screen before its turn
            // comes (2026-08-15).
            "updated:pending:call_1",
            "part:tool_pending:call_2",
            "updated:pending:call_2",
            "part:step_finish:3/5",
            "updated:running:call_1",
            "updated:completed:call_1",
            "updated:running:call_2",
            "updated:completed:call_2",
            "part:step_start",
            "part:text",
            "delta:done",
            "part:step_finish:7/11",
            "finished:completed",
        ],
        "the event order is the loop's contract"
    );

    // Both calls executed, sequentially, in arrival order.
    assert_eq!(
        *calls.lock().expect("the call log is never poisoned"),
        vec![serde_json::json!({"key": "a"}), serde_json::json!({"key": "b"}),]
    );

    // Tool parts belong to the assistant message, and the second request
    // carries them — results included — so the model reads what its calls
    // returned.
    let Some(Event::MessageStarted { session_id: _, message: assistant }) = seen.get(1) else {
        panic!("the assistant envelope should be second, got {seen:?}");
    };
    for event in &seen {
        if let Event::PartStarted { message_id, .. } | Event::PartUpdated { message_id, .. } = event
        {
            assert_eq!(*message_id, assistant.id, "every part is the reply's");
        }
    }

    let requests = seen_requests.lock().expect("the request log is never poisoned");
    assert_eq!(requests.len(), 2, "one request per step");
    assert_eq!(requests[0].messages.len(), 1, "the first request carries only the prompt");
    let second = &requests[1];
    assert_eq!(second.messages.len(), 2, "the second request adds the reply so far");
    let states = tool_states(second);
    assert_eq!(states.len(), 2);
    for (index, (call_id, state)) in states.iter().enumerate() {
        assert_eq!(*call_id, format!("call_{}", index + 1));
        let ToolState::Completed { input, output, .. } = state else {
            panic!("the model should read a completed call, got {state:?}");
        };
        assert_eq!(output, "found it");
        assert!(input.is_object());
    }

    // Usage sums across steps.
    let Some(Event::MessageFinished { usage: total, .. }) = seen.last() else {
        panic!("a turn ends with a finish, got {seen:?}");
    };
    assert_eq!(*total, Some(usage(10, 16)));
}

/// Two thoughts split by the wire's boundary render as two parts: the break
/// closes the open reasoning part, so the next delta opens a fresh one and
/// the transcript draws each summary behind a marker of its own instead of
/// splicing them together ("PlanningDesigning…", 2026-08-25).
#[tokio::test]
async fn a_reasoning_break_starts_a_new_thought_part() {
    let script = vec![vec![
        ProviderEvent::ReasoningDelta("Planning".to_owned()),
        ProviderEvent::ReasoningBreak,
        ProviderEvent::ReasoningDelta("Designing".to_owned()),
        ProviderEvent::TextDelta("ok".to_owned()),
        ProviderEvent::Finish(FinishReason::Completed),
    ]];
    let (provider, _requests) = ScriptedProvider::strict("step-scripted", script);
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    let seen = drain(&mut events).await;

    let shapes: Vec<String> = seen.iter().map(shape).collect();
    let thoughts: Vec<usize> = shapes
        .iter()
        .enumerate()
        .filter(|(_, shape)| *shape == "part:reasoning_text")
        .map(|(index, _)| index)
        .collect();
    assert_eq!(thoughts.len(), 2, "two thoughts, two parts: {shapes:?}");
    assert_eq!(shapes[thoughts[0] + 1], "delta:Planning", "{shapes:?}");
    assert_eq!(shapes[thoughts[1] + 1], "delta:Designing", "{shapes:?}");
}

#[tokio::test]
async fn a_call_with_no_arguments_runs_with_an_empty_object() {
    let (provider, _requests) = ScriptedProvider::strict(
        "step-scripted",
        vec![
            vec![
                ProviderEvent::ToolCallStart { id: "call_1".to_owned(), name: "lookup".to_owned() },
                ProviderEvent::ToolCallEnd { id: "call_1".to_owned() },
                ProviderEvent::Finish(FinishReason::Completed),
            ],
            vec![ProviderEvent::Finish(FinishReason::Completed)],
        ],
    );
    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "found it");
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    drain(&mut events).await;

    assert_eq!(
        *calls.lock().expect("the call log is never poisoned"),
        vec![serde_json::json!({})],
        "no fragments parse as an empty arguments object"
    );
}

#[tokio::test]
async fn a_permission_answered_once_runs_the_call() {
    let mut step_one = call("call_1", "shell", r#"{"key":"a"}"#);
    step_one.push(ProviderEvent::Finish(FinishReason::Completed));
    let (provider, _requests) = ScriptedProvider::strict(
        "step-scripted",
        vec![step_one, vec![ProviderEvent::Finish(FinishReason::Completed)]],
    );
    let (tool, calls) = RecorderTool::new("shell", "shell ran", "found it");
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");

    let (id, seen) = until_permission(&mut events).await;
    let Some(Event::PermissionRequested { call_id, tool, title, args, .. }) = seen.last() else {
        panic!("the drain stops on the request");
    };
    assert_eq!(call_id, "call_1");
    assert_eq!(tool, "shell");
    assert_eq!(title, "shell", "the default title names the tool");
    assert_eq!(*args, serde_json::json!({"key": "a"}));
    assert!(
        calls.lock().expect("the call log is never poisoned").is_empty(),
        "nothing runs while the question is open"
    );

    engine
        .send(Command::ReplyPermission { id, reply: PermissionReply::Once })
        .await
        .expect("a reply is always accepted");

    let rest = drain(&mut events).await;
    let shapes: Vec<String> = rest.iter().map(shape).collect();
    assert_eq!(
        shapes,
        vec![
            "perm_replied:once",
            "updated:running:call_1",
            "updated:completed:call_1",
            "part:step_start",
            "part:step_finish:0/0",
            "finished:completed",
        ]
    );
    assert_eq!(
        calls.lock().expect("the call log is never poisoned").len(),
        1,
        "an allowed call runs exactly once"
    );
}

#[tokio::test]
async fn a_permission_answered_always_stops_the_asking() {
    let mut first_turn = call("call_1", "shell", r#"{"key":"a"}"#);
    first_turn.push(ProviderEvent::Finish(FinishReason::Completed));
    let mut second_turn = call("call_2", "shell", r#"{"key":"b"}"#);
    second_turn.push(ProviderEvent::Finish(FinishReason::Completed));
    let done = vec![ProviderEvent::Finish(FinishReason::Completed)];

    let (provider, _requests) = ScriptedProvider::strict(
        "step-scripted",
        vec![first_turn, done.clone(), second_turn, done],
    );
    let (tool, calls) = RecorderTool::new("shell", "shell ran", "found it");
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    let (id, _) = until_permission(&mut events).await;
    engine
        .send(Command::ReplyPermission { id, reply: PermissionReply::Always })
        .await
        .expect("a reply is always accepted");
    drain(&mut events).await;

    // The answer stuck: the shared rules now allow the tool outright.
    assert_eq!(
        engine
            .permissions()
            .lock()
            .expect("the permission rules are never poisoned")
            .gate("shell", &serde_json::json!({"key": "b"}))
            .action,
        Decision::Allow
    );

    engine.send(prompt()).await.expect("the engine is idle again");
    let second = drain(&mut events).await;
    assert!(
        !second.iter().any(|event| matches!(event, Event::PermissionRequested { .. })),
        "an always answer means the next call does not ask, got {second:?}"
    );
    assert_eq!(
        calls.lock().expect("the call log is never poisoned").len(),
        2,
        "both turns ran the call"
    );
}

#[tokio::test]
async fn a_rejected_call_does_not_run_and_the_turn_continues() {
    let mut step_one = call("call_1", "shell", r#"{"key":"a"}"#);
    step_one.push(ProviderEvent::Finish(FinishReason::Completed));
    let step_two = vec![
        ProviderEvent::TextDelta("understood".to_owned()),
        ProviderEvent::Finish(FinishReason::Completed),
    ];
    let (provider, seen_requests) =
        ScriptedProvider::strict("step-scripted", vec![step_one, step_two]);
    let (tool, calls) = RecorderTool::new("shell", "shell ran", "found it");
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    let (id, _) = until_permission(&mut events).await;
    engine
        .send(Command::ReplyPermission { id, reply: PermissionReply::Reject })
        .await
        .expect("a reply is always accepted");

    let rest = drain(&mut events).await;
    let shapes: Vec<String> = rest.iter().map(shape).collect();
    assert_eq!(
        shapes,
        vec![
            "perm_replied:reject",
            "updated:error:call_1",
            "part:step_start",
            "part:text",
            "delta:understood",
            "part:step_finish:0/0",
            "finished:completed",
        ],
        "a rejection is information, not a turn abort"
    );

    assert!(
        calls.lock().expect("the call log is never poisoned").is_empty(),
        "a rejected call must not run"
    );

    // The model reads the rejection as the call's result on the next request.
    let requests = seen_requests.lock().expect("the request log is never poisoned");
    let states = tool_states(&requests[1]);
    let Some((call_id, ToolState::Error { error, .. })) = states.first() else {
        panic!("the rejection should travel as an error state, got {states:?}");
    };
    assert_eq!(*call_id, "call_1");
    assert_eq!(error, REJECTED, "the rejection wording is upstream's");
}

#[tokio::test]
async fn cancelling_while_a_permission_waits_refuses_it() {
    let mut step_one = call("call_1", "shell", r#"{"key":"a"}"#);
    step_one.push(ProviderEvent::Finish(FinishReason::Completed));
    let (provider, _requests) = ScriptedProvider::strict(
        "step-scripted",
        vec![step_one, vec![ProviderEvent::Finish(FinishReason::Completed)]],
    );
    let (tool, calls) = RecorderTool::new("shell", "shell ran", "found it");
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    let (id, _) = until_permission(&mut events).await;

    engine.send(Command::CancelTurn).await.expect("a waiting engine accepts a cancel");

    let rest = drain(&mut events).await;
    let shapes: Vec<String> = rest.iter().map(shape).collect();
    assert_eq!(
        shapes,
        vec!["perm_replied:reject", "updated:error:call_1", "finished:cancelled",],
        "a cancel answers the open request before the turn closes"
    );
    let Some(Event::PermissionReplied { id: replied, .. }) = rest.first() else {
        panic!("the refusal names the request, got {rest:?}");
    };
    assert_eq!(*replied, id);
    assert!(
        calls.lock().expect("the call log is never poisoned").is_empty(),
        "a refused call must not run"
    );

    engine.send(prompt()).await.expect("a cancelled turn leaves the engine idle");
    drain(&mut events).await;
}

#[tokio::test]
async fn a_prompt_is_refused_while_a_permission_waits() {
    let mut step_one = call("call_1", "shell", r#"{"key":"a"}"#);
    step_one.push(ProviderEvent::Finish(FinishReason::Completed));
    let (provider, _requests) = ScriptedProvider::strict(
        "step-scripted",
        vec![step_one, vec![ProviderEvent::Finish(FinishReason::Completed)]],
    );
    let (tool, _calls) = RecorderTool::new("shell", "shell ran", "found it");
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    let (id, _) = until_permission(&mut events).await;

    assert!(
        matches!(engine.send(prompt()).await, Err(EngineError::Busy)),
        "a turn waiting on a permission is still a turn"
    );

    engine
        .send(Command::ReplyPermission { id, reply: PermissionReply::Once })
        .await
        .expect("a reply is always accepted");
    drain(&mut events).await;
}

#[tokio::test]
async fn a_stale_permission_reply_is_ignored() {
    let mut step_one = call("call_1", "shell", r#"{"key":"a"}"#);
    step_one.push(ProviderEvent::Finish(FinishReason::Completed));
    let (provider, _requests) = ScriptedProvider::strict(
        "step-scripted",
        vec![step_one, vec![ProviderEvent::Finish(FinishReason::Completed)]],
    );
    let (tool, calls) = RecorderTool::new("shell", "shell ran", "found it");
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    let (id, _) = until_permission(&mut events).await;

    // A stale id carrying a rejection: were it routed, the call would never
    // run. It is ignored instead, and the real answer still lands.
    engine
        .send(Command::ReplyPermission {
            id: PermissionId::from("perm_stale".to_owned()),
            reply: PermissionReply::Reject,
        })
        .await
        .expect("a stale reply is ignored, not an error");

    engine
        .send(Command::ReplyPermission { id, reply: PermissionReply::Once })
        .await
        .expect("a reply is always accepted");

    let rest = drain(&mut events).await;
    assert_eq!(
        rest.iter().filter(|event| matches!(event, Event::PermissionReplied { .. })).count(),
        1,
        "one request, one reply, got {rest:?}"
    );
    assert_eq!(
        calls.lock().expect("the call log is never poisoned").len(),
        1,
        "the real reply ran the call"
    );
}

#[tokio::test]
async fn an_unknown_tool_becomes_an_error_the_model_reads() {
    let mut step_one = call("call_1", "no_such_tool", r#"{"key":"a"}"#);
    step_one.push(ProviderEvent::Finish(FinishReason::Completed));
    let step_two = vec![ProviderEvent::Finish(FinishReason::Completed)];
    let (provider, seen_requests) =
        ScriptedProvider::strict("step-scripted", vec![step_one, step_two]);
    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "found it");
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    let seen = drain(&mut events).await;

    assert!(
        seen.iter().map(shape).any(|it| it == "updated:error:call_1"),
        "an unknown tool errors its part, got {seen:?}"
    );
    assert!(calls.lock().expect("the call log is never poisoned").is_empty(), "nothing ran");

    let requests = seen_requests.lock().expect("the request log is never poisoned");
    assert_eq!(requests.len(), 2, "the loop continued past the error");
    let states = tool_states(&requests[1]);
    let Some((_, ToolState::Error { error, .. })) = states.first() else {
        panic!("the model should read the failure, got {states:?}");
    };
    assert!(
        error.starts_with(INVALID_PREFIX)
            && error.contains("unavailable tool 'no_such_tool'")
            && error.contains("Available tools: lookup."),
        "the wording is upstream's invalid-tool output, got {error:?}"
    );
}

#[tokio::test]
async fn malformed_arguments_become_an_error_the_model_reads() {
    let step_one = vec![
        ProviderEvent::ToolCallStart { id: "call_1".to_owned(), name: "lookup".to_owned() },
        ProviderEvent::ToolCallDelta { id: "call_1".to_owned(), json: "{not json".to_owned() },
        ProviderEvent::ToolCallEnd { id: "call_1".to_owned() },
        ProviderEvent::Finish(FinishReason::Completed),
    ];
    let step_two = vec![ProviderEvent::Finish(FinishReason::Completed)];
    let (provider, seen_requests) =
        ScriptedProvider::strict("step-scripted", vec![step_one, step_two]);
    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "found it");
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    drain(&mut events).await;

    assert!(
        calls.lock().expect("the call log is never poisoned").is_empty(),
        "a call whose arguments never parsed must not run"
    );

    let requests = seen_requests.lock().expect("the request log is never poisoned");
    assert_eq!(requests.len(), 2, "the loop continued past the error");
    let states = tool_states(&requests[1]);
    let Some((_, ToolState::Error { error, input, .. })) = states.first() else {
        panic!("the model should read the failure, got {states:?}");
    };
    assert!(
        error.starts_with(INVALID_PREFIX),
        "the wording is upstream's invalid-tool output, got {error:?}"
    );
    assert_eq!(*input, serde_json::json!({}), "unparseable arguments leave an empty input");
}

#[tokio::test]
async fn a_tool_failure_is_a_result_not_a_turn_abort() {
    let mut step_one = call("call_1", "lookup", r#"{"key":"a"}"#);
    step_one.push(ProviderEvent::Finish(FinishReason::Completed));
    let step_two = vec![ProviderEvent::Finish(FinishReason::Completed)];
    let (provider, seen_requests) =
        ScriptedProvider::strict("step-scripted", vec![step_one, step_two]);
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![Arc::new(FailingTool)])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    let seen = drain(&mut events).await;

    let Some(Event::MessageFinished { reason, .. }) = seen.last() else {
        panic!("a turn ends with a finish, got {seen:?}");
    };
    assert_eq!(*reason, FinishReason::Completed);

    let requests = seen_requests.lock().expect("the request log is never poisoned");
    let states = tool_states(&requests[1]);
    let Some((_, ToolState::Error { error, .. })) = states.first() else {
        panic!("the model should read the failure, got {states:?}");
    };
    assert_eq!(error, "the index is corrupt", "the model reads the tool's own words");
}

#[tokio::test]
async fn cancelling_mid_execution_errors_the_call_and_finishes_cancelled() {
    let mut step_one = call("call_1", "lookup", r#"{"key":"a"}"#);
    step_one.push(ProviderEvent::Finish(FinishReason::Completed));
    let (provider, _requests) = ScriptedProvider::strict(
        "step-scripted",
        vec![step_one, vec![ProviderEvent::Finish(FinishReason::Completed)]],
    );
    let (entered, mut wait_entered) = tokio::sync::mpsc::channel(1);
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![BlockingTool::with_entry_signal(
            "lookup",
            "waits to be cancelled",
            entered,
        )])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    wait_entered.recv().await.expect("the tool should start executing");

    engine.send(Command::CancelTurn).await.expect("an executing engine accepts a cancel");

    let seen = drain(&mut events).await;
    let shapes: Vec<String> = seen.iter().map(shape).collect();
    assert_eq!(shapes.last().expect("events arrived"), "finished:cancelled");
    assert!(
        shapes.contains(&"updated:error:call_1".to_owned()),
        "the interrupted call closes as an error, got {shapes:?}"
    );

    // The part's wording is the cancel's.
    let cancelled = seen.iter().find_map(|event| match event {
        Event::PartUpdated { part, .. } => match &part.body {
            PartBody::Tool { state: ToolState::Error { error, .. }, .. } => Some(error.clone()),
            _ => None,
        },
        _ => None,
    });
    assert_eq!(cancelled.as_deref(), Some("the call was cancelled"));

    engine.send(prompt()).await.expect("a cancelled turn leaves the engine idle");
    drain(&mut events).await;
}

#[tokio::test]
async fn a_provider_failure_mid_loop_strands_the_buffered_call() {
    let step_one = vec![
        ProviderEvent::TextDelta("checking".to_owned()),
        ProviderEvent::ToolCallStart { id: "call_1".to_owned(), name: "lookup".to_owned() },
        ProviderEvent::ToolCallDelta { id: "call_1".to_owned(), json: r#"{"key":"a"}"#.to_owned() },
        ProviderEvent::Failed(ProviderError::Transport("connection reset".to_owned())),
    ];
    let (provider, _requests) = ScriptedProvider::strict("step-scripted", vec![step_one]);
    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "found it");
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    let seen = drain(&mut events).await;

    let shapes: Vec<String> = seen.iter().map(shape).collect();
    assert_eq!(
        shapes,
        vec![
            "started:user",
            "started:assistant",
            "part:step_start",
            "part:text",
            "delta:checking",
            "part:tool_pending:call_1",
            "updated:error:call_1",
            "finished:failed",
        ],
        "a stranded call is closed before the failure is reported"
    );

    let stranded = seen.iter().find_map(|event| match event {
        Event::PartUpdated { part, .. } => match &part.body {
            PartBody::Tool { state: ToolState::Error { error, .. }, .. } => Some(error.clone()),
            _ => None,
        },
        _ => None,
    });
    assert_eq!(stranded.as_deref(), Some("the provider failed before this call could run"));
    assert!(
        calls.lock().expect("the call log is never poisoned").is_empty(),
        "a stranded call never ran"
    );

    let Some(Event::MessageFinished { reason, error, .. }) = seen.last() else {
        panic!("a turn ends with a finish, got {seen:?}");
    };
    assert_eq!(*reason, FinishReason::Failed);
    assert!(
        error.as_deref().is_some_and(|error| error.contains("connection reset")),
        "the failure explains itself, got {error:?}"
    );
}

/// The tool name every part event for `call_id` carried, in order — what a
/// frontend redraws a row's heading from.
fn names_of(events: &[Event], call_id: &str) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::PartStarted { part, .. } | Event::PartUpdated { part, .. } => match &part.body {
                PartBody::Tool { call_id: id, tool, .. } if id == call_id => Some(tool.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// The error the last part event for `call_id` closed it with, if it did.
fn closed_with(events: &[Event], call_id: &str) -> Option<String> {
    events.iter().rev().find_map(|event| match event {
        Event::PartUpdated { part, .. } => match &part.body {
            PartBody::Tool { call_id: id, state: ToolState::Error { error, .. }, .. }
                if id == call_id =>
            {
                Some(error.clone())
            }
            _ => None,
        },
        _ => None,
    })
}

/// The tool parts of `request`'s last message, as `(call id, tool)`.
fn tool_names(request: &ChatRequest) -> Vec<(&str, &str)> {
    request
        .messages
        .last()
        .expect("the request carries messages")
        .parts
        .iter()
        .filter_map(|part| match &part.body {
            PartBody::Tool { call_id, tool, .. } => Some((call_id.as_str(), tool.as_str())),
            _ => None,
        })
        .collect()
}

/// **D559.** A call a wire announced under `COMPOSING` and then named in the
/// same step is **one** row, renamed in place: one `PartStarted` under `…`,
/// one `PartUpdated` carrying the real name, and the call runs under that
/// name. Everything that reads the name reads it at `prepare` or later, after
/// the rename — which this pins through the permission gate: the dialog asks
/// about `shell`, never about `…` — and the transcript the next request
/// carries holds the real name too.
#[tokio::test]
async fn a_call_announced_before_it_is_named_is_renamed_in_place_and_runs_under_its_name() {
    let step_one = vec![
        ProviderEvent::ToolCallStart { id: "call_1".to_owned(), name: COMPOSING.to_owned() },
        ProviderEvent::ToolCallStart { id: "call_1".to_owned(), name: "shell".to_owned() },
        ProviderEvent::ToolCallDelta { id: "call_1".to_owned(), json: r#"{"key":"a"}"#.to_owned() },
        ProviderEvent::ToolCallEnd { id: "call_1".to_owned() },
        ProviderEvent::Finish(FinishReason::Completed),
    ];
    let (provider, requests) = ScriptedProvider::strict(
        "step-scripted",
        vec![step_one, vec![ProviderEvent::Finish(FinishReason::Completed)]],
    );
    let (tool, calls) = RecorderTool::new("shell", "shell ran", "found it");
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    let (id, mut seen) = until_permission(&mut events).await;
    let Some(Event::PermissionRequested { call_id, tool, .. }) = seen.last() else {
        panic!("the drain stops on the request");
    };
    assert_eq!(call_id, "call_1");
    assert_eq!(tool, "shell", "the gate reads the name the rename left");

    engine
        .send(Command::ReplyPermission { id, reply: PermissionReply::Once })
        .await
        .expect("a reply is always accepted");
    seen.extend(drain(&mut events).await);

    let shapes: Vec<String> = seen.iter().map(shape).collect();
    assert_eq!(
        shapes,
        vec![
            "started:user",
            "started:assistant",
            "part:step_start",
            "part:tool_pending:call_1",
            // The rename: the row the placeholder opened, redrawn.
            "updated:pending:call_1",
            // And the arguments completing, as on every wire.
            "updated:pending:call_1",
            "part:step_finish:0/0",
            "perm_requested:shell",
            "perm_replied:once",
            "updated:running:call_1",
            "updated:completed:call_1",
            "part:step_start",
            "part:step_finish:0/0",
            "finished:completed",
        ],
        "one row, renamed once"
    );
    assert_eq!(
        names_of(&seen, "call_1"),
        vec![COMPOSING, "shell", "shell", "shell", "shell"],
        "the row opened as the placeholder and every event after the rename names the tool"
    );
    assert_eq!(
        *calls.lock().expect("the call log is never poisoned"),
        vec![serde_json::json!({"key": "a"})],
        "the call ran once, under the name the rename gave it"
    );

    let requests = requests.lock().expect("the request log is never poisoned");
    assert_eq!(tool_names(&requests[1]), vec![("call_1", "shell")], "and the transcript names it");
}

/// **D559, the other half of the rule.** A second start under the name the
/// call already has says nothing new, so it draws nothing: one row, and the
/// only update before it runs is the one its arguments completing earns.
#[tokio::test]
async fn a_second_start_under_the_same_name_changes_nothing() {
    let step_one = vec![
        ProviderEvent::ToolCallStart { id: "call_1".to_owned(), name: "lookup".to_owned() },
        ProviderEvent::ToolCallStart { id: "call_1".to_owned(), name: "lookup".to_owned() },
        ProviderEvent::ToolCallDelta { id: "call_1".to_owned(), json: r#"{"key":"a"}"#.to_owned() },
        ProviderEvent::ToolCallEnd { id: "call_1".to_owned() },
        ProviderEvent::Finish(FinishReason::Completed),
    ];
    let (provider, _requests) = ScriptedProvider::strict(
        "step-scripted",
        vec![step_one, vec![ProviderEvent::Finish(FinishReason::Completed)]],
    );
    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "found it");
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    let seen = drain(&mut events).await;

    let shapes: Vec<String> = seen.iter().map(shape).collect();
    assert_eq!(
        shapes,
        vec![
            "started:user",
            "started:assistant",
            "part:step_start",
            "part:tool_pending:call_1",
            "updated:pending:call_1",
            "part:step_finish:0/0",
            "updated:running:call_1",
            "updated:completed:call_1",
            "part:step_start",
            "part:step_finish:0/0",
            "finished:completed",
        ]
    );
    assert_eq!(calls.lock().expect("the call log is never poisoned").len(), 1);
}

/// **D559, scoped.** Only a call still held under `COMPOSING` is renamed. A
/// second start that names a call already named — a degenerate wire reusing
/// one id for two calls, as chat-completions does with `""` on parallel
/// calls — changes nothing, exactly as before D559: one row, the first name,
/// no redraw, and the call runs under the name it opened with. Nor can a
/// start under `COMPOSING` turn a named call back into a placeholder the
/// engine would withhold.
#[tokio::test]
async fn a_second_start_naming_an_already_named_call_differently_changes_nothing() {
    for second in ["shell", COMPOSING] {
        let step_one = vec![
            ProviderEvent::ToolCallStart { id: "call_1".to_owned(), name: "lookup".to_owned() },
            ProviderEvent::ToolCallStart { id: "call_1".to_owned(), name: second.to_owned() },
            ProviderEvent::ToolCallDelta {
                id: "call_1".to_owned(),
                json: r#"{"key":"a"}"#.to_owned(),
            },
            ProviderEvent::ToolCallEnd { id: "call_1".to_owned() },
            ProviderEvent::Finish(FinishReason::Completed),
        ];
        let (provider, requests) = ScriptedProvider::strict(
            "step-scripted",
            vec![step_one, vec![ProviderEvent::Finish(FinishReason::Completed)]],
        );
        let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "found it");
        let engine = Engine::new(
            provider,
            "scripted-model",
            Arc::new(Registry::new(vec![tool])),
            Permissions::default(),
        );
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        engine.send(prompt()).await.expect("an idle engine accepts");
        let seen = drain(&mut events).await;

        let shapes: Vec<String> = seen.iter().map(shape).collect();
        assert_eq!(
            shapes,
            vec![
                "started:user",
                "started:assistant",
                "part:step_start",
                "part:tool_pending:call_1",
                "updated:pending:call_1",
                "part:step_finish:0/0",
                "updated:running:call_1",
                "updated:completed:call_1",
                "part:step_start",
                "part:step_finish:0/0",
                "finished:completed",
            ],
            "a second start under {second:?} draws nothing"
        );
        assert_eq!(
            names_of(&seen, "call_1"),
            vec!["lookup"; 4],
            "every event names the call as it opened, whatever the second start said"
        );
        assert_eq!(
            *calls.lock().expect("the call log is never poisoned"),
            vec![serde_json::json!({"key": "a"})],
            "the call ran once, under its first name"
        );
        let requests = requests.lock().expect("the request log is never poisoned");
        assert_eq!(tool_names(&requests[1]), vec![("call_1", "lookup")]);
    }
}

/// Answers each request with the next prepared answer — a stream the test
/// built, possibly a channel it feeds, or a refusal — for the two cases a
/// script cannot state: a turn held between two events, and a provider that
/// refuses a request before any stream exists.
///
/// It claims `"fake"` so a persistent engine's title path asks it nothing: a
/// title request would take an answer a turn's step was prepared for.
struct Prepared(Mutex<VecDeque<Result<BoxStream<'static, ProviderEvent>, ProviderError>>>);

impl Prepared {
    fn new(answers: Vec<Result<BoxStream<'static, ProviderEvent>, ProviderError>>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(answers.into())))
    }
}

#[async_trait]
impl Provider for Prepared {
    fn id(&self) -> &str {
        "fake"
    }

    async fn stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        self.0
            .lock()
            .expect("the answers are never poisoned")
            .pop_front()
            .expect("an answer was prepared for every request")
    }
}

/// The tool part for `call_id` as the store holds it, if it holds one.
fn stored_call(storage: &Storage, engine: &Engine, call_id: &str) -> Option<(String, ToolState)> {
    let session = engine.current_session()?.id;
    storage.load_transcript(&session).ok()?.iter().flat_map(|message| &message.parts).find_map(
        |part| match &part.body {
            PartBody::Tool { call_id: id, tool, state } if id == call_id => {
                Some((tool.clone(), state.clone()))
            }
            _ => None,
        },
    )
}

/// **D559.** A rename reaches the store when it happens, not only when the
/// call's arguments end. A crash between the two would otherwise leave the row
/// stored under `…`, which a resume closes as interrupted and cursor's history
/// then leaves out — the named call gone from the conversation. So, held
/// between the naming start and everything after it, the stored part already
/// carries the name while it is still pending with no input.
#[tokio::test]
async fn a_rename_is_stored_before_the_calls_arguments_end() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let storage = Storage::open(dir.path().join("storage"));
    let (feed, held) = futures::channel::mpsc::unbounded();
    let provider = Prepared::new(vec![
        Ok(held.boxed()),
        Ok(stream::iter([ProviderEvent::Finish(FinishReason::Completed)]).boxed()),
    ]);
    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "found it");
    let engine = Engine::persistent(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
        storage.clone(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    for name in [COMPOSING, "lookup"] {
        feed.unbounded_send(ProviderEvent::ToolCallStart {
            id: "call_1".to_owned(),
            name: name.to_owned(),
        })
        .expect("the turn is reading its stream");
    }
    let mut seen = Vec::new();
    while !names_of(&seen, "call_1").iter().any(|name| name == "lookup") {
        let event = tokio::time::timeout(Duration::from_secs(10), events.next())
            .await
            .expect("the rename is reported while the stream is held")
            .expect("the engine outlives its turn");
        seen.push(event);
    }

    // The store's writes go through a thread of their own, so the rename's is
    // waited for — bounded, and the last thing read is what a failure shows.
    let mut stored = None;
    for _ in 0..500 {
        stored = stored_call(&storage, &engine, "call_1");
        if stored.as_ref().is_some_and(|(tool, _)| tool == "lookup") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        matches!(&stored, Some((tool, ToolState::Pending { input: None })) if tool == "lookup"),
        "the rename's own write, ahead of the arguments: {stored:?}"
    );

    feed.unbounded_send(ProviderEvent::ToolCallDelta {
        id: "call_1".to_owned(),
        json: r#"{"key":"a"}"#.to_owned(),
    })
    .expect("the turn is reading its stream");
    feed.unbounded_send(ProviderEvent::ToolCallEnd { id: "call_1".to_owned() })
        .expect("the turn is reading its stream");
    feed.unbounded_send(ProviderEvent::Finish(FinishReason::Completed))
        .expect("the turn is reading its stream");
    drop(feed);
    seen.extend(drain(&mut events).await);

    assert_eq!(seen.last().map(shape).as_deref(), Some("finished:completed"));
    assert_eq!(
        *calls.lock().expect("the call log is never poisoned"),
        vec![serde_json::json!({"key": "a"})],
        "the call ran once, under its name"
    );
}

/// **D559, across a pause** — the recorded write, in the engine's terms. A
/// call is announced; another call is complete on the same step, which ends
/// with the placeholder still unnamed. That row is **withheld**, not run —
/// `…` is no tool — and stays open in the message, which the next request
/// carries it in; the next step names it, and it is the same row that is
/// renamed and run there, never a second part for the same id and never an
/// unknown-tool error.
#[tokio::test]
async fn a_call_announced_across_a_pause_keeps_its_row_and_runs_on_the_step_that_names_it() {
    let mut step_one =
        vec![ProviderEvent::ToolCallStart { id: "call_w".to_owned(), name: COMPOSING.to_owned() }];
    step_one.extend(call("call_r", "lookup", r#"{"key":"r"}"#));
    step_one.push(ProviderEvent::Finish(FinishReason::Completed));
    let mut step_two = call("call_w", "lookup", r#"{"key":"w"}"#);
    step_two.push(ProviderEvent::Finish(FinishReason::Completed));
    let step_three = vec![
        ProviderEvent::TextDelta("done".to_owned()),
        ProviderEvent::Finish(FinishReason::Completed),
    ];
    let (provider, requests) =
        ScriptedProvider::strict("step-scripted", vec![step_one, step_two, step_three]);
    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "found it");
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    let seen = drain(&mut events).await;

    let shapes: Vec<String> = seen.iter().map(shape).collect();
    assert_eq!(
        shapes,
        vec![
            "started:user",
            "started:assistant",
            "part:step_start",
            "part:tool_pending:call_w",
            "part:tool_pending:call_r",
            "updated:pending:call_r",
            "part:step_finish:0/0",
            // Only the call that was named runs; the placeholder waits.
            "updated:running:call_r",
            "updated:completed:call_r",
            "part:step_start",
            // The same row, renamed by the step that names it...
            "updated:pending:call_w",
            "updated:pending:call_w",
            "part:step_finish:0/0",
            // ...and run there.
            "updated:running:call_w",
            "updated:completed:call_w",
            "part:step_start",
            "part:text",
            "delta:done",
            "part:step_finish:0/0",
            "finished:completed",
        ],
        "one row for the announced call, run on the step that named it"
    );
    assert_eq!(
        *calls.lock().expect("the call log is never poisoned"),
        vec![serde_json::json!({"key": "r"}), serde_json::json!({"key": "w"})],
        "each call ran once, and nothing ran under the placeholder"
    );

    let requests = requests.lock().expect("the request log is never poisoned");
    assert_eq!(requests.len(), 3, "one request per step");
    assert_eq!(
        tool_names(&requests[1]),
        vec![("call_w", COMPOSING), ("call_r", "lookup")],
        "the step that names it is asked with the row still open in the message"
    );
    assert!(
        matches!(
            tool_states(&requests[1]).first(),
            Some(("call_w", ToolState::Pending { input: None }))
        ),
        "withheld, not run: {:?}",
        tool_states(&requests[1])
    );
    assert_eq!(
        tool_names(&requests[2]),
        vec![("call_w", "lookup"), ("call_r", "lookup")],
        "and afterwards the transcript holds it once, named"
    );
    assert!(
        tool_states(&requests[2])
            .iter()
            .all(|(_, state)| matches!(state, ToolState::Completed { .. })),
        "no call closed as an error: {:?}",
        tool_states(&requests[2])
    );
}

/// **D559.** A call announced and never named — the stream ended without it
/// — is closed unrun on the step that ended, with a sentence of its own for
/// the person reading the transcript, and the turn finishes as the model
/// meant it to. Nothing runs, and no second request is spent on it.
#[tokio::test]
async fn a_call_announced_and_never_named_is_closed_unrun_when_the_stream_ends() {
    let (provider, requests) = ScriptedProvider::strict(
        "step-scripted",
        vec![vec![
            ProviderEvent::ToolCallStart { id: "call_w".to_owned(), name: COMPOSING.to_owned() },
            ProviderEvent::TextDelta("On second thought, no.".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ]],
    );
    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "found it");
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    let seen = drain(&mut events).await;

    let shapes: Vec<String> = seen.iter().map(shape).collect();
    assert_eq!(
        shapes,
        vec![
            "started:user",
            "started:assistant",
            "part:step_start",
            "part:tool_pending:call_w",
            "part:text",
            "delta:On second thought, no.",
            "updated:error:call_w",
            "part:step_finish:0/0",
            "finished:completed",
        ]
    );
    assert_eq!(
        closed_with(&seen, "call_w").as_deref(),
        Some("the model began this call, but it never reached a tool, so nothing ran")
    );
    assert_eq!(names_of(&seen, "call_w"), vec![COMPOSING, COMPOSING], "never named, never renamed");
    assert!(calls.lock().expect("the call log is never poisoned").is_empty());
    assert_eq!(requests.lock().expect("the request log is never poisoned").len(), 1);
}

/// **D559.** A cancel that lands while the engine runs the call a step paused
/// on — the placeholder beside it withheld, one of no step's calls at that
/// moment — still closes the placeholder, cancelled. A turn never ends with
/// one open, so a new turn never inherits one.
#[tokio::test]
async fn cancelling_while_an_announced_call_waits_across_a_pause_closes_it_cancelled() {
    let mut step_one =
        vec![ProviderEvent::ToolCallStart { id: "call_w".to_owned(), name: COMPOSING.to_owned() }];
    step_one.extend(call("call_r", "lookup", r#"{"key":"r"}"#));
    step_one.push(ProviderEvent::Finish(FinishReason::Completed));
    let (provider, _requests) = ScriptedProvider::strict("step-scripted", vec![step_one]);
    let (entered, mut wait_entered) = tokio::sync::mpsc::channel(1);
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![BlockingTool::with_entry_signal(
            "lookup",
            "waits to be cancelled",
            entered,
        )])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    wait_entered.recv().await.expect("the named call should start executing");
    engine.send(Command::CancelTurn).await.expect("an executing engine accepts a cancel");
    let seen = drain(&mut events).await;

    let shapes: Vec<String> = seen.iter().map(shape).collect();
    assert_eq!(shapes.last().expect("events arrived"), "finished:cancelled");
    assert_eq!(closed_with(&seen, "call_r").as_deref(), Some("the call was cancelled"));
    assert_eq!(
        closed_with(&seen, "call_w").as_deref(),
        Some("the call was cancelled"),
        "the withheld placeholder is closed with the turn: {shapes:?}"
    );
    let finished = shapes.iter().position(|shape| shape == "finished:cancelled");
    let closed = shapes.iter().rposition(|shape| shape == "updated:error:call_w");
    assert!(closed < finished, "closed before the finish that ends the turn: {shapes:?}");
}

/// **D559.** A provider that dies on the step after a pause strands the
/// placeholder that step inherited, exactly as it strands any call of its
/// own: the step seeded it, so the step's interruption closes it.
#[tokio::test]
async fn a_provider_failure_on_the_step_after_a_pause_strands_the_announced_call() {
    let mut step_one =
        vec![ProviderEvent::ToolCallStart { id: "call_w".to_owned(), name: COMPOSING.to_owned() }];
    step_one.extend(call("call_r", "lookup", r#"{"key":"r"}"#));
    step_one.push(ProviderEvent::Finish(FinishReason::Completed));
    let step_two =
        vec![ProviderEvent::Failed(ProviderError::Transport("connection reset".to_owned()))];
    let (provider, _requests) = ScriptedProvider::strict("step-scripted", vec![step_one, step_two]);
    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "found it");
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    let seen = drain(&mut events).await;

    let shapes: Vec<String> = seen.iter().map(shape).collect();
    assert_eq!(
        &shapes[shapes.len() - 3..],
        ["part:step_start", "updated:error:call_w", "finished:failed"],
        "the inherited row is closed by the step that died: {shapes:?}"
    );
    assert_eq!(
        closed_with(&seen, "call_w").as_deref(),
        Some("the provider failed before this call could run")
    );
    assert_eq!(
        *calls.lock().expect("the call log is never poisoned"),
        vec![serde_json::json!({"key": "r"})],
        "only the named call ever ran"
    );
}

/// **D559.** A provider that refuses the request of the step after a pause —
/// no stream at all, so the step returns before it holds any call — still
/// leaves no placeholder open: the turn's own sweep closes the withheld row,
/// stranded, before the failure is reported. No step could: the only one that
/// would have seeded the row never began reading.
#[tokio::test]
async fn a_provider_refusing_the_step_after_a_pause_strands_the_announced_call() {
    let mut step_one =
        vec![ProviderEvent::ToolCallStart { id: "call_w".to_owned(), name: COMPOSING.to_owned() }];
    step_one.extend(call("call_r", "lookup", r#"{"key":"r"}"#));
    step_one.push(ProviderEvent::Finish(FinishReason::Completed));
    let provider = Prepared::new(vec![
        Ok(stream::iter(step_one).boxed()),
        Err(ProviderError::Transport("connection refused".to_owned())),
    ]);
    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "found it");
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    let seen = drain(&mut events).await;

    let shapes: Vec<String> = seen.iter().map(shape).collect();
    assert_eq!(
        &shapes[shapes.len() - 2..],
        ["updated:error:call_w", "finished:failed"],
        "the withheld row is closed just before the failure: {shapes:?}"
    );
    assert_eq!(
        closed_with(&seen, "call_w").as_deref(),
        Some("the provider failed before this call could run")
    );
    assert_eq!(
        names_of(&seen, "call_w").last().map(String::as_str),
        Some(COMPOSING),
        "never named, so it closes under the placeholder"
    );
    assert_eq!(
        *calls.lock().expect("the call log is never poisoned"),
        vec![serde_json::json!({"key": "r"})],
        "only the named call ever ran"
    );
    let Some(Event::MessageFinished { error, .. }) = seen.last() else {
        panic!("a turn ends with a finish, got {seen:?}");
    };
    assert!(
        error.as_deref().is_some_and(|error| error.contains("connection refused")),
        "the failure explains itself, got {error:?}"
    );
}
