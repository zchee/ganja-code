//! The task tool end to end: a call spawns a **real** second agent loop, and
//! the parent reads back one answer.
//!
//! Everything here is driven from **one ordered script**. The child asks the
//! same provider the parent does, so the entries pop in the order the two loops
//! actually run: parent, child, child, parent. That ordering is the proof the
//! child is a real loop rather than a canned string — a fake would never
//! consume a script entry.
//!
//! What is *not* observable from the parent's event stream is the point of half
//! these tests. A child's messages never reach the frontend, because nothing on
//! this wire carries a session id and a frontend applying the stream would file
//! them under the parent's own transcript. What crosses over is the child's
//! permission dialogs and the progress metadata on the parent's tool part —
//! both asserted below.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures::{
    StreamExt as _,
    stream::{self, BoxStream},
};
use ganja_core::{
    AgentRegistry, Command, Config, Engine, Event, FinishReason, PartBody, PermissionReply,
    Permissions, Registry, Storage, Tool, ToolCtx, ToolError, ToolOutput, ToolState,
    provider::{ChatRequest, Provider, ProviderError, ProviderEvent},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Answers each request with the next script, and records what it was asked.
///
/// Shared by the parent loop and every child loop, which is what makes one
/// ordered script able to drive both.
struct Recorder {
    scripts: Mutex<VecDeque<Vec<ProviderEvent>>>,
    seen: Arc<Mutex<Vec<ChatRequest>>>,
}

impl Recorder {
    fn new(scripts: Vec<Vec<ProviderEvent>>) -> (Arc<Self>, Arc<Mutex<Vec<ChatRequest>>>) {
        let seen: Arc<Mutex<Vec<ChatRequest>>> = Arc::default();
        (
            Arc::new(Self {
                scripts: Mutex::new(scripts.into()),
                seen: Arc::clone(&seen),
            }),
            seen,
        )
    }

    /// Adds a step to the end of the script, for a test whose next answer
    /// depends on what the first turn produced.
    fn push(&self, script: Vec<ProviderEvent>) {
        self.scripts
            .lock()
            .expect("the scripts are never poisoned")
            .push_back(script);
    }
}

#[async_trait]
impl Provider for Recorder {
    fn id(&self) -> &str {
        "recorder"
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
            .unwrap_or_else(|| vec![ProviderEvent::Finish(FinishReason::Completed)]);

        Ok(stream::iter(script).boxed())
    }
}

/// Arguments the test tools nominally take.
#[derive(schemars::JsonSchema)]
#[allow(dead_code)]
struct Args {
    url: Option<String>,
}

/// Answers with a canned output and records that it ran.
struct Canned {
    id: &'static str,
    calls: Arc<Mutex<usize>>,
}

impl Canned {
    fn new(id: &'static str) -> (Arc<Self>, Arc<Mutex<usize>>) {
        let calls = Arc::new(Mutex::new(0));
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
impl Tool for Canned {
    fn id(&self) -> &'static str {
        self.id
    }

    fn description(&self) -> &str {
        "answers with a canned output"
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    async fn run(&self, _args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        *self.calls.lock().expect("the call log is never poisoned") += 1;

        Ok(ToolOutput {
            title: self.id.to_owned(),
            output: "canned".to_owned(),
            metadata: json!({}),
        })
    }
}

/// Blocks until the turn is cancelled, so a test can watch a cancel travel
/// from the parent's token down into the child's tool.
struct Blocking;

#[async_trait]
impl Tool for Blocking {
    fn id(&self) -> &'static str {
        "blocking"
    }

    fn description(&self) -> &str {
        "blocks until it is cancelled"
    }

    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(Args)
    }

    async fn run(&self, _args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        ctx.cancel.cancelled().await;

        Err(ToolError::Cancelled)
    }
}

/// A step that calls `tool` with `args` and stops.
fn calls(tool: &str, args: serde_json::Value) -> Vec<ProviderEvent> {
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

/// A step that says `text` and stops.
fn says(text: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::TextDelta(text.to_owned()),
        ProviderEvent::Finish(FinishReason::Completed),
    ]
}

/// A step that delegates to `subagent`.
fn delegates(subagent: &str) -> Vec<ProviderEvent> {
    calls(
        "task",
        json!({
            "description": "find the thing",
            "prompt": "go and find the thing",
            "subagent_type": subagent,
        }),
    )
}

/// Drains until the turn finishes, answering every permission with `reply`.
async fn drain_answering(
    engine: &Engine,
    events: &mut BoxStream<'static, Event>,
    reply: PermissionReply,
) -> Vec<Event> {
    let mut seen = Vec::new();

    loop {
        let Some(event) = events.next().await else {
            return seen;
        };
        if let Event::PermissionRequested { id, .. } = &event {
            engine
                .send(Command::ReplyPermission {
                    id: id.clone(),
                    reply,
                })
                .await
                .expect("a reply is never refused");
        }
        let finished = matches!(event, Event::MessageFinished { .. });
        seen.push(event);

        if finished {
            return seen;
        }
    }
}

fn agents(config: &Config) -> Arc<AgentRegistry> {
    Arc::new(AgentRegistry::build(config).expect("the fixture config resolves an agent"))
}

/// An engine over `provider` offering `tools`, running the builtin agents.
fn engine(provider: Arc<dyn Provider>, tools: Vec<Arc<dyn Tool>>, config: &Config) -> Engine {
    Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(tools)),
        Permissions::default(),
    )
    .with_agents(agents(config))
}

/// The task tool part as it finally stood, whatever it finally was.
fn task_part(seen: &[Event]) -> ToolState {
    seen.iter()
        .rev()
        .find_map(|event| match event {
            Event::PartUpdated { part, .. } => match &part.body {
                PartBody::Tool { tool, state, .. } if tool == "task" => Some(state.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("the turn made a task call")
}

/// The parent runs, delegates, the child runs its own loop, and what comes back
/// is the child's last words — wrapped so the parent model can tell a delegated
/// answer from its own.
#[tokio::test]
async fn a_task_call_runs_a_child_loop_and_hands_back_its_last_words() {
    let (provider, requests) = Recorder::new(vec![
        delegates("general"),
        // The child's own turn: one tool call, then its answer.
        calls("webfetch", json!({ "url": "https://example.test" })),
        says("the thing is in src/main.rs"),
        says("thanks, it is in src/main.rs"),
    ]);
    let (webfetch, fetches) = Canned::new("webfetch");
    let engine = engine(provider, vec![webfetch], &Config::default());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "where is the thing".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain_answering(&engine, &mut events, PermissionReply::Once).await;

    let requests = requests.lock().expect("the request log is never poisoned");
    assert_eq!(
        requests.len(),
        4,
        "one ordered script drives both loops: parent, child, child, parent"
    );
    assert_eq!(
        *fetches.lock().expect("the call log"),
        1,
        "the child really executed its own tool call"
    );

    let ToolState::Completed { output, .. } = task_part(&seen) else {
        panic!("the delegated call completed");
    };
    assert!(
        output.starts_with("<task id=\"ses_") && output.contains("state=\"completed\""),
        "a delegated answer is wrapped so the model can tell it apart: {output}"
    );
    assert!(
        output.contains("<task_result>\nthe thing is in src/main.rs\n</task_result>"),
        "and what is wrapped is the child's LAST text part: {output}"
    );
    assert!(
        !output.contains("canned"),
        "the child's tool output is the child's business: {output}"
    );

    // The parent's fourth request is the one that reads the delegated answer.
    let last = requests.last().expect("the parent asked again");
    assert!(
        last.messages.iter().any(|message| message.parts.iter().any(
            |part| matches!(&part.body, PartBody::Tool { state: ToolState::Completed { output, .. }, .. }
                if output.contains("<task_result>"))
        )),
        "the parent's next request carries the wrapped result"
    );

    assert!(
        !seen.iter().any(|event| matches!(
            event,
            Event::MessageStarted { message } if message
                .parts
                .iter()
                .any(|part| part.as_text() == Some("the thing is in src/main.rs"))
        )),
        "the child's own transcript never reaches the frontend: {seen:?}"
    );
}

/// The parent's inline row is built from the tool part alone, so the child's
/// progress has to arrive as metadata on that part and as nothing else.
#[tokio::test]
async fn a_running_child_reports_its_progress_on_the_parents_tool_part() {
    let (provider, _) = Recorder::new(vec![
        delegates("general"),
        calls("webfetch", json!({ "url": "https://example.test" })),
        says("done"),
        says("thanks"),
    ]);
    let (webfetch, _) = Canned::new("webfetch");
    let engine = engine(provider, vec![webfetch], &Config::default());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "delegate it".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain_answering(&engine, &mut events, PermissionReply::Once).await;

    let progress: Vec<serde_json::Value> = seen
        .iter()
        .filter_map(|event| match event {
            Event::PartUpdated { part, .. } => match &part.body {
                PartBody::Tool {
                    tool,
                    state: ToolState::Running { metadata, .. },
                    ..
                } if tool == "task" && !metadata.is_null() => Some(metadata.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();

    assert!(
        !progress.is_empty(),
        "the child's work has to show somewhere: {seen:?}"
    );
    let last = progress.last().expect("at least one update");
    assert_eq!(last["toolcalls"], json!(1), "one child call was counted");
    assert_eq!(
        last["current_tool"],
        json!("webfetch"),
        "and the row names what the child was running: {last}"
    );

    assert!(
        !seen.iter().any(|event| matches!(
            event,
            Event::PartStarted { part, .. } if matches!(
                &part.body,
                PartBody::Tool { tool, .. } if tool == "webfetch"
            )
        )),
        "no new event variant and no child part: the row is the metadata"
    );
}

/// One level, and it is enforced by absence rather than by refusal: a subagent
/// is never offered the tool that would let it delegate again (**D9**).
#[tokio::test]
async fn a_subagent_is_never_offered_the_tool_that_spawned_it() {
    let (provider, requests) = Recorder::new(vec![
        delegates("general"),
        says("nothing to delegate"),
        says("thanks"),
    ]);
    let (webfetch, _) = Canned::new("webfetch");
    let engine = engine(provider, vec![webfetch], &Config::default());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "delegate it".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain_answering(&engine, &mut events, PermissionReply::Once).await;

    let requests = requests.lock().expect("the request log is never poisoned");
    let offered = |request: &ChatRequest| -> Vec<String> {
        request
            .tools
            .iter()
            .map(|definition| definition.name.clone())
            .collect()
    };

    assert!(
        offered(&requests[0]).contains(&"task".to_owned()),
        "the parent may delegate: {:?}",
        offered(&requests[0])
    );
    assert_eq!(
        offered(&requests[1]),
        vec!["webfetch".to_owned()],
        "the child's registry has no task tool at all"
    );
}

/// The dialog names the subagent, because that is what an answer is about — and
/// an "always" is remembered for the tool as a whole, which is upstream's
/// `always: ["*"]`.
#[tokio::test]
async fn delegating_asks_about_the_named_subagent_and_an_always_covers_the_tool() {
    let (provider, _) = Recorder::new(vec![
        delegates("explore"),
        says("found it"),
        says("thanks"),
        delegates("general"),
        says("found it again"),
        says("thanks again"),
    ]);
    let engine = engine(provider, Vec::new(), &Config::default());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "delegate it".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain_answering(&engine, &mut events, PermissionReply::Always).await;

    let asked: Vec<(String, serde_json::Value)> = seen
        .iter()
        .filter_map(|event| match event {
            Event::PermissionRequested { tool, args, .. } => {
                Some((tool.clone(), args["subagent_type"].clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        asked,
        vec![("task".to_owned(), json!("explore"))],
        "the question is which subagent, not which tool"
    );

    let stored = engine
        .permissions()
        .lock()
        .expect("the rules are never poisoned")
        .relevant("task");
    assert!(
        stored
            .iter()
            .any(|rule| rule.permission == "task" && rule.pattern == "*"),
        "an always answer covers the tool: {stored:?}"
    );

    // A second delegation — to a different subagent — now runs unasked.
    engine
        .send(Command::SendPrompt {
            text: "delegate again".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("a finished turn leaves the engine idle");
    let seen = drain_answering(&engine, &mut events, PermissionReply::Once).await;
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, Event::PermissionRequested { .. })),
        "the remembered answer covers it: {seen:?}"
    );
}

/// An "always" is an answer about the turn the person was watching. It must
/// not travel down to a subagent: the whole point of delegating is that nobody
/// is watching the child, and a stored allow reaching it would turn one
/// supervised "yes" into standing permission for every later delegation.
///
/// The tool here is one the child's own agent says nothing about, so the only
/// thing that could authorize the child's call is the answer given about the
/// parent's.
#[tokio::test]
async fn an_always_the_parent_was_given_does_not_authorize_the_child() {
    let (provider, _) = Recorder::new(vec![
        // The parent's own call, answered "always".
        calls("webfetch", json!({ "url": "https://example.test" })),
        says("fetched it"),
        // A later turn delegates, and the child tries the same call.
        delegates("general"),
        calls("webfetch", json!({ "url": "https://example.test" })),
        says("the child is done"),
        says("so is the parent"),
    ]);
    let (webfetch, fetches) = Canned::new("webfetch");
    let engine = engine(provider, vec![webfetch], &Config::default());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "fetch it".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain_answering(&engine, &mut events, PermissionReply::Always).await;

    let stored = engine
        .permissions()
        .lock()
        .expect("the rules are never poisoned")
        .relevant("webfetch");
    assert!(
        stored
            .iter()
            .any(|rule| rule.permission == "webfetch" && rule.pattern == "*"),
        "the parent really is holding an always-allow, or this proves nothing: {stored:?}"
    );
    assert_eq!(
        *fetches.lock().expect("the call log"),
        1,
        "the parent's own call ran"
    );

    engine
        .send(Command::SendPrompt {
            text: "delegate it".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("a finished turn leaves the engine idle");
    let seen = drain_answering(&engine, &mut events, PermissionReply::Once).await;

    let asked: Vec<String> = seen
        .iter()
        .filter_map(|event| match event {
            Event::PermissionRequested { tool, .. } => Some(tool.clone()),
            _ => None,
        })
        .collect();
    assert!(
        asked.iter().any(|tool| tool == "webfetch"),
        "the child's call had to be asked about on its own: {asked:?}"
    );
}

/// A refusal the parent session is under has to reach what the parent
/// delegates. Otherwise "take this tool away" would mean "take it away unless
/// you ask somebody else to use it".
///
/// The rule is deliberately written on the **parent agent alone**, not globally:
/// a global `permission` block reaches every agent's own ruleset, so the child
/// would deny the call under its own rules and the test would prove nothing
/// about inheritance.
#[tokio::test]
async fn a_refusal_the_parent_is_under_reaches_the_child() {
    let config: Config = serde_json::from_value(
        json!({ "agent": { "build": { "permission": { "webfetch": "deny" } } } }),
    )
    .expect("the fixture is a config");
    let agents = AgentRegistry::build(&config).expect("the fixture resolves an agent");
    assert!(
        !agents
            .get("general")
            .expect("general is builtin")
            .rules
            .iter()
            .any(|rule| rule.permission == "webfetch"),
        "the subagent's own rules must say nothing about it, or this proves nothing"
    );

    let (provider, requests) = Recorder::new(vec![
        delegates("general"),
        // The child tries the tool its parent may not use.
        calls("webfetch", json!({ "url": "https://example.test" })),
        says("I could not fetch it"),
        says("neither could I"),
    ]);
    let (webfetch, fetches) = Canned::new("webfetch");
    let engine = engine(provider, vec![webfetch], &config);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "delegate it".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain_answering(&engine, &mut events, PermissionReply::Once).await;

    assert_eq!(
        *fetches.lock().expect("the call log"),
        0,
        "the child's call must never have run"
    );

    // The child's own next request is where its refusal is visible: its events
    // never reach the frontend, but the request it built from them does.
    let requests = requests.lock().expect("the request log is never poisoned");
    let refusal = requests[2]
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .find_map(|part| match &part.body {
            PartBody::Tool {
                state: ToolState::Error { error, .. },
                ..
            } => Some(error.clone()),
            _ => None,
        })
        .expect("the child read a refusal");
    assert!(
        refusal.contains("specified a rule which prevents you"),
        "and it is a rule refusing, not a dialog: {refusal}"
    );
}

/// Cancelling the parent turn ends the child promptly: the child's token is a
/// child of the parent call's, so one cancel travels the whole way down.
#[tokio::test]
async fn cancelling_the_parent_turn_ends_the_child_promptly() {
    let (provider, _) = Recorder::new(vec![
        delegates("general"),
        calls("blocking", json!({})),
        says("unreachable"),
    ]);
    let engine = engine(provider, vec![Arc::new(Blocking)], &Config::default());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "delegate it".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");

    // Wait until the child has actually started its blocking call, which is
    // exactly when the parent's part first reports progress.
    let mut seen = Vec::new();
    loop {
        let event = events.next().await.expect("the stream is live");
        if let Event::PermissionRequested { id, .. } = &event {
            engine
                .send(Command::ReplyPermission {
                    id: id.clone(),
                    reply: PermissionReply::Once,
                })
                .await
                .expect("a reply is never refused");
        }
        let started = matches!(
            &event,
            Event::PartUpdated { part, .. } if matches!(
                &part.body,
                PartBody::Tool { tool, state: ToolState::Running { metadata, .. }, .. }
                    if tool == "task" && metadata["toolcalls"] == json!(1)
            )
        );
        seen.push(event);
        if started {
            break;
        }
    }

    let started = Instant::now();
    engine
        .send(Command::CancelTurn)
        .await
        .expect("a cancel is never refused");

    let finished = loop {
        let event = events.next().await.expect("a turn always finishes");
        if let Event::MessageFinished { reason, .. } = event {
            break reason;
        }
    };

    assert_eq!(finished, FinishReason::Cancelled);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the cancel took {:?} to reach the child",
        started.elapsed()
    );

    engine
        .send(Command::SendPrompt {
            text: "again".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("a cancelled turn leaves the engine idle");
}

/// An agent that does not exist is a tool result like any other: the model
/// reads why and the turn carries on.
#[tokio::test]
async fn delegating_to_an_agent_that_does_not_exist_is_information_not_an_abort() {
    let (provider, requests) =
        Recorder::new(vec![delegates("nope"), says("I will do it myself then")]);
    let engine = engine(provider, Vec::new(), &Config::default());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "delegate it".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain_answering(&engine, &mut events, PermissionReply::Once).await;

    let ToolState::Error { error, .. } = task_part(&seen) else {
        panic!("an unknown agent fails the call");
    };
    assert_eq!(error, "Unknown agent type: nope is not a valid agent type");

    assert_eq!(
        requests.lock().expect("the request log").len(),
        2,
        "and the turn asked again rather than ending"
    );
    let Some(Event::MessageFinished { reason, .. }) = seen.last() else {
        panic!("a turn always finishes");
    };
    assert_eq!(*reason, FinishReason::Completed);
}

/// A *primary* agent is not a subagent, and running one unattended is the one
/// thing subagent mode exists to prevent (deviation:
/// task-spawns-subagents-only).
#[tokio::test]
async fn a_primary_agent_may_not_be_run_as_a_subagent() {
    let (provider, _) = Recorder::new(vec![delegates("build"), says("fine, myself then")]);
    let engine = engine(provider, Vec::new(), &Config::default());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "delegate it".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain_answering(&engine, &mut events, PermissionReply::Once).await;

    let ToolState::Error { error, .. } = task_part(&seen) else {
        panic!("a primary agent is refused");
    };
    assert_eq!(error, "Unknown agent type: build is not a valid agent type");
}

/// A `task_id` names a session an earlier call left behind. A **root** id is
/// not one — least of all the live conversation's own — and running a child
/// into it would interleave two transcripts in one record. An id that answers
/// to nothing usable starts a fresh child, which is what an unanswerable one
/// already did.
#[tokio::test]
async fn a_task_id_naming_a_root_session_starts_a_fresh_child_instead() {
    let directory = tempfile::TempDir::new().expect("a temporary directory is creatable");
    let storage = Storage::open(directory.path().join("storage"));
    let (recorder, _) = Recorder::new(vec![says("noted")]);
    let provider: Arc<dyn Provider> = Arc::clone(&recorder) as Arc<dyn Provider>;
    let engine = Engine::persistent(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        Storage::open(directory.path().join("storage")),
    )
    .with_agents(agents(&Config::default()));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "remember this".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain_answering(&engine, &mut events, PermissionReply::Once).await;

    let parent = engine
        .current_session()
        .expect("the first prompt minted a session")
        .id;

    // The model hands back the id of the conversation it is having.
    recorder.push(calls(
        "task",
        json!({
            "description": "find the thing",
            "prompt": "go and find the thing",
            "subagent_type": "general",
            "task_id": parent.as_str(),
        }),
    ));
    recorder.push(says("the child spoke here"));
    recorder.push(says("and the parent finished"));

    engine
        .send(Command::SendPrompt {
            text: "delegate it".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("a finished turn leaves the engine idle");
    drain_answering(&engine, &mut events, PermissionReply::Once).await;

    let said: Vec<String> = storage
        .load_transcript(&parent)
        .expect("the parent record reads back")
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter_map(|part| match &part.body {
            PartBody::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        !said.iter().any(|text| text == "the child spoke here"),
        "the child wrote into the conversation it was told to continue: {said:?}"
    );

    let sessions = storage
        .list_sessions()
        .expect("the store lists what it holds");
    assert!(
        sessions
            .iter()
            .any(|info| info.parent.as_ref() == Some(&parent)),
        "the child got a session of its own, naming the turn that spawned it: {sessions:?}"
    );
}
