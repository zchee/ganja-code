//! A `hooks` block written in the **global** home is this session's hooks.
//!
//! One of three binaries — with `config_hooks_project.rs` and
//! `config_hooks_tiers.rs` — proving that hooks ride the config discovery that
//! already exists rather than a loader of their own. Split three ways for the
//! house rule every other environment-mutating suite here follows: each rewrites
//! `GANJA_CONFIG_HOME`, and a plain `cargo test` runs the tests inside one
//! binary on parallel threads.
//!
//! The global file this plants is `<config home>/ganja.toml` — the same
//! directory the global `AGENTS.md` and `skills/` sit in, resolved once by
//! `config::config_home`.
//!
//! Written in TOML, where a list of groups is an **array of tables**. The
//! shape a `hooks` block takes is the one thing about it a person migrating
//! has to re-learn, so at least one tier proves it end to end rather than
//! only in the loader's own unit tests; `config_hooks_tiers.rs` then proves a
//! tier in each dialect stacks against the other.

use std::{env, fs};

use ganja_core::{
    Config,
    config::{CONFIG_ENV, CONFIG_HOME_ENV, HookCommand, HookHandler},
    hook::{HookEvent, Hooks},
};

#[test]
fn hooks_written_in_the_global_home_are_the_sessions_hooks() {
    let home = tempfile::tempdir().expect("a temporary directory");
    let global = home.path().join("global");
    let project = home.path().join("project");
    // A checkout, so the project walk stops here instead of climbing out of the
    // fixture — and one with no config of its own, so what is read is the
    // global tier and nothing else.
    fs::create_dir_all(project.join(".git")).expect("the fixture repository is creatable");
    fs::create_dir_all(&global).expect("the fixture config directory is creatable");
    fs::write(
        global.join("ganja.toml"),
        r#"
          [[hooks.PreToolUse]]
          matcher = "edit"

          [[hooks.PreToolUse.hooks]]
          type = "command"
          command = "global-hook"
        "#,
    )
    .expect("the fixture file is writable");

    // SAFETY: this binary holds one test, so nothing else in the process is
    // reading the environment while it is being written.
    unsafe {
        env::set_var(CONFIG_HOME_ENV, &global);
        env::set_var("XDG_DATA_HOME", home.path().join("data"));
        env::remove_var(CONFIG_ENV);
    }

    let config = Config::load(&project).expect("the global file parses");

    let groups = config
        .hooks
        .get(HookEvent::PreToolUse.name())
        .expect("the global tier's hooks reach the loaded config");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].matcher.as_deref(), Some("edit"));
    assert_eq!(
        groups[0].hooks,
        vec![HookHandler::Command(HookCommand {
            command: "global-hook".to_owned(),
            timeout: None,
        })]
    );
    // And the engine really would fire them: what a frontend builds from that
    // block is a registry with this event in it.
    let hooks = Hooks::new(&config.hooks, &project).expect("the block describes hooks");
    assert!(hooks.fires(HookEvent::PreToolUse));
    assert!(!hooks.fires(HookEvent::Stop));
}
