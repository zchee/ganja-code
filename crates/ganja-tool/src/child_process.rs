//! Environment policy shared by engine child processes.

use std::process::Command;

/// Server and hosted-worker credentials belong to the parent, never to its children.
pub const PRIVATE_ENV: &[&str] =
    &["GANJA_SERVER_PASSWORD", "GANJA_SERVER_USERNAME", "GANJA_CLOUD_PASSWORD"];

/// Keeps server and hosted-worker credentials out of a child environment.
///
/// Apply after any configured environment overrides so they cannot restore these names.
pub fn isolate(command: &mut Command) {
    for name in PRIVATE_ENV {
        command.env_remove(name);
    }
}
