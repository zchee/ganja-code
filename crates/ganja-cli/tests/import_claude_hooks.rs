//! `ganja config import-claude-hooks`, driven through the built binary.
//!
//! What a group becomes and what is reported instead is settled beside the
//! extraction, in `src/claude_hooks_tests.rs`. What is settled here is
//! everything it cannot see: which settings files are read and in what order,
//! which `ganja.toml` they land in, that the merged file is one the real
//! loader reads, and that a dry run writes nothing.
//!
//! Every invocation redirects `XDG_CONFIG_HOME` into a fixture and pins
//! `HOME` — the second twice over, since `--global` reads *Claude's* home
//! directory as well as writing ganja's — while clearing
//! `GANJA_CONFIG_HOME`, so the machine running the suite cannot contribute a
//! settings file of its own and nothing here can read or write a real user's.
//! That redirection is per-subprocess, never a `set_var`, so these can share a
//! binary.

use std::{fs, path::Path};

use assert_cmd::Command;
use ganja_testkit::temp_dir as temporary;
use predicates::prelude::*;
use tempfile::TempDir;

/// A settings file carrying one of everything this command has an answer for:
/// two keys it does not read, an event this build fires, an event it does
/// not, a handler kind it cannot run beside one it can, and both of the
/// groups the loader would refuse.
const FIXTURE: &str = r#"{
  "model": "claude-sonnet-4-5",
  "permissions": { "allow": ["Bash(git status)"] },
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "./guard.sh", "timeout": 5 }] }
    ],
    "PreResponse": [
      { "hooks": [{ "type": "command", "command": "./never.sh" }] }
    ],
    "Stop": [
      {
        "hooks": [
          { "type": "prompt", "prompt": "summarise" },
          { "type": "command", "command": "./done.sh" }
        ]
      },
      { "hooks": [{ "type": "command", "command": "   " }] },
      { "matcher": "Edit(", "hooks": [{ "type": "command", "command": "./broken.sh" }] }
    ]
  }
}
"#;

/// The machine-specific overlay Claude reads after the committed file.
const LOCAL_FIXTURE: &str = r#"{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Edit", "hooks": [{ "type": "command", "command": "./local.sh" }] }
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

/// A checkout, so the project root the target resolves to is the fixture's
/// own directory rather than whatever the temporary directory sits under.
fn checkout(root: &Path) {
    fs::create_dir_all(root.join(".git")).expect("the fixture repository is creatable");
}

/// An invocation with its own config and data homes, run from `cwd`.
fn ganja(home: &TempDir, cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
    command
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        // `HOME` decides two different things here — where Claude's global
        // settings are read from, and where ganja's own `~/.ganja` fallback
        // would be — so it is pinned for both reasons at once.
        .env("HOME", home.path())
        .env_remove(ganja_core::config::CONFIG_HOME_ENV)
        .env_remove(ganja_core::config::CONFIG_ENV)
        .current_dir(cwd);

    command
}

/// The same, already pointed at the subcommand under test.
fn import(home: &TempDir, cwd: &Path) -> Command {
    let mut command = ganja(home, cwd);
    command.args(["config", "import-claude-hooks"]);

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

/// The whole run, end to end: both project settings files are read in Claude's
/// own order, every group that maps lands in order, every one that does not is
/// named with its reason, and the file that comes out is one the *real* loader
/// reads in a separate process.
///
/// `ganja skills` is the cheapest subcommand that calls `Config::load`, so a
/// merged file this build could not read would fail it — which is what makes
/// this the round trip rather than an assertion about text.
#[test]
fn both_project_settings_files_are_read_and_the_result_loads() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(&project.path().join(".claude/settings.json"), FIXTURE);
    plant(
        &project.path().join(".claude/settings.local.json"),
        LOCAL_FIXTURE,
    );

    import(&home, project.path())
        .assert()
        .success()
        .stdout(
            // The resolved target, before anything is written — the
            // countermeasure to the pre-mortem's third failure.
            predicate::str::contains(format!(
                "writing {}",
                resolved(project.path()).join("ganja.toml").display()
            ))
            .and(predicate::str::contains("settings.json:hooks.PreResponse"))
            .and(predicate::str::contains("unrun"))
            .and(predicate::str::contains(
                "settings.json:hooks.Stop[0].hooks[0]",
            ))
            .and(predicate::str::contains("unsupported"))
            .and(predicate::str::contains("settings.json:model"))
            .and(predicate::str::contains("unread")),
        )
        .stderr(
            predicate::str::contains(
                "settings.json:hooks.Stop[1] was left out — a command handler with no command",
            )
            .and(predicate::str::contains(
                "settings.json:hooks.Stop[2] was left out — a matcher that is not a regular \
                 expression",
            )),
        );

    let written = fs::read_to_string(project.path().join("ganja.toml")).expect("it was written");
    let committed = written.find("./guard.sh").expect("the committed group");
    let local = written.find("./local.sh").expect("the local group");
    assert!(
        committed < local,
        "the local file is read second, so its group is appended second:\n{written}"
    );
    assert!(
        written.contains("./done.sh") && !written.contains("summarise"),
        "the command handler travels and the prompt handler does not:\n{written}"
    );
    assert!(
        !written.contains("./never.sh"),
        "an event this build fires nothing for is not written:\n{written}"
    );
    assert!(
        !written.contains("./broken.sh"),
        "a group the loader would refuse is not written:\n{written}"
    );

    ganja(&home, project.path())
        .arg("skills")
        .assert()
        .success();
}

/// A target that is already there is edited, not replaced: every comment and
/// every key it held comes out where it went in, and the groups land after
/// whatever it already said for the event.
#[test]
fn an_existing_target_keeps_its_comments_and_its_own_groups_come_first() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(
        &project.path().join(".claude/settings.json"),
        r#"{ "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "./second.sh" }] }] } }"#,
    );
    plant(
        &project.path().join("ganja.toml"),
        "# The model this checkout uses.\nmodel = \"anthropic/claude-sonnet-5\"\n\n\
         [[hooks.Stop]]\n\n[[hooks.Stop.hooks]]\ntype = \"command\"\ncommand = \"./first.sh\"\n",
    );

    import(&home, project.path()).assert().success();

    let written = fs::read_to_string(project.path().join("ganja.toml")).expect("it was written");
    assert!(
        written.starts_with("# The model this checkout uses.\nmodel = "),
        "the comment and its key are where they were:\n{written}"
    );
    assert!(
        written.find("./first.sh").unwrap() < written.find("./second.sh").unwrap(),
        "an append lands after what was already there:\n{written}"
    );

    ganja(&home, project.path())
        .arg("skills")
        .assert()
        .success();
}

/// A dry run prints the table and the resolved target, and writes nothing —
/// including the file it would otherwise have created.
#[test]
fn a_dry_run_prints_the_target_and_writes_nothing() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(&project.path().join(".claude/settings.json"), FIXTURE);

    import(&home, project.path())
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(
            predicate::str::contains(format!(
                "would write {}",
                resolved(project.path()).join("ganja.toml").display()
            ))
            .and(predicate::str::contains("dry run — nothing written")),
        );

    assert!(
        !project.path().join("ganja.toml").exists(),
        "a dry run writes nothing"
    );
}

/// `--global` reads Claude's own home directory and writes ganja's, which are
/// two different homes resolved by two different conventions.
#[test]
fn a_global_import_reads_claudes_home_and_writes_ganjas() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(&home.path().join(".claude/settings.json"), FIXTURE);
    // A project settings file too, to prove `--global` reads past it.
    plant(
        &project.path().join(".claude/settings.json"),
        r#"{ "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "./project.sh" }] }] } }"#,
    );

    import(&home, project.path())
        .arg("--global")
        .assert()
        .success();

    let written =
        fs::read_to_string(config_home(&home).join("ganja.toml")).expect("it was written");
    assert!(written.contains("./guard.sh"), "{written}");
    assert!(!written.contains("./project.sh"), "{written}");
    assert!(
        !project.path().join("ganja.toml").exists(),
        "the project tier was not the target"
    );
}

/// A named file is the whole import: a caller who said which file to read did
/// not ask what else is lying around.
#[test]
fn a_named_file_is_the_whole_import() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    let named = project.path().join("elsewhere.json");
    plant(&named, FIXTURE);
    plant(
        &project.path().join(".claude/settings.json"),
        r#"{ "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "./ignored.sh" }] }] } }"#,
    );

    import(&home, project.path())
        .args(["--file", &named.display().to_string()])
        .assert()
        .success();

    let written = fs::read_to_string(project.path().join("ganja.toml")).expect("it was written");
    assert!(written.contains("./guard.sh"), "{written}");
    assert!(!written.contains("./ignored.sh"), "{written}");
}

/// A run with nothing to read says where it looked, and writes nothing.
#[test]
fn no_settings_file_says_where_it_looked() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());

    import(&home, project.path())
        .assert()
        .success()
        .stdout(
            predicate::str::contains("nothing to import: no Claude settings file was found")
                // And no target announced: a run that named a file and then
                // wrote nothing to it would be contradicting itself.
                .and(predicate::str::contains("writing ").not()),
        )
        .stderr(predicate::str::contains(
            resolved(project.path())
                .join(".claude")
                .display()
                .to_string(),
        ));

    assert!(!project.path().join("ganja.toml").exists());
}

/// A settings file whose every group was left out writes nothing at all: the
/// table said what happened, and an empty edit to a config file is noise in
/// somebody's diff.
#[test]
fn a_settings_file_whose_groups_all_fall_out_writes_nothing() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(
        &project.path().join(".claude/settings.json"),
        r#"{ "hooks": { "PreResponse": [{ "hooks": [{ "type": "command", "command": "./x.sh" }] }] } }"#,
    );

    import(&home, project.path()).assert().success().stdout(
        predicate::str::contains("nothing to import: no hooks group survived")
            // The second early return, and the same rule: nothing to write
            // means no target is named.
            .and(predicate::str::contains("writing ").not()),
    );

    assert!(!project.path().join("ganja.toml").exists());
}

/// A settings file that is not JSON is refused by name rather than half-read:
/// Claude's file is plain JSON, and a comment in one is a file Claude itself
/// would not read.
#[test]
fn a_settings_file_that_is_not_json_is_refused_by_name() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(
        &project.path().join(".claude/settings.json"),
        "{ // a comment\n  \"hooks\": {} }",
    );

    import(&home, project.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("settings.json"));

    assert!(!project.path().join("ganja.toml").exists());
}

/// A settings file whose one group runs two commands, which is the shape the
/// per-command rows exist for.
const TWO_COMMANDS: &str = r#"{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          { "type": "command", "command": "./first.sh --audit" },
          { "type": "command", "command": "curl https://example.test/telemetry" }
        ]
      }
    ]
  }
}
"#;

/// The command lines are the thing being approved, so they are printed.
///
/// A hook runs with the user's own authority and crosses no permission dialog,
/// which makes the report the only place anybody sees what is about to be
/// installed. A group row alone would be asking for approval of a payload the
/// command declined to show.
#[test]
fn every_command_a_group_installs_gets_a_row_of_its_own() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(&project.path().join(".claude/settings.json"), TWO_COMMANDS);

    import(&home, project.path()).assert().success().stdout(
        // The group row keeps its own column padding, which the two
        // longer handler keys below set; only the handler rows can be
        // matched with their spacing spelled out.
        predicate::str::contains("settings.json:hooks.PreToolUse[0]  ")
            .and(predicate::str::contains("[[hooks.PreToolUse]]"))
            .and(predicate::str::contains(
                "settings.json:hooks.PreToolUse[0].hooks[0]  ./first.sh --audit",
            ))
            .and(predicate::str::contains(
                "settings.json:hooks.PreToolUse[0].hooks[1]  curl https://example.test/telemetry",
            )),
    );
}

/// The preview is where somebody decides, so it shows exactly what the write
/// would — and still writes nothing.
#[test]
fn a_dry_run_lists_the_command_lines_it_would_install() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(&project.path().join(".claude/settings.json"), TWO_COMMANDS);

    import(&home, project.path())
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("./first.sh --audit")
                .and(predicate::str::contains(
                    "curl https://example.test/telemetry",
                ))
                .and(predicate::str::contains("dry run — nothing written")),
        );

    assert!(!project.path().join("ganja.toml").exists());
}

/// The guard `ganja mcp add` writes through, on the other command that writes
/// a `ganja.toml`.
///
/// The loader refuses a directory holding a legacy config whole, so an import
/// that reported success into the `ganja.toml` beside one would be reporting a
/// hook installed into a file the very next launch declines to read.
#[test]
fn a_directory_holding_a_legacy_config_is_refused_and_nothing_is_written() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(&project.path().join(".claude/settings.json"), FIXTURE);
    plant(&project.path().join("ganja.jsonc"), "{}\n");

    import(&home, project.path()).assert().failure().stderr(
        predicate::str::contains("ganja.jsonc")
            .and(predicate::str::contains("ganja config migrate")),
    );

    assert!(
        !project.path().join("ganja.toml").exists(),
        "a refused run writes nothing"
    );

    // The same invocation, once the legacy file is gone.
    fs::remove_file(project.path().join("ganja.jsonc")).expect("the fixture is removable");
    import(&home, project.path()).assert().success();
    assert!(project.path().join("ganja.toml").exists());
}

/// A target that does not parse is named by position, and never by its bytes.
///
/// `toml_edit`'s own `Display` reproduces the offending line, and the line an
/// existing config fails on may be an `mcp` entry's `headers` — the one place
/// a bearer token is spelled. What comes out is what went wrong and where to
/// look, so the refusal cannot carry a credential into a shared terminal.
#[test]
fn a_target_that_does_not_parse_is_named_by_position_and_never_by_its_bytes() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    plant(&project.path().join(".claude/settings.json"), FIXTURE);
    plant(
        &project.path().join("ganja.toml"),
        "[mcp.vendor]\ntype = \"remote\"\nheaders = { Authorization = \"Bearer NEVER-PRINT-ME }\n",
    );

    import(&home, project.path()).assert().failure().stderr(
        predicate::str::contains("line 3")
            .and(predicate::str::contains("column"))
            .and(predicate::str::contains("NEVER-PRINT-ME").not()),
    );
}

/// The warnings channel prints beside the table, so it is filtered like the
/// table.
///
/// The one warning this command builds quotes the settings file's own matcher
/// back through a regex error, which means an escape sequence planted in a
/// matcher would otherwise reach the terminal on the line printed right next
/// to the rows it could repaint.
#[test]
fn a_warning_quoting_the_file_back_is_neutralized_like_a_row_is() {
    let home = temporary();
    let project = temporary();
    checkout(project.path());
    // An unbalanced group, so the matcher fails to compile and the pattern is
    // quoted back — with the escape sequence still inside it.
    plant(
        &project.path().join(".claude/settings.json"),
        r#"{ "hooks": { "Stop": [{ "matcher": "Edit(\u001b[2K", "hooks": [
             { "type": "command", "command": "./x.sh" }
           ] }] } }"#,
    );

    import(&home, project.path()).assert().success().stderr(
        predicate::str::contains("not a regular expression")
            .and(predicate::str::contains("\u{fffd}[2K"))
            .and(predicate::str::contains("\u{1b}").not()),
    );
}
