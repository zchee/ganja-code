//! The `ganja plugin` surface, walked as a person would walk it: add a
//! marketplace, install, disable, enable, remove — asserting the state file
//! transitions and the readable refusals on the way.
//!
//! Every invocation pins its own config home (`GANJA_CONFIG_HOME`) and data
//! home, per the standing rule for stored-state tests: nothing here may read
//! or write the plugins of whoever runs the suite. The git lane clones from
//! a bare repository the test itself creates — the network is never asked.

use std::fs;
use std::path::Path;
use std::process::Command as Process;

use assert_cmd::Command;
use ganja_testkit::plant;
use predicates::prelude::*;
use serde_json::Value;
use tempfile::TempDir;

/// Builds an invocation whose config home — and therefore whose plugin
/// store — is the test's own temporary directory.
fn ganja(home: &TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
    command.env("GANJA_CONFIG_HOME", home.path().join("ganja-home"));
    command.env("XDG_DATA_HOME", home.path().join("data"));
    command.env("XDG_CONFIG_HOME", home.path().join("config"));
    command.env("HOME", home.path());
    command.env_remove("GANJA_CONFIG");

    command
}

fn home() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

/// A marketplace offering one plugin with a hook and a skill — enough that
/// `list` has components to name.
fn plant_marketplace(market: &Path) {
    plant(
        market,
        ".claude-plugin/marketplace.json",
        r#"{
          "name": "walk-market",
          "owner": { "name": "The Suite" },
          "plugins": [
            { "name": "walker", "source": "./plugins/walker", "description": "walks" }
          ]
        }"#,
    );
    plant(
        market,
        "plugins/walker/.claude-plugin/plugin.json",
        r#"{ "name": "walker", "version": "0.1.0" }"#,
    );
    plant(
        market,
        "plugins/walker/hooks/hooks.json",
        r#"{"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "echo done"}]}]}}"#,
    );
    plant(market, "plugins/walker/skills/hello/SKILL.md", "Say hello.\n");
}

/// The store's state file, parsed — what add/install/enable/disable/remove
/// are asserted against.
fn state(home: &TempDir) -> Value {
    let path = home.path().join("ganja-home").join("plugins").join("plugins.json");
    let text = fs::read_to_string(&path).expect("the state file exists");
    serde_json::from_str(&text).expect("the state file is JSON")
}

#[test]
fn the_full_walk_moves_the_state_file_through_every_transition() {
    let home = home();
    let market = home.path().join("market");
    plant_marketplace(&market);

    ganja(&home)
        .args(["plugin", "marketplace", "add"])
        .arg(&market)
        .assert()
        .success()
        .stdout(predicate::str::contains("added marketplace walk-market"));
    assert!(
        state(&home)["marketplaces"]["walk-market"]["origin"]
            .as_str()
            .expect("the origin is recorded")
            .contains("market"),
        "the state remembers where the marketplace came from"
    );

    ganja(&home)
        .args(["plugin", "install", "walker@walk-market"])
        .assert()
        .success()
        .stdout(predicate::str::contains("installed walker"));
    let installed = state(&home);
    assert_eq!(installed["plugins"]["walker"]["marketplace"], "walk-market");
    assert_eq!(installed["plugins"]["walker"]["enabled"], true);

    ganja(&home).args(["plugin", "list"]).assert().success().stdout(
        predicate::str::contains("walker (enabled, from walk-market)")
            .and(predicate::str::contains("hook Stop"))
            .and(predicate::str::contains("skills")),
    );

    ganja(&home).args(["plugin", "disable", "walker"]).assert().success();
    assert_eq!(state(&home)["plugins"]["walker"]["enabled"], false);
    ganja(&home)
        .args(["plugin", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("walker (disabled, from walk-market)"));

    ganja(&home).args(["plugin", "enable", "walker"]).assert().success();
    assert_eq!(state(&home)["plugins"]["walker"]["enabled"], true);

    ganja(&home).args(["plugin", "remove", "walker"]).assert().success();
    let removed = state(&home);
    assert!(removed["plugins"].as_object().expect("a map").is_empty());
    assert!(
        removed["marketplaces"]["walk-market"].is_object(),
        "removing a plugin keeps the marketplace added"
    );
    assert!(
        !home.path().join("ganja-home/plugins/installed/walker").exists(),
        "the installed copy is deleted"
    );
}

/// The git lane, against a bare repository this test creates — a real clone,
/// no network. The identity and signing flags are pinned per invocation so
/// the machine's own git config (a GPG key, a template) cannot decide
/// whether this passes.
#[test]
fn a_marketplace_adds_from_a_git_url_cloned_locally() {
    let home = home();
    let work = home.path().join("work");
    plant_marketplace(&work);

    let git = |args: &[&str], cwd: &Path| {
        let output = Process::new("git")
            .args([
                "-c",
                "user.email=suite@example.com",
                "-c",
                "user.name=The Suite",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git is runnable");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--quiet"], &work);
    git(&["add", "."], &work);
    git(&["commit", "--quiet", "-m", "fixture"], &work);
    let bare = home.path().join("market.git");
    git(
        &[
            "clone",
            "--quiet",
            "--bare",
            work.to_str().expect("a utf-8 path"),
            bare.to_str().expect("a utf-8 path"),
        ],
        home.path(),
    );

    ganja(&home)
        .args(["plugin", "marketplace", "add"])
        .arg(&bare)
        .assert()
        .success()
        .stdout(predicate::str::contains("added marketplace walk-market"));

    ganja(&home).args(["plugin", "install", "walker@walk-market"]).assert().success();
    assert_eq!(state(&home)["plugins"]["walker"]["enabled"], true);
}

#[test]
fn every_refusal_names_what_it_is_refusing() {
    let home = home();
    let market = home.path().join("market");
    plant_marketplace(&market);

    // An install spelled without the `@` is corrected, not guessed at.
    ganja(&home)
        .args(["plugin", "install", "walker"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("<plugin>@<marketplace>"));

    // Installing from a marketplace never added names the missing step.
    ganja(&home).args(["plugin", "install", "walker@nowhere"]).assert().failure().stderr(
        predicate::str::contains("nowhere").and(predicate::str::contains("marketplace add")),
    );

    ganja(&home).args(["plugin", "marketplace", "add"]).arg(&market).assert().success();

    // A plugin the marketplace does not list is refused with what it does.
    ganja(&home)
        .args(["plugin", "install", "stranger@walk-market"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("stranger").and(predicate::str::contains("walker")));

    // Enabling, disabling or removing what was never installed says so.
    for verb in ["enable", "disable", "remove"] {
        ganja(&home)
            .args(["plugin", verb, "ghost"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("ghost"));
    }

    // A directory that is not a marketplace fails the add and leaves no
    // half-added state behind.
    let empty = home.path().join("empty");
    fs::create_dir_all(&empty).expect("the fixture directory is creatable");
    ganja(&home)
        .args(["plugin", "marketplace", "add"])
        .arg(&empty)
        .assert()
        .failure()
        .stderr(predicate::str::contains("marketplace.json"));
    assert!(
        !home.path().join("ganja-home/plugins/marketplaces").join("empty").exists(),
        "a failed add keeps nothing"
    );
}
