//! What the two homes do when they are the same directory.
//!
//! Three rosters resolve ganja's own pair of homes — skills
//! (`config::default_skill_dirs`), agent definition files (`agent.rs`) and
//! command files (`command.rs`) — and each of the three ends its walk with the
//! same guard: the project half is added only when the global half is not
//! already it. They collapse for somebody whose `GANJA_CONFIG_HOME` **is**
//! `<project root>/.ganja`, which is what a person whose worktree is their home
//! directory gets for free.
//!
//! Nothing pinned that guard. Dropping it reads one directory twice, finds
//! every file twice and reports each as shadowing itself — with a green suite,
//! because the second read of a file overwrites the first with itself. The
//! directly observable half is the skills walk, whose directory list is public;
//! the other two are pinned by what they produce, which is the half a reader of
//! the consolidated helper would break first.
//!
//! # Why this is one binary with one test in it
//!
//! `GANJA_CONFIG_HOME` is process-wide, and the value this case needs — a
//! config home inside the fixture project — is one no other test wants. The
//! arrangement is `tests/agent_files.rs`'s, for its reason.

use std::fs;
use std::path::Path;

use ganja_core::{AgentRegistry, Config, command, config};
use tempfile::TempDir;

#[test]
fn a_project_whose_own_ganja_directory_is_the_config_home_is_read_once() {
    let home = TempDir::new().expect("a temporary directory is creatable");
    // Canonical, because the guard compares paths as text: `Project::resolve`
    // resolves symbolic links and `config_home` hands back what the
    // environment said, so a fixture under macOS's symlinked `$TMPDIR` would
    // hold two spellings of one directory and never reach the branch this
    // pins. Real homes are not usually reached through a link; the guard's
    // textual comparison is what it is, and this test is about the guard.
    let root = fs::canonicalize(home.path()).expect("a temporary directory resolves");
    let project = root.join("project");
    let ganja = project.join(".ganja");
    fs::create_dir_all(&ganja).expect("the project's own home is creatable");

    // SAFETY: nothing else runs yet — this is the only test in this binary.
    // The config home is deliberately *inside* the project: that is the
    // collapse, and pointing XDG elsewhere keeps the machine running the
    // suite out of the answer.
    unsafe {
        std::env::set_var("GANJA_CONFIG_HOME", &ganja);
        std::env::set_var("XDG_CONFIG_HOME", root.join("xdg-config"));
        std::env::set_var("XDG_DATA_HOME", root.join("xdg-data"));
    }

    plant(
        &ganja.join("skills").join("solo"),
        "SKILL.md",
        "---\nname: solo\ndescription: The only skill here.\n---\n# solo\n",
    );
    plant(
        &ganja.join("agents"),
        "solo.md",
        "---\ndescription: The only agent here.\n---\nBe brief.\n",
    );
    plant(
        &ganja.join("commands"),
        "solo.md",
        "---\ndescription: The only command here.\n---\nDo the thing.\n",
    );

    // The load-bearing assertion: the pair really did collapse, rather than
    // the same path being listed twice.
    assert_eq!(
        config::default_skill_dirs(&project),
        vec![ganja.join("skills")],
        "one directory answers for both homes when they are one directory"
    );

    let config = Config::default();
    let agents = AgentRegistry::build(&config, &project).expect("the fixture agent is selectable");
    assert_eq!(
        agents.agents().iter().filter(|agent| agent.name == "solo").count(),
        1,
        "the definition file is one agent, not one agent found twice"
    );

    let commands = command::Registry::build(&config, &project);
    assert_eq!(
        commands.commands().iter().filter(|definition| definition.name == "solo").count(),
        1,
        "and the command file is one command"
    );
}

/// Writes `contents` to `directory/name`, creating whatever it needs.
fn plant(directory: &Path, name: &str, contents: &str) {
    fs::create_dir_all(directory).expect("the fixture directory is creatable");
    fs::write(directory.join(name), contents).expect("the fixture file is writable");
}
