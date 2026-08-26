//! The credential store a *live engine* names is the one `read` refuses.
//!
//! Which file holds this machine's keys is the auth layer's answer, and the
//! tools are handed it rather than going and asking — so the guard is only
//! worth anything if the engine really does hand it over. The unit tests
//! around `tool/mod.rs` pin what the comparison does with a path; this pins
//! that a path arrives at all, through the one route a model can reach.
//!
//! One test, one binary: it points `XDG_DATA_HOME` at a temporary directory,
//! which is process-wide, and `cargo test` runs a binary's tests on parallel
//! threads.

use std::sync::Arc;

use ganja_core::{
    Engine,
    permission::Permissions,
    protocol::{Command, Event, PartBody, ToolState},
    tool::{Registry, read::ReadTool},
};
use ganja_testkit::{ScriptedProvider, drain_allowing, says, tool_call};
use serde_json::json;

/// The canary planted in the store. A model that got past the guard would put
/// this in the transcript, which is sent to a provider.
const CANARY: &str = "sk-test-canary-9713";

/// How the last tool call ended, whatever it was.
fn tool_part(seen: &[Event]) -> ToolState {
    seen.iter()
        .rev()
        .find_map(|event| match event {
            Event::PartUpdated { part, .. } => match &part.body {
                PartBody::Tool { state, .. } => Some(state.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("the turn made a tool call")
}

#[tokio::test]
async fn an_engines_own_credential_store_is_refused_to_the_model_that_asks_for_it() {
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    let home = unsafe { ganja_testkit::redirect_xdg_data_home() };

    let store = ganja_core::auth::store_path().expect("the redirect resolves a store path");
    std::fs::create_dir_all(store.parent().expect("the store sits in a directory"))
        .expect("the fixture nests");
    std::fs::write(
        &store,
        json!({ "anthropic": { "key": CANARY } }).to_string(),
    )
    .expect("the fixture writes");
    assert!(
        store.starts_with(home.path()),
        "the fixture must not be pointed at the real user's store: {}",
        store.display()
    );

    let (provider, _requests) = ScriptedProvider::new(vec![
        tool_call("read", json!({ "filePath": store.to_str().unwrap() })),
        says("I cannot read that"),
    ]);
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(vec![Arc::new(ReadTool)])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "read the auth file".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain_allowing(&engine, &mut events).await;

    let ToolState::Error { error, .. } = tool_part(&seen) else {
        panic!("the store is never readable: {:?}", tool_part(&seen));
    };
    assert!(
        error.contains("is ganja's credential store"),
        "the refusal names what it refused: {error}"
    );
    assert!(
        !error.contains(CANARY),
        "and the refusal is not a way to read it: {error}"
    );
}
