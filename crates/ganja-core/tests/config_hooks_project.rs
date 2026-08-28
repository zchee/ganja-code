//! A `hooks` block written in the **project** file is this session's hooks.
//!
//! The second of the three tier binaries; see `config_hooks_global.rs` for why
//! they are three.
//!
//! The project file here is `<project root>/ganja.toml` — the file the
//! ancestor walk in `config::project_files` finds, the same one `model` and
//! `permission` are written in. There is deliberately **no**
//! `.ganja/ganja.toml`: the `.ganja/` directory holds what a project *gives*
//! ganja (its skills), and the config file has always sat at the root beside
//! the rest of a checkout's configuration.

use std::{env, fs};

use ganja_core::Config;
use ganja_core::config::{CONFIG_ENV, CONFIG_HOME_ENV, HookCommand, HookHandler};
use ganja_core::hook::{HookEvent, Hooks};

#[test]
fn hooks_written_in_the_project_file_are_the_sessions_hooks() {
    let home = tempfile::tempdir().expect("a temporary directory");
    // A global home that exists and holds nothing, so the machine running this
    // suite cannot contribute a hook of its own — and so what is asserted below
    // came from the project file alone.
    let global = home.path().join("global");
    let project = home.path().join("project");
    fs::create_dir_all(&global).expect("the fixture config directory is creatable");
    fs::create_dir_all(project.join(".git")).expect("the fixture repository is creatable");
    fs::write(
        project.join("ganja.toml"),
        r#"
          [[hooks.Stop]]
          hooks = [{ type = "command", command = "project-hook", timeout = 3 }]
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

    let config = Config::load(&project).expect("the project file parses");

    let groups = config
        .hooks
        .get(HookEvent::Stop.name())
        .expect("the project tier's hooks reach the loaded config");
    assert_eq!(
        groups[0].hooks,
        vec![HookHandler::Command(HookCommand {
            command: "project-hook".to_owned(),
            timeout: Some(3),
        })]
    );

    let hooks = Hooks::new(&config.hooks, &project).expect("the block describes hooks");
    assert!(hooks.fires(HookEvent::Stop));
    assert!(!hooks.fires(HookEvent::PreToolUse));
}
