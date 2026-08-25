//! Markdown command files (**D481**) read from ganja's own two homes, and the
//! four-tier precedence a session resolves them under:
//!
//! ```text
//! builtin  <  <config home>/commands  <  <worktree>/.ganja/commands  <  config `command`
//! ```
//!
//! One binary, one environment-mutating test — the house rule every config
//! suite here follows, because the global half of that pair is resolved through
//! `GANJA_CONFIG_HOME` and a plain `cargo test` runs a binary's tests on
//! parallel threads. Everything below runs sequentially inside the single
//! `#[test]`, against one pinned temporary home and one temporary worktree, so
//! nothing here can read the commands directory of whoever is running the
//! suite — and nothing in the suite depends on that directory being absent.
//!
//! The parsing rules themselves — frontmatter, hostile files, name rules — are
//! unit tests beside the parser in `src/command.rs`; what is proved here is the
//! part that needs a real config home: which directories are read, and which
//! tier wins a name.

use std::{
    fs,
    io::{self, Write as _},
};

use ganja_core::{
    Config,
    command::{INIT, Registry},
    config::{CONFIG_ENV, CONFIG_HOME_ENV},
};
use ganja_testkit::{LogCapture as Capture, plant};
use serde_json::json;

#[test]
fn command_files_join_the_roster_under_the_precedence_their_home_gives_them() {
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

    // The global home: one command of its own, one that tries to take a
    // builtin's name, and one the project will shadow.
    plant(
        &config_home,
        "commands/greet.md",
        "---\ndescription: say hello\nargument-hint: <name>\n---\ngreet $1\n",
    );
    plant(
        &config_home,
        "commands/init.md",
        "this is not the init anybody means\n",
    );
    plant(&config_home, "commands/shared.md", "the global one\n");

    // The worktree: the shadowing file, one of its own, and one the config
    // file will shadow in turn.
    plant(&project, ".ganja/commands/shared.md", "the project one\n");
    plant(
        &project,
        ".ganja/commands/only.md",
        "---\nagent: plan\nmodel: anthropic/claude-sonnet-4-5\n---\nonly here\n",
    );
    plant(&project, ".ganja/commands/configured.md", "the file one\n");

    let config: Config = serde_json::from_value(json!({
        "command": {
            "configured": { "template": "the config one", "description": "declared" }
        }
    }))
    .expect("the fixture is a config");

    let registry = Registry::build(&config, &project);

    assert_eq!(
        registry.names(),
        vec![
            "configured".to_owned(),
            "greet".to_owned(),
            INIT.to_owned(),
            "only".to_owned(),
            "shared".to_owned(),
        ],
        "both homes are read, and nothing else is"
    );

    // builtin < global file, resolved as a refusal: a file may not take a
    // builtin's name, so `/init` is still the one this build ships.
    let init = registry.get(INIT).expect("the builtin survives");
    assert!(
        init.template
            .starts_with("Create or update `AGENTS.md` for this repository."),
        "a file named after a builtin never replaces it: {}",
        init.template
    );

    // The global tier's own command, hint folded into the line a palette shows.
    let greet = registry.get("greet").expect("the global file is a command");
    assert_eq!(greet.description.as_deref(), Some("say hello — <name>"));
    assert_eq!(greet.template, "greet $1\n");

    // global file < project file.
    assert_eq!(
        registry
            .get("shared")
            .expect("the shadowed name is still a command")
            .template,
        "the project one\n",
        "the worktree's file wins the name"
    );

    // The project tier's own command keeps its frontmatter.
    let only = registry.get("only").expect("the project file is a command");
    assert_eq!(only.agent.as_deref(), Some("plan"));
    assert_eq!(only.model.as_deref(), Some("anthropic/claude-sonnet-4-5"));

    // project file < config-declared: the curated config still wins.
    let configured = registry.get("configured").expect("the config entry wins");
    assert_eq!(configured.template, "the config one");
    assert_eq!(configured.description.as_deref(), Some("declared"));

    // And the tier rule the config table has always had, re-pinned here rather
    // than beside the parser: a config entry may still take a builtin's name,
    // which is the one shadowing this build does deliberately and silently.
    let overriding: Config = serde_json::from_value(json!({
        "command": { "init": { "template": "mine instead" } }
    }))
    .expect("the fixture is a config");
    let registry = Registry::build(&overriding, &project);
    assert_eq!(
        registry
            .get(INIT)
            .expect("init is still a command")
            .template,
        "mine instead",
        "a config command replaces the builtin it names"
    );
    assert!(registry.get("nope").is_none());

    let logged = capture.logged();
    io::stdout()
        .write_all(logged.as_bytes())
        .expect("the captured log is printable");
    assert!(
        logged.contains("a command file names a builtin command and was skipped")
            && logged.contains("command=init"),
        "the refused file is named: {logged}"
    );
    assert!(
        logged.contains("command=shared") && logged.contains("command=configured"),
        "every shadowed name is reported: {logged}"
    );
    assert!(
        !logged.contains("command=greet") && !logged.contains("command=only"),
        "and a name nothing collided over is reported by nobody: {logged}"
    );
}
