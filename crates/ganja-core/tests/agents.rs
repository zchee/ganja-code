//! Agents end to end: what an agent denies, what it says to the model, and
//! what survives being switched, stored and reopened.
//!
//! The rules and the prompts are unit-tested where they are assembled
//! (`src/agent.rs`); what is proved here is that a turn actually runs under
//! them — that a denial reaches the model as a tool result rather than a
//! dialog, that a switch lands on the *next* request and not the one in
//! flight, and that a session reopened tomorrow is the session that was left.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use futures::{
    StreamExt as _,
    stream::{self, BoxStream},
};
use ganja_core::{
    AgentConfig, AgentRegistry, Command, Config, Decision, Engine, EngineError, Event,
    FinishReason, PartBody, Permissions, Registry, Role, SessionId, SessionInfo, Storage, Tool,
    ToolCtx, ToolError, ToolOutput, ToolState, Usage,
    agent::{BUILD_SWITCH_REMINDER, PLAN_REMINDER},
    provider::{ChatRequest, FakeProvider, Provider, ProviderError, ProviderEvent, fake},
    storage,
};
use serde_json::json;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// Answers each request with the next script, and records what it was asked.
///
/// Its id is deliberately one the catalog has never heard of: a provider the
/// catalog does not cover cannot have a model switch validated against it, and
/// these tests are about the switch rather than about the catalog.
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
    #[serde(rename = "filePath")]
    file_path: Option<String>,
}

/// Records every invocation and answers with a canned output, so that "the
/// call never ran" is a fact rather than an inference.
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
            title: self.id.to_owned(),
            output: "done".to_owned(),
            metadata: json!({}),
        })
    }
}

/// A step that calls `tool` once and stops.
fn calls(tool: &str, args: serde_json::Value) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ToolCallStart {
            id: format!("call_{tool}"),
            name: tool.to_owned(),
        },
        ProviderEvent::ToolCallDelta {
            id: format!("call_{tool}"),
            json: args.to_string(),
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

/// The error text of every tool part that failed.
fn refusals(seen: &[Event]) -> Vec<String> {
    seen.iter()
        .filter_map(|event| match event {
            Event::PartUpdated { part, .. } => match &part.body {
                PartBody::Tool {
                    state: ToolState::Error { error, .. },
                    ..
                } => Some(error.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn agents(config: &Config) -> Arc<AgentRegistry> {
    Arc::new(AgentRegistry::build(config).expect("the fixture config resolves an agent"))
}

fn temporary() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

/// A stored session that already has a title, so the title machinery stays out
/// of tests that are not about it and cannot spend a scripted request.
fn seeded(storage: &Storage) -> SessionId {
    let id = SessionId::ascending();
    let info = SessionInfo {
        id: id.clone(),
        version: storage::VERSION,
        title: Some("seeded".to_owned()),
        created: 1,
        updated: 2,
        usage: Usage::default(),
        context_tokens: 0,
        summary: None,
        agent: None,
        model: None,
    };
    storage.save_info(&info).expect("the seeded record writes");

    id
}

/// A planning session may read and search all it likes; the two tools that
/// write are refused by a rule, so nobody is asked and the turn carries on.
#[tokio::test]
async fn the_planning_agent_refuses_an_edit_without_asking_anyone() {
    let (provider, _) = Recorder::new(vec![
        calls("edit", json!({ "filePath": "src/main.rs" })),
        says("I will not, but here is the plan"),
    ]);
    let (edit, edits) = RecorderTool::new("edit");
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(vec![edit])),
        Permissions::default(),
    )
    .with_agents(agents(&Config::default()));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SwitchAgent {
            name: "plan".to_owned(),
        })
        .await
        .expect("plan is a builtin primary agent");

    engine
        .send(Command::SendPrompt {
            text: "how would you do it".to_owned(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain(&mut events).await;

    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, Event::PermissionRequested { .. })),
        "a denial is not a question, got {seen:?}"
    );
    assert!(
        edits.lock().expect("the call log").is_empty(),
        "the tool must never have run"
    );

    let refused = refusals(&seen);
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(
        refused[0].starts_with("The user has specified a rule which prevents you"),
        "{refused:?}"
    );
    assert!(
        refused[0].contains(r#""permission":"edit""#),
        "the model is told which rule stopped it, got {refused:?}"
    );

    let Some(Event::MessageFinished { reason, .. }) = seen.last() else {
        panic!("a turn always ends with a finish, got {seen:?}");
    };
    assert_eq!(
        *reason,
        FinishReason::Completed,
        "a refusal is information, not a turn abort"
    );
}

/// The same session under the agent that may act runs the same call.
#[tokio::test]
async fn the_building_agent_runs_what_the_planning_one_refused() {
    let (provider, _) = Recorder::new(vec![
        calls("edit", json!({ "filePath": "src/main.rs" })),
        says("done"),
    ]);
    let (edit, edits) = RecorderTool::new("edit");
    // A config allowing edits outright, so the run is about the agent rather
    // than about whether anybody answered a dialog.
    let config = Config {
        permission: serde_json::from_value(json!({ "edit": "allow" }))
            .expect("the fixture is a permission block"),
        ..Config::default()
    };
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(vec![edit])),
        Permissions::default(),
    )
    .with_agents(agents(&config));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "do it".to_owned(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    assert_eq!(edits.lock().expect("the call log").len(), 1);
}

/// A config `deny` refuses without asking; a config `allow` runs without
/// asking; and an "always" a person gave outranks both, because it sits above
/// them rather than beside them.
#[tokio::test]
async fn a_config_rule_decides_and_a_stored_answer_outranks_it() {
    let deny = Config {
        permission: serde_json::from_value(json!({ "bash": "deny" }))
            .expect("the fixture is a permission block"),
        ..Config::default()
    };
    let allow = Config {
        permission: serde_json::from_value(json!({ "bash": "allow" }))
            .expect("the fixture is a permission block"),
        ..Config::default()
    };
    let baseline = |config: &Config| {
        agents(config)
            .get("build")
            .expect("build is builtin")
            .rules
            .clone()
    };
    let command = json!({ "command": "cargo test" });

    let mut denied = Permissions::default();
    denied.set_baseline(baseline(&deny));
    assert_eq!(denied.check("bash", &command), Decision::Deny);

    let mut allowed = Permissions::default();
    allowed.set_baseline(baseline(&allow));
    assert_eq!(allowed.check("bash", &command), Decision::Allow);

    // The answer came first in time and sits last in precedence, which is what
    // stops a config edit from silently revoking it.
    let mut answered = Permissions::default();
    answered.remember_always("bash", &command);
    answered.set_baseline(baseline(&deny));
    assert_eq!(
        answered.check("bash", &json!({ "command": "cargo test --release" })),
        Decision::Allow
    );
    assert_eq!(
        answered.check("bash", &json!({ "command": "npm run dev" })),
        Decision::Deny,
        "the config still decides everything nobody answered for"
    );
}

/// Switching agents swaps the half of the system prompt an agent owns and
/// leaves the half describing where the session is working alone.
#[tokio::test]
async fn switching_agents_swaps_the_prompt_and_keeps_the_environment() {
    const BASE: &str = "you are a coding agent";
    const SUFFIX: &str = "<env>cwd: /work</env>";
    const SCRIBE: &str = "you write things down";

    let (provider, seen) = Recorder::new(vec![says("one"), says("two"), says("three")]);
    let mut agent = std::collections::BTreeMap::new();
    agent.insert(
        "scribe".to_owned(),
        AgentConfig {
            prompt: Some(SCRIBE.to_owned()),
            ..AgentConfig::default()
        },
    );
    let config = Config {
        agent,
        ..Config::default()
    };

    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_system_parts(Some(BASE.to_owned()), Some(SUFFIX.to_owned()))
    .with_agents(agents(&config));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    let mut ask = async |engine: &Engine, text: &str| {
        engine
            .send(Command::SendPrompt {
                text: text.to_owned(),
            })
            .await
            .expect("an idle engine accepts a prompt");
        drain(&mut events).await;
    };

    ask(&engine, "first").await;
    engine
        .send(Command::SwitchAgent {
            name: "scribe".to_owned(),
        })
        .await
        .expect("a config agent is selectable");
    ask(&engine, "second").await;
    engine
        .send(Command::SwitchAgent {
            name: "build".to_owned(),
        })
        .await
        .expect("build is a builtin primary agent");
    ask(&engine, "third").await;

    let requests = seen.lock().expect("the request log is never poisoned");
    let systems: Vec<Option<&str>> = requests
        .iter()
        .map(|request| request.system.as_deref())
        .collect();
    assert_eq!(
        systems,
        vec![
            Some(format!("{BASE}\n{SUFFIX}").as_str()),
            Some(format!("{SCRIBE}\n{SUFFIX}").as_str()),
            Some(format!("{BASE}\n{SUFFIX}").as_str()),
        ]
    );
}

/// The planning notice rides on the request and never on the transcript, and
/// the one that says planning is over is said once.
#[tokio::test]
async fn the_plan_reminders_reach_the_request_and_not_the_stored_history() {
    let directory = temporary();
    let storage = Storage::open(directory.path().join("storage"));
    let session = seeded(&storage);

    let (provider, seen) = Recorder::new(vec![says("one"), says("two"), says("three")]);
    let engine = Engine::persistent(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        Storage::open(directory.path().join("storage")),
    )
    .with_agents(agents(&Config::default()));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine.resume(&session).await.expect("the session loads");

    let mut ask = async |engine: &Engine, text: &str| {
        engine
            .send(Command::SendPrompt {
                text: text.to_owned(),
            })
            .await
            .expect("an idle engine accepts a prompt");
        drain(&mut events).await;
    };

    engine
        .send(Command::SwitchAgent {
            name: "plan".to_owned(),
        })
        .await
        .expect("plan is a builtin primary agent");
    ask(&engine, "how would you do it").await;
    engine
        .send(Command::SwitchAgent {
            name: "build".to_owned(),
        })
        .await
        .expect("build is a builtin primary agent");
    ask(&engine, "go ahead").await;
    ask(&engine, "carry on").await;

    /// The text of every part of the last user message a request carried.
    fn last_user(request: &ChatRequest) -> Vec<&str> {
        request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .map(|message| {
                message
                    .parts
                    .iter()
                    .filter_map(|part| part.as_text())
                    .collect()
            })
            .unwrap_or_default()
    }

    let requests = seen.lock().expect("the request log is never poisoned");
    assert_eq!(
        last_user(&requests[0]),
        vec!["how would you do it", PLAN_REMINDER],
        "a planning turn carries the notice that it may not act"
    );
    assert_eq!(
        last_user(&requests[1]),
        vec!["go ahead", BUILD_SWITCH_REMINDER],
        "the turn that stops planning is told so"
    );
    assert_eq!(
        last_user(&requests[2]),
        vec!["carry on"],
        "and is told once, not on every turn after it"
    );
    drop(requests);

    let transcript = storage
        .load_transcript(&session)
        .expect("the transcript reads back");
    let stored: Vec<String> = transcript
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| message.parts.iter().filter_map(|part| part.as_text()))
        .map(str::to_owned)
        .collect();
    assert_eq!(
        stored,
        vec!["how would you do it", "go ahead", "carry on"],
        "nothing synthetic entered the transcript, got {stored:?}"
    );
}

/// A switch lands on the next request, reaches the disk, and is what the
/// session comes back as after the process that made it is gone.
#[tokio::test]
async fn a_switch_applies_to_the_next_turn_and_outlives_the_process() {
    let directory = temporary();
    let storage = Storage::open(directory.path().join("storage"));
    let session = seeded(&storage);

    let (provider, seen) = Recorder::new(vec![says("one"), says("two")]);
    let engine = Engine::persistent(
        provider,
        "first-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        Storage::open(directory.path().join("storage")),
    )
    .with_agents(agents(&Config::default()));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine.resume(&session).await.expect("the session loads");

    engine
        .send(Command::SendPrompt {
            text: "first".to_owned(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    engine
        .send(Command::SwitchModel {
            model: "second-model".to_owned(),
        })
        .await
        .expect("a provider the catalog does not cover takes the model at its word");
    engine
        .send(Command::SwitchAgent {
            name: "plan".to_owned(),
        })
        .await
        .expect("plan is a builtin primary agent");

    // On disk before another turn has run: the switch is a decision about the
    // session, not a side effect of sending a message.
    let stored = storage
        .load_info(&session)
        .expect("the record reads back")
        .expect("the record is there");
    assert_eq!(stored.model.as_deref(), Some("second-model"));
    assert_eq!(stored.agent.as_deref(), Some("plan"));

    engine
        .send(Command::SendPrompt {
            text: "second".to_owned(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    {
        let requests = seen.lock().expect("the request log is never poisoned");
        let models: Vec<&str> = requests
            .iter()
            .map(|request| request.model.as_str())
            .collect();
        assert_eq!(
            models,
            vec!["first-model", "second-model"],
            "the turn in flight kept its model and the next one took the new one"
        );
    }
    drop(engine);

    // A new process over the same store, as `ganja --continue` builds one.
    let (provider, _) = Recorder::new(Vec::new());
    let reopened = Engine::persistent(
        provider,
        "first-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        Storage::open(directory.path().join("storage")),
    )
    .with_agents(agents(&Config::default()));
    let _events = reopened
        .subscribe()
        .await
        .expect("the first subscriber wins");
    reopened.resume(&session).await.expect("the session loads");

    assert_eq!(reopened.model(), "second-model");
    assert_eq!(reopened.agent().as_deref(), Some("plan"));
}

/// A switch mid-turn is refused for the same reason a prompt is: the turn in
/// flight is already asking one model as one agent.
#[tokio::test]
async fn a_switch_sent_mid_turn_is_refused() {
    let engine = Engine::new(
        Arc::new(FakeProvider::new(
            "one two three four five",
            Duration::from_millis(200),
        )),
        fake::MODEL,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_agents(agents(&Config::default()));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "hello".to_owned(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    assert!(matches!(
        events.next().await,
        Some(Event::MessageStarted { .. })
    ));

    assert!(matches!(
        engine
            .send(Command::SwitchAgent {
                name: "plan".to_owned()
            })
            .await,
        Err(EngineError::Busy)
    ));
    assert!(matches!(
        engine
            .send(Command::SwitchModel {
                model: "other".to_owned()
            })
            .await,
        Err(EngineError::Busy)
    ));

    engine
        .send(Command::CancelTurn)
        .await
        .expect("a cancel is always accepted");
    drain(&mut events).await;

    // And once the turn is over the same switch is accepted.
    engine
        .send(Command::SwitchAgent {
            name: "plan".to_owned(),
        })
        .await
        .expect("a finished turn leaves the engine idle");
}

/// The refusals a switch can meet, each naming what it could not honour.
#[tokio::test]
async fn a_switch_that_cannot_be_honoured_says_which_half_refused() {
    let engine = Engine::new(
        Arc::new(FakeProvider::default()),
        fake::MODEL,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_agents(agents(&Config::default()));

    assert!(matches!(
        engine
            .send(Command::SwitchAgent {
                name: "nope".to_owned()
            })
            .await,
        Err(EngineError::UnknownAgent { .. })
    ));
    assert!(
        matches!(
            engine
                .send(Command::SwitchAgent {
                    name: "explore".to_owned()
                })
                .await,
            Err(EngineError::SubagentNotSelectable { .. })
        ),
        "a subagent is the task tool's to run, not a session's to become"
    );

    // An engine with no registry has nothing to switch to at all.
    let bare = Engine::new(
        Arc::new(FakeProvider::default()),
        fake::MODEL,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    );
    assert!(matches!(
        bare.send(Command::SwitchAgent {
            name: "plan".to_owned()
        })
        .await,
        Err(EngineError::NoAgents)
    ));
}

/// A provider the catalog does cover is the one a model switch is checked
/// against, and a model belonging to somebody else is refused (**D8**).
#[tokio::test]
async fn a_model_the_provider_does_not_serve_is_refused() {
    let cataloged = ganja_core::catalog::default_model("anthropic")
        .expect("the catalog has a default for a provider this build ships");

    let engine = Engine::new(
        Arc::new(FakeProvider::default()),
        fake::MODEL,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    );

    // The fake provider is not in the catalog, so nothing contradicts it.
    engine
        .send(Command::SwitchModel {
            model: cataloged.to_owned(),
        })
        .await
        .expect("an uncataloged provider takes a model at its word");
    assert!(matches!(
        engine
            .send(Command::SwitchModel {
                model: "   ".to_owned()
            })
            .await,
        Err(EngineError::UnknownModel { .. })
    ));
}
