//! Agents end to end: what an agent denies, what it says to the model, and
//! what survives being switched, stored and reopened.
//!
//! The rules and the prompts are unit-tested where they are assembled
//! (`src/agent.rs`); what is proved here is that a turn actually runs under
//! them — that a denial reaches the model as a tool result rather than a
//! dialog, that a switch lands on the *next* request and not the one in
//! flight, and that a session reopened tomorrow is the session that was left.

use std::{sync::Arc, time::Duration};

use futures::StreamExt as _;
use ganja_core::{
    AgentConfig, Config, Engine, EngineError, Storage,
    agent::{BUILD_SWITCH_REMINDER, PLAN_REMINDER},
    permission::{Decision, Permissions},
    protocol::{Command, Event, FinishReason, PartBody, Role, ToolState},
    provider::{ChatRequest, FakeProvider, ProviderEvent, fake},
    tool::Registry,
};
use ganja_testkit::{RecorderTool, ScriptedProvider, drain, says};
use serde_json::json;

/// A step that calls `tool` once and stops.
///
/// Kept local rather than folded into `ganja_testkit::tool_call`: it never
/// closes the call with a [`ProviderEvent::ToolCallEnd`], which is the point
/// of several tests here — the loop has to finalize a call from `Finish`
/// alone.
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

/// A planning session may read and search all it likes; the two tools that
/// write are refused by a rule, so nobody is asked and the turn carries on.
#[tokio::test]
async fn the_planning_agent_refuses_an_edit_without_asking_anyone() {
    let (provider, _) = ScriptedProvider::new(vec![
        calls("edit", json!({ "filePath": "src/main.rs" })),
        says("I will not, but here is the plan"),
    ]);
    let (edit, edits) = RecorderTool::new("edit", "edit", "done");
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(vec![edit])),
        Permissions::default(),
    )
    .with_agents(ganja_testkit::agent_registry(&Config::default()));
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
            mentions: Vec::new(),
            skills: Vec::new(),
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
    let (provider, _) = ScriptedProvider::new(vec![
        calls("edit", json!({ "filePath": "src/main.rs" })),
        says("done"),
    ]);
    let (edit, edits) = RecorderTool::new("edit", "edit", "done");
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
    .with_agents(ganja_testkit::agent_registry(&config));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "do it".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
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
        ganja_testkit::agent_registry(config)
            .get("build")
            .expect("build is builtin")
            .rules
            .clone()
    };
    let command = json!({ "command": "cargo test" });

    let mut denied = Permissions::default();
    denied.set_baseline(baseline(&deny));
    assert_eq!(denied.gate("bash", &command).action, Decision::Deny);

    let mut allowed = Permissions::default();
    allowed.set_baseline(baseline(&allow));
    assert_eq!(allowed.gate("bash", &command).action, Decision::Allow);

    // The answer came first in time and sits last in precedence, which is what
    // stops a config edit from silently revoking it.
    let mut answered = Permissions::default();
    let decision = answered.gate("bash", &command);
    answered.remember(&decision);
    answered.set_baseline(baseline(&deny));
    assert_eq!(
        answered
            .gate("bash", &json!({ "command": "cargo test --release" }))
            .action,
        Decision::Allow
    );
    assert_eq!(
        answered
            .gate("bash", &json!({ "command": "npm run dev" }))
            .action,
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

    let (provider, seen) = ScriptedProvider::new(vec![says("one"), says("two"), says("three")]);
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
    .with_agents(ganja_testkit::agent_registry(&config));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    let mut ask = async |engine: &Engine, text: &str| {
        engine
            .send(Command::SendPrompt {
                text: text.to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
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

/// The environment block states the model as fact — "you are powered by the
/// model named X", twice over — so a session that switches model and keeps the
/// block it launched with spends the rest of the conversation telling the new
/// model it is the old one.
///
/// Composed here through the real `instruction::suffix`, not a stand-in, so
/// what is asserted is the sentence a model would actually read.
#[tokio::test]
async fn switching_models_recomposes_the_environment_block_for_the_new_model() {
    const BASE: &str = "you are a coding agent";
    const LAUNCH: &str = "launch-model";
    const PICKED: &str = "picked-model";

    let directory = ganja_testkit::temp_dir();
    let cwd = directory.path().to_owned();

    let (provider, seen) = ScriptedProvider::new(vec![says("one"), says("two")]);
    let engine = Engine::new(
        provider,
        LAUNCH,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_system_parts(Some(BASE.to_owned()), None)
    .with_environment(move |model| {
        ganja_core::instruction::suffix(&Config::default(), &cwd, model)
    });
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "first".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    engine
        .send(Command::SwitchModel {
            model: PICKED.to_owned(),
        })
        .await
        .expect("a provider the catalog does not cover takes the model at its word");

    engine
        .send(Command::SendPrompt {
            text: "second".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    let requests = seen.lock().expect("the request log is never poisoned");
    let systems: Vec<&str> = requests
        .iter()
        .map(|request| {
            request
                .system
                .as_deref()
                .expect("every request carries the prompt it was built with")
        })
        .collect();
    assert_eq!(systems.len(), 2, "one request per prompt");

    for (index, model) in [(0, LAUNCH), (1, PICKED)] {
        let system = systems[index];
        assert!(
            system.starts_with(&format!("{BASE}\n")),
            "the base half is not what a model switch touches: {system}"
        );
        assert!(
            system.contains(&format!("You are powered by the model named {model}")),
            "request {index} should name {model}: {system}"
        );
    }
    assert!(
        !systems[1].contains(LAUNCH),
        "and the model it switched away from is gone: {}",
        systems[1]
    );
}

/// The same block, moved by the other route: an agent that prefers a model
/// switches the model with it, so it has to move the environment block too.
#[tokio::test]
async fn switching_to_an_agent_that_prefers_a_model_recomposes_the_environment_block() {
    const LAUNCH: &str = "launch-model";
    const PREFERRED: &str = "preferred-model";

    let directory = ganja_testkit::temp_dir();
    let cwd = directory.path().to_owned();

    let mut agent = std::collections::BTreeMap::new();
    agent.insert(
        "scribe".to_owned(),
        AgentConfig {
            model: Some(format!("recorder/{PREFERRED}")),
            ..AgentConfig::default()
        },
    );
    let config = Config {
        agent,
        ..Config::default()
    };

    let (provider, seen) = ScriptedProvider::new(vec![says("one"), says("two")]);
    let engine = Engine::new(
        provider,
        LAUNCH,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_agents(ganja_testkit::agent_registry(&config))
    .with_environment(move |model| {
        ganja_core::instruction::suffix(&Config::default(), &cwd, model)
    });
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "first".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    engine
        .send(Command::SwitchAgent {
            name: "scribe".to_owned(),
        })
        .await
        .expect("a config agent is selectable");
    assert_eq!(
        engine.model(),
        PREFERRED,
        "the fixture only proves anything while the agent really moves the model"
    );

    engine
        .send(Command::SendPrompt {
            text: "second".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    let requests = seen.lock().expect("the request log is never poisoned");
    let systems: Vec<&str> = requests
        .iter()
        .map(|request| {
            request
                .system
                .as_deref()
                .expect("every request carries the prompt it was built with")
        })
        .collect();
    assert_eq!(systems.len(), 2, "one request per prompt");
    assert!(
        systems[0].contains(&format!("You are powered by the model named {LAUNCH}")),
        "the first turn names the model it launched on: {}",
        systems[0]
    );
    assert!(
        systems[1].contains(&format!("You are powered by the model named {PREFERRED}")),
        "and the second names the one the agent brought with it: {}",
        systems[1]
    );
}

/// A `!` passthrough between the switch and the first build prompt does not
/// spend the notice that planning is over.
///
/// The reminder is defined as a comparison against the previous turn
/// (**D37**), and only a turn that asks the model anything can carry it: a
/// passthrough puts a command and its output in the transcript without a
/// request. Counting one as "the previous turn" retired a notice that was
/// never delivered.
#[tokio::test]
async fn a_shell_passthrough_does_not_consume_the_notice_that_planning_is_over() {
    let directory = ganja_testkit::temp_dir();
    let storage = Storage::open(directory.path().join("storage"));
    let session = ganja_testkit::seed_session(&storage, 0);

    let (provider, seen) = ScriptedProvider::new(vec![says("one"), says("two")]);
    let engine = Engine::persistent(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        Storage::open(directory.path().join("storage")),
    )
    .with_agents(ganja_testkit::agent_registry(&Config::default()));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine.resume(&session).await.expect("the session loads");

    engine
        .send(Command::SwitchAgent {
            name: "plan".to_owned(),
        })
        .await
        .expect("plan is a builtin primary agent");
    engine
        .send(Command::SendPrompt {
            text: "how would you do it".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    engine
        .send(Command::SwitchAgent {
            name: "build".to_owned(),
        })
        .await
        .expect("build is a builtin primary agent");
    engine
        .send(Command::RunShell {
            command: "printf 'on branch main'".to_owned(),
        })
        .await
        .expect("an idle engine accepts a command");
    drain(&mut events).await;

    engine
        .send(Command::SendPrompt {
            text: "go ahead".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    let requests = seen.lock().expect("the request log is never poisoned");
    assert_eq!(
        requests.len(),
        2,
        "a passthrough asks the model nothing, so only the two prompts are here"
    );
    let carried: Vec<&str> = requests[1]
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
        .unwrap_or_default();
    assert_eq!(
        carried,
        vec!["go ahead", BUILD_SWITCH_REMINDER],
        "the first turn that asks the model after the switch is the one that is told"
    );
}

/// The planning notice rides on the request and never on the transcript, and
/// the one that says planning is over is said once.
#[tokio::test]
async fn the_plan_reminders_reach_the_request_and_not_the_stored_history() {
    let directory = ganja_testkit::temp_dir();
    let storage = Storage::open(directory.path().join("storage"));
    let session = ganja_testkit::seed_session(&storage, 0);

    let (provider, seen) = ScriptedProvider::new(vec![says("one"), says("two"), says("three")]);
    let engine = Engine::persistent(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        Storage::open(directory.path().join("storage")),
    )
    .with_agents(ganja_testkit::agent_registry(&Config::default()));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine.resume(&session).await.expect("the session loads");

    let mut ask = async |engine: &Engine, text: &str| {
        engine
            .send(Command::SendPrompt {
                text: text.to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
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
    let directory = ganja_testkit::temp_dir();
    let storage = Storage::open(directory.path().join("storage"));
    let session = ganja_testkit::seed_session(&storage, 0);

    let (provider, seen) = ScriptedProvider::new(vec![says("one"), says("two")]);
    let engine = Engine::persistent(
        provider,
        "first-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        Storage::open(directory.path().join("storage")),
    )
    .with_agents(ganja_testkit::agent_registry(&Config::default()));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine.resume(&session).await.expect("the session loads");

    engine
        .send(Command::SendPrompt {
            text: "first".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
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
            mentions: Vec::new(),
            skills: Vec::new(),
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
    let (provider, _) = ScriptedProvider::new(Vec::new());
    let reopened = Engine::persistent(
        provider,
        "first-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        Storage::open(directory.path().join("storage")),
    )
    .with_agents(ganja_testkit::agent_registry(&Config::default()));
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
    .with_agents(ganja_testkit::agent_registry(&Config::default()));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "hello".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
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
    .with_agents(ganja_testkit::agent_registry(&Config::default()));

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

/// A config spells an agent's model `"provider/model"`, and the catalog knows
/// only the half after the slash. A provider the catalog covers is the only
/// place that difference is visible: there the whole spelling matches nothing,
/// and an agent naming a real model would keep the session's instead.
#[tokio::test]
async fn an_agents_model_is_adopted_from_the_spelling_a_config_writes() {
    let mut served = ganja_core::catalog::models()
        .filter(|model| model.provider_id == "anthropic")
        .map(|model| model.id.clone());
    let start = served
        .next()
        .expect("the compiled-in catalog covers anthropic");
    let wanted = served
        .next()
        .expect("and covers more than one model of theirs");

    let config: Config = serde_json::from_value(json!({
        "agent": {
            "review": { "mode": "primary", "model": format!("anthropic/{wanted}") }
        }
    }))
    .expect("the fixture is a config");

    let (provider, seen) = ScriptedProvider::named("anthropic", vec![says("reviewed")]);
    let engine = Engine::new(
        provider,
        &start,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_agents(ganja_testkit::agent_registry(&config));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SwitchAgent {
            name: "review".to_owned(),
        })
        .await
        .expect("the config names review a primary agent");
    engine
        .send(Command::SendPrompt {
            text: "look at it".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    let seen = seen.lock().expect("the request log is never poisoned");
    assert_eq!(
        seen[0].model, wanted,
        "the request carries the catalog id the agent named, not the spelling it was written in"
    );
}
