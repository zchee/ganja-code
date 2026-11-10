//! Engine subprocesses keep parent-only credentials out of their environments.

#![cfg(unix)]

use std::collections::BTreeMap;

use ganja_core::config::{HookCommand, HookHandler, HookMatcher};
use ganja_core::hook::{Hooks, Payload};

const PROBE: &str = "printf '%s|%s|%s|%s' \"${GANJA_SERVER_PASSWORD+present}\" \"${GANJA_SERVER_USERNAME+present}\" \"${GANJA_CLOUD_PASSWORD+present}\" \"$GANJA_TEST_CHILD_VISIBLE\"";

fn isolated(test: &str) -> bool {
    const CHILD: &str = "GANJA_TEST_CHILD_ENVIRONMENT";
    if std::env::var(CHILD).as_deref() == Ok(test) {
        return true;
    }
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", test, "--nocapture"])
        .env(CHILD, test)
        .env("GANJA_SERVER_PASSWORD", "synthetic-server-password")
        .env("GANJA_SERVER_USERNAME", "synthetic-server-username")
        .env("GANJA_CLOUD_PASSWORD", "synthetic-cloud-password")
        .env("GANJA_TEST_CHILD_VISIBLE", "retained")
        .output()
        .expect("isolated test process");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

#[tokio::test]
async fn hook_children_do_not_receive_server_credentials() {
    if !isolated("hook_children_do_not_receive_server_credentials") {
        return;
    }
    let root = tempfile::tempdir().expect("hook root");
    let hooks = Hooks::new(
        &BTreeMap::from([(
            "UserPromptSubmit".to_owned(),
            vec![HookMatcher {
                matcher: None,
                hooks: vec![HookHandler::Command(HookCommand {
                    command: PROBE.to_owned(),
                    timeout: Some(5),
                })],
            }],
        )]),
        root.path(),
    )
    .expect("configured hook");
    let outcome =
        hooks.fire("fixture", &Payload::UserPromptSubmit { prompt: "test".to_owned() }).await;
    assert!(outcome.notices.is_empty(), "{:?}", outcome.notices);
    assert_eq!(outcome.context, ["|||retained"]);
}
