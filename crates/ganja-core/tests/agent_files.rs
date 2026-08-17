//! Agent definition files, from the two homes they are discovered in to the
//! turn one of them runs (**D482**).
//!
//! What `src/agent.rs`'s own tests assert is what a definition *says* — they
//! hand the directories in, so they can say it on any machine. What this
//! binary adds is the half that cannot be handed in: that the two directories
//! a session really scans are `<config home>/agents` and
//! `<project root>/.ganja/agents`, and that an agent read out of one of them
//! is an agent in every sense the rest of the build means it — its prompt is
//! the one the model is sent, and its `tools:` roster is what the gate judges
//! its calls by.
//!
//! # Why this is one binary with one test in it
//!
//! `GANJA_CONFIG_HOME` and the XDG homes beneath it are process-wide, and
//! nothing here may read the agents of whoever is running the suite. So the
//! file is one `#[tokio::test]` calling one function per behaviour, in order —
//! the arrangement `tests/memory.rs` and `tests/config_schema.rs` already use,
//! for the same reason.

use std::{fs, path::Path, sync::Arc};

use ganja_core::{
    AgentRegistry, Config, Engine,
    protocol::{Command, Event, PartBody, ToolState},
    tool::Registry,
};
use ganja_testkit::{RecorderTool, ScriptedProvider, drain, says};
use serde_json::json;
use tempfile::TempDir;

/// The prompt the definition file below carries, which has to reach the model
/// **instead of** the base prompt.
const PROMPT: &str = "You read code and report what you found. You never change a file.";

#[tokio::test]
async fn agent_definition_files_are_discovered_read_and_obeyed() {
    let home = TempDir::new().expect("a temporary directory is creatable");
    let config_home = home.path().join("ganja-home");
    let project = home.path().join("project");
    fs::create_dir_all(&project).expect("the project directory is creatable");

    // SAFETY: nothing else runs yet — this is the only test in this binary,
    // and the runtime it starts on is current-thread.
    unsafe {
        std::env::set_var("GANJA_CONFIG_HOME", &config_home);
        std::env::set_var("XDG_CONFIG_HOME", home.path().join("xdg-config"));
        std::env::set_var("XDG_DATA_HOME", home.path().join("xdg-data"));
    }

    both_homes_are_scanned_and_the_project_one_wins_the_name(&config_home, &project);
    a_file_agents_prompt_is_what_the_model_is_sent(&project).await;
    a_tool_its_roster_leaves_out_is_refused_at_the_gate(&project).await;
}

/// Where a file has to be for a session to find it: ganja's own two homes, and
/// the checkout's is the one that wins a name they both claim.
fn both_homes_are_scanned_and_the_project_one_wins_the_name(config_home: &Path, project: &Path) {
    plant(
        &config_home.join("agents"),
        "librarian.md",
        "---\ndescription: the global one\n---\nGlobal.\n",
    );
    plant(
        &config_home.join("agents"),
        "researcher.md",
        "---\ndescription: only in the config home\n---\nResearch.\n",
    );
    plant(
        &project.join(".ganja").join("agents"),
        "librarian.md",
        "---\ndescription: the project one\n---\nProject.\n",
    );

    let registry = AgentRegistry::build(&Config::default(), project)
        .expect("the builtins resolve whatever the files say");

    assert_eq!(
        registry
            .get("researcher")
            .expect("the config home is scanned")
            .description
            .as_deref(),
        Some("only in the config home")
    );
    assert_eq!(
        registry
            .get("librarian")
            .expect("both homes define it")
            .description
            .as_deref(),
        Some("the project one"),
        "the checkout wins a name the two homes both claim"
    );
    assert!(
        registry.get("build").is_some(),
        "and the builtins are still underneath all of it"
    );
}

/// The body of the file is the system prompt of the turn — it **replaces** the
/// base prompt rather than adding to it, which is `AgentConfig::prompt`'s
/// standing meaning and now a file's too.
async fn a_file_agents_prompt_is_what_the_model_is_sent(project: &Path) {
    plant(
        &project.join(".ganja").join("agents"),
        "reader.md",
        &format!("---\ntools: read\nmode: primary\n---\n{PROMPT}\n"),
    );

    let (provider, requests) = ScriptedProvider::new(vec![says("here is what I found")]);
    let engine = Engine::new(
        provider,
        "claude-fixture",
        Arc::new(Registry::new(Vec::new())),
        ganja_core::permission::Permissions::default(),
    )
    .with_base_for_model()
    .with_agents(agents(project, "reader"));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "what does this crate do".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    let system = requests
        .lock()
        .expect("the request log is never poisoned")
        .first()
        .and_then(|request| request.system.clone())
        .expect("every request carries the prompt it was built with");
    assert_eq!(
        system, PROMPT,
        "a file agent's body is the whole base half, exactly as a configured prompt is"
    );
}

/// **AC4, end to end.** A tool the roster leaves out is still offered to the
/// model — this build refuses rather than hides — and the call comes back as
/// the refusal text the model reads, with nobody asked and the turn carrying
/// on.
async fn a_tool_its_roster_leaves_out_is_refused_at_the_gate(project: &Path) {
    let (provider, _) = ScriptedProvider::new(vec![
        calls("edit", json!({ "filePath": "src/main.rs" })),
        calls("read", json!({ "filePath": "src/main.rs" })),
        says("read is all I have"),
    ]);
    let (edit, edits) = RecorderTool::new("edit", "edit", "edited");
    let (read, reads) = RecorderTool::new("read", "read", "the file");
    let engine = Engine::new(
        provider,
        "claude-fixture",
        Arc::new(Registry::new(vec![edit, read])),
        ganja_core::permission::Permissions::default(),
    )
    .with_agents(agents(project, "reader"));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "fix the bug".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain(&mut events).await;

    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, Event::PermissionRequested { .. })),
        "a rule decided it, so nobody was asked: {seen:?}"
    );
    assert!(
        edits.lock().expect("the call log").is_empty(),
        "the tool the roster left out must never have run"
    );
    assert_eq!(
        reads.lock().expect("the call log").len(),
        1,
        "and the one it named runs unasked"
    );

    let refused = refusals(&seen);
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(
        refused[0].contains(r#""permission":"edit""#),
        "the model is told which rule stopped it, got {refused:?}"
    );
}

/// The registry `project`'s files resolve, started on `default_agent`.
fn agents(project: &Path, default_agent: &str) -> Arc<AgentRegistry> {
    let config = Config {
        default_agent: Some(default_agent.to_owned()),
        ..Config::default()
    };

    Arc::new(AgentRegistry::build(&config, project).expect("the definition file resolves an agent"))
}

/// Writes `contents` to `directory/name`, creating the directory.
fn plant(directory: &Path, name: &str, contents: &str) {
    fs::create_dir_all(directory).expect("the fixture directory is creatable");
    fs::write(directory.join(name), contents).expect("the fixture is written");
}

/// A step that calls `tool` once and stops.
fn calls(tool: &str, args: serde_json::Value) -> Vec<ganja_core::provider::ProviderEvent> {
    ganja_testkit::tool_call(tool, args)
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
