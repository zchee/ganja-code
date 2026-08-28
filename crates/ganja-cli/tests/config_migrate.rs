//! `ganja config migrate`, driven through the built binary.
//!
//! What the translation does with a shape is settled beside the translation,
//! in `src/migrate_tests.rs`. What is settled here is everything the
//! translation cannot see: which file discovery picks, where the result lands,
//! what a run refuses to overwrite, that the source comes out of it untouched,
//! and that the file this wrote is one the real loader reads.
//!
//! Every invocation redirects `XDG_CONFIG_HOME` into a fixture — and pins
//! `HOME` while clearing `GANJA_CONFIG_HOME`, because the `--global` source
//! resolves through ganja's config-home seam and two of that seam's three
//! places reach past the XDG redirect — so the machine running the suite
//! cannot contribute a config of its own and nothing here can read or write a
//! real user's. That redirection is per-subprocess, never a `set_var`, so
//! these can share a binary.

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use ganja_testkit::temp_dir as temporary;
use predicates::prelude::*;
use tempfile::TempDir;

/// A config carrying one of every shape that can go wrong in translation: an
/// ordered `permission` table whose first key is an object, a map keyed by a
/// name somebody chose, the array-of-tables `hooks` block, a null, and two
/// comments.
const FIXTURE: &str = r#"{
  // The model this checkout uses.
  "model": "anthropic/claude-sonnet-5",
  "permission": {
    "bash": { "git status": "allow", "*": "deny" },
    "webfetch": "allow",
    "edit": "ask"
  },
  "theme": null,
  "mcp": {
    "docs": { "type": "local", "command": ["bun", "x", "docs"], "timeout": 1234 }
  },
  /* and a block
     comment */
  "hooks": {
    "Stop": [
      { "hooks": [{ "type": "command", "command": "./done.sh", "timeout": 5 }] }
    ]
  }
}
"#;

/// Writes `text` to `path`, creating whatever directories it needs.
fn plant(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the fixture tree is creatable");
    }
    fs::write(path, text).expect("the fixture file is writable");
}

/// A checkout, so the project walk stops at `root` instead of climbing out of
/// the fixture and into whatever the temporary directory sits under.
fn checkout(root: &Path) {
    fs::create_dir_all(root.join(".git")).expect("the fixture repository is creatable");
}

/// An invocation with its own config and data homes, run from `cwd`.
fn ganja(home: &TempDir, cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
    command
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        // The `--global` source resolves through ganja's config-home seam,
        // which reaches past the XDG redirect: `~/.ganja` through `HOME`, and
        // `GANJA_CONFIG_HOME` past everything. Pin both, or a runner holding
        // either would have this suite read their real home.
        .env("HOME", home.path())
        .env_remove(ganja_core::config::CONFIG_HOME_ENV)
        .env_remove(ganja_core::config::CONFIG_ENV)
        .current_dir(cwd);

    command
}

/// The same, already pointed at the subcommand under test.
fn migrate(home: &TempDir, cwd: &Path) -> Command {
    let mut command = ganja(home, cwd);
    command.args(["config", "migrate"]);

    command
}

/// The path the command will print for `path`.
///
/// The project walk canonicalises the way `Project::resolve` does, and a
/// macOS temporary directory is reached through a symlink — so the string a
/// test builds from `TempDir::path` is not the string the command prints.
fn resolved(path: &Path) -> std::path::PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Where the config home's own files live inside a fixture home.
fn config_home(home: &TempDir) -> std::path::PathBuf {
    home.path().join("config").join("ganja")
}

/// The whole claim of the command, asserted the only way that settles it: the
/// file it wrote is loaded by the *real* loader, in a separate process, and
/// the rules come out in the order the source wrote them.
///
/// `ganja skills` is the cheapest subcommand that calls `Config::load`, so a
/// file this build could not read would fail it. The command itself already
/// refuses to write a file that does not read back as its source — that check
/// is what a zero exit code above means — and this is the independent half:
/// a second process, no shared state, reading the file off the disk.
///
/// The source is removed before that second process runs, because that is the
/// workflow the command's own closing line describes: this build refuses a
/// directory holding a legacy file at all, so "the written file loads" is a
/// claim about the tree somebody has after they finish the migration, not
/// during it.
#[test]
fn the_written_file_is_one_the_real_loader_reads() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(&project.path().join("ganja.jsonc"), FIXTURE);

    migrate(&home, project.path()).assert().success();

    let written = fs::read_to_string(project.path().join("ganja.toml")).expect("it was written");
    assert!(
        written.contains(r#"bash = { "git status" = "allow", "*" = "deny" }"#),
        "the ordered table is inline, so `bash` keeps the position it was written in:\n{written}"
    );
    assert!(
        written.find("bash").unwrap() < written.find("webfetch").unwrap(),
        "document order, not sorted order:\n{written}"
    );

    fs::remove_file(project.path().join("ganja.jsonc")).expect("the source is removable");
    ganja(&home, project.path()).arg("skills").assert().success();
}

/// The source is not this command's to remove, rename or rewrite — the loader
/// prefers the `ganja.toml` beside it either way, so there is nothing to gain
/// by hurrying it out of the tree and a comparison to lose.
#[test]
fn the_source_comes_out_byte_for_byte() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    let source = project.path().join("ganja.jsonc");
    plant(&source, FIXTURE);

    migrate(&home, project.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("was left exactly as it is"));

    assert_eq!(fs::read_to_string(&source).expect("the source is still there"), FIXTURE);
}

/// A destination that already exists is refused rather than overwritten, and
/// the run that refuses writes nothing at all.
#[test]
fn an_occupied_destination_is_refused() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(&project.path().join("ganja.jsonc"), FIXTURE);
    plant(&project.path().join("ganja.toml"), "# written by hand\nmodel = \"anthropic/other\"\n");

    migrate(&home, project.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));

    assert_eq!(
        fs::read_to_string(project.path().join("ganja.toml")).expect("it is still there"),
        "# written by hand\nmodel = \"anthropic/other\"\n",
        "the file that was there is the file that is there"
    );
}

/// A comment cannot survive a translation into a format whose value model has
/// none, so the one thing left worth doing is saying which lines held one.
#[test]
fn every_comment_line_is_named_in_a_warning() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(&project.path().join("ganja.jsonc"), FIXTURE);

    migrate(&home, project.path()).assert().success().stderr(predicate::str::contains(
        "comments do not survive the translation; these lines held one: 2, 13, 14",
    ));
}

/// A dry run prints everything the real run would — the table, the warning and
/// both resolved paths — and writes nothing.
#[test]
fn a_dry_run_prints_the_destination_and_writes_nothing() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(&project.path().join("ganja.jsonc"), FIXTURE);

    migrate(&home, project.path()).arg("--dry-run").assert().success().stdout(
        predicate::str::contains(format!(
            "would write {}",
            resolved(project.path()).join("ganja.toml").display()
        ))
        .and(predicate::str::contains("dry run — nothing written"))
        .and(predicate::str::contains("[permission]")),
    );

    assert!(!project.path().join("ganja.toml").exists(), "a dry run writes nothing");
}

/// The countermeasure to the pre-mortem's second failure: a refusal fires from
/// one tier, the migration fixes that tier, and the next launch names the
/// next file. One run says how many there are.
#[test]
fn the_closing_line_names_the_other_legacy_files_the_walk_can_still_see() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(&project.path().join("ganja.jsonc"), FIXTURE);
    let global = config_home(&home).join("ganja.json");
    plant(&global, r#"{ "theme": "gruvbox" }"#);

    migrate(&home, project.path()).assert().success().stdout(
        predicate::str::contains(format!(
            "still legacy, and still refused by this build: {}",
            global.display()
        ))
        .and(predicate::str::contains("ganja config migrate --file")),
    );
}

/// Nothing else to say, and so nothing said: the closing line is silent when
/// this run was the whole story.
#[test]
fn the_closing_line_is_silent_when_there_is_nothing_else() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(&project.path().join("ganja.jsonc"), FIXTURE);

    migrate(&home, project.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("still legacy").not());
}

/// `--global` reads the config home's legacy file and writes beside it — the
/// same directory the next launch reads the global tier through, wherever
/// `GANJA_CONFIG_HOME` or a `~/.ganja` has moved it.
#[test]
fn a_global_migration_writes_the_config_home() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(&config_home(&home).join("ganja.jsonc"), FIXTURE);
    // A project file too, to prove `--global` reads past it rather than
    // picking whichever is closer.
    plant(&project.path().join("ganja.json"), r#"{ "theme": "one" }"#);

    migrate(&home, project.path()).arg("--global").assert().success();

    assert!(config_home(&home).join("ganja.toml").is_file());
    assert!(!project.path().join("ganja.toml").exists(), "the project tier was not the target");
}

/// A named file is the whole import, and the result still lands beside it:
/// a caller who said which file to read did not ask for it to be moved.
#[test]
fn a_named_file_is_migrated_where_it_sits() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    let elsewhere = project.path().join("somewhere").join("ganja.jsonc");
    plant(&elsewhere, FIXTURE);

    migrate(&home, project.path())
        .args(["--file", &elsewhere.display().to_string()])
        .assert()
        .success();

    assert!(elsewhere.with_file_name("ganja.toml").is_file());
    assert!(!project.path().join("ganja.toml").exists());
}

/// The closest file at or above the working directory is the source, because
/// it is the one the loader's project walk lets win.
#[test]
fn the_closest_file_in_the_walk_is_the_source() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(&project.path().join("ganja.jsonc"), r#"{ "theme": "root" }"#);
    let inner = project.path().join("crate");
    plant(&inner.join("ganja.jsonc"), r#"{ "theme": "inner" }"#);

    migrate(&home, &inner).assert().success();

    assert!(
        fs::read_to_string(inner.join("ganja.toml"))
            .expect("the inner one was migrated")
            .contains("inner")
    );
    assert!(!project.path().join("ganja.toml").exists());
}

/// A run with nothing to do says where it looked and what it would have read,
/// because the alternative is a person guessing at the name of a file.
#[test]
fn a_project_with_no_legacy_file_says_what_it_looked_for() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());

    migrate(&home, project.path()).assert().failure().stderr(
        predicate::str::contains("ganja.jsonc")
            .and(predicate::str::contains("ganja.json"))
            .and(predicate::str::contains("--global")),
    );
}

/// A source this build would refuse to *load* is refused before it is
/// translated: a `ganja.toml` the next launch declines is exactly the file
/// this command exists not to produce.
///
/// Two cases, and the pair is the point. The MCP entry is the one that was
/// always caught — `McpServer::check` has a public authority anyone can call,
/// so this command could make that refusal even while it read the legacy
/// dialect itself. The hooks matcher is the one that was not: the other six
/// post-decode checks are private to the loader, so a source failing any of
/// them translated cleanly here and was declined at the *next* launch
/// instead. Reading through `config::legacy` runs the loader's own seven, and
/// a run that reported success over a file the next session refuses is what
/// that closed.
#[test]
fn a_source_the_loader_would_refuse_is_refused_before_anything_is_written() {
    for (named, fixture) in [
        (
            "an mcp entry with nothing to run",
            r#"{ "mcp": { "docs": { "type": "local", "command": [] } } }"#,
        ),
        (
            "a hooks matcher that does not compile",
            r#"{
                 "hooks": {
                   "PreToolUse": [
                     {
                       "matcher": "Edit(",
                       "hooks": [{ "type": "command", "command": "./check.sh" }]
                     }
                   ]
                 }
               }"#,
        ),
    ] {
        let home = temporary();
        let project = temporary();
        checkout(project.path());
        let source = project.path().join("ganja.jsonc");
        plant(&source, fixture);

        migrate(&home, project.path())
            .assert()
            .failure()
            .stderr(predicate::str::contains(resolved(&source).display().to_string()));

        assert!(
            !project.path().join("ganja.toml").exists(),
            "{named}: nothing is written for a source that would not load"
        );
    }
}
