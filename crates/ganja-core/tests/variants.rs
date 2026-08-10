//! Model variants end to end: the catalog names them, the engine validates
//! and remembers them, the request carries their options, and the session row
//! round-trips them across a resume.
//!
//! In its own binary because the catalog is a process-global table and these
//! tests install one: [`fixture_catalog`] points [`GANJA_MODELS_PATH`] at a
//! written `api.json` whose `fake` provider carries variants, exactly once
//! per process. Every test calls it first, so the unsafe environment writes
//! all happen inside the `Once` before anything else in this binary reads the
//! environment — the discipline `catalog_offline.rs` established. Storage
//! never leaves a per-test temporary directory, so nothing here can read or
//! write a real user's sessions.
//!
//! [`GANJA_MODELS_PATH`]: ganja_core::catalog::MODELS_PATH_ENV

use std::sync::{Arc, Once, OnceLock};

use futures::{StreamExt as _, stream::BoxStream};
use ganja_core::{
    Engine, catalog,
    permission::Permissions,
    protocol::{Command, Event},
    provider::{FakeProvider, fake},
    tool::Registry,
};
use tempfile::TempDir;

/// The options the fixture publishes under `canned`'s `max` variant, spelled
/// once: what the catalog carries is what the request must carry.
fn max_options() -> serde_json::Map<String, serde_json::Value> {
    serde_json::json!({"thinking": {"type": "enabled", "budgetTokens": 32000}})
        .as_object()
        .cloned()
        .expect("the fixture options are an object")
}

/// Installs a catalog whose `fake` provider serves two models: `canned` with
/// the `max` and `mini` variants, and `plain` with none.
fn fixture_catalog() {
    static FIXTURE: OnceLock<TempDir> = OnceLock::new();
    static INSTALL: Once = Once::new();

    INSTALL.call_once(|| {
        let dir = TempDir::new().expect("a temporary directory");
        let path = dir.path().join("api.json");
        let body = serde_json::json!({
            "fake": {
                "models": {
                    fake::MODEL: {
                        "limit": {"context": 200_000, "output": 64_000},
                        "cost": {"input": 1.0, "output": 2.0},
                        "variants": {
                            "max": max_options(),
                            "mini": {"reasoningEffort": "low"},
                        },
                    },
                    "plain": {
                        "limit": {"context": 200_000, "output": 64_000},
                    },
                },
            },
        });
        std::fs::write(&path, body.to_string()).expect("the fixture is writable");

        // SAFETY: this runs exactly once, inside the `Once` every test in the
        // binary enters before touching anything else, so no other thread is
        // reading the environment while it is written.
        unsafe {
            std::env::set_var(catalog::MODELS_PATH_ENV, &path);
            std::env::set_var(catalog::DISABLE_FETCH_ENV, "1");
        }
        assert!(catalog::load_cached(), "the fixture catalog adopts");

        // Held for the life of the process: the catalog re-reads nothing, but
        // a vanished fixture would turn a later `load_cached` into a delete.
        FIXTURE.set(dir).expect("the fixture installs once");
    });
}

/// An ephemeral engine over `provider`, with no tools: these tests prove the
/// selection machinery, not the loop.
fn engine_over(provider: FakeProvider) -> Engine {
    Engine::new(
        Arc::new(provider),
        fake::MODEL,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
}

/// Drains events until the turn closes, returning everything seen.
async fn until_finished(events: &mut BoxStream<'static, Event>) -> Vec<Event> {
    let mut seen = Vec::new();
    while let Some(event) = events.next().await {
        let done = matches!(event, Event::MessageFinished { .. });
        seen.push(event);
        if done {
            return seen;
        }
    }

    panic!("the stream ended without finishing the turn: {seen:?}");
}

#[tokio::test]
async fn selecting_a_variant_lands_its_option_map_in_the_next_request() {
    fixture_catalog();
    let provider = FakeProvider::new("one two", std::time::Duration::from_millis(1));
    let recorder = provider.clone();
    let engine = engine_over(provider);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SwitchVariant {
            variant: Some("max".to_owned()),
        })
        .await
        .expect("the fixture catalog carries max");
    assert_eq!(engine.variant().as_deref(), Some("max"));

    engine
        .send(Command::SendPrompt {
            text: "hi".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    let seen = until_finished(&mut events).await;
    assert!(
        seen.iter().any(|event| matches!(
            event,
            Event::VariantChanged { variant: Some(name), .. } if name == "max"
        )),
        "the adoption was announced: {seen:?}"
    );

    let requests = recorder.recorded();
    assert_eq!(requests.len(), 1, "one turn, one request");
    assert_eq!(
        requests[0].variant_options,
        max_options(),
        "the catalog's option map rides the request verbatim"
    );
}

#[tokio::test]
async fn switching_to_a_model_without_the_variant_clears_it_and_announces_the_clear() {
    fixture_catalog();
    let engine = engine_over(FakeProvider::new("hi", std::time::Duration::from_millis(1)));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(Command::SwitchVariant {
            variant: Some("mini".to_owned()),
        })
        .await
        .expect("the fixture catalog carries mini");
    engine
        .send(Command::SwitchModel {
            model: "plain".to_owned(),
        })
        .await
        .expect("the fixture catalog carries plain");

    assert_eq!(
        engine.variant(),
        None,
        "a model without the name cannot keep the selection (upstream prompt.ts:654)"
    );
    let adopted = events.next().await.expect("the adoption frame");
    assert!(
        matches!(&adopted, Event::VariantChanged { variant: Some(name), .. } if name == "mini"),
        "got {adopted:?}"
    );
    let cleared = events.next().await.expect("the clearing frame");
    assert!(
        matches!(&cleared, Event::VariantChanged { variant: None, .. }),
        "the clear is announced, not silent: {cleared:?}"
    );
}

#[tokio::test]
async fn a_wrong_name_is_refused_listing_the_real_names() {
    fixture_catalog();
    let engine = engine_over(FakeProvider::new("hi", std::time::Duration::from_millis(1)));

    let refusal = engine
        .send(Command::SwitchVariant {
            variant: Some("nope".to_owned()),
        })
        .await
        .expect_err("the fixture catalog has no such name")
        .to_string();

    assert!(refusal.contains("nope"), "got {refusal}");
    assert!(
        refusal.contains("max, mini"),
        "the useful half of the refusal is the names that would have worked: {refusal}"
    );
    assert_eq!(engine.variant(), None);
}

#[tokio::test]
async fn the_stored_variant_survives_a_resume() {
    fixture_catalog();
    let directory = TempDir::new().expect("a temporary directory");
    let storage = || ganja_core::Storage::open(directory.path().join("storage"));

    let first = Engine::persistent(
        Arc::new(FakeProvider::new(
            "hello",
            std::time::Duration::from_millis(1),
        )),
        fake::MODEL,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage(),
    );
    let mut events = first.subscribe().await.expect("the first subscriber wins");
    first
        .send(Command::SwitchVariant {
            variant: Some("max".to_owned()),
        })
        .await
        .expect("the fixture catalog carries max");
    first
        .send(Command::SendPrompt {
            text: "hi".to_owned(),
            mentions: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
    until_finished(&mut events).await;
    let session = first
        .current_session()
        .expect("the prompt minted a session")
        .id;
    drop(events);
    drop(first);

    let second = Engine::persistent(
        Arc::new(FakeProvider::new(
            "hello",
            std::time::Duration::from_millis(1),
        )),
        fake::MODEL,
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage(),
    );
    second
        .resume(&session)
        .await
        .expect("the stored session resumes");

    assert_eq!(
        second.variant().as_deref(),
        Some("max"),
        "the row carried the variant across the restart"
    );
}
