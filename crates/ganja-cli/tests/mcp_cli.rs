//! The `ganja mcp add/get/remove` surface, walked as a person would walk it:
//! add a server, read it back through the loader, override it from the other
//! tier, remove it again — asserting the file on disk and the readable
//! refusals on the way (**D483**).
//!
//! Two properties are what this binary exists to pin, and both are about the
//! file rather than about the command:
//!
//! * **The loader reads back what this wrote.** Every round trip ends in
//!   `ganja mcp get`, which runs `Config::load` — the real loader, over the
//!   real project and global tiers — so a written entry this build could not
//!   read would fail the test rather than the next launch.
//! * **Nothing else in the file moves.** A config holding keys this build
//!   does not have (and will not have: they belong to whatever wrote it) has
//!   to survive an edit with its meaning intact, which is asserted as parsed
//!   equality *and* as key-set equality — the second catches a key that was
//!   dropped and re-added with the same value by a typed round trip.
//!
//! Every invocation pins its own config home and data home, per the standing
//! rule for stored-state tests: nothing here may read or write the config of
//! whoever runs the suite. Nothing here reaches the network — no server is
//! ever connected, because none of these three subcommands connects.

use std::{collections::BTreeSet, fs, path::Path};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::{Value, json};
use tempfile::TempDir;

/// A temporary home holding a project directory and a config home, neither of
/// which is anybody's real one.
struct Home {
    directory: TempDir,
}

impl Home {
    fn new() -> Self {
        let directory = TempDir::new().expect("a temporary directory is creatable");
        let project = directory.path().join("project");
        fs::create_dir_all(&project).expect("the project directory is creatable");
        // Planted so `Project::resolve` stops here rather than walking into
        // whatever encloses the system temporary directory.
        fs::create_dir_all(project.join(".git")).expect("the marker is creatable");

        Self { directory }
    }

    /// The worktree a command runs in, and the tier `--global` does not write.
    fn project(&self) -> std::path::PathBuf {
        self.directory.path().join("project")
    }

    /// The tier `--global` writes.
    fn config_home(&self) -> std::path::PathBuf {
        self.directory.path().join("ganja-home")
    }

    /// `<project>/ganja.json`.
    fn project_config(&self) -> std::path::PathBuf {
        self.project().join("ganja.json")
    }

    /// `<config home>/ganja.json`.
    fn global_config(&self) -> std::path::PathBuf {
        self.config_home().join("ganja.json")
    }

    /// An invocation that runs in the project and sees only these homes.
    fn ganja(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
        command.current_dir(self.project());
        command.env("GANJA_CONFIG_HOME", self.config_home());
        command.env("XDG_DATA_HOME", self.directory.path().join("data"));
        command.env("XDG_CONFIG_HOME", self.directory.path().join("config"));
        command.env("HOME", self.directory.path());
        command.env_remove("GANJA_CONFIG");

        command
    }
}

/// A config file parsed, for asserting about its content rather than its
/// formatting.
fn read(path: &Path) -> Value {
    let text = fs::read_to_string(path).expect("the config file exists");
    serde_json::from_str(&text).expect("the config file is JSON")
}

/// Every key path in `value`, flattened — what "the same keys" is asserted
/// against, so a dropped-and-restored key is still a difference if its
/// neighbours moved.
fn keys(value: &Value, prefix: &str, into: &mut BTreeSet<String>) {
    let Value::Object(map) = value else {
        return;
    };
    for (key, child) in map {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        into.insert(path.clone());
        keys(child, &path, into);
    }
}

fn key_set(value: &Value) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    keys(value, "", &mut set);

    set
}

/// Writes `text` to `path`, creating whatever directories it needs.
fn plant(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the fixture tree is creatable");
    }
    fs::write(path, text).expect("the fixture file is writable");
}

#[test]
fn a_local_server_round_trips_through_the_loader_that_will_read_it() {
    let home = Home::new();

    home.ganja()
        .args([
            "mcp",
            "add",
            "docs",
            "--env",
            "TOKEN=a=b",
            "--cwd",
            "./tools",
            "--timeout",
            "9000",
            "--",
            "bun",
            "server.ts",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("added mcp server \"docs\""))
        .stdout(predicate::str::contains("Reconnect"));

    assert_eq!(
        read(&home.project_config()),
        json!({
            "mcp": {
                "docs": {
                    "type": "local",
                    "command": ["bun", "server.ts"],
                    "cwd": "./tools",
                    // The value keeps its own `=`.
                    "environment": {"TOKEN": "a=b"},
                    "timeout": 9_000,
                }
            }
        }),
        "only what was asked for is written"
    );

    // `get` runs `Config::load` — if the written file were not one this build
    // reads, this is where it would fail.
    home.ganja()
        .args(["mcp", "get", "docs"])
        .assert()
        .success()
        .stdout(predicate::str::contains("type         local"))
        .stdout(predicate::str::contains("bun server.ts"))
        .stdout(predicate::str::contains("timeout      9000"))
        .stdout(predicate::str::contains(
            home.project_config().display().to_string(),
        ));
}

#[test]
fn a_remote_server_round_trips_and_get_withholds_the_header_values() {
    let home = Home::new();

    home.ganja()
        .args([
            "mcp",
            "add",
            "hosted",
            "--url",
            "https://mcp.example/api",
            "--header",
            "Authorization=Bearer swordfish",
            "--output-limit",
            "4096",
            "--disabled",
        ])
        .assert()
        .success();

    assert_eq!(
        read(&home.project_config())["mcp"]["hosted"],
        json!({
            "type": "remote",
            "url": "https://mcp.example/api",
            "headers": {"Authorization": "Bearer swordfish"},
            "enabled": false,
            "output_limit": 4_096,
        })
    );

    home.ganja()
        .args(["mcp", "get", "hosted"])
        .assert()
        .success()
        .stdout(predicate::str::contains("type         remote"))
        .stdout(predicate::str::contains("https://mcp.example/api"))
        .stdout(predicate::str::contains("headers      Authorization"))
        .stdout(predicate::str::contains("enabled      false"))
        // The name is what somebody is checking; the value is a credential,
        // and this output lands in scrollback and in pasted bug reports.
        .stdout(predicate::str::contains("swordfish").not());
}

#[test]
fn an_edit_leaves_every_unrelated_key_exactly_as_it_found_it() {
    let home = Home::new();
    plant(
        &home.project_config(),
        r#"{
  "model": "anthropic/claude-sonnet-4-5",
  "theme": "ganja",
  "permission": {"edit": "ask"},
  "instructions": ["./NOTES.md"],
  "mcp": {
    "keep-me": {"type": "local", "command": ["cat"]}
  }
}
"#,
    );
    let before = read(&home.project_config());

    home.ganja()
        .args(["mcp", "add", "docs", "--", "bun", "server.ts"])
        .assert()
        .success();
    home.ganja()
        .args(["mcp", "remove", "docs"])
        .assert()
        .success()
        .stdout(predicate::str::contains("removed mcp server \"docs\""));

    let after = read(&home.project_config());
    assert_eq!(
        after, before,
        "an add and its removal leave the file as it was"
    );
    assert_eq!(
        key_set(&after),
        key_set(&before),
        "no key was dropped, and none was invented"
    );

    // And the loader still reads what is left — the entry the edit was never
    // about included.
    home.ganja()
        .args(["mcp", "get", "keep-me"])
        .assert()
        .success()
        .stdout(predicate::str::contains("command      cat"));
}

#[test]
fn a_key_this_build_does_not_have_survives_the_edit_it_cannot_be_loaded_through() {
    let home = Home::new();
    // Whatever wrote `experimental` was working from a key set that is not
    // this one. This build refuses such a file *at load* — that is
    // `config.rs`'s standing "unknown keys are refused by name" — but the
    // writer is not entitled to drop it, which is exactly why the file is
    // read as a `Value` and never as a typed `Config`.
    plant(
        &home.project_config(),
        r#"{
  "theme": "ganja",
  "experimental": {"from": "a newer build", "keep": [1, 2, {"nested": true}]},
  "mcp": {"keep-me": {"type": "local", "command": ["cat"]}}
}
"#,
    );
    let before = read(&home.project_config());

    home.ganja()
        .args(["mcp", "add", "docs", "--", "bun"])
        .assert()
        .success();
    home.ganja()
        .args(["mcp", "remove", "docs"])
        .assert()
        .success();

    let after = read(&home.project_config());
    assert_eq!(
        after, before,
        "the key nobody here understands is still there"
    );
    assert_eq!(key_set(&after), key_set(&before));

    // And the refusal is the loader's, by name, unchanged by any of this.
    home.ganja()
        .args(["mcp", "get", "keep-me"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown field `experimental`"));
}

#[test]
fn a_jsonc_at_the_target_tier_refuses_both_a_write_and_a_removal() {
    let home = Home::new();
    let commented = home.project().join("ganja.jsonc");
    plant(
        &commented,
        "{\n  // the comment a rewrite would delete\n  \"mcp\": {\"docs\": {\"type\": \"local\", \"command\": [\"cat\"]}}\n}\n",
    );

    for arguments in [
        vec!["mcp", "add", "other", "--", "bun"],
        vec!["mcp", "remove", "docs"],
    ] {
        home.ganja()
            .args(&arguments)
            .assert()
            .failure()
            .stderr(predicate::str::contains(commented.display().to_string()))
            .stderr(predicate::str::contains("comments"))
            .stderr(predicate::str::contains("by hand"));
    }

    assert!(
        !home.project_config().exists(),
        "a refused write never leaves a `.json` beside the `.jsonc` that beats it"
    );
    assert!(
        fs::read_to_string(&commented)
            .expect("the fixture survives")
            .contains("// the comment"),
        "the comment is still there"
    );
}

#[test]
fn a_url_the_loader_would_refuse_is_never_written() {
    let home = Home::new();

    home.ganja()
        .args(["mcp", "add", "hosted", "--url", "http://mcp.example/api"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("https"))
        // Not even in the refusal: a URL may carry a credential in its
        // userinfo, and echoing one back is how it reaches a log.
        .stderr(predicate::str::contains("mcp.example").not());
    assert!(
        !home.project_config().exists(),
        "a refused entry leaves no file behind"
    );

    // And with a file already there, it is left untouched rather than rewritten.
    plant(&home.project_config(), "{\n  \"theme\": \"ganja\"\n}\n");
    let before = fs::read_to_string(home.project_config()).expect("the fixture exists");
    home.ganja()
        .args(["mcp", "add", "hosted", "--url", "ftp://mcp.example/api"])
        .assert()
        .failure();
    assert_eq!(
        fs::read_to_string(home.project_config()).expect("the fixture survives"),
        before,
        "a refused entry does not rewrite the file it would have landed in"
    );
}

#[test]
fn a_file_that_does_not_parse_refuses_rather_than_being_overwritten() {
    let home = Home::new();
    plant(&home.project_config(), "{ this is not JSON");

    home.ganja()
        .args(["mcp", "add", "docs", "--", "bun"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("could not be parsed"));
    assert_eq!(
        fs::read_to_string(home.project_config()).expect("the fixture survives"),
        "{ this is not JSON",
        "the file nobody could read is the file nobody overwrote"
    );
}

#[test]
fn a_second_add_of_one_name_needs_force_and_then_replaces() {
    let home = Home::new();

    home.ganja()
        .args(["mcp", "add", "docs", "--", "bun", "one.ts"])
        .assert()
        .success();
    home.ganja()
        .args(["mcp", "add", "docs", "--", "bun", "two.ts"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--force"));
    assert_eq!(
        read(&home.project_config())["mcp"]["docs"]["command"],
        json!(["bun", "one.ts"]),
        "a refused add changes nothing"
    );

    home.ganja()
        .args(["mcp", "add", "docs", "--force", "--", "bun", "two.ts"])
        .assert()
        .success()
        .stdout(predicate::str::contains("replaced mcp server \"docs\""));
    assert_eq!(
        read(&home.project_config())["mcp"]["docs"]["command"],
        json!(["bun", "two.ts"])
    );
}

#[test]
fn global_writes_the_config_home_and_the_project_tier_overrides_it() {
    let home = Home::new();

    home.ganja()
        .args(["mcp", "add", "docs", "--global", "--", "bun", "global.ts"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            home.global_config().display().to_string(),
        ));
    assert!(
        !home.project_config().exists(),
        "`--global` writes the config home and nothing else"
    );
    home.ganja()
        .args(["mcp", "get", "docs"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            home.global_config().display().to_string(),
        ));

    // The same name in the project tier: both files hold it, and the project
    // is merged last.
    home.ganja()
        .args(["mcp", "add", "docs", "--", "bun", "project.ts"])
        .assert()
        .success()
        .stderr(predicate::str::contains("also in"))
        .stderr(predicate::str::contains("this project"));

    home.ganja()
        .args(["mcp", "get", "docs"])
        .assert()
        .success()
        .stdout(predicate::str::contains("bun project.ts"))
        .stdout(predicate::str::contains("overridden by"))
        .stdout(predicate::str::contains(
            home.project_config().display().to_string(),
        ));

    // Removing the project's says the global one is still there.
    home.ganja()
        .args(["mcp", "remove", "docs"])
        .assert()
        .success()
        .stdout(predicate::str::contains("still configured in"))
        .stdout(predicate::str::contains(
            home.global_config().display().to_string(),
        ));
    home.ganja()
        .args(["mcp", "get", "docs"])
        .assert()
        .success()
        .stdout(predicate::str::contains("bun global.ts"));
}

#[test]
fn adding_globally_over_a_project_entry_says_which_tier_wins() {
    let home = Home::new();

    home.ganja()
        .args(["mcp", "add", "docs", "--", "bun", "project.ts"])
        .assert()
        .success();
    home.ganja()
        .args(["mcp", "add", "docs", "--global", "--", "bun", "global.ts"])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            home.project_config().display().to_string(),
        ))
        .stderr(predicate::str::contains(
            "this project's entry wins at load",
        ));
}

#[test]
fn removing_a_name_the_target_file_does_not_hold_names_the_file() {
    let home = Home::new();

    home.ganja()
        .args(["mcp", "remove", "docs"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            home.project_config().display().to_string(),
        ));

    // Including when the *other* tier holds it: a `remove` that silently did
    // nothing to the tier asked for is the failure this refusal prevents.
    home.ganja()
        .args(["mcp", "add", "docs", "--global", "--", "bun"])
        .assert()
        .success();
    home.ganja()
        .args(["mcp", "remove", "docs"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            home.project_config().display().to_string(),
        ));
}

#[test]
fn getting_a_name_nothing_configures_lists_the_ones_that_are() {
    let home = Home::new();

    home.ganja()
        .args(["mcp", "get", "docs"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("neither is any other"));

    home.ganja()
        .args(["mcp", "add", "notes", "--", "bun"])
        .assert()
        .success();
    home.ganja()
        .args(["mcp", "get", "docs"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("configured: notes"));
}

#[test]
fn naming_both_kinds_of_server_or_neither_is_refused_before_anything_runs() {
    let home = Home::new();

    // Neither.
    home.ganja()
        .args(["mcp", "add", "docs"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--url"));
    // Both.
    home.ganja()
        .args([
            "mcp",
            "add",
            "docs",
            "--url",
            "https://mcp.example",
            "--",
            "bun",
        ])
        .assert()
        .failure();
    // A local-only flag on a remote, and a remote-only flag on a local.
    home.ganja()
        .args([
            "mcp",
            "add",
            "docs",
            "--url",
            "https://mcp.example",
            "--cwd",
            ".",
        ])
        .assert()
        .failure();
    home.ganja()
        .args(["mcp", "add", "docs", "--header", "K=V", "--", "bun"])
        .assert()
        .failure();

    assert!(
        !home.project_config().exists(),
        "nothing that clap refused ever reached the disk"
    );
}

#[test]
fn a_name_that_is_a_path_is_refused_before_anything_is_written() {
    let home = Home::new();

    home.ganja()
        .args(["mcp", "add", "team/docs", "--", "bun"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not a path"));
    assert!(!home.project_config().exists());
}

#[test]
fn the_bare_listing_and_the_list_word_are_the_same_command() {
    let home = Home::new();

    let bare = home
        .ganja()
        .arg("mcp")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let worded = home
        .ganja()
        .args(["mcp", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(
        bare, worded,
        "`list` is the word for what the bare form does"
    );
    assert!(
        String::from_utf8_lossy(&bare).contains("no MCP servers configured"),
        "an empty project says so"
    );
}

#[test]
fn the_help_documents_the_whole_surface() {
    let home = Home::new();

    home.ganja()
        .args(["mcp", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("add"))
        .stdout(predicate::str::contains("list"))
        .stdout(predicate::str::contains("get"))
        .stdout(predicate::str::contains("remove"))
        .stdout(predicate::str::contains("login"));
}
