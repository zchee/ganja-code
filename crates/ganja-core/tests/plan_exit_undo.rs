//! The undo revocation drill: an `/undo` after a plan-exit Yes takes the
//! approval back **with** the plan it approved — the pending cell cleared and
//! the durable row re-asserted to the still-active plan selection — and a
//! redo does not resurrect the Yes.
//!
//! Its own binary rather than a fourteenth test in `plan_exit.rs`, because
//! the snapshot repository `/undo` needs hangs off `XDG_DATA_HOME` and an
//! env-mutating test holds a binary alone (the repo rule `undo.rs` states).
//! `git` on `PATH` is a prerequisite, not a skip, for the same reason given
//! there: a run that snapshotted nothing would prove nothing.

use std::path::Path;
use std::process::Command as Process;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use futures::stream::BoxStream;
use ganja_core::agent::{BUILD_SWITCH_REMINDER, PLAN_REMINDER};
use ganja_core::permission::Permissions;
use ganja_core::project::Project;
use ganja_core::protocol::{Command, Event, QuestionId};
use ganja_core::provider::{ChatRequest, Provider};
use ganja_core::tool::Registry;
use ganja_core::{Config, Engine, EngineError, Snapshots, Storage};
use ganja_testkit::{ScriptedProvider, drain, tool_call};
use serde_json::json;

/// The approval sentence, spelled out for the reason `plan_exit.rs` spells it.
const APPROVAL: &str = "The plan has been approved, you can now edit files. Execute the plan";

/// How long any single event may take to arrive before the drill gives up.
const PATIENCE: Duration = Duration::from_secs(30);

/// The commit the snapshot repository needs to exist over, mirrored from
/// `undo.rs`'s seeder.
fn seed_repository(root: &Path) {
    let common = [
        "-c",
        "user.name=ganja drill",
        "-c",
        "user.email=drill@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "core.hooksPath=",
        "-c",
        "init.defaultBranch=main",
    ];

    for arguments in
        [vec!["init"], vec!["add", "-A"], vec!["commit", "-m", "the state before anything"]]
    {
        let status = Process::new("git")
            .args(common)
            .args(&arguments)
            .current_dir(root)
            .output()
            .expect("git is a prerequisite of this drill");
        assert!(
            status.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }
}

async fn next_event(events: &mut BoxStream<'static, Event>) -> Event {
    tokio::time::timeout(PATIENCE, events.next())
        .await
        .expect("an event should arrive before the patience runs out")
        .expect("the stream should not end mid-test")
}

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

fn parts_saying(request: &ChatRequest, text: &str) -> usize {
    request
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter_map(ganja_core::protocol::Part::as_text)
        .filter(|part| *part == text)
        .count()
}

fn agent_changes(seen: &[Event]) -> usize {
    seen.iter().filter(|event| matches!(event, Event::AgentChanged { .. })).count()
}

async fn until_question(events: &mut BoxStream<'static, Event>) -> QuestionId {
    loop {
        if let Event::QuestionAsked { id, .. } = next_event(events).await {
            return id;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_undo_after_a_yes_takes_the_approval_back_with_the_plan() {
    let data = tempfile::tempdir().expect("a temporary data home");
    let project = tempfile::tempdir().expect("a temporary project");
    let store = tempfile::tempdir().expect("a temporary storage home");

    // SAFETY: this binary holds one test, so nothing else in the process is
    // reading the environment while it is being written.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", data.path());
    }

    let root = project.path();
    std::fs::write(root.join("tracked.txt"), "one line\n").expect("the fixture file is writable");
    seed_repository(root);
    let snapshots = Snapshots::new(&Project::resolve(root), true);
    assert!(
        snapshots.enabled(),
        "the drill needs git on PATH and a checkout to snapshot: {:?}",
        snapshots.notice()
    );

    let storage = Storage::open(store.path().join("storage"));
    // Pre-titled, so the title machinery never spends a scripted request.
    let session = ganja_testkit::seed_session(&storage, 0);
    let (provider, requests) = ScriptedProvider::new(vec![tool_call("plan_exit", json!({}))]);
    let engine = Engine::persistent(
        provider as Arc<dyn Provider>,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage,
    )
    .with_agents(ganja_testkit::agent_registry(&Config::default()))
    .with_snapshots(Arc::new(snapshots));
    engine.resume(&session).await.expect("the seeded session resumes");
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SwitchAgent { name: "plan".to_owned() })
        .await
        .expect("plan is a builtin primary agent");
    assert!(
        matches!(next_event(&mut events).await, Event::AgentChanged { agent, .. } if agent == "plan")
    );

    // The approving turn: the Yes, the finish, the boundary's announcement
    // and its durable row write.
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
    let id = until_question(&mut events).await;
    engine
        .send(Command::ReplyQuestion { id, answers: vec![vec!["Yes".to_owned()]] })
        .await
        .expect("a reply is never refused");
    drain(&mut events).await;
    assert!(
        matches!(next_event(&mut events).await, Event::AgentChanged { agent, .. } if agent == "build")
    );
    assert_eq!(
        engine.current_session().expect("the session has a row").agent.as_deref(),
        Some("build"),
        "the boundary wrote the row"
    );

    // The undo: two-sided revocation. The cell clears, and the row is
    // re-asserted to the plan selection that is still active in memory —
    // without it a restart would resume a planning session as build.
    send_when_idle(&engine, Command::Undo).await;
    assert!(
        matches!(next_event(&mut events).await, Event::RevertChanged { revert: Some(_), .. }),
        "the undo announces its revert"
    );
    assert_eq!(
        engine.current_session().expect("the session has a row").agent.as_deref(),
        Some("plan"),
        "the row returns to the plan the session still runs as"
    );
    assert_eq!(engine.agent().as_deref(), Some("plan"));

    // A redo does not resurrect the Yes.
    send_when_idle(&engine, Command::Redo).await;
    assert!(
        matches!(next_event(&mut events).await, Event::RevertChanged { revert: None, .. }),
        "the redo clears the revert"
    );
    assert_eq!(engine.agent().as_deref(), Some("plan"));

    // And the next prompt runs plan, with the plan reminder and no trace of
    // the revoked approval.
    send_when_idle(
        &engine,
        Command::SendPrompt {
            text: "keep planning".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        },
    )
    .await;
    let seen = drain(&mut events).await;
    assert_eq!(agent_changes(&seen), 0, "nothing announces after a revocation");
    assert_eq!(engine.agent().as_deref(), Some("plan"));

    let requests = requests.lock().expect("the request log is never poisoned");
    let next = requests.last().expect("the plan turn asked the provider");
    assert_eq!(parts_saying(next, PLAN_REMINDER), 1);
    assert_eq!(parts_saying(next, APPROVAL), 0);
    assert_eq!(parts_saying(next, BUILD_SWITCH_REMINDER), 0);
}
