//! Environment policy shared by engine and editor child processes.
//!
//! New spawn sites must call [`isolate`] after applying configured environment overrides.
//! Real-spawn regressions cover shell children in `ganja-tool/src/shell_tests.rs`, hooks,
//! MCP, language servers and plugin git in `ganja-core/tests/child_environment.rs`,
//! provider subprocesses in `ganja-provider/tests/child_environment.rs`, snapshot git in
//! `ganja-storage/tests/child_environment.rs`, and editors in `ganja-tui/src/external_tests.rs`.

use std::process::Command;

/// Server and hosted-worker credentials reserved for the parent process.
pub const PRIVATE_ENV: &[&str] =
    &["GANJA_SERVER_PASSWORD", "GANJA_SERVER_USERNAME", "GANJA_CLOUD_PASSWORD"];

/// Removes server and hosted-worker credentials from a child's own environment.
///
/// This prevents accidental disclosure, such as a tool printing its environment. It does not
/// stop a process running as the same user from reading the parent's environment through
/// other operating-system interfaces; it is not a process-isolation boundary.
///
/// Apply after any configured environment overrides so they cannot restore these names.
pub fn isolate(command: &mut Command) {
    for name in PRIVATE_ENV {
        command.env_remove(name);
    }
}
