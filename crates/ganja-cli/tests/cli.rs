//! Command-line surface of the `ganja` binary.

use assert_cmd::Command;
use predicates::prelude::*;

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
