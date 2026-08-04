//! Proves the agent loop end to end: a turn spans as many model requests as
//! its tool calls demand, every call is gated, executed and answered in
//! order, and the event stream tells the whole story.
//!
//! Providers and tools here are test doubles scripted per request, because
//! the loop under test is the engine's, not theirs.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use futures::{
    StreamExt as _,
    stream::{self, BoxStream},
};
use ganja_core::{
    Command, Decision, Engine, EngineError, Event, FinishReason, PartBody, PermissionId,
    PermissionReply, Permissions, Registry, Role, Tool, ToolCtx, ToolError, ToolOutput, ToolState,
    Usage,
    provider::{ChatRequest, Provider, ProviderError, ProviderEvent},
};
use tokio_util::sync::CancellationToken;

/// The rejection text the model reads, pinned to upstream
/// `packages/core/src/v1/permission.ts`.
const REJECTED: &str = "The user rejected permission to use this specific tool call.";

/// The invalid-call prefix, pinned to upstream `tool/invalid.ts`.
const INVALID_PREFIX: &str = "The arguments provided to the tool are invalid:";

/// Answers each request with the next script, and records what it was asked.
struct StepProvider {
    scripts: Mutex<VecDeque<Vec<ProviderEvent>>>,
    seen: Arc<Mutex<Vec<ChatRequest>>>,
}

impl StepProvider {
    fn new(scripts: Vec<Vec<ProviderEvent>>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into()),
            seen: Arc::default(),
        }
    }
}

#[async_trait]
impl Provider for StepProvider {
    fn id(&self) -> &str {
        "step-scripted"
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

        let script = self
            .scripts
            .lock()
            .expect("the scripts are never poisoned")
            .pop_front()
            .expect("the script has a step for every request");

        Ok(stream::iter(script).boxed())
    }
}

/// Arguments every test tool nominally takes; the loop never validates them
/// against the schema, but the registry has to advertise one.
#[derive(schemars::JsonSchema)]
#[allow(dead_code)]
struct Args {
    key: Option<String>,
}

/// Records every invocation and answers with a canned output.
struct RecorderTool {
    id: &'static str,
    calls: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl RecorderTool {
    fn new(id: &'static str) -> (Arc<Self>, Arc<Mutex<Vec<serde_json::Value>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                id,
                calls: Arc::clone(&calls),
            }),
            calls,
        )
    }
}

#[async_trait]
impl Tool for RecorderTool {
    fn id(&self) -> &'static str {
        self.id
    }

    fn description(&self) -> &str {
        "records what it was asked"
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    async fn run(&self, args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        self.calls
            .lock()
            .expect("the call log is never poisoned")
            .push(args);

        Ok(ToolOutput {
            title: format!("{} ran", self.id),
            output: "found it".to_owned(),
            metadata: serde_json::json!({}),
        })
    }
}

/// Fails every invocation with a message the model is meant to read.
struct FailingTool;

#[async_trait]
impl Tool for FailingTool {
    fn id(&self) -> &'static str {
        "lookup"
    }

    fn description(&self) -> &str {
        "fails on purpose"
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    async fn run(&self, _args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Failed("the index is corrupt".to_owned()))
    }
}

/// Announces that it started, then waits for the cancel it expects.
struct StallTool {
    entered: tokio::sync::mpsc::Sender<()>,
}

#[async_trait]
impl Tool for StallTool {
    fn id(&self) -> &'static str {
        "lookup"
    }

    fn description(&self) -> &str {
        "waits to be cancelled"
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    async fn run(&self, _args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let _ = self.entered.send(()).await;
        ctx.cancel.cancelled().await;

        Err(ToolError::Cancelled)
    }
}

/// A tool-call script fragment: start, arguments in two pieces, end.
fn call(id: &str, tool: &str, json: &str) -> Vec<ProviderEvent> {
    let (head, tail) = json.split_at(json.len() / 2);

    vec![
        ProviderEvent::ToolCallStart {
            id: id.to_owned(),
            name: tool.to_owned(),
        },
        ProviderEvent::ToolCallDelta {
            id: id.to_owned(),
            json: head.to_owned(),
        },
        ProviderEvent::ToolCallDelta {
            id: id.to_owned(),
            json: tail.to_owned(),
        },
        ProviderEvent::ToolCallEnd { id: id.to_owned() },
    ]
}

fn usage(input: u64, output: u64) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        ..Usage::default()
    }
}

/// One line per event, carrying exactly what the order tests pin.
fn shape(event: &Event) -> String {
    fn state_tag(state: &ToolState) -> &'static str {
        match state {
            ToolState::Pending => "pending",
            ToolState::Running { .. } => "running",
            ToolState::Completed { .. } => "completed",
            ToolState::Error { .. } => "error",
        }
    }

    match event {
        Event::MessageStarted { message } => match message.role {
            Role::User => "started:user".to_owned(),
            Role::Assistant => "started:assistant".to_owned(),
        },
        Event::PartStarted { part, .. } => match &part.body {
            PartBody::Text { .. } => "part:text".to_owned(),
            PartBody::File { path, .. } => format!("part:file:{path}"),
            PartBody::StepStart => "part:step_start".to_owned(),
            PartBody::StepFinish { usage } => format!(
                "part:step_finish:{}/{}",
                usage.input_tokens, usage.output_tokens
            ),
            PartBody::Tool { call_id, state, .. } => {
                format!("part:tool_{}:{call_id}", state_tag(state))
            }
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
        Event::MessageFinished { reason, .. } => format!(
            "finished:{}",
            match reason {
                FinishReason::Completed => "completed",
                FinishReason::Cancelled => "cancelled",
                FinishReason::Failed => "failed",
            }
        ),
    }
}

/// Drains events until the turn finishes, returning everything seen.
async fn drain(events: &mut BoxStream<'static, Event>) -> Vec<Event> {
    let mut seen = Vec::new();

    loop {
        let event = events
            .next()
            .await
            .expect("the turn should finish before the stream ends");
        let finished = matches!(event, Event::MessageFinished { .. });
        seen.push(event);

        if finished {
            return seen;
        }
    }
}

/// Drains until a permission request arrives, returning its id and everything
/// seen so far.
async fn until_permission(events: &mut BoxStream<'static, Event>) -> (PermissionId, Vec<Event>) {
    let mut seen = Vec::new();

    loop {
        let event = events
            .next()
            .await
            .expect("a permission request should arrive before the stream ends");
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
    }
}

/// The loop advertises `Registry::definitions()` on every request, so the
/// builtin set has to produce them without panicking — this is what makes it
/// safe for a frontend to construct its engine over `with_builtins`.
#[test]
fn the_builtin_registry_advertises_every_tool() {
    let definitions = Registry::with_builtins().definitions();
    let names: Vec<&str> = definitions
        .iter()
        .map(|definition| definition.name.as_str())
        .collect();

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

    let provider = Arc::new(StepProvider::new(vec![step_one, step_two]));
    let seen_requests = Arc::clone(&provider.seen);
    let (tool, calls) = RecorderTool::new("lookup");
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
            "part:tool_pending:call_2",
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
        vec![
            serde_json::json!({"key": "a"}),
            serde_json::json!({"key": "b"}),
        ]
    );

    // Tool parts belong to the assistant message, and the second request
    // carries them — results included — so the model reads what its calls
    // returned.
    let Some(Event::MessageStarted { message: assistant }) = seen.get(1) else {
        panic!("the assistant envelope should be second, got {seen:?}");
    };
    for event in &seen {
        if let Event::PartStarted { message_id, .. } | Event::PartUpdated { message_id, .. } = event
        {
            assert_eq!(*message_id, assistant.id, "every part is the reply's");
        }
    }

    let requests = seen_requests
        .lock()
        .expect("the request log is never poisoned");
    assert_eq!(requests.len(), 2, "one request per step");
    assert_eq!(
        requests[0].messages.len(),
        1,
        "the first request carries only the prompt"
    );
    let second = &requests[1];
    assert_eq!(
        second.messages.len(),
        2,
        "the second request adds the reply so far"
    );
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

#[tokio::test]
async fn a_call_with_no_arguments_runs_with_an_empty_object() {
    let provider = Arc::new(StepProvider::new(vec![
        vec![
            ProviderEvent::ToolCallStart {
                id: "call_1".to_owned(),
                name: "lookup".to_owned(),
            },
            ProviderEvent::ToolCallEnd {
                id: "call_1".to_owned(),
            },
            ProviderEvent::Finish(FinishReason::Completed),
        ],
        vec![ProviderEvent::Finish(FinishReason::Completed)],
    ]));
    let (tool, calls) = RecorderTool::new("lookup");
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
    let provider = Arc::new(StepProvider::new(vec![
        step_one,
        vec![ProviderEvent::Finish(FinishReason::Completed)],
    ]));
    let (tool, calls) = RecorderTool::new("shell");
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");

    let (id, seen) = until_permission(&mut events).await;
    let Some(Event::PermissionRequested {
        call_id,
        tool,
        title,
        args,
        ..
    }) = seen.last()
    else {
        panic!("the drain stops on the request");
    };
    assert_eq!(call_id, "call_1");
    assert_eq!(tool, "shell");
    assert_eq!(title, "shell", "the default title names the tool");
    assert_eq!(*args, serde_json::json!({"key": "a"}));
    assert!(
        calls
            .lock()
            .expect("the call log is never poisoned")
            .is_empty(),
        "nothing runs while the question is open"
    );

    engine
        .send(Command::ReplyPermission {
            id,
            reply: PermissionReply::Once,
        })
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

    let provider = Arc::new(StepProvider::new(vec![
        first_turn,
        done.clone(),
        second_turn,
        done,
    ]));
    let (tool, calls) = RecorderTool::new("shell");
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
        .send(Command::ReplyPermission {
            id,
            reply: PermissionReply::Always,
        })
        .await
        .expect("a reply is always accepted");
    drain(&mut events).await;

    // The answer stuck: the shared rules now allow the tool outright.
    assert_eq!(
        engine
            .permissions()
            .lock()
            .expect("the permission rules are never poisoned")
            .check("shell", &serde_json::json!({"key": "b"})),
        Decision::Allow
    );

    engine
        .send(prompt())
        .await
        .expect("the engine is idle again");
    let second = drain(&mut events).await;
    assert!(
        !second
            .iter()
            .any(|event| matches!(event, Event::PermissionRequested { .. })),
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
    let provider = Arc::new(StepProvider::new(vec![step_one, step_two]));
    let seen_requests = Arc::clone(&provider.seen);
    let (tool, calls) = RecorderTool::new("shell");
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
        .send(Command::ReplyPermission {
            id,
            reply: PermissionReply::Reject,
        })
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
        calls
            .lock()
            .expect("the call log is never poisoned")
            .is_empty(),
        "a rejected call must not run"
    );

    // The model reads the rejection as the call's result on the next request.
    let requests = seen_requests
        .lock()
        .expect("the request log is never poisoned");
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
    let provider = Arc::new(StepProvider::new(vec![
        step_one,
        vec![ProviderEvent::Finish(FinishReason::Completed)],
    ]));
    let (tool, calls) = RecorderTool::new("shell");
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
        .send(Command::CancelTurn)
        .await
        .expect("a waiting engine accepts a cancel");

    let rest = drain(&mut events).await;
    let shapes: Vec<String> = rest.iter().map(shape).collect();
    assert_eq!(
        shapes,
        vec![
            "perm_replied:reject",
            "updated:error:call_1",
            "finished:cancelled",
        ],
        "a cancel answers the open request before the turn closes"
    );
    let Some(Event::PermissionReplied { id: replied, .. }) = rest.first() else {
        panic!("the refusal names the request, got {rest:?}");
    };
    assert_eq!(*replied, id);
    assert!(
        calls
            .lock()
            .expect("the call log is never poisoned")
            .is_empty(),
        "a refused call must not run"
    );

    engine
        .send(prompt())
        .await
        .expect("a cancelled turn leaves the engine idle");
    drain(&mut events).await;
}

#[tokio::test]
async fn a_prompt_is_refused_while_a_permission_waits() {
    let mut step_one = call("call_1", "shell", r#"{"key":"a"}"#);
    step_one.push(ProviderEvent::Finish(FinishReason::Completed));
    let provider = Arc::new(StepProvider::new(vec![
        step_one,
        vec![ProviderEvent::Finish(FinishReason::Completed)],
    ]));
    let (tool, _calls) = RecorderTool::new("shell");
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
        .send(Command::ReplyPermission {
            id,
            reply: PermissionReply::Once,
        })
        .await
        .expect("a reply is always accepted");
    drain(&mut events).await;
}

#[tokio::test]
async fn a_stale_permission_reply_is_ignored() {
    let mut step_one = call("call_1", "shell", r#"{"key":"a"}"#);
    step_one.push(ProviderEvent::Finish(FinishReason::Completed));
    let provider = Arc::new(StepProvider::new(vec![
        step_one,
        vec![ProviderEvent::Finish(FinishReason::Completed)],
    ]));
    let (tool, calls) = RecorderTool::new("shell");
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
        .send(Command::ReplyPermission {
            id,
            reply: PermissionReply::Once,
        })
        .await
        .expect("a reply is always accepted");

    let rest = drain(&mut events).await;
    assert_eq!(
        rest.iter()
            .filter(|event| matches!(event, Event::PermissionReplied { .. }))
            .count(),
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
    let provider = Arc::new(StepProvider::new(vec![step_one, step_two]));
    let seen_requests = Arc::clone(&provider.seen);
    let (tool, calls) = RecorderTool::new("lookup");
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
        seen.iter()
            .map(shape)
            .any(|it| it == "updated:error:call_1"),
        "an unknown tool errors its part, got {seen:?}"
    );
    assert!(
        calls
            .lock()
            .expect("the call log is never poisoned")
            .is_empty(),
        "nothing ran"
    );

    let requests = seen_requests
        .lock()
        .expect("the request log is never poisoned");
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
        ProviderEvent::ToolCallStart {
            id: "call_1".to_owned(),
            name: "lookup".to_owned(),
        },
        ProviderEvent::ToolCallDelta {
            id: "call_1".to_owned(),
            json: "{not json".to_owned(),
        },
        ProviderEvent::ToolCallEnd {
            id: "call_1".to_owned(),
        },
        ProviderEvent::Finish(FinishReason::Completed),
    ];
    let step_two = vec![ProviderEvent::Finish(FinishReason::Completed)];
    let provider = Arc::new(StepProvider::new(vec![step_one, step_two]));
    let seen_requests = Arc::clone(&provider.seen);
    let (tool, calls) = RecorderTool::new("lookup");
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
        calls
            .lock()
            .expect("the call log is never poisoned")
            .is_empty(),
        "a call whose arguments never parsed must not run"
    );

    let requests = seen_requests
        .lock()
        .expect("the request log is never poisoned");
    assert_eq!(requests.len(), 2, "the loop continued past the error");
    let states = tool_states(&requests[1]);
    let Some((_, ToolState::Error { error, input, .. })) = states.first() else {
        panic!("the model should read the failure, got {states:?}");
    };
    assert!(
        error.starts_with(INVALID_PREFIX),
        "the wording is upstream's invalid-tool output, got {error:?}"
    );
    assert_eq!(
        *input,
        serde_json::json!({}),
        "unparseable arguments leave an empty input"
    );
}

#[tokio::test]
async fn a_tool_failure_is_a_result_not_a_turn_abort() {
    let mut step_one = call("call_1", "lookup", r#"{"key":"a"}"#);
    step_one.push(ProviderEvent::Finish(FinishReason::Completed));
    let step_two = vec![ProviderEvent::Finish(FinishReason::Completed)];
    let provider = Arc::new(StepProvider::new(vec![step_one, step_two]));
    let seen_requests = Arc::clone(&provider.seen);
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

    let requests = seen_requests
        .lock()
        .expect("the request log is never poisoned");
    let states = tool_states(&requests[1]);
    let Some((_, ToolState::Error { error, .. })) = states.first() else {
        panic!("the model should read the failure, got {states:?}");
    };
    assert_eq!(
        error, "the index is corrupt",
        "the model reads the tool's own words"
    );
}

#[tokio::test]
async fn cancelling_mid_execution_errors_the_call_and_finishes_cancelled() {
    let mut step_one = call("call_1", "lookup", r#"{"key":"a"}"#);
    step_one.push(ProviderEvent::Finish(FinishReason::Completed));
    let provider = Arc::new(StepProvider::new(vec![
        step_one,
        vec![ProviderEvent::Finish(FinishReason::Completed)],
    ]));
    let (entered, mut wait_entered) = tokio::sync::mpsc::channel(1);
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(vec![Arc::new(StallTool { entered })])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts");
    wait_entered
        .recv()
        .await
        .expect("the tool should start executing");

    engine
        .send(Command::CancelTurn)
        .await
        .expect("an executing engine accepts a cancel");

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
            PartBody::Tool {
                state: ToolState::Error { error, .. },
                ..
            } => Some(error.clone()),
            _ => None,
        },
        _ => None,
    });
    assert_eq!(cancelled.as_deref(), Some("the call was cancelled"));

    engine
        .send(prompt())
        .await
        .expect("a cancelled turn leaves the engine idle");
    drain(&mut events).await;
}

#[tokio::test]
async fn a_provider_failure_mid_loop_strands_the_buffered_call() {
    let step_one = vec![
        ProviderEvent::TextDelta("checking".to_owned()),
        ProviderEvent::ToolCallStart {
            id: "call_1".to_owned(),
            name: "lookup".to_owned(),
        },
        ProviderEvent::ToolCallDelta {
            id: "call_1".to_owned(),
            json: r#"{"key":"a"}"#.to_owned(),
        },
        ProviderEvent::Failed(ProviderError::Transport("connection reset".to_owned())),
    ];
    let provider = Arc::new(StepProvider::new(vec![step_one]));
    let (tool, calls) = RecorderTool::new("lookup");
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
            PartBody::Tool {
                state: ToolState::Error { error, .. },
                ..
            } => Some(error.clone()),
            _ => None,
        },
        _ => None,
    });
    assert_eq!(
        stranded.as_deref(),
        Some("the provider failed before this call could run")
    );
    assert!(
        calls
            .lock()
            .expect("the call log is never poisoned")
            .is_empty(),
        "a stranded call never ran"
    );

    let Some(Event::MessageFinished { reason, error, .. }) = seen.last() else {
        panic!("a turn ends with a finish, got {seen:?}");
    };
    assert_eq!(*reason, FinishReason::Failed);
    assert!(
        error
            .as_deref()
            .is_some_and(|error| error.contains("connection reset")),
        "the failure explains itself, got {error:?}"
    );
}
