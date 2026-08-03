//! The scripted fake provider, reached the way a demo reaches it.
//!
//! Everything about scripts is unit-tested through an explicit path, which
//! proves the playing but not the wiring: a demo does not call `with_script`,
//! it exports [`SCRIPT_ENV`](provider::fake::SCRIPT_ENV) and runs the binary.
//! What this pins is that the variable is read at all, on both routes a session
//! can take to a provider, and that it is *not* read on the route the rest of
//! the test suite takes — because that carve-out is what keeps a script
//! exported in one shell from quietly rewriting what the lib tests assert.
//!
//! One test, one binary, on purpose: it mutates process-wide environment
//! variables, and `cargo test` runs the tests inside a binary on parallel
//! threads.

use std::{env, fs, time::Duration};

use futures::StreamExt as _;
use ganja_core::{
    Message,
    provider::{self, ChatRequest, FakeProvider, Provider, ProviderEvent, fake},
};
use tokio_util::sync::CancellationToken;

/// A script whose first turn is unmistakably not the canned reply.
const SCRIPT: &str = r#"{
    "cadence_ms": 0,
    "turns": [
        {
            "text": "Scripted.",
            "tool_calls": [{"name": "read", "args": {"filePath": "src/main.rs"}}]
        }
    ]
}"#;

/// Everything `provider` streams for one prompt.
async fn turn(provider: &dyn Provider) -> Vec<ProviderEvent> {
    provider
        .stream(
            ChatRequest {
                model: fake::MODEL.to_owned(),
                system: None,
                messages: vec![Message::user("go")],
                tools: Vec::new(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("the script plays")
        .collect()
        .await
}

/// The reply text a turn streamed.
fn text(events: &[ProviderEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::TextDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn an_exported_script_is_what_the_fake_provider_plays() {
    let home = tempfile::tempdir().expect("a temp directory");
    let script = home.path().join("demo.json");
    fs::write(&script, SCRIPT).expect("the script is writable");

    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        env::set_var(fake::SCRIPT_ENV, &script);
        env::set_var(provider::PROVIDER_ENV, fake::ID);
        // The catalog does not price the canned model, and a stray model name
        // would be handed to the provider instead of it.
        env::remove_var(provider::MODEL_ENV);
    }

    // The route a demo takes: the variables pick the provider, and the provider
    // picks up the script without anyone naming it in code.
    let selection = provider::from_env().expect("the fake provider needs no credentials");
    assert_eq!(selection.provider.id(), fake::ID);

    let events = turn(selection.provider.as_ref()).await;

    assert_eq!(
        text(&events),
        "Scripted.",
        "the exported script should answer, not the canned reply: {events:?}"
    );
    assert!(
        events.contains(&ProviderEvent::ToolCallStart {
            id: "call_1".to_owned(),
            name: "read".to_owned(),
        }),
        "a scripted call should reach the engine as a call: {events:?}"
    );
    assert_eq!(
        events.last(),
        Some(&ProviderEvent::Finish(ganja_core::FinishReason::Completed)),
        "a scripted turn still ends like a turn, got {events:?}"
    );

    // The same for a provider built directly, which is what a frontend that
    // skips `from_env` does.
    assert_eq!(text(&turn(&FakeProvider::default()).await), "Scripted.");

    // And the carve-out that makes the rest of the suite safe: a provider given
    // its reply in code ignores the variable entirely. Without this, exporting
    // a script would rewrite what every other test in the workspace asserts.
    let canned = turn(&FakeProvider::new("one two three", Duration::ZERO)).await;

    assert_eq!(text(&canned), "one two three");
    assert!(
        !canned
            .iter()
            .any(|event| matches!(event, ProviderEvent::ToolCallStart { .. })),
        "an explicit reply must not pick up the exported script: {canned:?}"
    );
}
