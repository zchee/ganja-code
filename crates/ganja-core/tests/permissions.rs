//! Permission rules, from a working directory to a file and back.
//!
//! The unit tests point the store at a path directly; this one goes the whole
//! way round — resolve the project from a directory, find its data directory
//! under the XDG data home, store an answer, and see a later session honour it.
//!
//! It lives out here rather than beside the module because it has to set
//! `XDG_DATA_HOME`, which is process-wide: an integration test gets a process of
//! its own, so the credential tests cannot see this one move the data home out
//! from under them. Everything in this file is therefore one test.

use std::sync::Arc;
use std::{fs, thread};

use ganja_core::permission::{Action, Decision, FILE, Permissions, Rule, VERSION};
use ganja_core::project::Project;
use serde_json::json;
use tempfile::TempDir;

/// Answers that overlap, to prove a rename cannot leave a half-written store.
const OVERLAPPING: usize = 16;

#[test]
fn an_answer_is_stored_per_project_under_the_data_home() {
    // SAFETY: nothing else runs yet — this is the only test in this binary, and
    // it has not started a thread.
    let home = unsafe { ganja_testkit::redirect_xdg_data_home() };

    let workspace = TempDir::new().expect("a temporary directory is creatable");
    let api = workspace.path().join("api");
    let nested = api.join("crates").join("core");
    let other = workspace.path().join("other");
    fs::create_dir_all(&nested).expect("the fixture tree is creatable");
    fs::create_dir_all(&other).expect("the fixture tree is creatable");
    fs::create_dir(api.join(".git")).expect("the fixture repository is creatable");

    let command = json!({ "command": "cargo test --release" });

    // Nothing is stored, so the defaults decide.
    let mut permissions = Permissions::load(&nested);
    assert_eq!(permissions.gate("shell", &command).action, Decision::Ask);
    assert_eq!(permissions.gate("read", &json!({})).action, Decision::Allow);

    let decision = permissions.gate("shell", &command);
    permissions.remember(&decision);
    assert_eq!(permissions.gate("shell", &command).action, Decision::Allow);
    drop(permissions);

    // The answer landed where the project says its state belongs.
    let store =
        Project::resolve(&nested).data_dir().expect("the data directory resolves").join(FILE);
    assert!(store.starts_with(home.path().join("ganja").join("project")), "{}", store.display());
    let written: serde_json::Value =
        serde_json::from_slice(&fs::read(&store).expect("the store exists"))
            .expect("the store is JSON");
    assert_eq!(written["version"], VERSION);
    assert_eq!(
        written["rules"],
        json!([{ "permission": "shell", "pattern": "cargo test *", "action": "allow" }])
    );

    // A later session anywhere in the same checkout honours it; the answer
    // belongs to the project, not to the directory it was given in.
    for directory in [api.as_path(), nested.as_path()] {
        assert_eq!(
            Permissions::load(directory)
                .gate("shell", &json!({ "command": "cargo test --lib" }))
                .action,
            Decision::Allow,
            "{}",
            directory.display()
        );
    }

    // Another project is another set of rules.
    assert_eq!(
        Permissions::load(&other).gate("shell", &command).action,
        Decision::Ask,
        "an answer given in one project must not answer for another"
    );

    // Answers given at the same moment may lose one to the last writer, but
    // must never leave the store unreadable or half written.
    let nested = Arc::new(nested);
    let threads: Vec<_> = (0..OVERLAPPING)
        .map(|index| {
            let nested = Arc::clone(&nested);
            thread::spawn(move || {
                let mut permissions = Permissions::load(&nested);
                let decision =
                    permissions.gate("shell", &json!({ "command": format!("tool{index} run") }));
                permissions.remember(&decision);
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("no answer panicked");
    }

    let after: serde_json::Value =
        serde_json::from_slice(&fs::read(&store).expect("the store exists"))
            .expect("overlapping answers left the store readable");
    assert_eq!(after["version"], VERSION);
    let rules: Vec<Rule> =
        serde_json::from_value(after["rules"].clone()).expect("every stored rule is still a rule");
    assert!(rules.iter().all(|rule| rule.action == Action::Allow));
    assert!(
        rules.iter().any(|rule| rule.pattern == "cargo test *" && rule.permission == "shell"),
        "the answer from before the crowd is still there"
    );
    assert!(
        fs::read_dir(store.parent().expect("the store has a directory"))
            .expect("the directory lists")
            .filter_map(Result::ok)
            .all(|entry| entry.file_name() == FILE),
        "no temporary file should outlive a write"
    );
}
