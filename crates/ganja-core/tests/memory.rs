//! Per-project memory: the prompt it adds, and the one door it opens
//! (**D478**).
//!
//! A session whose config says `memory: true` is told what its project's
//! `MEMORY.md` holds and how to keep it, and may write the files it is told to
//! keep — which live under the project's own data directory, **outside** the
//! worktree, where every other write asks first. Both halves are asserted
//! here against the real seams: `instruction::suffix`, which is what the
//! engine composes a request's system prompt from, and `Permissions::gate`,
//! which is what decides a call.
//!
//! # Why this is one binary with one test in it
//!
//! Three pieces of process-wide state are needed at once. `XDG_DATA_HOME`
//! decides where a project's data directory is, and nothing here may read or
//! write the real user's memory; the working directory decides which project
//! this is, and `agent::Registry::build` reads the process's own; and the
//! `.git` marker that makes the fixture a project root has to be inside that
//! directory. So the suite is one `#[tokio::test]` calling one function per
//! behaviour, in order, the way `tests/config_schema.rs` is arranged and for
//! the same reason.
//!
//! # The evidence the door rests on
//!
//! [`a_write_outside_the_worktree_asks_before_any_door_is_opened`] is not
//! decoration: the design question W3 had to answer first was whether a write
//! under the memory root passes the gate as it stands. It does not — it asks,
//! twice over — and that is what makes `agent::memory_door` necessary rather
//! than tidy. The test stays as the pin for it, so a later change that made
//! outside writes free would fail here rather than quietly widen the feature.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ganja_core::permission::{Decision, Permissions};
use ganja_core::project::Project;
use ganja_core::tool::Registry;
use ganja_core::{AgentRegistry, Config, Engine};
use ganja_testkit::ScriptedProvider;
use serde_json::json;

/// The model every composition here is for. Which one decides only the base
/// prompt, which this suite never looks at.
const MODEL: &str = "fake-1";

/// The first line of the memory section, as `instruction` composes it.
const MEMORY_HEAD: &str =
    "Project memory: durable facts about this project, kept outside the repository.";

/// A config with memory switched on and nothing else said.
fn asked_for_memory() -> Config {
    Config { memory: Some(true), ..Config::default() }
}

/// Where this project's memory lives, resolved the way the prompt and the
/// door both resolve it.
fn memory_dir(root: &Path) -> PathBuf {
    Project::resolve(root).data_dir().expect("the fixture has a data home").join("memory")
}

/// The rules a session at `root` runs under, with `config`'s agents beneath
/// the answers a person would have given — which is exactly how the engine
/// assembles them.
fn permissions(root: &Path, config: &Config) -> Permissions {
    let registry =
        AgentRegistry::from_config(config).expect("the fixture config resolves an agent");
    let mut permissions = Permissions::load(root);
    permissions.set_baseline(
        registry.get(registry.default_agent()).expect("the default agent exists").rules.clone(),
    );

    permissions
}

/// What the gate says about writing `path`.
fn writing(permissions: &Permissions, path: &Path) -> Decision {
    permissions.gate("write", &json!({ "filePath": path.to_string_lossy() })).action
}

/// Writes `text` at `path`, creating whatever directories it needs.
fn plant(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("the fixture tree is creatable");
    }
    std::fs::write(path, text).expect("the fixture file is writable");
}

#[tokio::test]
async fn project_memory_is_off_until_asked_for_and_scoped_to_its_own_directory() {
    let home = tempfile::tempdir().expect("a temporary directory");
    let checkout = tempfile::tempdir().expect("a temporary directory");
    std::fs::create_dir_all(checkout.path().join(".git")).expect("the fixture repository");
    // SAFETY: this binary holds one test, so nothing else in the process is
    // reading the environment while it is written.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", home.path());
        std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
        std::env::set_var("HOME", home.path());
        std::env::remove_var("GANJA_CONFIG_HOME");
        std::env::remove_var("GANJA_CONFIG");
    }
    std::env::set_current_dir(checkout.path()).expect("the fixture is enterable");
    // What the engine would read back: on a platform whose temporary
    // directory is a symbolic link, the spelling the fixture was created
    // under is not the one anything resolves to.
    let root = std::env::current_dir().expect("the fixture is readable");

    a_write_outside_the_worktree_asks_before_any_door_is_opened(&root);
    the_suffix_says_nothing_about_memory_until_a_config_asks_for_it(&root);
    a_project_with_no_index_yet_is_still_told_how_to_start_one(&root);
    the_suffix_carries_the_index_and_the_upkeep_block_when_memory_is_on(&root);
    a_write_under_the_memory_root_runs_unasked_when_memory_is_on(&root);
    a_subagent_is_given_no_door_to_write_the_memory_it_was_shown(&root);
    what_memory_adds_to_the_prompt_is_priced_as_instructions(&root).await;
}

/// The finding the door rests on: with memory off, a write under the memory
/// root is a write outside the project like any other, and the gate asks.
fn a_write_outside_the_worktree_asks_before_any_door_is_opened(root: &Path) {
    let permissions = permissions(root, &Config::default());

    assert_eq!(
        writing(&permissions, &memory_dir(root).join("MEMORY.md")),
        Decision::Ask,
        "nothing about the memory root is special until a config asks for memory"
    );
    assert_eq!(
        writing(&permissions, &root.parent().unwrap_or(root).join("notes.md")),
        Decision::Ask,
        "and neither is anywhere else outside the worktree"
    );
    assert_eq!(
        writing(&permissions, &root.join("src").join("main.rs")),
        Decision::Ask,
        "a write inside it asks too — this build gates the tool, not only the place"
    );
}

/// The default-off regression pin (AC3): a config that never mentions memory
/// composes exactly what a config that switched it off does, and neither
/// carries a word about it.
fn the_suffix_says_nothing_about_memory_until_a_config_asks_for_it(root: &Path) {
    plant(&memory_dir(root).join("MEMORY.md"), "- planted, and unread");

    let absent = ganja_core::instruction::suffix(&Config::default(), root, MODEL)
        .expect("the environment block always says something");
    let refused = ganja_core::instruction::suffix(
        &Config { memory: Some(false), ..Config::default() },
        root,
        MODEL,
    )
    .expect("the environment block always says something");

    assert_eq!(absent, refused, "saying no and saying nothing must compose the same prompt");
    assert!(
        !absent.contains(MEMORY_HEAD) && !absent.contains("planted, and unread"),
        "a session that did not ask for memory is told nothing about it: {absent}"
    );
}

/// Bootstrapping: the upkeep block arrives before there is anything to keep,
/// because a model never told how to start an index can never write the first
/// fact.
fn a_project_with_no_index_yet_is_still_told_how_to_start_one(root: &Path) {
    let index = memory_dir(root).join("MEMORY.md");
    std::fs::remove_file(&index).expect("the planted index is removable");

    let suffix = ganja_core::instruction::suffix(&asked_for_memory(), root, MODEL)
        .expect("the environment block always says something");

    assert!(suffix.contains(MEMORY_HEAD), "{suffix}");
    assert!(suffix.contains("Keeping it: record a fact"), "{suffix}");
    assert!(
        !suffix.contains(&format!("Instructions from: {}", index.display())),
        "a file that is not there is not quoted: {suffix}"
    );
}

/// And with an index planted, the whole of it reaches the prompt, under the
/// path it really sits at, with the upkeep block after it.
fn the_suffix_carries_the_index_and_the_upkeep_block_when_memory_is_on(root: &Path) {
    let index = memory_dir(root).join("MEMORY.md");
    plant(&index, "- deploys are manual, by hand, on fridays");

    let suffix = ganja_core::instruction::suffix(&asked_for_memory(), root, MODEL)
        .expect("the environment block always says something");

    assert!(
        suffix.contains(&format!("Instructions from: {}\n- deploys are manual", index.display())),
        "{suffix}"
    );
    let facts = suffix.find("deploys are manual").expect("the index is quoted");
    let upkeep = suffix.find("Keeping it: record a fact").expect("the upkeep block is composed");
    assert!(facts < upkeep, "the facts come first: {suffix}");
    assert!(
        suffix.contains("Never record a secret."),
        "the prohibition travels with the invitation: {suffix}"
    );
}

/// The door itself, at the gate rather than in the rule table: recording a
/// fact runs unasked, and nothing around the memory root moves.
fn a_write_under_the_memory_root_runs_unasked_when_memory_is_on(root: &Path) {
    let permissions = permissions(root, &asked_for_memory());
    let memory = memory_dir(root);

    assert_eq!(
        writing(&permissions, &memory.join("MEMORY.md")),
        Decision::Allow,
        "the index is the model's to keep"
    );
    assert_eq!(
        writing(&permissions, &memory.join("deploys.md")),
        Decision::Allow,
        "and so is a topic file that does not exist yet"
    );
    assert_eq!(
        writing(&permissions, &memory.join("topics").join("style.md")),
        Decision::Allow,
        "one directory deeper is still the memory root"
    );
    assert_eq!(
        writing(
            &permissions,
            &memory.parent().expect("the memory root has a parent").join("permissions.json")
        ),
        Decision::Ask,
        "one directory up is the permission store, and is nobody's to rewrite"
    );
    assert_eq!(
        writing(&permissions, &root.join("src").join("main.rs")),
        Decision::Ask,
        "and the worktree's own posture is untouched"
    );
}

/// A child inherits refusals and never allows (AC4). It may read what the
/// prompt showed it — the location gate travels — and its own write falls back
/// to asking, which for unattended work is where it belongs.
fn a_subagent_is_given_no_door_to_write_the_memory_it_was_shown(root: &Path) {
    let config = asked_for_memory();
    let parent = permissions(root, &config);
    let registry =
        AgentRegistry::from_config(&config).expect("the fixture config resolves an agent");
    let child = parent.derive_subagent(
        registry.get("general").expect("the general subagent is builtin").rules.clone(),
    );

    assert_eq!(
        writing(&child, &memory_dir(root).join("MEMORY.md")),
        Decision::Ask,
        "a subagent may not rewrite the project's memory unwatched"
    );
}

/// The honesty clause (AC5): the section's weight lands in `/context`'s
/// instruction category, through the same accessor every other surface reads.
async fn what_memory_adds_to_the_prompt_is_priced_as_instructions(root: &Path) {
    let breakdown = |config: &Config| {
        let suffix = ganja_core::instruction::suffix(config, root, MODEL);
        let (provider, _requests) = ScriptedProvider::new(Vec::new());
        Engine::new(provider, MODEL, Arc::new(Registry::with_builtins()), Permissions::default())
            .with_system_parts(None, suffix)
    };

    let off = breakdown(&Config::default()).context_breakdown().await;
    let on = breakdown(&asked_for_memory()).context_breakdown().await;

    assert!(
        on.instructions > off.instructions,
        "memory is instruction weight and has to show as some: {on:?} against {off:?}"
    );
    assert_eq!(
        on.system_prompt, off.system_prompt,
        "and it is not smuggled into the environment block: {on:?}"
    );
    assert_eq!(on.skills, off.skills, "nor into the skills block: {on:?}");
}
