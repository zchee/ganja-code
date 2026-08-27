//! The `ganja mcp add/get/remove` surface, walked as a person would walk it:
//! add a server, read it back through the loader, override it from the other
//! tier, remove it again — asserting the file on disk and the readable
//! refusals on the way (**D483**).
//!
//! Three properties are what this binary exists to pin, and all three are
//! about the file rather than about the command:
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
//! * **A commented file survives being edited.** This shipped refusing a
//!   commented config outright, and the field found within the day that a
//!   commented config is precisely what somebody who configures anything has —
//!   so the refusal refused the feature. It now edits that file through a
//!   format-preserving document, and the tests that pinned the refusal pin the
//!   preservation instead: every comment line still there, byte for byte, no
//!   line that was not the inserted entry changed, and a replaced entry left
//!   where it was with the comment above it intact.
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

    /// `<project>/ganja.toml`.
    fn project_config(&self) -> std::path::PathBuf {
        self.project().join("ganja.toml")
    }

    /// `<config home>/ganja.toml`.
    fn global_config(&self) -> std::path::PathBuf {
        self.config_home().join("ganja.toml")
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

    toml_edit::de::from_str(&text).expect("the config file is TOML")
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

/// The field sequence that motivated `--oauth`: an entry added without the
/// marker cannot `mcp login`, and `add` used to have no way to write it.
#[test]
fn oauth_added_from_the_flag_reaches_login_past_the_no_oauth_refusal() {
    let home = Home::new();

    // A loopback endpoint with nothing listening: discovery dies on a refused
    // connection in milliseconds, so the test proves login got PAST the
    // no-oauth gate without a single byte leaving the machine.
    home.ganja()
        .args([
            "mcp",
            "add",
            "context7",
            "--global",
            "--oauth",
            "--url",
            "http://127.0.0.1:1/mcp",
        ])
        .assert()
        .success();

    assert_eq!(
        read(&home.global_config()),
        json!({
            "mcp": {
                "context7": {
                    "type": "remote",
                    "url": "http://127.0.0.1:1/mcp",
                    "oauth": {},
                }
            }
        }),
        "the marker is written exactly as the loader's vocabulary spells it"
    );

    home.ganja()
        .args(["mcp", "login", "context7"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("has no `oauth` configured").not());
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
        r#"model = "anthropic/claude-sonnet-4-5"
theme = "ganja"
instructions = ["./NOTES.md"]

[permission]
edit = "ask"

[mcp.keep-me]
command = ["cat"]
type = "local"
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
    // edited as a document and never as a typed `Config`.
    plant(
        &home.project_config(),
        r#"theme = "ganja"

[experimental]
from = "a newer build"
keep = [1, 2, { nested = true }]

[mcp.keep-me]
command = ["cat"]
type = "local"
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

/// A commented config of the kind somebody actually keeps: a note above a
/// key, one above the table, one beside an entry, and one after everything.
const COMMENTED: &str = r#"# What model a session starts on. Changed 2026-03; see the team doc.
model = "anthropic/claude-sonnet-4-5"

# The theme is deliberately not the default — the default's diff colours
# are unreadable on this terminal.
theme = "ganja"

# Servers. Anything added here needs a review first.

# Reads the design tokens. Do not point this at staging.
[mcp.tokens]
command = ["cat", "tokens.json"]
type = "local"

# Everything below this line is deliberately unset.
"#;

/// Every line of `text` that carries a comment, trimmed — what "the comments
/// survived" is asserted over.
fn comment_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

#[test]
fn adding_to_a_commented_config_edits_it_and_keeps_every_comment_in_it() {
    let home = Home::new();
    plant(&home.project_config(), COMMENTED);

    home.ganja()
        .args(["mcp", "add", "docs", "--", "bun", "server.ts"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            home.project_config().display().to_string(),
        ));

    let after = fs::read_to_string(home.project_config()).expect("the fixture survives");
    assert_eq!(
        comment_lines(&after),
        comment_lines(COMMENTED),
        "every comment line is still there, byte for byte"
    );
    // And every line that was not the inserted entry is untouched — which
    // catches reindentation and reordering that the comment check alone would
    // not.
    let kept: Vec<&str> = COMMENTED.lines().collect();
    let inserted: Vec<&str> = after.lines().filter(|line| !kept.contains(line)).collect();
    assert_eq!(
        inserted,
        vec!["[mcp.docs]", "command = [\"bun\", \"server.ts\"]"],
        "the entry's own lines are the only new ones — `type = \"local\"` is \
         not among them because the entry it replaces nothing spells it too"
    );

    // The loader — the real one — reads back what was written.
    home.ganja()
        .args(["mcp", "get", "docs"])
        .assert()
        .success()
        .stdout(predicate::str::contains("bun server.ts"))
        .stdout(predicate::str::contains(
            home.project_config().display().to_string(),
        ));
    home.ganja()
        .args(["mcp", "get", "tokens"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cat tokens.json"));
}

#[test]
fn removing_from_a_commented_config_keeps_every_comment_in_it_too() {
    let home = Home::new();
    plant(&home.project_config(), COMMENTED);

    home.ganja()
        .args(["mcp", "add", "docs", "--", "bun"])
        .assert()
        .success();
    home.ganja()
        .args(["mcp", "remove", "docs"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            home.project_config().display().to_string(),
        ));

    // Not merely "the comments survived" — an add and its removal give the
    // file back byte for byte, blank line included. That is the whole claim
    // the old refusal said could not be made.
    assert_eq!(
        fs::read_to_string(home.project_config()).expect("the fixture survives"),
        COMMENTED,
        "an add and its removal leave the commented file exactly as it was"
    );
    home.ganja()
        .args(["mcp", "get", "docs"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("configured: tokens"));
}

/// The other half of the comment promise: a `--force` replacement is written
/// into the slot the old entry held, so the note above it still describes it
/// and the table after it has not moved.
#[test]
fn replacing_an_entry_leaves_it_where_it_was_with_its_comment_above_it() {
    let home = Home::new();
    plant(&home.project_config(), COMMENTED);

    home.ganja()
        .args(["mcp", "add", "tokens", "--force", "--", "cat", "moved.json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("replaced mcp server \"tokens\""));

    assert_eq!(
        fs::read_to_string(home.project_config()).expect("the fixture survives"),
        COMMENTED.replace("tokens.json", "moved.json"),
        "one value changed and nothing else did — not the comment above the \
         entry, not its position, not the blank lines around it"
    );
    home.ganja()
        .args(["mcp", "get", "tokens"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cat moved.json"));
}

#[test]
fn adding_where_there_is_no_mcp_table_yet_creates_one_and_disturbs_nothing() {
    let home = Home::new();
    let before = "# the only thing configured here\ntheme = \"ganja\"\n";
    plant(&home.project_config(), before);

    home.ganja()
        .args(["mcp", "add", "docs", "--", "bun"])
        .assert()
        .success();

    let after = fs::read_to_string(home.project_config()).expect("the fixture survives");
    assert!(
        after.starts_with(before),
        "the comment and the sibling key are the bytes they were, and the \
         table arrived under them: {after}"
    );
    assert!(
        !after.contains("[mcp]\n"),
        "no empty header was written above the entry: {after}"
    );
    home.ganja()
        .args(["mcp", "get", "docs"])
        .assert()
        .success()
        .stdout(predicate::str::contains("command      bun"));
}

/// A name with a dot in it is two keys if the header is written bare, and
/// `check_name` refuses only path separators — so quoting is the library's
/// job and this is where that is pinned.
#[test]
fn a_name_that_needs_quoting_round_trips_through_the_loader() {
    let home = Home::new();

    home.ganja()
        .args(["mcp", "add", "tools.v2", "--", "bun"])
        .assert()
        .success();

    assert!(
        fs::read_to_string(home.project_config())
            .expect("the config exists")
            .contains("[mcp.\"tools.v2\"]"),
        "the dot is inside the name rather than a path through two tables"
    );
    home.ganja()
        .args(["mcp", "get", "tools.v2"])
        .assert()
        .success()
        .stdout(predicate::str::contains("command      bun"));
    home.ganja()
        .args(["mcp", "remove", "tools.v2"])
        .assert()
        .success();
    assert_eq!(
        read(&home.project_config()),
        json!({}),
        "and the same name is what the removal found — a table this created \
         and then emptied leaves no header behind either"
    );
}

/// The format ganja has left. Editing one would land an entry in a file its
/// author still has to convert, so the refusal names the file and the command
/// that converts it — and it fires only where there is no `ganja.toml` to
/// edit instead.
#[test]
fn a_tier_holding_only_the_older_config_is_refused_by_name() {
    let home = Home::new();
    let legacy = home.project().join("ganja.jsonc");
    plant(&legacy, "{\n  \"mcp\": {}\n}\n");
    let untouched = fs::read_to_string(&legacy).expect("the fixture exists");

    home.ganja()
        .args(["mcp", "add", "docs", "--", "bun"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(legacy.display().to_string()))
        .stderr(predicate::str::contains("ganja config migrate"));
    assert!(
        !home.project_config().exists(),
        "the refusal wrote nothing beside the file it refused"
    );
    assert_eq!(
        fs::read_to_string(&legacy).expect("the fixture survives"),
        untouched,
        "and it did not edit the file it refused either"
    );

    // The other tier says the same about its own file.
    let global = home.config_home().join("ganja.json");
    plant(&global, "{\"mcp\": {}}\n");
    home.ganja()
        .args(["mcp", "add", "docs", "--global", "--", "bun"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(global.display().to_string()))
        .stderr(predicate::str::contains("ganja config migrate"));

    // And a `ganja.toml` beside one is the file that is edited: what the
    // legacy file's presence then means is the loader's sentence to say.
    plant(&home.project_config(), "# converted\n");
    home.ganja()
        .args(["mcp", "add", "docs", "--", "bun"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            home.project_config().display().to_string(),
        ));
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
    plant(&home.project_config(), "theme = \"ganja\"\n");
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
    plant(&home.project_config(), "[unclosed\n");

    home.ganja()
        .args(["mcp", "add", "docs", "--", "bun"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("could not be parsed"));
    assert_eq!(
        fs::read_to_string(home.project_config()).expect("the fixture survives"),
        "[unclosed\n",
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
