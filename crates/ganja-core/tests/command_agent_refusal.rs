//! A command whose `agent:` names nobody, refused at both of the two moments
//! it can be — and refused *differently*, on purpose.
//!
//! A command **file** is refused at load: it leaves the roster, and the log
//! names the file and the agent, which is the posture `command.rs` already
//! applies to every other way a file can be wrong (an unclosed fence, a name
//! that is not a name). A `command` table entry is refused at **dispatch**,
//! with `EngineError::UnknownAgent` naming it — a config file is a curated key
//! set whose author is told by name that a value is wrong, and quietly dropping
//! an entry out of one would be the opposite of how every other key there
//! behaves.
//!
//! One binary, one environment-mutating test — the house rule every config
//! suite here follows, because the global command tier is resolved through
//! `GANJA_CONFIG_HOME` and a plain `cargo test` runs a binary's tests on
//! parallel threads.

use std::{
    fs,
    io::{self, Write as _},
    sync::Arc,
};

use ganja_core::{
    Config, Engine, EngineError,
    command::Registry,
    config::{CONFIG_ENV, CONFIG_HOME_ENV},
    permission::Permissions,
    protocol::Command,
    tool,
};
use ganja_testkit::{LogCapture as Capture, ScriptedProvider, agent_registry, plant};
use serde_json::json;

#[tokio::test]
async fn a_command_naming_an_agent_nobody_has_is_refused_by_file_at_load_and_by_name_at_dispatch() {
    let home = tempfile::tempdir().expect("a temporary directory");
    let config_home = home.path().join("ganja-home");
    let project = home.path().join("project");
    fs::create_dir_all(&config_home).expect("the config home is creatable");
    fs::create_dir_all(&project).expect("the worktree is creatable");

    // SAFETY: this binary holds one environment-mutating test, so nothing else
    // in the process is reading the environment while it is written.
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
        std::env::set_var("XDG_DATA_HOME", home.path().join("data"));
        std::env::set_var(CONFIG_HOME_ENV, &config_home);
        std::env::remove_var(CONFIG_ENV);
    }

    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("this binary holds one test, so nothing else has installed one");

    // Three files: one naming a real agent, one naming nobody, one naming no
    // agent at all. Only the middle one is in question.
    plant(
        &project,
        ".ganja/commands/planned.md",
        "---\nagent: plan\n---\nplan the work\n",
    );
    plant(
        &project,
        ".ganja/commands/ghosted.md",
        "---\nagent: nobody-by-that-name\n---\nrun the work\n",
    );
    plant(&project, ".ganja/commands/plain.md", "just do it\n");

    // …and a config-declared command naming the same absent agent, which is
    // the half that must survive the load to be refused at dispatch.
    let config: Config = serde_json::from_value(json!({
        "command": {
            "declared": { "template": "declared work", "agent": "nobody-by-that-name" }
        }
    }))
    .expect("the fixture is a config");

    let commands = Registry::build(&config, &project);
    assert_eq!(
        commands.names(),
        vec![
            "declared".to_owned(),
            "ghosted".to_owned(),
            "init".to_owned(),
            "plain".to_owned(),
            "planned".to_owned(),
        ],
        "the roster the loader builds knows nothing about agents yet"
    );

    let (provider, _requests) = ScriptedProvider::new(Vec::new());
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(tool::Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_agents(agent_registry(&config))
    .with_commands(Arc::new(commands));

    assert_eq!(
        engine.commands().names(),
        vec![
            "declared".to_owned(),
            "init".to_owned(),
            "plain".to_owned(),
            "planned".to_owned(),
        ],
        "the file naming nobody is gone; the file naming a real agent, the \
         file naming none, and the config entry all stay"
    );

    // The refusal named the file somebody has to edit, and the agent that is
    // not there — the two things a log line has to carry to be actionable.
    let logged = capture.logged();
    io::stdout()
        .write_all(logged.as_bytes())
        .expect("the captured log is printable");
    assert!(
        logged.contains("a command file names an agent this session does not have")
            && logged.contains("ghosted.md")
            && logged.contains("agent=nobody-by-that-name"),
        "the refusal names the file and the agent: {logged}"
    );
    assert!(
        !logged.contains("planned.md"),
        "and says nothing about the file that named a real agent: {logged}"
    );

    // The config-declared half, refused at dispatch and by name — the check
    // this build has always had, still there.
    let refused = engine
        .send(Command::RunCommand {
            name: "declared".to_owned(),
            args: String::new(),
        })
        .await
        .expect_err("no agent answers to that name");
    let EngineError::UnknownAgent { name } = &refused else {
        panic!("expected the dispatch-time refusal, got {refused:?}");
    };
    assert_eq!(name, "nobody-by-that-name");

    // And the file command that named a real agent still runs as far as the
    // agent lookup, which is all this test is about: it is not refused.
    assert!(
        engine
            .commands()
            .get("planned")
            .expect("the file command survived")
            .agent
            .as_deref()
            == Some("plan")
    );
}
