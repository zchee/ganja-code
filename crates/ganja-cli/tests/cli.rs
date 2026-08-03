//! Command-line surface of the `ganja` binary.

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn an_unknown_provider_is_refused_before_the_terminal_is_taken_over() {
    Command::new(env!("CARGO_BIN_EXE_ganja"))
        .env("GANJA_PROVIDER", "anthropic")
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("GANJA_PROVIDER")
                .and(predicate::str::contains("anthropic"))
                .and(predicate::str::contains("fake")),
        );
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
