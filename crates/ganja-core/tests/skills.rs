//! Skills end to end: what a session is told it can load, and what loading one
//! actually hands the model.
//!
//! The frontmatter parser, the discovery walk and the `<available_skills>`
//! block are unit-tested where each is written (`ganja-tool`'s `skill` module
//! and `src/instruction.rs`). What is proved here is the join: that the list a
//! model is offered in its system prompt and the list a `skill` call can load
//! from are **the same list**, because both are built from one
//! `instruction::skill_roots` value — and that an engine assembled the way a
//! fixture run assembles one is offered nothing at all.
//!
//! # Process-wide state
//!
//! One test, one binary. `HOME` is redirected because the conventional
//! discovery tiers include `~/.claude/skills` and `~/.agents/skills`, which is
//! exactly upstream's behaviour and exactly what must not reach a test: a suite
//! that read the developer's own skills would assert something different on
//! every machine, and the golden differential would be comparing this port
//! against whatever that developer happened to have installed.
//! `XDG_DATA_HOME` is redirected for the usual reason — nothing here may read
//! or write the real user's stored permissions or spilled output.

use std::sync::Arc;

use ganja_core::{
    Config,
    Engine,
    // Spelled through the module rather than the crate root: `WebfetchConfig`
    // is re-exported there and this is not, and adding it is a one-line change
    // to a file this lane does not own.
    config::SkillsConfig,
    instruction,
    permission::Permissions,
    protocol::{Command, PartBody, ToolState},
    provider::ChatRequest,
    tool::{
        Registry,
        skill::{Roots, SkillTool},
    },
};
use ganja_testkit::{ScriptedProvider, drain, says, tool_call};

/// Model both engines ask for; nothing depends on its family.
const MODEL: &str = "skills-model";

/// Writes a skill at `<root>/<name>/SKILL.md` and returns the directory it
/// went into.
fn plant(root: &std::path::Path, name: &str, frontmatter: &str, body: &str) -> std::path::PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).expect("the fixture's directories are creatable");
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\n{frontmatter}\n---\n{body}"),
    )
    .expect("the fixture is writable");

    dir
}

/// The system prompt of the first request a scripted provider was sent.
fn system(seen: &Arc<std::sync::Mutex<Vec<ChatRequest>>>) -> String {
    seen.lock()
        .expect("the request log is never poisoned")
        .first()
        .expect("the engine reached the provider")
        .system
        .clone()
        .unwrap_or_default()
}

#[tokio::test]
async fn a_session_is_offered_the_skills_it_was_told_to_look_for_and_no_others() {
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently. Both are set before
    // anything composes a prompt, resolves a store or spills tool output.
    let home = ganja_testkit::temp_dir();
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var("XDG_DATA_HOME", home.path().join("xdg"));
        std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
    }

    let directory = ganja_testkit::temp_dir();
    let cwd = directory.path().to_path_buf();
    std::fs::create_dir_all(cwd.join(".git")).expect("the fixture checkout is creatable");
    let elsewhere = directory.path().join("shared-skills");
    plant(
        &elsewhere,
        "porting",
        "name: porting\ndescription: How to port a module.",
        "# Porting\n\nRead the upstream file first.\n",
    );

    // --- The fixture-run engine: no config, no environment half, nothing
    // discovered. This is how `tests/golden.rs` assembles its leg, and the
    // reason it must stay this way is that a differential comparing tool calls
    // has to compare two agents rather than two machines.
    let bare_roots = instruction::skill_roots(&Config::default(), &cwd);
    assert!(
        ganja_core::tool::skill::discover(&bare_roots).is_empty(),
        "a session nobody told about a skills directory finds none: {:?}",
        bare_roots.dirs()
    );

    let (provider, bare_seen) = ScriptedProvider::new(vec![says("nothing to load")]);
    let engine = Engine::new(
        provider,
        MODEL,
        Arc::new(Registry::with_builtins()),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::SendPrompt {
            text: "hello".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    drain(&mut events).await;

    assert_eq!(
        system(&bare_seen),
        String::new(),
        "a fixture-built engine carries no system prompt at all, so no skill can reach one"
    );

    // --- The session a frontend assembles: one config, one set of roots, and
    // both halves built from it.
    let config = Config {
        skills: SkillsConfig {
            paths: vec![elsewhere.display().to_string()],
            urls: Vec::new(),
        },
        ..Config::default()
    };
    let roots: Roots = instruction::skill_roots(&config, &cwd);
    let tools = Registry::with_builtins().with(Arc::new(SkillTool::over(roots.clone())));

    let (provider, seen) = ScriptedProvider::new(vec![
        tool_call("skill", serde_json::json!({ "name": "porting" })),
        says("read it"),
    ]);
    let engine = Engine::new(provider, MODEL, Arc::new(tools), Permissions::default())
        .with_environment({
            let config = config.clone();
            let cwd = cwd.clone();
            move |model| instruction::suffix(&config, &cwd, model)
        });
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(Command::SendPrompt {
            text: "port the module".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen_events = drain(&mut events).await;

    // The prompt offers it, in upstream's shape.
    let prompt = system(&seen);
    assert!(
        prompt.contains("<available_skills>")
            && prompt.contains("<name>porting</name>")
            && prompt.contains("<description>How to port a module.</description>"),
        "the skill the config pointed at is offered: {prompt}"
    );
    assert!(
        prompt
            .find("Working directory")
            .expect("the environment block")
            < prompt.find("<available_skills>").expect("the skills block"),
        "and last, where upstream puts it: {prompt}"
    );

    // And the call loads it — the same skill, out of the same roots, without
    // the registry having been told anything the prompt was not.
    let loaded: Vec<String> = seen_events
        .iter()
        .filter_map(|event| match event {
            ganja_core::protocol::Event::PartUpdated { part, .. } => match &part.body {
                PartBody::Tool {
                    state: ToolState::Completed { output, .. },
                    ..
                } => Some(output.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();

    assert_eq!(loaded.len(), 1, "one call, one result: {loaded:?}");
    assert!(
        loaded[0].contains("<skill_content name=\"porting\">")
            && loaded[0].contains("Read the upstream file first."),
        "the model is handed the skill's own instructions: {}",
        loaded[0]
    );
    assert!(
        loaded[0].contains(&format!(
            "Base directory for this skill: {}",
            elsewhere.join("porting").display()
        )),
        "and where its relative paths start from: {}",
        loaded[0]
    );
}
