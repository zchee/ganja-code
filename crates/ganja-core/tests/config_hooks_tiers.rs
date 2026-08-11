//! Both tiers write hooks: the project's list replaces the global one **for
//! that event**, and leaves the events it said nothing about alone.
//!
//! The third of the three tier binaries; see `config_hooks_global.rs` for why
//! they are three.
//!
//! The rule is the `mcp` arm's, applied for a reason of its own: these are
//! commands. Concatenating would make a hook written in somebody's home
//! directory unremovable from a checkout — a project that lists what to run
//! before a tool call would still be running the global one underneath it, with
//! nothing it could write to stop that.

use std::{env, fs};

use ganja_core::{
    Config,
    config::{CONFIG_ENV, CONFIG_HOME_ENV, HookCommand, HookHandler},
    hook::{HookEvent, Hooks},
};

/// The one handler in `groups`' first entry, as a command line.
fn only_command(groups: &[ganja_core::config::HookMatcher]) -> &str {
    assert_eq!(groups.len(), 1, "one group: {groups:?}");
    assert_eq!(groups[0].hooks.len(), 1, "one handler: {groups:?}");
    let HookHandler::Command(HookCommand { command, .. }) = &groups[0].hooks[0];

    command
}

#[test]
fn a_project_hook_replaces_the_global_one_for_its_own_event_only() {
    let home = tempfile::tempdir().expect("a temporary directory");
    let global = home.path().join("global");
    let project = home.path().join("project");
    fs::create_dir_all(&global).expect("the fixture config directory is creatable");
    fs::create_dir_all(project.join(".git")).expect("the fixture repository is creatable");
    fs::write(
        global.join("ganja.jsonc"),
        r#"{
          "hooks": {
            "PreToolUse": [{ "hooks": [{ "type": "command", "command": "global-pre" }] }],
            "SessionEnd": [{ "hooks": [{ "type": "command", "command": "global-end" }] }]
          }
        }"#,
    )
    .expect("the fixture file is writable");
    fs::write(
        project.join("ganja.jsonc"),
        r#"{
          "hooks": {
            "PreToolUse": [{ "hooks": [{ "type": "command", "command": "project-pre" }] }]
          }
        }"#,
    )
    .expect("the fixture file is writable");

    // SAFETY: this binary holds one test, so nothing else in the process is
    // reading the environment while it is being written.
    unsafe {
        env::set_var(CONFIG_HOME_ENV, &global);
        env::set_var("XDG_DATA_HOME", home.path().join("data"));
        env::remove_var(CONFIG_ENV);
    }

    let config = Config::load(&project).expect("both files parse");

    assert_eq!(
        only_command(&config.hooks[HookEvent::PreToolUse.name()]),
        "project-pre",
        "the closest tier's list is the list, not an addition to the one above it"
    );
    assert_eq!(
        only_command(&config.hooks[HookEvent::SessionEnd.name()]),
        "global-end",
        "an event the project said nothing about keeps the global tier's hooks"
    );

    let hooks = Hooks::new(&config.hooks, &project).expect("the merged block describes hooks");
    assert!(hooks.fires(HookEvent::PreToolUse) && hooks.fires(HookEvent::SessionEnd));
}
