//! Which tier reaches `[evaluate] screen` (**D567**), proved through the real
//! loader.
//!
//! The list decides which tool results leave the machine for TypeSafe's host,
//! so its tiers are not later-wins all the way down: the trusted tiers (the
//! global config home, then the file `GANJA_CONFIG` or `--config` names)
//! replace it whole, and each project file may only narrow it — losing every
//! server that file's own `mcp` table defines, then intersecting with its own
//! list. The unit tests beside the code pin each step against one file at a
//! time; what this binary adds is `Config::load_with` doing the walking, where
//! the environment decides which file is which tier and the project walk
//! decides which file is outer. Criterion 12 in particular is a claim about
//! that walk's order and can only be proved here.
//!
//! One test, one binary, on purpose: it mutates process-wide environment
//! variables, and a plain `cargo test` runs the tests inside a binary on
//! parallel threads. `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `HOME` are
//! redirected into a temporary tree — and `GANJA_CONFIG_HOME` and
//! `GANJA_CONFIG` cleared — so the machine running the suite cannot contribute
//! a config of its own.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::{env, fs};

use ganja_core::config::{CONFIG_ENV, CONFIG_HOME_ENV};
use ganja_core::judge::Screen;
use ganja_core::{Config, ConfigError, Overrides};
use ganja_testkit::LogCapture;

/// Writes `text` to `path`, creating whatever directories it needs.
fn plant(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the fixture tree is creatable");
    }
    fs::write(path, text).expect("the fixture file is writable");
}

/// The screen a step expects.
fn screening(webfetch: bool, websearch: bool, mcp: &[&str]) -> Screen {
    Screen {
        webfetch,
        websearch,
        mcp: mcp.iter().map(|server| (*server).to_owned()).collect::<BTreeSet<_>>(),
    }
}

/// Loads the config for `cwd` with `--config` naming `explicit` when given,
/// and hands back the resolved screen with every `evaluate.screen` warning the
/// load logged.
fn load(cwd: &Path, explicit: Option<&Path>) -> (Screen, Vec<String>) {
    let overrides =
        Overrides { config_file: explicit.map(Path::to_path_buf), ..Overrides::default() };
    let (capture, _guard) = LogCapture::install(tracing::Level::WARN);
    let config = Config::load_with(cwd, &overrides).expect("every tier parses");
    let warnings = capture
        .logged()
        .lines()
        .filter(|line| line.contains("evaluate.screen"))
        .map(str::to_owned)
        .collect();

    (config.evaluate_screen(), warnings)
}

/// What an `evaluate.screen` warning says when the project file's own `mcp`
/// table took the entry off the screen.
const REMOVED: &str = "evaluate.screen: this project file defines or redefines this server, so \
                       the entry is removed";
/// What it says when the file's list left out a source the screen held.
const NARROWED: &str = "evaluate.screen: this project file narrows the screened source away";
/// What it says when the file's list named a source the screen did not hold.
const NOT_IN_SCREEN: &str = "evaluate.screen: a project file may only narrow the screen the \
                             tiers above it left; this source is not in it and is ignored";

/// What the one warning says when a project file's `webfetch.allow_private`
/// takes webfetch out of a screen that still names it.
const LIFTED: &str = "webfetch.allow_private: this project file takes webfetch out of screening";

/// Asserts that `warnings` are exactly the ones `expected` lists, in order:
/// each one's text, the entry it names and the file it names.
fn assert_warned(warnings: &[String], expected: &[(&str, &str, &str)], step: &str) {
    assert_eq!(
        warnings.len(),
        expected.len(),
        "{step}: one warning per dropped entry: {warnings:?}"
    );
    for (line, (says, entry, path)) in warnings.iter().zip(expected) {
        assert!(line.contains(says), "{step}: the warning for {entry} says {says:?}: {line}");
        assert!(line.contains(&format!("entry={entry:?}")), "{step}: it names {entry}: {line}");
        assert!(line.contains(path), "{step}: it names {path}: {line}");
    }
}

/// A project file's path as the loader names it: the walk canonicalises its
/// start, so a warning spells the file the canonical way.
fn canonical(file: &Path) -> PathBuf {
    let parent = file.parent().expect("a fixture file has a directory");
    fs::canonicalize(parent)
        .expect("the fixture directory resolves")
        .join(file.file_name().expect("a fixture file has a name"))
}

#[test]
fn the_screen_list_crosses_the_tiers_under_the_narrow_only_rule() {
    let home = tempfile::tempdir().expect("a temporary directory");
    let config_home = home.path().join("config");
    let global = config_home.join("ganja").join("ganja.toml");
    let explicit = home.path().join("explicit.toml");
    // A checkout, so the project walk stops at its root instead of climbing
    // out of the fixture, with one directory inside it: the root's file is
    // the outer project file and the directory's is the inner one.
    let project = home.path().join("project");
    let inner = project.join("inner");
    let outer_file = project.join("ganja.toml");
    let inner_file = inner.join("ganja.toml");
    fs::create_dir_all(project.join(".git")).expect("the fixture repository is creatable");
    fs::create_dir_all(&inner).expect("the fixture directory is creatable");

    // SAFETY: this binary holds one test, so nothing else in the process is
    // reading the environment while it is being written.
    unsafe {
        env::set_var("XDG_CONFIG_HOME", &config_home);
        env::set_var("XDG_DATA_HOME", home.path().join("data"));
        // `~/.ganja` reaches past the XDG redirect through `HOME`, and
        // `GANJA_CONFIG_HOME` past everything: pin both, or a runner who
        // adopted either contributes a global config to every step below.
        env::set_var("HOME", home.path());
        env::remove_var(CONFIG_HOME_ENV);
        env::remove_var(CONFIG_ENV);
    }

    // Criterion 7: every shape the grammar accepts, from the global tier.
    plant(
        &global,
        "[evaluate]\nscreen = [\"webfetch\", \"websearch\", \"mcp:github\", \"mcp:plugin:foo:bar\"]\n",
    );
    let (screen, warnings) = load(&project, None);
    assert_eq!(screen, screening(true, true, &["github", "plugin:foo:bar"]));
    assert!(warnings.is_empty(), "a trusted tier is never warned about: {warnings:?}");

    // Default off: no tier writing the table screens nothing.
    plant(&global, "");
    assert_eq!(load(&project, None).0, Screen::default(), "absent at every tier is off");

    // Criterion 8: a refused entry fails the load naming the file, the key,
    // the entry and the accepted shapes — from any tier, the project's too.
    // `--config` names a file that has to exist, so it starts out empty.
    plant(&explicit, "");
    for (file, list, entry) in [
        (&global, r#"["web"]"#, "\"web\""),
        (&explicit, r#"["mcp:"]"#, "\"mcp:\""),
        (&outer_file, r#"["webfetch", "webfetch"]"#, "\"webfetch\""),
    ] {
        plant(file, &format!("[evaluate]\nscreen = {list}\n"));
        let error = Config::load_with(
            &project,
            &Overrides { config_file: Some(explicit.clone()), ..Overrides::default() },
        )
        .expect_err("the entry is refused at load");
        let ConfigError::Parse { path, message } = &error else {
            panic!("expected a parse failure for {list}, got {error:?}");
        };
        let expected_path =
            if file == &outer_file { canonical(&outer_file) } else { (*file).clone() };
        assert_eq!(path, &expected_path, "the complaint names the file that said it");
        assert!(message.contains("evaluate.screen"), "{message}");
        assert!(message.contains(entry), "{message}");
        assert!(message.contains(r#""webfetch", "websearch", or "mcp:<server>""#), "{message}");
        plant(file, "");
    }

    // Criterion 9: the trusted tiers replace, and `--config`'s empty list is
    // screening switched off.
    plant(&global, "[evaluate]\nscreen = [\"webfetch\"]\n");
    plant(&explicit, "[evaluate]\nscreen = [\"websearch\"]\n");
    assert_eq!(load(&project, Some(explicit.as_path())).0, screening(false, true, &[]));
    plant(&explicit, "[evaluate]\nscreen = []\n");
    assert_eq!(load(&project, Some(explicit.as_path())).0, Screen::default());
    // `GANJA_CONFIG` is the same tier as `--config`.
    plant(&explicit, "[evaluate]\nscreen = [\"websearch\"]\n");
    // SAFETY: as above.
    unsafe { env::set_var(CONFIG_ENV, &explicit) };
    assert_eq!(load(&project, None).0, screening(false, true, &[]));
    // SAFETY: as above.
    unsafe { env::remove_var(CONFIG_ENV) };
    plant(&explicit, "");

    // Criterion 10: the project tier narrows what the trusted tiers named,
    // and each entry it drops — on either side — is warned about once.
    plant(&global, "[evaluate]\nscreen = [\"webfetch\", \"websearch\", \"mcp:github\"]\n");
    let outer = canonical(&outer_file).display().to_string();
    let outer = outer.as_str();
    for (listed, expected, warned) in [
        (
            r#"["websearch"]"#,
            screening(false, true, &[]),
            &[(NARROWED, "webfetch", outer), (NARROWED, "mcp:github", outer)][..],
        ),
        (
            r#"["mcp:other"]"#,
            Screen::default(),
            &[
                (NARROWED, "webfetch", outer),
                (NARROWED, "websearch", outer),
                (NARROWED, "mcp:github", outer),
                (NOT_IN_SCREEN, "mcp:other", outer),
            ][..],
        ),
        (
            "[]",
            Screen::default(),
            &[
                (NARROWED, "webfetch", outer),
                (NARROWED, "websearch", outer),
                (NARROWED, "mcp:github", outer),
            ][..],
        ),
    ] {
        plant(&outer_file, &format!("[evaluate]\nscreen = {listed}\n"));
        let (screen, warnings) = load(&project, None);
        assert_eq!(screen, expected, "project lists {listed}");
        assert_warned(&warnings, warned, &format!("project lists {listed}"));
    }
    plant(&outer_file, "model = \"openai/gpt-5.6\"\n");
    let (screen, warnings) = load(&project, None);
    assert_eq!(screen, screening(true, true, &["github"]), "a silent project file inherits");
    assert!(warnings.is_empty(), "{warnings:?}");
    // A trusted tier that named nothing granted nothing.
    plant(&global, "");
    plant(&outer_file, "[evaluate]\nscreen = [\"webfetch\"]\n");
    let (screen, warnings) = load(&project, None);
    assert_eq!(screen, Screen::default(), "a project list alone screens nothing");
    assert_warned(&warnings, &[(NOT_IN_SCREEN, "webfetch", outer)], "a project list alone");

    // Criterion 11: a server the project file defines leaves the screen.
    plant(&global, "[evaluate]\nscreen = [\"webfetch\", \"mcp:github\"]\n");
    let github = "[mcp.github]\ntype = \"local\"\ncommand = [\"./github-mcp\"]\n";
    plant(&outer_file, github);
    let (screen, warnings) = load(&project, None);
    assert_eq!(screen, screening(true, false, &[]), "the redefined server is not screened");
    assert_warned(&warnings, &[(REMOVED, "mcp:github", outer)], "the file defines it");
    // Defining it and listing it too is one line for that entry, the removal:
    // the trusted tier did name it, so no second line may say otherwise.
    plant(&outer_file, &format!("{github}\n[evaluate]\nscreen = [\"webfetch\", \"mcp:github\"]\n"));
    let (screen, warnings) = load(&project, None);
    assert_eq!(screen, screening(true, false, &[]), "listing it does not keep it");
    assert_warned(&warnings, &[(REMOVED, "mcp:github", outer)], "the file defines and lists it");
    plant(&outer_file, github);

    // Criterion 12: the outer file's definition is permanent — the inner file,
    // which the walk merges after it, lists the server and cannot restore it.
    // Its line says the source is not in the screen the files above it left,
    // which is true, and not that the person never named it, which is not.
    plant(&inner_file, "[evaluate]\nscreen = [\"webfetch\", \"mcp:github\"]\n");
    let (screen, warnings) = load(&inner, None);
    assert_eq!(screen, screening(true, false, &[]), "the inner list cannot put it back");
    let inner_path = canonical(&inner_file).display().to_string();
    assert_warned(
        &warnings,
        &[(REMOVED, "mcp:github", outer), (NOT_IN_SCREEN, "mcp:github", inner_path.as_str())],
        "the outer file's definition is merged first, and the inner file's list second",
    );
    plant(&inner_file, "");

    // A project file that opens private fetches keeps that power: the key's
    // meaning and tier are unchanged. But every page fetched with the guard
    // lifted is stamped `private_allowed: true` and never screened, so the
    // file takes webfetch out of screening — once, by name, whenever the
    // screen that survives the file's own narrowing still names webfetch.
    plant(&global, "[evaluate]\nscreen = [\"webfetch\", \"websearch\"]\n");
    for (project_text, expected, warned) in [
        ("[webfetch]\nallow_private = true\n", screening(true, true, &[]), true),
        // The file narrows webfetch away itself: its own list's warning says
        // so, and there is nothing left for the key to take out.
        (
            "[webfetch]\nallow_private = true\n\n[evaluate]\nscreen = [\"websearch\"]\n",
            screening(false, true, &[]),
            false,
        ),
        ("[webfetch]\nallow_private = false\n", screening(true, true, &[]), false),
    ] {
        plant(&outer_file, project_text);
        let (capture, _guard) = LogCapture::install(tracing::Level::WARN);
        let config = Config::load_with(&project, &Overrides::default()).expect("every tier parses");
        let lifted: Vec<String> = capture
            .logged()
            .lines()
            .filter(|line| line.contains(LIFTED))
            .map(str::to_owned)
            .collect();

        assert_eq!(config.evaluate_screen(), expected, "{project_text:?}");
        assert_eq!(
            config.webfetch_allows_private(),
            project_text.contains("true"),
            "{project_text:?}: the project file's value stands"
        );
        assert_eq!(lifted.len(), usize::from(warned), "{project_text:?}: {lifted:?}");
        assert!(lifted.iter().all(|line| line.contains(outer)), "it names the file: {lifted:?}");
    }
    // With no trusted screen naming webfetch there is nothing to take out.
    plant(&global, "[evaluate]\nscreen = [\"websearch\"]\n");
    plant(&outer_file, "[webfetch]\nallow_private = true\n");
    let (capture, _guard) = LogCapture::install(tracing::Level::WARN);
    Config::load_with(&project, &Overrides::default()).expect("every tier parses");
    assert!(!capture.logged().contains(LIFTED), "{}", capture.logged());
}
