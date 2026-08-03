//! `ganja config import-opencode`, driven through the built binary.
//!
//! What the mapping does with a key is settled beside the mapping, in
//! `src/import.rs`. What is settled here is everything the mapping cannot see:
//! which files discovery reads and in what order, where the result lands, and
//! that a run which would overwrite or leak something refuses to.
//!
//! Every invocation redirects `XDG_CONFIG_HOME` into a fixture, so the machine
//! running the suite cannot contribute a config of its own and nothing here can
//! read or write a real user's. That redirection is per-subprocess, never a
//! `set_var`, so these can share a binary.

use std::{fs, path::Path};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// A key shaped like the real thing, planted so a test can prove the importer
/// never writes one.
const CANARY: &str = "sk-canary-8842";

/// The fixture the table test in `src/import.rs` maps, driven here through the
/// binary so the two cannot drift apart.
const FIXTURE: &str = include_str!("fixtures/opencode.jsonc");

fn temporary() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

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
        .current_dir(cwd)
        .args(["config", "import-opencode"]);

    command
}

/// opencode's global config directory inside a fixture home.
fn opencode_dir(home: &TempDir) -> std::path::PathBuf {
    home.path().join("config").join("opencode")
}

/// Where a `--global` import lands.
fn global_destination(home: &TempDir) -> std::path::PathBuf {
    home.path().join("config").join("ganja").join("ganja.json")
}

/// All three global files are read, and later beats earlier — so the last one
/// decides a key they all name, while a key only the first sets still survives.
/// A tier that merely replaced the ones below it would pass a "the last one
/// wins" assertion on its own, which is why both are asserted at once.
#[test]
fn the_global_tier_merges_all_three_files_with_the_jsonc_last() {
    let home = temporary();
    let project = temporary();
    let opencode = opencode_dir(&home);
    plant(
        &opencode.join("config.json"),
        r#"{"model": "anthropic/first", "shell": "/bin/from-config-json"}"#,
    );
    plant(
        &opencode.join("opencode.json"),
        r#"{"model": "anthropic/second", "theme": "gruvbox"}"#,
    );
    plant(
        &opencode.join("opencode.jsonc"),
        r#"{
          // the file with the last word
          "model": "anthropic/third",
        }"#,
    );

    ganja(&home, project.path())
        .arg("--global")
        .assert()
        .success()
        .stdout(predicate::str::contains("wrote"));

    let written = fs::read_to_string(global_destination(&home)).expect("the import wrote a file");
    assert!(
        written.contains(r#""model": "anthropic/third""#),
        "{written}"
    );
    assert!(
        written.contains(r#""shell": "/bin/from-config-json""#),
        "config.json is read too, not just skipped past: {written}"
    );
    assert!(written.contains(r#""theme": "gruvbox""#), "{written}");
}

/// Every directory from the working directory up to the project root
/// contributes, and the closest one has the last word — while a key only an
/// ancestor named still survives.
#[test]
fn the_project_walk_stacks_from_the_root_down_and_the_closest_file_wins() {
    let home = temporary();
    let project = temporary();
    let root = project.path();
    let nested = root.join("crates").join("core");
    fs::create_dir_all(&nested).expect("the fixture tree is creatable");
    checkout(root);
    plant(
        &root.join("opencode.json"),
        r#"{"model": "anthropic/outermost", "theme": "gruvbox"}"#,
    );
    plant(
        &nested.join("opencode.jsonc"),
        r#"{"model": "anthropic/closest"}"#,
    );

    ganja(&home, &nested).assert().success();

    // The project root, not the working directory: the file is the project's.
    let written = fs::read_to_string(root.join("ganja.json")).expect("the import wrote a file");
    assert!(
        written.contains(r#""model": "anthropic/closest""#),
        "{written}"
    );
    assert!(written.contains(r#""theme": "gruvbox""#), "{written}");
}

/// `opencode.jsonc` beats `opencode.json` in one directory, which is the second
/// effect of upstream's reversal and the one a user is least likely to expect.
#[test]
fn jsonc_beats_json_in_the_same_directory() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(
        &project.path().join("opencode.json"),
        r#"{"model": "anthropic/from-json", "shell": "/bin/zsh"}"#,
    );
    plant(
        &project.path().join("opencode.jsonc"),
        r#"{"model": "anthropic/from-jsonc"}"#,
    );

    ganja(&home, project.path()).assert().success();

    let written =
        fs::read_to_string(project.path().join("ganja.json")).expect("the import wrote a file");
    assert!(
        written.contains(r#""model": "anthropic/from-jsonc""#),
        "{written}"
    );
    assert!(written.contains(r#""shell": "/bin/zsh""#), "{written}");
}

/// Two tiers stack rather than replace each other: an object merges key by key,
/// and `instructions` is upstream's one array that concatenates — a project adds
/// to the global list instead of replacing it, with repeats dropped and order
/// kept.
#[test]
fn a_project_adds_to_the_global_tier_rather_than_replacing_it() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(
        &opencode_dir(&home).join("opencode.jsonc"),
        r#"{
          "instructions": ["global.md", "shared.md"],
          "agent": {"build": {"model": "anthropic/from-global", "description": "builds"}}
        }"#,
    );
    plant(
        &project.path().join("opencode.jsonc"),
        r#"{
          "instructions": ["shared.md", "local.md"],
          "agent": {"build": {"description": "still builds"}}
        }"#,
    );

    ganja(&home, project.path()).assert().success();

    let written =
        fs::read_to_string(project.path().join("ganja.json")).expect("the import wrote a file");
    assert!(
        written.contains("\"global.md\",\n    \"shared.md\",\n    \"local.md\""),
        "the two lists concatenate, deduplicated, in order: {written}"
    );
    assert!(
        written.contains(r#""model": "anthropic/from-global""#),
        "a field only the global tier set survives: {written}"
    );
    assert!(
        written.contains(r#""description": "still builds""#),
        "a field both set takes the closer value: {written}"
    );
}

/// `--file` is the whole import: neither the global tier nor the project walk
/// is consulted, so a config lying around cannot change what was asked for.
#[test]
fn a_named_file_is_imported_on_its_own() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(
        &opencode_dir(&home).join("opencode.jsonc"),
        r#"{"theme": "gruvbox"}"#,
    );
    plant(
        &project.path().join("opencode.json"),
        r#"{"shell": "/bin/zsh"}"#,
    );
    let named = project.path().join("elsewhere.jsonc");
    plant(&named, r#"{"model": "anthropic/named"}"#);

    ganja(&home, project.path())
        .args(["--file".as_ref(), named.as_os_str()])
        .assert()
        .success();

    let written =
        fs::read_to_string(project.path().join("ganja.json")).expect("the import wrote a file");
    assert!(
        written.contains(r#""model": "anthropic/named""#),
        "{written}"
    );
    assert!(
        !written.contains("gruvbox") && !written.contains("/bin/zsh"),
        "discovery was skipped, so neither other file may contribute: {written}"
    );
}

#[test]
fn a_named_file_that_is_not_there_is_refused_by_name() {
    let home = temporary();
    let project = temporary();

    ganja(&home, project.path())
        .args(["--file", "definitely-not-a-config.jsonc"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("definitely-not-a-config.jsonc")
                .and(predicate::str::contains("does not exist")),
        );
}

/// A config that will not parse is fatal, and the message has to say which file
/// and where — a caret-less "invalid JSON" would leave a user hunting.
#[test]
fn a_malformed_config_names_the_file_and_the_position() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(&project.path().join("opencode.json"), r#"{"model": }"#);

    ganja(&home, project.path()).assert().failure().stderr(
        predicate::str::contains("opencode.json")
            .and(predicate::str::contains("line 1"))
            .and(predicate::str::contains("column")),
    );

    assert!(
        !project.path().join("ganja.json").exists(),
        "a failed import writes nothing"
    );
}

/// The table is printed, both sections, and nothing lands.
#[test]
fn a_dry_run_prints_the_table_and_writes_nothing() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(&project.path().join("opencode.jsonc"), FIXTURE);

    ganja(&home, project.path())
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("mapped")
                .and(predicate::str::contains("skipped"))
                .and(predicate::str::contains("model"))
                .and(predicate::str::contains("mcp"))
                .and(predicate::str::contains("unsupported"))
                .and(predicate::str::contains("dry run"))
                .and(predicate::str::contains("wrote").not()),
        );

    assert!(
        !project.path().join("ganja.json").exists(),
        "a dry run writes nothing"
    );
}

/// The one value in an opencode config that must never travel. The fixture
/// carries a key shaped like a real one, and neither the file nor the table may
/// repeat it.
#[test]
fn an_api_key_in_the_source_reaches_neither_the_file_nor_the_terminal() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(
        &project.path().join("opencode.jsonc"),
        &format!(
            r#"{{
              "model": "anthropic/claude-sonnet-5",
              "provider": {{"anthropic": {{"options": {{"apiKey": "{CANARY}"}}}}}}
            }}"#
        ),
    );

    ganja(&home, project.path())
        .assert()
        .success()
        .stdout(
            predicate::str::contains("provider.anthropic.options.apiKey")
                .and(predicate::str::contains("credential"))
                .and(predicate::str::contains(CANARY).not()),
        )
        .stderr(
            predicate::str::contains("ganja auth login")
                .and(predicate::str::contains(CANARY).not()),
        );

    let written =
        fs::read_to_string(project.path().join("ganja.json")).expect("the import wrote a file");
    assert!(
        !written.contains(CANARY),
        "a credential was written into a config file: {written}"
    );
}

/// Both names ganja reads are refused, `ganja.jsonc` included — it would *beat*
/// the file this writes, so overwriting around it would make the import look
/// like it had done nothing.
#[test]
fn an_existing_config_is_never_overwritten() {
    for existing in ["ganja.json", "ganja.jsonc"] {
        let home = temporary();
        let project = temporary();
        checkout(project.path());
        plant(
            &project.path().join("opencode.jsonc"),
            r#"{"theme": "aura"}"#,
        );
        let occupied = project.path().join(existing);
        plant(&occupied, r#"{"theme": "gruvbox"}"#);

        ganja(&home, project.path()).assert().failure().stderr(
            predicate::str::contains(existing).and(predicate::str::contains("already exists")),
        );

        assert_eq!(
            fs::read_to_string(&occupied).expect("the file is still there"),
            r#"{"theme": "gruvbox"}"#,
            "{existing} was written over"
        );
    }
}

/// Nothing to import is not a failure: it is what every machine without
/// opencode installed looks like, and it has to read as an answer — with where
/// the search went, because a user whose config is elsewhere cannot guess the
/// global directory.
#[test]
fn a_machine_with_no_opencode_config_says_so_and_says_where_it_looked() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());

    ganja(&home, project.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("nothing to import"))
        .stderr(
            predicate::str::contains(opencode_dir(&home).display().to_string())
                .and(predicate::str::contains("up to the project root")),
        );

    assert!(!project.path().join("ganja.json").exists());
}

/// A config whose every key is one ganja has no home for is also nothing to
/// import — but the rows still say what was dropped, or the user would be told
/// their config is empty when it is not.
#[test]
fn a_config_of_nothing_but_skipped_keys_writes_no_file_but_still_reports() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(
        &project.path().join("opencode.jsonc"),
        r#"{"mcp": {"fs": {}}, "autoupdate": false}"#,
    );

    ganja(&home, project.path()).assert().success().stdout(
        predicate::str::contains("mcp")
            .and(predicate::str::contains("unsupported"))
            .and(predicate::str::contains("nothing to import")),
    );

    assert!(!project.path().join("ganja.json").exists());
}
