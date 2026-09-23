//! An MCP server's own `output_limit` clamps a result over its budget, with
//! the spill-file notice every one-shot tool in the tree gives — and the spill
//! lands in this binary's data home, never in the person's.
//!
//! **One test, one binary**, `judge_mcp_clamp.rs`'s treatment: the shipped
//! `McpTool` clamps through `truncate::clamp_bytes`, which spills into the
//! resolved data home and takes no other directory, so this binary points
//! `XDG_DATA_HOME` at a temporary directory before anything starts. That is
//! process-wide state; a second test here would silently invalidate the
//! `// SAFETY:` comment below.
//!
//! The server is the streamable-HTTP double the judge's suites share
//! (`judge_support`), not the upstream SDK server `mcp.rs` speaks to: the
//! clamp is the client's, so which implementation answers the call does not
//! change what is under test, and the double needs no `bun`.

mod judge_support;

use std::sync::Arc;
use std::time::Duration;

use ganja_core::permission::Permissions;
use ganja_core::tool::Registry;
use ganja_core::{Config, Engine, McpServers};
use ganja_testkit::{ScriptedProvider, says, tool_call};
use judge_support::{PATIENCE, completed, mcp_server, turn};
use serde_json::json;

/// What the server hands back: twenty times the 100-byte budget.
const LONG: usize = 2_000;

#[test]
fn an_over_cap_result_is_clamped_and_spills_under_the_redirected_data_home() {
    // SAFETY: this binary holds exactly one test, and it has started neither
    // a thread nor a runtime yet, so nothing else is reading the environment.
    let data = unsafe { ganja_testkit::redirect_xdg_data_home() };
    let spills = data.path().join("ganja").join("tool-output");
    let spilled = spills.to_string_lossy().into_owned();

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime builds")
        .block_on(async {
            let address = mcp_server(|_| "x".repeat(LONG)).await;
            let config: Config = serde_json::from_value(json!({
                "mcp": {
                    "hub": {
                        "type": "remote",
                        "url": format!("http://{address}/mcp"),
                        "output_limit": 100,
                    }
                }
            }))
            .expect("the fixture config is a config");
            let (provider, _requests) =
                ScriptedProvider::new(vec![tool_call("mcp__hub__fetch", json!({})), says("done")]);
            let engine = Engine::new(
                provider,
                "recorder-model",
                Arc::new(Registry::new(Vec::new())),
                Permissions::default(),
            )
            .with_mcp(McpServers::new(config.mcp.clone(), std::path::Path::new(".")));
            engine.connect_mcp();
            let deadline = tokio::time::Instant::now() + PATIENCE;
            while tokio::time::Instant::now() < deadline && engine.mcp_status().is_empty() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            let mut events = engine.subscribe().await.expect("the first subscriber wins");

            let seen = turn(&engine, &mut events, "fetch something huge").await;

            let parts = completed(&seen, "mcp__hub__fetch");
            let (_, output, metadata) = parts.last().expect("the call completed");
            assert_eq!(metadata["truncated"], true, "{metadata}");
            assert!(output.contains("bytes truncated"), "{output}");
            assert!(output.contains("Full output saved to:"), "{output}");
            assert!(
                output.contains(&spilled),
                "the notice names this binary's data home: {output}"
            );
            assert!(
                output.len() < LONG,
                "the 100-byte budget must have decided the outcome: {} bytes",
                output.len()
            );
            engine.shutdown_mcp().await;
        });

    let files: Vec<_> = std::fs::read_dir(&spills)
        .expect("the clamp made its directory under the redirected home")
        .collect::<Result<_, _>>()
        .expect("the spill directory lists");
    assert_eq!(files.len(), 1, "the one clamped result spilled here: {files:?}");
    let whole = std::fs::read_to_string(files[0].path()).expect("the spill file reads");
    assert_eq!(whole, "x".repeat(LONG), "the spill holds the server's whole answer");
}
