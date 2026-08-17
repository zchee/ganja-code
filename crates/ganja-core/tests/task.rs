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
//! these tests. A child's messages never reach the frontend: events name their
//! session now, but the child session is one no frontend can see, and today's
//! consumers apply the whole stream into the conversation they are showing.
//! What crosses over is the child's permission dialogs — re-addressed to the
//! parent's session — and the progress metadata on the parent's tool part,
//! both asserted below.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures::StreamExt as _;
use ganja_core::{
    AgentRegistry, Config, Engine, SessionId, SessionInfo, Storage,
    permission::Permissions,
    protocol::{Command, Event, FinishReason, PartBody, PermissionReply, Role, ToolState, Usage},
    provider::{ChatRequest, Provider},
    storage,
    tool::{Registry, Tool, ToolCtx, ToolError, ToolOutput},
};
use ganja_testkit::{BlockingTool, ScriptedProvider, drain_answering, says, tool_call};
use serde_json::json;

/// Answers with a canned output and records that it ran.
///
/// Kept local rather than folded into `ganja_testkit::RecorderTool`: its
/// handle is a call *count*, not a log of arguments, which every assertion
/// against it (`assert_eq!(*fetches.lock()…, 1)`) is written against — a
/// shared type would have to change that handle's shape, and the assertions
/// with it.
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
    fn id(&self) -> &str {
        self.id
    }

    fn description(&self) -> &str {
        "answers with a canned output"
    }

    fn schema(&self) -> schemars::Schema {
        ganja_testkit::placeholder_schema()
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

/// A step that delegates to `subagent`.
fn delegates(subagent: &str) -> Vec<ganja_core::provider::ProviderEvent> {
    tool_call(
        "task",
        json!({
            "description": "find the thing",
            "prompt": "go and find the thing",
            "subagent_type": subagent,
        }),
    )
}

/// An engine over `provider` offering `tools`, running the builtin agents.
fn engine(provider: Arc<dyn Provider>, tools: Vec<Arc<dyn Tool>>, config: &Config) -> Engine {
    Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(tools)),
        Permissions::default(),
    )
    .with_agents(ganja_testkit::agent_registry(config))
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

/// Everything of an event a frontend would render, minus the parent's own
/// `task` part.
///
/// That one part is the whole of what a child is allowed to reach the stream
/// through: it carries `{current_tool, toolcalls}` while the child works and
/// the delegated answer once it stops. Every other rendering is the parent's
/// own transcript, and a child's words appearing in one is the leak.
fn published(event: &Event) -> Vec<String> {
    fn render(part: &ganja_core::protocol::Part) -> Option<String> {
        let delegated = matches!(&part.body, PartBody::Tool { tool, .. } if tool == "task");

        (!delegated).then(|| format!("{:?}", part.body))
    }

    match event {
        Event::MessageStarted {
            session_id: _,
            message,
        } => message.parts.iter().filter_map(render).collect(),
        Event::PartStarted { part, .. } | Event::PartUpdated { part, .. } => {
            render(part).into_iter().collect()
        }
        // A delta names a part id and not a part, so none of them is exempt.
        // The parent's task part never takes one — deltas carry streamed text —
        // so a delta bearing the child's words is a leak by construction.
        Event::PartDelta { delta, .. } => vec![delta.clone()],
        _ => Vec::new(),
    }
}

/// The parent runs, delegates, the child runs its own loop, and what comes back
/// is the child's last words — wrapped so the parent model can tell a delegated
/// answer from its own.
#[tokio::test]
async fn a_task_call_runs_a_child_loop_and_hands_back_its_last_words() {
    let (provider, requests) = ScriptedProvider::new(vec![
        delegates("general"),
        // The child's own turn: one tool call, then its answer.
        tool_call("webfetch", json!({ "url": "https://example.test" })),
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
            skills: Vec::new(),
            peers: Vec::new(),
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
    let id = output
        .strip_prefix("<task id=\"")
        .and_then(|rest| rest.split_once('"'))
        .map(|(id, _)| id)
        .expect("the wrapper names an id");
    assert!(
        ganja_protocol::is_uuidv7(id),
        "the delegated child's own session id, a bare UUIDv7: {id}"
    );
    assert!(
        output.contains("\" state=\"completed\""),
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
            Event::MessageStarted { session_id: _, message } if message
                .parts
                .iter()
                .any(|part| part.as_text() == Some("the thing is in src/main.rs"))
        )),
        "the child's own transcript never reaches the frontend: {seen:?}"
    );
}

/// A child's turn is a turn nobody subscribed to.
///
/// Events name their session now, so a frontend *could* tell a child's
/// messages apart — but today's consumers file everything the stream carries
/// under the conversation they are showing, and the child session is one no
/// frontend can see. Upstream publishes and lets its frontend filter by
/// session id; this build keeps the child off the stream until a consumer
/// exists that asked to filter (deviation:
/// `subagent-events-stay-off-the-stream`).
///
/// What may cross is named exactly: the child's permission dialogs, and the
/// parent's own `task` part carrying `{current_tool, toolcalls}` and, at the
/// end, the answer it delegated for. The sentinel below is a phrase only the
/// child ever utters, so any other rendering carrying it is a leak — and the
/// property is what a served engine would stand on, where a subscriber is on
/// another process and cannot be asked to sort the two transcripts out.
#[tokio::test]
async fn a_childs_own_messages_never_reach_the_subscribed_stream() {
    /// Said by the child and by nothing else in this script.
    const CHILD_ONLY: &str = "the child alone utters this sentence";

    let (provider, requests) = ScriptedProvider::new(vec![
        delegates("general"),
        // The child's own turn: one tool call, then its answer.
        tool_call("webfetch", json!({ "url": "https://example.test" })),
        says(CHILD_ONLY),
        says("the parent speaks for itself"),
    ]);
    let (webfetch, fetches) = Canned::new("webfetch");
    let engine = engine(provider, vec![webfetch], &Config::default());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "delegate it".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain_answering(&engine, &mut events, PermissionReply::Once).await;

    assert_eq!(
        requests
            .lock()
            .expect("the request log is never poisoned")
            .len(),
        4,
        "the child has to have really run, or there is nothing to leak"
    );
    assert_eq!(
        *fetches.lock().expect("the call log"),
        1,
        "and to have executed a tool call of its own"
    );

    // The one rendering the sentinel is *supposed* to reach, which is also what
    // stops the sweep below from passing because the child never said it.
    let ToolState::Completed { output, .. } = task_part(&seen) else {
        panic!("the delegated call completed");
    };
    assert!(
        output.contains(CHILD_ONLY),
        "the parent's own task part carries the delegated answer: {output}"
    );

    for event in &seen {
        for rendering in published(event) {
            assert!(
                !rendering.contains(CHILD_ONLY),
                "the child's own words reached the stream: {rendering}"
            );
        }
    }

    let roles: Vec<Role> = seen
        .iter()
        .filter_map(|event| match event {
            Event::MessageStarted {
                session_id: _,
                message,
            } => Some(message.role),
            _ => None,
        })
        .collect();
    assert_eq!(
        roles,
        vec![Role::User, Role::Assistant],
        "one prompt and one assistant turn — the child's own pair is not on the stream: {seen:?}"
    );
}

/// The parent's inline row is built from the tool part alone, so the child's
/// progress has to arrive as metadata on that part and as nothing else.
#[tokio::test]
async fn a_running_child_reports_its_progress_on_the_parents_tool_part() {
    let (provider, _) = ScriptedProvider::new(vec![
        delegates("general"),
        tool_call("webfetch", json!({ "url": "https://example.test" })),
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
            skills: Vec::new(),
            peers: Vec::new(),
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
    let (provider, requests) = ScriptedProvider::new(vec![
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
            skills: Vec::new(),
            peers: Vec::new(),
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
    let (provider, _) = ScriptedProvider::new(vec![
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
            skills: Vec::new(),
            peers: Vec::new(),
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
        .gate("task", &json!({}))
        .rules;
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
            skills: Vec::new(),
            peers: Vec::new(),
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
    let (provider, _) = ScriptedProvider::new(vec![
        // The parent's own call, answered "always".
        tool_call("webfetch", json!({ "url": "https://example.test" })),
        says("fetched it"),
        // A later turn delegates, and the child tries the same call.
        delegates("general"),
        tool_call("webfetch", json!({ "url": "https://example.test" })),
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
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain_answering(&engine, &mut events, PermissionReply::Always).await;

    let stored = engine
        .permissions()
        .lock()
        .expect("the rules are never poisoned")
        .gate("webfetch", &json!({}))
        .rules;
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
            skills: Vec::new(),
            peers: Vec::new(),
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

/// A child's permission dialog crosses to the parent's stream **re-addressed**:
/// it carries the parent's session id, not the child's. The child session is
/// invisible to every frontend — never seeded, never listed — so a dialog
/// naming it would hand a session-filtering client a question it could not
/// attribute; the parent's is the conversation whose turn is waiting on the
/// answer.
#[tokio::test]
async fn a_crossing_permission_dialog_carries_the_parents_session_id() {
    let (provider, _) = ScriptedProvider::new(vec![
        delegates("general"),
        tool_call("webfetch", json!({ "url": "https://example.test" })),
        says("the child is done"),
        says("so is the parent"),
    ]);
    let (webfetch, _fetches) = Canned::new("webfetch");
    let engine = engine(provider, vec![webfetch], &Config::default());
    let parent = engine.session_id();
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "delegate it".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain_answering(&engine, &mut events, PermissionReply::Once).await;

    let crossed: Vec<&Event> = seen
        .iter()
        .filter(
            |event| matches!(event, Event::PermissionRequested { tool, .. } if tool == "webfetch"),
        )
        .collect();
    assert_eq!(
        crossed.len(),
        1,
        "the child's own call asked exactly once: {seen:?}"
    );
    assert_eq!(
        crossed[0].session_id(),
        &parent,
        "the crossing dialog is addressed to the delegating session"
    );

    // And not just the request: every dialog event on this stream — the
    // parent's own `task` ask, the crossing ask, and both replies — belongs
    // to the parent's session, because the stream never shows another one.
    for event in seen.iter().filter(|event| {
        matches!(
            event,
            Event::PermissionRequested { .. } | Event::PermissionReplied { .. }
        )
    }) {
        assert_eq!(
            event.session_id(),
            &parent,
            "a dialog event named a session the stream cannot show: {event:?}"
        );
    }
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
    let agents = AgentRegistry::from_config(&config).expect("the fixture resolves an agent");
    assert!(
        !agents
            .get("general")
            .expect("general is builtin")
            .rules
            .iter()
            .any(|rule| rule.permission == "webfetch"),
        "the subagent's own rules must say nothing about it, or this proves nothing"
    );

    let (provider, requests) = ScriptedProvider::new(vec![
        delegates("general"),
        // The child tries the tool its parent may not use.
        tool_call("webfetch", json!({ "url": "https://example.test" })),
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
            skills: Vec::new(),
            peers: Vec::new(),
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
    let (provider, _) = ScriptedProvider::new(vec![
        delegates("general"),
        tool_call("blocking", json!({})),
        says("unreachable"),
    ]);
    let engine = engine(
        provider,
        vec![BlockingTool::new(
            "blocking",
            "blocks until it is cancelled",
        )],
        &Config::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "delegate it".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
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
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("a cancelled turn leaves the engine idle");
}

/// An agent that does not exist is a tool result like any other: the model
/// reads why and the turn carries on.
#[tokio::test]
async fn delegating_to_an_agent_that_does_not_exist_is_information_not_an_abort() {
    let (provider, requests) =
        ScriptedProvider::new(vec![delegates("nope"), says("I will do it myself then")]);
    let engine = engine(provider, Vec::new(), &Config::default());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "delegate it".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
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
    let (provider, _) = ScriptedProvider::new(vec![delegates("build"), says("fine, myself then")]);
    let engine = engine(provider, Vec::new(), &Config::default());
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "delegate it".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain_answering(&engine, &mut events, PermissionReply::Once).await;

    let ToolState::Error { error, .. } = task_part(&seen) else {
        panic!("a primary agent is refused");
    };
    assert_eq!(error, "Unknown agent type: build is not a valid agent type");
}

/// A delegated conversation is a session of its own, and the record it leaves
/// says whose errand it was (**R7**).
///
/// Read back off the disk rather than from the engine, because the field is
/// there for the next process: a picker listing roots, and a person asking
/// later what a stored transcript was for, both have only the file to go on.
///
/// The parent session is seeded already titled, so the title machinery cannot
/// spend a scripted answer and shift the turn boundaries this depends on.
#[tokio::test]
async fn a_delegated_child_is_stored_as_a_session_of_its_own_naming_its_parent() {
    let directory = tempfile::TempDir::new().expect("a temporary directory is creatable");
    let storage = Storage::open(directory.path().join("storage"));
    // Fixed rather than read from the clock: nothing here compares timestamps.
    let created = 1;
    let parent = SessionId::ascending();
    storage
        .save_info(&SessionInfo {
            id: parent.clone(),
            version: storage::VERSION,
            title: Some("seeded".to_owned()),
            created,
            updated: created,
            usage: Usage::default(),
            context_tokens: 0,
            summary: None,
            agent: None,
            model: None,
            effort: None,
            activated_tools: std::collections::BTreeSet::new(),
            parent: None,
            revert: None,
        })
        .expect("the seeded record writes");

    let (provider, _) = ScriptedProvider::new(vec![
        delegates("general"),
        says("the child's own answer"),
        says("and the parent signs off"),
    ]);
    let engine = Engine::persistent(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        Storage::open(directory.path().join("storage")),
    )
    .with_agents(ganja_testkit::agent_registry(&Config::default()));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine.resume(&parent).await.expect("the session loads");

    engine
        .send(Command::SendPrompt {
            text: "delegate it".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain_answering(&engine, &mut events, PermissionReply::Once).await;

    let child = storage
        .list_sessions()
        .expect("the store lists what it holds")
        .into_iter()
        .find(|info| info.id != parent)
        .expect("the child got a record of its own")
        .id;

    let stored = storage
        .load_info(&child)
        .expect("the child's record reads back")
        .expect("and it is there");
    assert_eq!(
        stored.parent,
        Some(parent.clone()),
        "the stored child names the conversation that delegated to it"
    );
    assert_eq!(
        stored.agent.as_deref(),
        Some("general"),
        "and the subagent it ran as"
    );

    let said: Vec<String> = storage
        .load_transcript(&child)
        .expect("the child's transcript reads back")
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter_map(|part| match &part.body {
            PartBody::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        said.iter().any(|text| text == "the child's own answer"),
        "the record is the child's own conversation, not an empty stub: {said:?}"
    );
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
    let (recorder, _) = ScriptedProvider::new(vec![says("noted")]);
    let provider: Arc<dyn Provider> = Arc::clone(&recorder) as Arc<dyn Provider>;
    let engine = Engine::persistent(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        Storage::open(directory.path().join("storage")),
    )
    .with_agents(ganja_testkit::agent_registry(&Config::default()));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "remember this".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain_answering(&engine, &mut events, PermissionReply::Once).await;

    let parent = engine
        .current_session()
        .expect("the first prompt minted a session")
        .id;

    // The model hands back the id of the conversation it is having.
    recorder.push(tool_call(
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
            skills: Vec::new(),
            peers: Vec::new(),
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
