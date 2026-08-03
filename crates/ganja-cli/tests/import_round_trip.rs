//! What the importer writes is what the next launch reads.
//!
//! The importer validates its own output before writing it, which proves the
//! bytes decode. This proves the other half — that the file lands where
//! `ganja_core::config` looks, and that every value survives the trip with its
//! meaning intact, permission order included.
//!
//! One test, one binary, on purpose: it mutates process-wide environment
//! variables so that the in-process load and the subprocess that wrote the file
//! agree about where the config homes are, and a plain `cargo test` runs the
//! tests inside a binary on parallel threads. `XDG_CONFIG_HOME` and
//! `XDG_DATA_HOME` are redirected into a temporary tree, so the machine running
//! the suite cannot contribute a config of its own.

use std::{env, fs};

use assert_cmd::Command;
use ganja_core::{
    Config,
    config::{AgentMode, CONFIG_ENV},
    permission::Action,
};

/// An imported `deny` is a rule this build carries out: it refuses the call
/// without asking anybody.
fn deny() -> Action {
    Action::Deny
}

#[test]
fn an_imported_config_is_one_the_next_launch_reads_back_whole() {
    let home = tempfile::tempdir().expect("a temporary directory");
    let project = home.path().join("project");
    // A checkout, so the project walk stops here rather than climbing out of
    // the fixture and into whatever the temporary directory sits under.
    fs::create_dir_all(project.join(".git")).expect("the fixture repository is creatable");
    fs::write(
        project.join("opencode.jsonc"),
        include_str!("fixtures/opencode.jsonc"),
    )
    .expect("the fixture file is writable");

    // SAFETY: this binary holds one test, so nothing else in the process is
    // reading the environment while it is being written.
    unsafe {
        env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
        env::set_var("XDG_DATA_HOME", home.path().join("data"));
        // Otherwise a developer's exported file would be read as a tier of its
        // own, on top of the one under test.
        env::remove_var(CONFIG_ENV);
    }

    Command::new(env!("CARGO_BIN_EXE_ganja"))
        .current_dir(&project)
        .args(["config", "import-opencode"])
        .assert()
        .success();

    let config = Config::load(&project).expect("the imported config is one ganja loads");

    assert_eq!(config.model.as_deref(), Some("anthropic/claude-sonnet-5"));
    assert_eq!(
        config.small_model, None,
        "the model that was only an {{env:}} token stays out"
    );
    assert_eq!(config.default_agent.as_deref(), Some("plan"));
    assert_eq!(config.theme.as_deref(), Some("tokyonight"));
    assert_eq!(config.shell.as_deref(), Some("/bin/zsh"));
    assert_eq!(
        config.instructions,
        vec!["AGENTS.md", "docs/{env:TEAM}/style.md"],
        "the entry that was only a token is gone, the one that embeds one is not"
    );

    // Order is the whole semantics of a rule set: evaluation is
    // last-match-wins, so a rule that moved is a rule that stopped applying.
    let rules: Vec<(String, String, Action)> = config
        .permission
        .rules()
        .into_iter()
        .map(|rule| (rule.permission, rule.pattern, rule.action))
        .collect();
    assert_eq!(
        rules,
        vec![
            // Derived from the legacy `tools` map, which keeps its position…
            ("webfetch".to_owned(), "*".to_owned(), deny()),
            // …and loses every tool the explicit rules also name.
            ("bash".to_owned(), "git status".to_owned(), Action::Allow),
            ("bash".to_owned(), "git *".to_owned(), Action::Ask),
            ("bash".to_owned(), "*".to_owned(), deny()),
            ("edit".to_owned(), "*".to_owned(), Action::Ask),
            ("read".to_owned(), "*".to_owned(), Action::Allow),
        ]
    );

    let review = &config.agent["review"];
    assert_eq!(review.model.as_deref(), Some("anthropic/claude-haiku-4-5"));
    assert_eq!(
        review.description.as_deref(),
        Some("reads a diff and complains")
    );
    assert_eq!(review.mode, Some(AgentMode::Subagent));
    assert_eq!(
        review
            .permission
            .rules()
            .into_iter()
            .map(|rule| (rule.permission, rule.action))
            .collect::<Vec<_>>(),
        vec![
            ("edit".to_owned(), deny()),
            ("webfetch".to_owned(), Action::Allow),
        ]
    );

    // A `mode` entry is an agent only the user can pick.
    let ship = &config.agent["ship"];
    assert_eq!(
        ship.prompt.as_deref(),
        Some("You ship what is already green.")
    );
    assert_eq!(ship.mode, Some(AgentMode::Primary));
    assert_eq!(ship.hidden, Some(false));

    let release = &config.command["release"];
    assert_eq!(release.template, "cut a release for $ARGUMENTS");
    assert_eq!(release.description.as_deref(), Some("tag and push"));
    assert_eq!(release.agent.as_deref(), Some("build"));
    assert_eq!(release.model, None);
}
