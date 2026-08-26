//! The engine's own half of the `task {name}` teammate door (**D504**).
//!
//! Spec: Claude Code's teammates — §4.1's spawn sequence. Upstream opencode
//! has no teammates and no counterpart to any of it.
//!
//! `teammate_doors.rs` proves the door from [`Teammates`] down — a test-local
//! [`Subagents`] standing in for the one forwarding call a session installs.
//! This binary is that missing call: a **real engine** whose scripted model
//! issues `task {name}` must reach the same door through `spawn_host`'s
//! `Host`, which is exactly where the team once failed to cross (bead `8o8`:
//! `teammates: None` on the root turn's `Host` left the model-side door
//! answering `NO_TEAM` while the schema advertised `name=`).
//!
//! One test, because it redirects `XDG_DATA_HOME` and **withdraws `TMUX`**,
//! and a binary that mutates process-wide state holds exactly one — a plain
//! `cargo test` runs a binary's tests on threads of one process. `TMUX` goes
//! for `teammate_doors.rs`'s reason: the pane backends are real, and this
//! claim is about the door, not about a window.
//!
//! [`Subagents`]: ganja_core::tool::task::Subagents
//! [`Teammates`]: ganja_core::Teammates

use std::sync::Arc;

use ganja_core::{
    Config, Engine, Storage,
    permission::Permissions,
    protocol::{Command, Event, PartBody, PermissionReply, ToolState},
    team::{TeamName, TeamsRoot},
    teammate::{TeammateRegistry, tmux},
    tool::Registry,
};
use ganja_testkit::{ScriptedProvider, drain_answering, says, tool_call};
use serde_json::json;

/// The scripted model's whole conversation: spawn a teammate, then read the
/// spawn's answer back. The two closing entries are **identical on purpose** —
/// the teammate's own first turn asks the same provider the parent does, and
/// which of the two loops reaches it first is not this test's claim.
fn script() -> Vec<Vec<ganja_core::provider::ProviderEvent>> {
    vec![
        tool_call(
            "task",
            json!({
                "description": "hold the fort",
                "prompt": "watch the build and say what breaks",
                "subagent_type": "general",
                "name": "worker",
                "backend": "in-process",
            }),
        ),
        says("the teammate is off"),
        says("the teammate is off"),
    ]
}

/// The `task {name}` door, through the engine itself: the team installed by
/// [`Engine::with_teammates`] crosses the root turn's `Host`, the only dialog
/// raised is the `task` tool's own per-call gate (the spawn gate behind it
/// discloses nothing for a caller working inside its project), and the call
/// answers with the teammate's name rather than `NO_TEAM`.
#[tokio::test]
async fn a_scripted_task_name_call_starts_a_teammate_through_the_engine() {
    // SAFETY: this binary holds exactly one test, so nothing else in this
    // process is reading the environment while it is being written.
    let data = unsafe { ganja_testkit::redirect_xdg_data_home() };
    // SAFETY: the same invariant. Outside tmux, a pane request would refuse
    // (D501) instead of splitting a pane of whatever tmux runs this suite —
    // and this test names `in-process`, so nothing here should look at tmux
    // at all.
    unsafe {
        std::env::remove_var(tmux::TMUX);
        std::env::remove_var(tmux::TMUX_PANE);
    }
    let home = ganja_testkit::temp_dir();
    let (provider, _requests) = ScriptedProvider::new(script());
    let engine = Engine::persistent(
        provider,
        "recorder-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        Storage::open(home.path().join("storage")),
    )
    .with_agents(ganja_testkit::agent_registry(&Config::default()))
    .with_teammates(Arc::new(TeammateRegistry::new(
        TeamsRoot::new(home.path().join("teams")),
        TeamName::parse("session-abcd1234").expect("a team name"),
        "session-abcd1234",
        home.path().to_path_buf(),
    )));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "hold the fort while I look at the parser".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain_answering(&engine, &mut events, PermissionReply::Once).await;

    // Exactly one dialog, and it is the `task` tool's own — the per-call gate
    // every delegation crosses too. The **spawn gate** behind it added none:
    // an engine-built caller works where its project is (`cwd` and `root` are
    // resolved from one another), so it has no directory to disclose, which
    // is what the empty `directories` says.
    let dialogs: Vec<_> = seen
        .iter()
        .filter_map(|event| match event {
            Event::PermissionRequested {
                tool, directories, ..
            } => Some((tool.clone(), directories.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        dialogs,
        vec![("task".to_owned(), Vec::new())],
        "one dialog — the tool's, disclosing nothing: {seen:?}"
    );

    // The call completed with the teammate's name — the exact surface that
    // once answered `NO_TEAM` as an error the model reads.
    let state = seen
        .iter()
        .rev()
        .find_map(|event| match event {
            Event::PartUpdated { part, .. } => match &part.body {
                PartBody::Tool { tool, state, .. } if tool == "task" => Some(state.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("the turn made a task call");
    let ToolState::Completed { metadata, .. } = state else {
        panic!("the spawn completed rather than erroring: {state:?}");
    };
    assert_eq!(
        metadata.get("teammate").and_then(|name| name.as_str()),
        Some("worker"),
        "the result names the teammate a later message has to address: {metadata:?}"
    );
    assert_eq!(
        metadata.get("backend").and_then(|on| on.as_str()),
        Some("in-process"),
        "and the surface it really runs on: {metadata:?}"
    );
    assert_eq!(
        engine
            .teammates()
            .expect("this session leads a team")
            .registry()
            .running(),
        1,
        "and the teammate is running under the engine's own registry"
    );

    engine.shutdown_teammates().await;
    // Held to the end so the redirected data home outlives every write the
    // teammate's turn might make on its way out.
    drop(data);
}
