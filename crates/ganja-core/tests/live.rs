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
use ganja_core::catalog;
use ganja_core::protocol::{FinishReason, Message, Part, Usage};
use ganja_core::provider::retry::MAX_ATTEMPTS;
use ganja_core::provider::{
    AnthropicProvider, ChatRequest, OpenAiProvider, OpencodeProvider, Provider, ProviderEvent,
    opencode, openrouter,
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
                effort_options: Default::default(),
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
        !events.iter().any(|event| matches!(event, ProviderEvent::Failed(_))),
        "a live turn should not fail: {events:?}"
    );
    assert!(!text.trim().is_empty(), "the model streamed no text at all: {events:?}");
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
        catalog::default_model("anthropic").expect("the catalog has a default").to_owned()
    });

    smoke(&AnthropicProvider::new(key).expect("a client builds"), &model).await;
}

/// The shape this build actually sends, against the vendor that receives it.
///
/// A turn whose tool results are followed by a steer drained at the step
/// boundary — and, since the team guards landed, by a request-only block
/// behind that — reaches the Messages API as **two or three `user` turns in a
/// row**, because `provider::anthropic`'s merge deliberately stops at the edge
/// of each canonical message. The wire's own suite pins that shape offline;
/// what it cannot pin is that the vendor takes it, and a doc sentence saying
/// consecutive same-role turns are "combined into a single turn" is a promise
/// rather than a measurement.
///
/// Observed 2026-09-02 on `claude-opus-4-8` (the catalog's default): the
/// request was accepted, the turn ended `Completed`, and the reply was exactly
/// `"alpha bravo"` — **both** user turns, the fact stated in the second-to-last
/// one and the instruction given in the last. That is combining doing what the
/// documentation says rather than the last turn winning, which is the half a
/// refusal-or-acceptance check alone would not have settled. A failure here
/// means the vendor changed the rule, not that this build started sending a
/// new shape.
#[tokio::test]
#[ignore = "talks to Anthropic; needs GANJA_LIVE_TEST=1 and ANTHROPIC_API_KEY"]
async fn anthropic_accepts_the_adjacent_user_turns_a_steer_produces() {
    let Some(key) = key("ANTHROPIC_API_KEY") else {
        return;
    };
    let model = env::var("GANJA_MODEL").ok().unwrap_or_else(|| {
        catalog::default_model("anthropic").expect("the catalog has a default").to_owned()
    });
    let provider = AnthropicProvider::new(key).expect("a client builds");

    // [user, assistant, user, user] — the last two adjacent on purpose, each
    // carrying one half of what a correct answer needs, so a reply holding
    // both is evidence the earlier one was combined rather than dropped.
    let mut assistant = Message::assistant(&model);
    assistant.parts.push(Part::text("Noted."));
    let events: Vec<ProviderEvent> = provider
        .stream(
            ChatRequest {
                effort_options: Default::default(),
                model: model.clone(),
                system: Some("Answer with the two words and nothing else.".to_owned()),
                messages: vec![
                    Message::user("My first word is alpha."),
                    assistant,
                    Message::user("My second word is bravo."),
                    Message::user("Reply with both of my words, lowercase, space-separated."),
                ],
                tools: Vec::new(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("the vendor accepted a transcript whose user turns do not alternate")
        .collect()
        .await;

    assert!(
        !events.iter().any(|event| matches!(event, ProviderEvent::Failed(_))),
        "adjacent user turns were refused mid-stream: {events:?}"
    );
    assert_eq!(
        events.last(),
        Some(&ProviderEvent::Finish(FinishReason::Completed)),
        "a turn carrying adjacent user turns should still end completed: {events:?}"
    );

    // Printed rather than asserted, for this file's standing reason: what the
    // wire did is the claim under test, and what the model chose to say is
    // not. It is the evidence recorded in the doc above.
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::TextDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    eprintln!("{model} answered {text:?} to two adjacent user turns");
}

#[tokio::test]
#[ignore = "talks to OpenAI; needs GANJA_LIVE_TEST=1 and OPENAI_API_KEY"]
async fn openai_answers_a_live_prompt() {
    let Some(key) = key("OPENAI_API_KEY") else {
        return;
    };
    let model = env::var("GANJA_MODEL").ok().unwrap_or_else(|| {
        catalog::default_model("openai").expect("the catalog has a default").to_owned()
    });
    let provider = match env::var("OPENAI_BASE_URL") {
        Ok(base) if !base.trim().is_empty() => {
            OpenAiProvider::new(key).expect("a client builds").with_base_url(base)
        }
        _ => OpenAiProvider::new(key).expect("a client builds"),
    };

    smoke(&provider, &model).await;
}

/// The gateway, over the Responses dialect it publishes.
///
/// This is the test that would settle what `provider::openrouter`'s ledger
/// refuses to guess at. It proves the floor — the request this build sends is
/// one the vendor accepts, on a model of its own namespaced spelling — and a
/// turn that ever needs to prove the *sealed-reasoning* rows should be a second
/// test with a tool call in it, not a wider assertion bolted onto this one.
///
/// The model is named rather than defaulted because this provider has no
/// catalog pin, which is the decision `provider::openrouter` documents:
/// `GANJA_MODEL` names one, and the constant below is the cheap fallback so the
/// opt-in needs one variable rather than two.
#[tokio::test]
#[ignore = "talks to OpenRouter; needs GANJA_LIVE_TEST=1 and OPENROUTER_API_KEY"]
async fn openrouter_answers_a_live_prompt() {
    /// Cheap, tool-capable, and in every published catalog vintage so far.
    const FALLBACK: &str = "openai/gpt-5-nano";

    if key("OPENROUTER_API_KEY").is_none() {
        return;
    }
    // Built from the environment rather than from the key this returned: that
    // constructor is the one a session actually takes, so the credential
    // precedence and the endpoint check are under test with it.
    let provider = openrouter::from_env().expect("an exported key builds the provider");
    let model = env::var("GANJA_MODEL")
        .ok()
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| FALLBACK.to_owned());

    smoke(&provider, &model).await;
}

/// The same gateway with an effort selected, which is the other half of what
/// its ledger cannot settle offline.
///
/// **What it asserts is that the vendor accepts the field**, not that a model
/// thinks out loud: `reasoning: {effort: …}` is documented for the surface and
/// not per model, so a row that streams no thinking is that row's business and
/// not a failure. What would be a failure is the request coming back refused —
/// which is exactly what would happen if the effort map this build synthesizes
/// were spelled the way the sibling vendor's is.
///
/// The thinking that did arrive is printed rather than asserted, so the run
/// that first sees `response.reasoning.delta` on a real turn says so.
#[tokio::test]
#[ignore = "talks to OpenRouter; needs GANJA_LIVE_TEST=1 and OPENROUTER_API_KEY"]
async fn openrouter_accepts_the_effort_its_reference_publishes() {
    /// A reasoning row of that vendor's own spelling, cheap enough to run.
    const REASONER: &str = "openai/o4-mini";

    if key("OPENROUTER_API_KEY").is_none() {
        return;
    }
    let provider = openrouter::from_env().expect("an exported key builds the provider");
    let model = env::var("GANJA_MODEL")
        .ok()
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| REASONER.to_owned());

    let events: Vec<ProviderEvent> = provider
        .stream(
            ChatRequest {
                // Exactly what `effort::roster` hands a session that picked
                // `high` on one of this gateway's rows.
                effort_options: serde_json::json!({"reasoning": {"effort": "high"}})
                    .as_object()
                    .cloned()
                    .expect("an object"),
                model: model.clone(),
                system: Some("Answer with a single word.".to_owned()),
                messages: vec![Message::user(PROMPT)],
                tools: Vec::new(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("the vendor accepted a request carrying an effort")
        .collect()
        .await;

    assert!(
        !events.iter().any(|event| matches!(event, ProviderEvent::Failed(_))),
        "the effort field was refused mid-stream: {events:?}"
    );
    assert_eq!(
        events.last(),
        Some(&ProviderEvent::Finish(FinishReason::Completed)),
        "a live turn under an effort should still end completed: {events:?}"
    );

    let thinking: String = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ReasoningDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    eprintln!("{model} thought {thinking:?} under effort=high");
}

/// The OpenCode gateways, one turn per dialect.
///
/// The only test here that drives **three wires through one provider**, which is
/// the whole of what this vendor is: the catalog picks the dialect and the wire
/// picks the header, and a live turn is the one thing that proves the gateway
/// agrees with both choices. `tests/opencode_dialects.rs` already pins the
/// request shape against a socket this build controls; what this adds is that
/// the *vendor* accepts it.
///
/// Models are named rather than defaulted, for `provider::openrouter`'s reason
/// — a gateway pins no default — and chosen as the cheapest row of each dialect
/// the probe actually ran. `GANJA_MODEL` overrides the chat one; the other two
/// are the dialects, so overriding them individually would defeat the point.
#[tokio::test]
#[ignore = "talks to OpenCode Zen; needs GANJA_LIVE_TEST=1 and OPENCODE_API_KEY"]
async fn opencode_zen_answers_a_live_prompt_on_every_dialect_it_serves() {
    if key(opencode::API_KEY_ENV).is_none() {
        return;
    }
    let provider = OpencodeProvider::zen().expect("an exported key builds the provider");

    // One per dialect: chat-completions (no transport of its own), Responses
    // (`@ai-sdk/openai`), and Messages (`@ai-sdk/anthropic`) — the last being
    // the one whose header the gateway refuses to accept as a bearer.
    let chat = env::var("GANJA_MODEL")
        .ok()
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| "glm-5".to_owned());
    for model in [chat.as_str(), "gpt-5.6-luna", "qwen3.6-plus"] {
        eprintln!("-- {} on {model}", opencode::ZEN_ID);
        smoke(&provider, model).await;
    }
}

/// Go, on the same credential and the one model that proves the dialect is per
/// (provider, model): `minimax-m3` is chat on Zen and Messages here.
#[tokio::test]
#[ignore = "talks to OpenCode Go; needs GANJA_LIVE_TEST=1 and OPENCODE_API_KEY"]
async fn opencode_go_answers_on_the_same_key_and_a_different_dialect() {
    if key(opencode::API_KEY_ENV).is_none() {
        return;
    }

    smoke(&OpencodeProvider::go().expect("one key serves both gateways"), "minimax-m3").await;
}

/// Not a network test: it pins the retry budget a live turn is willing to spend
/// so that raising it is a deliberate edit rather than something that happens
/// while tuning a delay.
#[test]
fn a_live_turn_gives_up_after_a_bounded_number_of_attempts() {
    assert_eq!(MAX_ATTEMPTS, 6, "upstream's RETRY_MAX_RETRIES, plus the first");
}
