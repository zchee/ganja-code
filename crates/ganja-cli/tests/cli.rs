//! Command-line surface of the `ganja` binary.
//!
//! Every credential assertion is on the redacted tail. A test that printed a
//! whole key would put it in CI output, which is the failure the redaction
//! exists to prevent.

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// A key shaped like the real thing, planted so the tests can prove it never
/// comes back out whole.
const CANARY: &str = "sk-canary-8842";

/// Builds an invocation with its own data directory and no inherited keys, so
/// that a developer's exported `ANTHROPIC_API_KEY` cannot decide whether these
/// pass.
fn ganja(data: &TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ganja"));
    command.env("XDG_DATA_HOME", data.path());
    for variable in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"] {
        command.env_remove(variable);
    }

    command
}

fn data() -> TempDir {
    TempDir::new().expect("a temporary directory is creatable")
}

fn stored_at(data: &TempDir) -> std::path::PathBuf {
    data.path().join("ganja").join("auth.json")
}

#[test]
fn version_flag_reports_the_binary_name_and_version() {
    Command::new(env!("CARGO_BIN_EXE_ganja"))
        .arg("--version")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("ganja")
                .and(predicate::str::contains(env!("CARGO_PKG_VERSION"))),
        );
}

#[test]
fn a_key_given_on_the_command_line_is_stored_and_reported_redacted() {
    let data = data();

    ganja(&data)
        .args(["auth", "login", "--provider", "anthropic", "--key", CANARY])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("anthropic")
                .and(predicate::str::contains("****8842"))
                .and(predicate::str::contains(CANARY).not()),
        );

    assert!(
        stored_at(&data).is_file(),
        "the key should have been stored"
    );

    ganja(&data)
        .args(["auth", "list"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("anthropic")
                .and(predicate::str::contains("****8842"))
                .and(predicate::str::contains("auth.json"))
                .and(predicate::str::contains(CANARY).not()),
        );
}

/// `pass show … | ganja auth login` has to work, which means a key arriving on
/// a pipe is read rather than prompted for.
#[test]
fn a_piped_key_is_read_from_standard_input() {
    let data = data();

    ganja(&data)
        .args(["auth", "login", "--provider", "openai"])
        .write_stdin(format!("{CANARY}\n"))
        .assert()
        .success()
        .stdout(
            predicate::str::contains("openai")
                .and(predicate::str::contains("****8842"))
                .and(predicate::str::contains(CANARY).not()),
        );

    ganja(&data)
        .args(["auth", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("openai"));
}

#[cfg(unix)]
#[test]
fn a_stored_key_is_written_where_only_its_owner_can_read_it() {
    use std::os::unix::fs::PermissionsExt as _;

    let data = data();
    ganja(&data)
        .args(["auth", "login", "--key", CANARY])
        .assert()
        .success();

    let mode = std::fs::metadata(stored_at(&data))
        .expect("the file exists")
        .permissions()
        .mode()
        & 0o777;

    assert_eq!(mode, 0o600, "got {mode:04o}");
}

#[test]
fn an_empty_key_is_refused_rather_than_stored() {
    let data = data();

    ganja(&data)
        .args(["auth", "login", "--key", "   "])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no key"));

    assert!(
        !stored_at(&data).exists(),
        "a refused login should write nothing"
    );
}

#[test]
fn logging_out_forgets_the_key_and_says_so_when_there_was_none() {
    let data = data();
    ganja(&data)
        .args(["auth", "login", "--provider", "openai", "--key", CANARY])
        .assert()
        .success();

    ganja(&data)
        .args(["auth", "logout", "--provider", "openai"])
        .assert()
        .success()
        .stdout(predicate::str::contains("forgot"));

    ganja(&data)
        .args(["auth", "logout", "--provider", "openai"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no stored"));

    ganja(&data)
        .args(["auth", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no credentials"));
}

/// A stored key that an exported variable outranks is the one way a successful
/// login can change nothing, so the listing shows which is in use and the
/// login says so.
#[test]
fn an_environment_variable_outranks_the_stored_key_and_is_pointed_out() {
    let data = data();
    ganja(&data)
        .args(["auth", "login", "--provider", "anthropic", "--key", CANARY])
        .assert()
        .success();

    ganja(&data)
        .env("ANTHROPIC_API_KEY", "sk-environment-4242")
        .args(["auth", "list"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("ANTHROPIC_API_KEY")
                .and(predicate::str::contains("****4242"))
                .and(predicate::str::contains("****8842").not()),
        );

    ganja(&data)
        .env("ANTHROPIC_API_KEY", "sk-environment-4242")
        .args(["auth", "login", "--provider", "anthropic", "--key", CANARY])
        .assert()
        .success()
        .stderr(
            predicate::str::contains("ANTHROPIC_API_KEY").and(predicate::str::contains(
                "used in preference to the stored key",
            )),
        );
}

#[test]
fn models_lists_the_catalog_and_marks_one_default_per_provider() {
    Command::new(env!("CARGO_BIN_EXE_ganja"))
        .arg("models")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("PROVIDER")
                .and(predicate::str::contains("$/MTOK IN"))
                .and(predicate::str::contains("claude-sonnet-5*"))
                .and(predicate::str::contains("gpt-5.6*"))
                .and(predicate::str::contains("claude-haiku-4-5"))
                // The context window is compacted rather than spelled out.
                .and(predicate::str::contains("1.0M"))
                .and(predicate::str::contains("200.0k")),
        );
}

/// Configuration mistakes have to be reported before the terminal is put into
/// raw mode, or the message is drawn over and lost.
#[test]
fn an_unknown_provider_is_refused_before_the_terminal_is_taken_over() {
    Command::new(env!("CARGO_BIN_EXE_ganja"))
        .env("GANJA_PROVIDER", "definitely-not-a-provider")
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("GANJA_PROVIDER")
                .and(predicate::str::contains("definitely-not-a-provider")),
        );
}

/// A provider with no credential anywhere is refused with the command that
/// fixes it, which is the whole point of storing keys.
#[test]
fn a_provider_without_a_credential_is_refused_and_says_how_to_fix_it() {
    let data = data();

    ganja(&data)
        .env("GANJA_PROVIDER", "anthropic")
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("ANTHROPIC_API_KEY")
                .and(predicate::str::contains("ganja auth login")),
        );
}

#[test]
fn an_unknown_subcommand_is_refused() {
    Command::new(env!("CARGO_BIN_EXE_ganja"))
        .arg("definitely-not-a-subcommand")
        .assert()
        .failure();
}
