//! Smoke tests against the real providers.
//!
//! Mock first, live second: everything these prove about mapping is already
//! covered by the fixture suite and `tests/http.rs`. What they add is the one
//! thing a loopback socket cannot — that the request this build sends is a
//! request the vendor accepts today, with the header names, the API version and
//! the model identifier all still current.
//!
//! Both are `#[ignore]`, so `cargo test` never reaches them and
//! `cargo test -- --ignored` reaches them and finds them inert unless
//! `GANJA_LIVE_TEST=1` and the provider's key are both set. That combination is
//! deliberate: a contributor running the full suite spends nothing, and CI can
//! opt in without the suite failing on a machine that simply has no key.

use std::env;

use futures::StreamExt as _;
use ganja_core::{
    catalog,
    protocol::{FinishReason, Message, Usage},
    provider::{
        AnthropicProvider, ChatRequest, OpenAiProvider, Provider, ProviderEvent,
        retry::MAX_ATTEMPTS,
    },
};
use tokio_util::sync::CancellationToken;

/// Variable that has to be `1` before any of this talks to a vendor.
const LIVE_ENV: &str = "GANJA_LIVE_TEST";

/// The prompt, chosen so the reply is one cheap token and the assertion is not
/// a judgement about what a model felt like saying.
const PROMPT: &str = "Reply with exactly: pong";

/// The credential to run a live test with, or [`None`] to skip it.
fn key(variable: &str) -> Option<String> {
    if env::var(LIVE_ENV).as_deref() != Ok("1") {
        eprintln!("skipping: {LIVE_ENV} is not 1");
        return None;
    }

    match env::var(variable) {
        Ok(key) if !key.trim().is_empty() => Some(key),
        _ => {
            eprintln!("skipping: {variable} is unset");
            None
        }
    }
}

/// Runs [`PROMPT`] and asserts the vendor answered with text and a bill.
async fn smoke(provider: &dyn Provider, model: &str) {
    let events: Vec<ProviderEvent> = provider
        .stream(
            ChatRequest {
                variant_options: Default::default(),
                model: model.to_owned(),
                system: Some("Answer with a single word.".to_owned()),
                messages: vec![Message::user(PROMPT)],
                tools: Vec::new(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("the vendor accepted the request")
        .collect()
        .await;

    let text: String = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::TextDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    let usage = events.iter().find_map(|event| match event {
        ProviderEvent::Usage(usage) => Some(*usage),
        _ => None,
    });

    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ProviderEvent::Failed(_))),
        "a live turn should not fail: {events:?}"
    );
    assert!(
        !text.trim().is_empty(),
        "the model streamed no text at all: {events:?}"
    );
    assert_eq!(
        events.last(),
        Some(&ProviderEvent::Finish(FinishReason::Completed)),
        "a live turn should end with a completed finish"
    );

    let usage = usage.unwrap_or_else(|| panic!("a live turn should be billed: {events:?}"));
    assert_ne!(usage, Usage::default(), "the bill should not be all zeroes");
    assert!(usage.input_tokens > 0, "the prompt costs something");
    assert!(usage.output_tokens > 0, "the reply costs something");
    assert!(
        catalog::model(model).is_some(),
        "the model a live turn defaults to should be one the catalog can price"
    );

    eprintln!("{model} answered {text:?} for {usage:?}");
}

#[tokio::test]
#[ignore = "talks to Anthropic; needs GANJA_LIVE_TEST=1 and ANTHROPIC_API_KEY"]
async fn anthropic_answers_a_live_prompt() {
    let Some(key) = key("ANTHROPIC_API_KEY") else {
        return;
    };
    let model = env::var("GANJA_MODEL").ok().unwrap_or_else(|| {
        catalog::default_model("anthropic")
            .expect("the catalog has a default")
            .to_owned()
    });

    smoke(
        &AnthropicProvider::new(key).expect("a client builds"),
        &model,
    )
    .await;
}

#[tokio::test]
#[ignore = "talks to OpenAI; needs GANJA_LIVE_TEST=1 and OPENAI_API_KEY"]
async fn openai_answers_a_live_prompt() {
    let Some(key) = key("OPENAI_API_KEY") else {
        return;
    };
    let model = env::var("GANJA_MODEL").ok().unwrap_or_else(|| {
        catalog::default_model("openai")
            .expect("the catalog has a default")
            .to_owned()
    });
    let provider = match env::var("OPENAI_BASE_URL") {
        Ok(base) if !base.trim().is_empty() => OpenAiProvider::new(key)
            .expect("a client builds")
            .with_base_url(base),
        _ => OpenAiProvider::new(key).expect("a client builds"),
    };

    smoke(&provider, &model).await;
}

/// Not a network test: it pins the retry budget a live turn is willing to spend
/// so that raising it is a deliberate edit rather than something that happens
/// while tuning a delay.
#[test]
fn a_live_turn_gives_up_after_a_bounded_number_of_attempts() {
    assert_eq!(MAX_ATTEMPTS, 5);
}
