//! What a frontend refuses for the whole session, and what an agent change
//! does to it.
//!
//! A headless run refuses the tools that would ask a question nobody is there
//! to answer. Those rules are neither an agent's nor a person's: they have to
//! survive the agent changing — which four things do, one of them an MCP
//! server finishing its dial in the background — and they have to survive the
//! per-turn ruleset a `/command` naming its own agent derives. Every test here
//! is about a rule that must still decide after something re-derived the
//! baseline beneath it.
//!
//! `question` is the tool the rules name because it is the one upstream's
//! non-interactive ruleset names first, and the one ganja will register next.
//! It is a recorder here, so "the refusal decided" and "the tool never ran"
//! are two separate observations rather than one.
//!
//! Nothing here *writes* anything: an in-memory engine over
//! [`Permissions::default`] has no store to put an answer in. One thing is read
//! from outside the fixture, and [`pin_config_home`] is what moves it.

use std::{
    path::PathBuf,
    sync::{Arc, LazyLock},
};

use ganja_core::{
    Config, Engine,
    config::CONFIG_HOME_ENV,
    permission::{Action, Permissions, Rule},
    protocol::{Command, Event, FinishReason, PartBody, ToolState},
    provider::ProviderEvent,
    tool::Registry,
};
use ganja_testkit::{RecorderTool, ScriptedProvider, agent_registry, drain, says};
use serde_json::json;

/// Points the global command tier (**D481**) at a home this binary owns,
/// before anything builds a command registry.
///
/// That tier is `<config home>/commands`, resolved through [`CONFIG_HOME_ENV`]
/// on every build, so without this the `/review` under test would run beside
/// whatever `*.md` files the developer running the suite keeps in their own
/// home — one of which could take the fixture's own name. Green only while
/// nobody has that directory (`ganja-code-qh1`).
///
/// The home named is a path this binary never creates: `config_home()` returns
/// the variable as written, and `commands/` under a directory that is not there
/// is the empty tier these tests want, with nothing left behind to clean up.
///
/// Forced from a `LazyLock` rather than written into each test because this
/// binary's tests share one process and run on parallel threads under a plain
/// `cargo test`: routing every build through here means the one `set_var`
/// happens before the first read of that variable, with any other builder
/// parked on the lock while it does.
fn pin_config_home() {
    static HOME: LazyLock<PathBuf> = LazyLock::new(|| {
        let home =
            std::env::temp_dir().join(format!("ganja-no-global-commands-{}", std::process::id()));
        // SAFETY: this binary's only write to the environment, run exactly
        // once, under the lock every reader of that variable here goes
        // through.
        unsafe { std::env::set_var(CONFIG_HOME_ENV, &home) };
        home
    });
    LazyLock::force(&HOME);
}

/// What a headless frontend imposes: the tool that would ask a question,
/// refused at every pattern.
///
/// The same shape `ganja run` installs (`ganja-cli`'s `run::REFUSED`), reduced
/// to the one permission these tests need.
fn refuse_question() -> Vec<Rule> {
    vec![Rule {
        permission: "question".to_owned(),
        pattern: "*".to_owned(),
        action: Action::Deny,
    }]
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

/// A config with a `reviewer` agent that allows `question` outright, and a
/// `/review` command that runs as it.
///
/// The allow is what gives the test its teeth: the agent's own ruleset says
/// yes, so a turn that refuses the call refused it because the standing rule
/// outranked the agent — not because nobody allowed it.
fn reviewer() -> Config {
    serde_json::from_value(json!({
        "agent": {
            "reviewer": {
                "prompt": "you review and nothing else",
                "mode": "primary",
                "permission": { "question": "allow", "edit": "deny" }
            }
        },
        "command": { "review": { "template": "review it", "agent": "reviewer" } }
    }))
    .expect("the fixture is a config")
}

/// What a recorder tool was asked, which is how a test tells a call that was
/// refused from one that ran.
type Calls = Arc<std::sync::Mutex<Vec<serde_json::Value>>>;

/// An engine over `script`, running `config`'s agents and commands, with
/// `question` and `edit` as recorders.
///
/// The standing rules are installed the way a headless frontend installs
/// them — after `with_agents`, over whatever the agent said — and the two call
/// logs come back in the order the recorders are named: `question`, then
/// `edit`.
fn engine(config: &Config, script: Vec<Vec<ProviderEvent>>) -> (Engine, Calls, Calls) {
    let (provider, _) = ScriptedProvider::new(script);
    let (question, questions) = RecorderTool::new("question", "question", "answered");
    let (edit, edits) = RecorderTool::new("edit", "edit", "edited");
    // Before the registry below is built. `/repo` does not exist, which empties
    // the project tier (`<worktree>/.ganja/commands`) the way the pinned home
    // empties the global one.
    pin_config_home();
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(vec![question, edit])),
        Permissions::default(),
    )
    .with_agents(agent_registry(config))
    .with_commands(Arc::new(ganja_core::command::Registry::build(
        config,
        std::path::Path::new("/repo"),
    )));
    engine.append_standing_rules(refuse_question());

    (engine, questions, edits)
}

/// The defect this file exists for: a `/command` naming its own agent derives
/// a ruleset for its turn, and what the session refused for every turn is
/// still refused inside it.
///
/// Before the fix the derivation was built from the command agent's rules
/// alone, so the standing refusal was absent for exactly that turn and the
/// agent's own `question: allow` decided instead.
#[tokio::test]
async fn a_command_running_as_another_agent_is_still_bound_by_the_standing_refusal() {
    let (engine, questions, _) = engine(
        &reviewer(),
        vec![
            calls("question", json!({ "text": "which one?" })),
            says("never mind"),
        ],
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::RunCommand {
            name: "review".to_owned(),
            args: String::new(),
        })
        .await
        .expect("the configured command runs");
    let seen = drain(&mut events).await;

    assert!(
        questions.lock().expect("the call log").is_empty(),
        "the refused tool must never have run"
    );
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, Event::PermissionRequested { .. })),
        "and a headless refusal is never a dialog, got {seen:?}"
    );

    let refused = refusals(&seen);
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(
        refused[0].contains(r#""permission":"question""#),
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

/// The inverse, so the fix is a layering and not a replacement: the command
/// agent's own rules still decide everything the standing rules say nothing
/// about.
///
/// `reviewer` denies `edit` where the session's `build` agent asks about it,
/// and the derived turn is judged by the agent it named.
#[tokio::test]
async fn a_command_agents_own_rules_still_decide_inside_the_derived_turn() {
    let (engine, _, edits) = engine(
        &reviewer(),
        vec![
            calls("edit", json!({ "filePath": "src/main.rs" })),
            says("I will not"),
        ],
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::RunCommand {
            name: "review".to_owned(),
            args: String::new(),
        })
        .await
        .expect("the configured command runs");
    let seen = drain(&mut events).await;

    assert!(
        edits.lock().expect("the call log").is_empty(),
        "the agent's own denial still stops the call"
    );

    let refused = refusals(&seen);
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(
        refused[0].contains(r#""permission":"edit""#),
        "and it is the agent's rule that is quoted, got {refused:?}"
    );
}

/// The same rules, through the other door: switching the session's agent
/// re-installs a baseline, and the standing refusal is not part of what the
/// old agent took with it.
///
/// Four things re-install a baseline — this one, a resume, the initial
/// `with_agents`, and the tool-set rebuild an MCP server's dial completes,
/// which is the one a headless run actually meets: `run` imposes its rules and
/// *then* dials. A switch is the observable that needs no server.
#[tokio::test]
async fn switching_the_session_agent_does_not_drop_the_standing_refusal() {
    let (engine, questions, _) = engine(
        &reviewer(),
        vec![
            calls("question", json!({ "text": "which one?" })),
            says("never mind"),
        ],
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SwitchAgent {
            name: "reviewer".to_owned(),
        })
        .await
        .expect("reviewer is a primary agent");
    engine
        .send(Command::SendPrompt {
            text: "ask me something".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain(&mut events).await;

    assert_eq!(
        engine.agent().as_deref(),
        Some("reviewer"),
        "the switch landed, so what follows is about its ruleset"
    );
    assert!(
        questions.lock().expect("the call log").is_empty(),
        "the refused tool must never have run"
    );

    let refused = refusals(&seen);
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(
        refused[0].contains(r#""permission":"question""#),
        "the standing rule still decides after the switch, got {refused:?}"
    );
}

/// Order does not matter either: rules imposed before the agents arrive are
/// not thrown away by the ruleset `with_agents` installs on top of them.
///
/// The call this replaces had to be made after the engine was assembled and
/// never before, because it wrote the baseline that `with_agents` then
/// overwrote. A rule that a frontend has to remember is a rule that will be
/// forgotten, so the seam does not have one.
#[tokio::test]
async fn standing_rules_imposed_before_the_agents_survive_them() {
    let (provider, _) = ScriptedProvider::new(vec![
        calls("question", json!({ "text": "which one?" })),
        says("never mind"),
    ]);
    let (question, questions) = RecorderTool::new("question", "question", "answered");
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(vec![question])),
        Permissions::default(),
    );
    engine.append_standing_rules(refuse_question());
    let engine = engine.with_agents(agent_registry(&reviewer()));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "ask me something".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain(&mut events).await;

    assert!(
        questions.lock().expect("the call log").is_empty(),
        "the refused tool must never have run"
    );
    let refused = refusals(&seen);
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(
        refused[0].contains(r#""permission":"question""#),
        "the rule imposed first still decides, got {refused:?}"
    );
}

/// And the standing rules are not a blanket refusal: an agent that allows a
/// tool nobody stood against still runs it.
///
/// Without this the three tests above would pass just as well if
/// `append_standing_rules` refused everything.
#[tokio::test]
async fn a_tool_no_standing_rule_names_still_runs_under_the_agent_that_allows_it() {
    let config: Config = serde_json::from_value(json!({
        "agent": {
            "reviewer": {
                "prompt": "you review and nothing else",
                "mode": "primary",
                "permission": { "edit": "allow" }
            }
        },
        "command": { "review": { "template": "review it", "agent": "reviewer" } }
    }))
    .expect("the fixture is a config");
    let (engine, _, edits) = engine(
        &config,
        vec![
            calls("edit", json!({ "filePath": "src/main.rs" })),
            says("done"),
        ],
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::RunCommand {
            name: "review".to_owned(),
            args: String::new(),
        })
        .await
        .expect("the configured command runs");
    let seen = drain(&mut events).await;

    assert_eq!(
        edits.lock().expect("the call log").len(),
        1,
        "the agent's allow survives the standing rules landing above it"
    );
    assert!(refusals(&seen).is_empty(), "{seen:?}");
}
