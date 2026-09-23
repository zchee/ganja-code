//! A clamped MCP result through the shipped `McpTool` (**D567**, criterion 28
//! for MCP, r6 B4): the result says what its clamp appended — `truncated`
//! and `hint_len`, beside the `server` it came from — and the judge sends
//! the server's own text without the spill hint or the path it names.
//!
//! **One test, one binary**, for this directory's `XDG_DATA_HOME` rule: the
//! shipped tool clamps through `truncate::clamp_bytes`, which spills into the
//! resolved data home and takes no other directory, so this binary points
//! that home at a temporary directory before anything starts. That is
//! process-wide state; a second test here would silently invalidate the
//! `// SAFETY:` comment below. Nothing reaches the vendor: the judge is built
//! by `Judge::from_settings` over a loopback double.

mod judge_support;

use ganja_core::Config;
use ganja_testkit::{ScriptedProvider, says, tool_call};
use judge_support::{Reply, Vendor, completed, mcp_engine, mcp_only, mcp_server, tuning, turn};
use serde_json::json;

#[test]
fn a_clamped_mcp_result_is_sent_without_its_hint_or_spill_path() {
    let data = ganja_testkit::temp_dir();
    // SAFETY: this binary holds exactly one test, and it has started neither
    // a thread nor a runtime yet, so nothing else is reading the environment.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", data.path());
    }
    let spills = data.path().join("ganja").join("tool-output");
    let spilled = spills.to_string_lossy().into_owned();

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime builds")
        .block_on(async {
            let address = mcp_server(|_| "a line an MCP server returned.\n".repeat(3_000)).await;
            let config: Config = serde_json::from_value(json!({
                "mcp": { "hub": { "type": "remote", "url": format!("http://{address}/mcp") } }
            }))
            .expect("the fixture config is a config");
            let vendor = Vendor::start(|_| Reply::Answer { fire: false }).await;
            let judge = vendor.judge(mcp_only(&["hub"]), tuning(20_000, 3, 60_000));
            let (provider, _requests) =
                ScriptedProvider::new(vec![tool_call("mcp__hub__fetch", json!({})), says("done")]);
            let engine = mcp_engine(provider, &config, judge).await;
            let mut events = engine.subscribe().await.expect("the first subscriber wins");

            let seen = turn(&engine, &mut events, "fetch a result over the cap").await;

            let parts = completed(&seen, "mcp__hub__fetch");
            let (_, output, metadata) = parts.last().expect("the call completed");
            assert_eq!(metadata["server"], "hub", "the server as the config keys it");
            assert_eq!(metadata["truncated"], true);
            let hint_len = metadata["hint_len"]
                .as_u64()
                .and_then(|len| usize::try_from(len).ok())
                .expect("a cut result reports hint_len");
            let (own, hint) = output.split_at(output.len() - hint_len);
            assert!(
                own.ends_with(" bytes truncated..."),
                "hint_len counts exactly what came after the notice: {hint:?}"
            );
            assert!(hint.starts_with("\n\nThe tool call succeeded"), "{hint:?}");
            assert!(hint.contains(&spilled), "the spill landed in this binary's data home");

            let requests = vendor.seen();
            assert!(!requests.is_empty());
            assert!(
                requests.iter().all(|request| !request.content.contains("Full output saved to"))
            );
            assert!(requests.iter().all(|request| !request.content.contains(&spilled)));
            assert!(requests.iter().all(|request| !request.content.contains("tool-output")));
            assert_eq!(
                requests.iter().map(|request| request.content.len()).sum::<usize>(),
                own.len(),
                "the server's own text is sent, all of it"
            );
            engine.shutdown_mcp().await;
        });

    let spill = std::fs::read_dir(&spills)
        .expect("the clamp made its directory under the redirected home")
        .next();
    assert!(spill.is_some(), "the clamp spilled here and nowhere else");
}
