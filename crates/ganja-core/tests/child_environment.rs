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

#[tokio::test]
async fn mcp_children_do_not_receive_server_credentials() {
    if !isolated("mcp_children_do_not_receive_server_credentials") {
        return;
    }
    let root = tempfile::tempdir().expect("MCP root");
    let observed = root.path().join("environment");
    let servers = ganja_core::McpServers::new(
        BTreeMap::from([(
            "environment-probe".to_owned(),
            ganja_core::config::McpServer::Local(ganja_core::config::McpLocal {
                command: vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    format!("{PROBE} > \"$1\""),
                    "probe".to_owned(),
                    observed.to_str().expect("fixture path").to_owned(),
                ],
                cwd: None,
                environment: BTreeMap::from([
                    ("GANJA_SERVER_PASSWORD".to_owned(), "configured-password".to_owned()),
                    ("GANJA_SERVER_USERNAME".to_owned(), "configured-username".to_owned()),
                    ("GANJA_CLOUD_PASSWORD".to_owned(), "configured-cloud-password".to_owned()),
                ]),
                enabled: true,
                timeout: None,
                output_limit: None,
            }),
        )]),
        root.path(),
    );
    servers.connect_all().await;
    servers.shutdown().await;
    assert_eq!(std::fs::read_to_string(observed).expect("MCP child ran"), "|||retained");
}

#[tokio::test]
async fn lsp_children_do_not_receive_server_credentials() {
    if !isolated("lsp_children_do_not_receive_server_credentials") {
        return;
    }
    let root = tempfile::tempdir().expect("LSP root");
    let observed = root.path().join("environment");
    let source = root.path().join("source.fixture");
    std::fs::write(&source, "fixture").expect("source file");
    let config = ganja_core::config::LspConfig::Servers(BTreeMap::from([(
        "environment-probe".to_owned(),
        ganja_core::config::LspEntry {
            command: Some(vec![
                "sh".to_owned(),
                "-c".to_owned(),
                format!("{PROBE} > \"$1\""),
                "probe".to_owned(),
                observed.to_str().expect("fixture path").to_owned(),
            ]),
            extensions: Some(vec![".fixture".to_owned()]),
            disabled: false,
            env: BTreeMap::from([
                ("GANJA_SERVER_PASSWORD".to_owned(), "configured-password".to_owned()),
                ("GANJA_SERVER_USERNAME".to_owned(), "configured-username".to_owned()),
                ("GANJA_CLOUD_PASSWORD".to_owned(), "configured-cloud-password".to_owned()),
            ]),
            initialization: None,
        },
    )]));
    let lsp = ganja_core::lsp::Lsp::new(Some(&config), root.path()).expect("LSP config");
    lsp.touch(&source, false).await;
    assert_eq!(std::fs::read_to_string(observed).expect("LSP child ran"), "|||retained");
}
