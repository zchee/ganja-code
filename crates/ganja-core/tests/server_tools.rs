//! A tool the *provider* ran, from the event to the transcript (**D489**).
//!
//! The wire half — an `openrouter:*` output item becoming a
//! [`ProviderEvent::ServerTool`], and an item this build has never seen still
//! skipping with a debug line — is pinned inside `ganja-provider`, against the
//! real SSE decoder. What this file pins is everything after that seam, which
//! is where the damage would be:
//!
//! 1. **It reaches the transcript** as a finished row, published to every
//!    frontend on the stream, carrying the call and what came back. That it
//!    survives a round trip through storage is `storage.rs`'s own suite: the
//!    record is serde-derived, so the interesting half is here.
//! 2. **Nothing runs.** The turn takes exactly one model request — a call the
//!    loop had executed would have forced a second one to carry its result —
//!    and the registry's own tool is never touched.
//! 3. **Nobody is asked.** No permission request crosses the stream: a dialog
//!    about work another machine already did has no honest answer.
//! 4. **The turn is unharmed.** The reply that came after the row is what the
//!    session ends up holding, and the turn completes.
//!
//! Driven through [`ScriptedProvider`] rather than a socket for the reason the
//! seam above exists: the gateway's own constructor points at that vendor and
//! nothing else, so a loopback build of it is only reachable from inside the
//! provider crate — which is exactly where the frame-level tests live.

use std::sync::Arc;

use ganja_core::{
    Engine,
    permission::Permissions,
    protocol::{Command, Event, FinishReason, PartBody},
    provider::ProviderEvent,
    tool::Registry,
};
use ganja_testkit::{RecorderTool, ScriptedProvider, drain};
use serde_json::json;

/// What the gateway reported it ran, as the wire hands it over.
fn searched() -> ProviderEvent {
    ProviderEvent::ServerTool {
        tool: "openrouter:web_search".to_owned(),
        input: json!({"query": "rust edition 2024"}),
        output: "3 results".to_owned(),
    }
}

#[tokio::test]
async fn a_provider_run_tool_is_recorded_rendered_and_never_executed() {
    let (provider, requests) = ScriptedProvider::new(vec![vec![
        searched(),
        ProviderEvent::TextDelta("Rust 2024 shipped in February 2025.".to_owned()),
        ProviderEvent::Finish(FinishReason::Completed),
    ]]);
    // A tool with the same *name* as the row, so that "nothing ran" is a fact
    // about the loop rather than about there being nothing to run.
    let (recorder, ran) = RecorderTool::new("openrouter:web_search", "searched", "done");
    let engine = Engine::new(
        provider,
        "recorder-model",
        Arc::new(Registry::new(vec![recorder])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SendPrompt {
            text: "when did rust 2024 ship".to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = drain(&mut events).await;

    // 1. The row reached a frontend, whole.
    let published: Vec<&PartBody> = seen
        .iter()
        .filter_map(|event| match event {
            Event::PartStarted { part, .. } => Some(&part.body),
            _ => None,
        })
        .collect();
    assert!(
        published.contains(&&PartBody::ServerTool {
            tool: "openrouter:web_search".to_owned(),
            input: json!({"query": "rust edition 2024"}),
            output: "3 results".to_owned(),
        }),
        "a frontend applying every event has to end up holding this: {published:?}"
    );

    // 2. Nothing ran, and the turn never asked for a second request — the two
    //    ways an executed call would have shown itself.
    assert!(
        ran.lock().expect("the call log").is_empty(),
        "a row is a report of work already done, not work to do"
    );
    assert_eq!(
        requests.lock().expect("the request log").len(),
        1,
        "a call the loop executed would have needed a second request to carry \
         its result back"
    );

    // 3. Nobody was asked about it.
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, Event::PermissionRequested { .. })),
        "a dialog about another machine's work has no honest answer: {seen:?}"
    );

    // 4. The turn is exactly what the model said, and it completed.
    let finished = seen
        .iter()
        .find_map(|event| match event {
            Event::MessageFinished { reason, .. } => Some(*reason),
            _ => None,
        })
        .expect("the turn finishes");
    assert_eq!(finished, FinishReason::Completed);

    let said: String = seen
        .iter()
        .filter_map(|event| match event {
            Event::PartDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        said, "Rust 2024 shipped in February 2025.",
        "the reply the gateway's own tool fed is the turn's own text"
    );
}
