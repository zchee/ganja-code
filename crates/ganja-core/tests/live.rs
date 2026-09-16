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
//!
//! The six **D563** platform and seat probes at the bottom are a different kind
//! of test under the same gate: measurements rather than smoke checks. Each prints markdown
//! rows for `.omc/research/2026-09-16-openai-platform-param-probe.md` and
//! asserts only that it ran, because what it measures moves a `const` rather
//! than failing a build. Run them one at a time and with output shown, or the
//! rows are captured and lost:
//!
//! ```sh
//! GANJA_LIVE_TEST=1 OPENAI_API_KEY=… cargo test -p ganja-core --test live \
//!     -- --ignored --nocapture --test-threads=1 probe_every_platform_option_this_build_sends
//! ```
//!
//! The four seat probes need `ganja auth login chatgpt` instead of a key. One
//! offline pin rides beside them and runs in the ordinary suite: the platform
//! probe's rows cover exactly the names this build would send.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use ganja_core::auth::openai::Login;
use ganja_core::config::ResponsesOptions;
use ganja_core::permission::Permissions;
use ganja_core::protocol::{
    Command, Event, FinishReason, Message, Part, PartBody, PermissionReply, Usage,
};
use ganja_core::provider::responses::options::{
    PLATFORM_ACCEPTED, PLATFORM_INCLUDE, PLATFORM_SERVER_TOOLS, PLATFORM_TIERS, RequestOptions,
};
use ganja_core::provider::retry::MAX_ATTEMPTS;
use ganja_core::provider::{
    AnthropicProvider, ChatRequest, OpenAiProvider, OpencodeProvider, Provider, ProviderEvent,
    ResponsesProvider, openai, opencode, openrouter, responses,
};
use ganja_core::tool::{Registry, ToolDefinition};
use ganja_core::{Engine, auth, catalog, responses_ladder};
use ganja_testkit::prompt;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
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
                turn_start: 0,
                responses: Default::default(),
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
                turn_start: 0,
                responses: Default::default(),
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
                turn_start: 0,
                // Exactly what `effort::roster` hands a session that picked
                // `high` on one of this gateway's rows.
                responses: Default::default(),
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

// ---------------------------------------------------------------------------
// D563: what the platform and the seat do with the Responses options.
//
// Six probes the user runs by hand. Each prints markdown table rows for the
// probe record the module doc names, and asserts only that the run itself
// completed: an answer here moves a name between two
// `const`s in `provider/responses/options.rs`, and a refusal is a finding
// rather than a failure. Run them one at a time with `--nocapture`, or the
// rows are swallowed and interleaved.
//
// Two ways of reaching a vendor, chosen by what each id lets a test point at:
//
// - **The seat** is reached through [`Forwarder`], a loopback relay the wire
//   is pointed at with `ResponsesProvider::at` — the constructor
//   `from_stored` itself is, differing only in the base URL — so the request
//   is this build's own bytes, headers included, and the whole
//   `response.completed` object comes back to be read. The wire hands back
//   only the four echoes `ServedOptions` carries, and "honored" versus
//   "recognized" needs the rest.
// - **The platform** is reached by `ResponsesProvider::from_env` directly,
//   because its base URL moves only through `OPENAI_BASE_URL`, and a test
//   that set a process variable to find its own relay would be racing every
//   other thread of the binary. The echo is read off a second, raw leg that
//   sends the same fragment; the wire's leg is the one whose answer moves a
//   `const`, and the two are printed side by side so a disagreement is seen.
// ---------------------------------------------------------------------------

/// Printed rows cut a refusal body here, so one runaway answer cannot bury the
/// table it belongs in.
const QUOTE_LIMIT: usize = 600;

/// Pause between two probe calls, for the rate limiter's sake rather than the
/// measurement's.
const BETWEEN_CALLS: Duration = Duration::from_millis(1500);

/// How long an engine turn in a seat probe is given to settle its tail.
const SETTLE: Duration = Duration::from_secs(30);

/// The model the platform's option set was designed against.
///
/// `GANJA_MODEL` overrides it, and the probe header names what ran, because
/// several keys (`temperature`, `top_p`, `reasoning.*`) are refused per model
/// rather than per endpoint.
const PLATFORM_MODEL: &str = "gpt-5.5";

/// The model whose tier table differs from every other's.
const SOL: &str = "gpt-5.6-sol";

/// The schema `ganja run --json-schema` is exercised with in
/// `crates/ganja-cli/tests/json_schema_run.rs` (AC-31), byte for byte.
///
/// It deliberately does **not** set `additionalProperties: false`, which is
/// the half of probe 3 no offline test can answer: whether the unconditional
/// `strict: true` is refused for such a schema.
const AC31_SCHEMA: &str =
    r#"{"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"]}"#;

/// Whether this machine may take a seat probe: [`LIVE_ENV`], and a stored
/// ChatGPT login under the seat's own id (**D555**).
fn seated() -> bool {
    if env::var(LIVE_ENV).as_deref() != Ok("1") {
        eprintln!("skipping: {LIVE_ENV} is not 1");
        return false;
    }
    if auth::oauth_for(responses::CHATGPT_ID).ok().flatten().is_none() {
        eprintln!("skipping: no stored ChatGPT login; run `ganja auth login chatgpt`");
        return false;
    }

    true
}

/// `GANJA_MODEL` when set and not blank, else `fallback`.
fn model_or(fallback: &str) -> String {
    env::var("GANJA_MODEL")
        .ok()
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

/// A positive integer from `variable`, else `fallback`.
fn knob(variable: &str, fallback: usize) -> usize {
    env::var(variable).ok().and_then(|value| value.trim().parse().ok()).unwrap_or(fallback)
}

/// `text` on one line and cut at [`QUOTE_LIMIT`], for a table cell.
fn quoted(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ").replace('|', "\\|");
    match flat.char_indices().nth(QUOTE_LIMIT) {
        Some((end, _)) => format!("{}…", &flat[..end]),
        None => flat,
    }
}

/// A JSON value as one table cell.
fn cell(value: Option<&Value>) -> String {
    value.map_or_else(|| "absent".to_owned(), |value| quoted(&value.to_string()))
}

/// What one request through the wire came to.
struct WireOutcome {
    /// Every event, when the request got as far as a stream.
    events: Vec<ProviderEvent>,
    /// The refusal, when the request or the stream was refused.
    refused: Option<String>,
    /// From the start of the request to its first content event.
    first_content: Option<Duration>,
    /// From the start of the request to the stream's end.
    total: Duration,
}

impl WireOutcome {
    /// `accepted`, or `rejected: "<sentence>"`.
    fn verdict(&self) -> String {
        self.refused.as_ref().map_or_else(
            || "accepted".to_owned(),
            |refusal| format!("rejected: \"{}\"", quoted(refusal)),
        )
    }

    /// The tier the terminal frame echoed, as the wire folded it.
    fn served_tier(&self) -> Option<String> {
        self.events.iter().find_map(|event| match event {
            ProviderEvent::Served(served) => served.service_tier.clone(),
            _ => None,
        })
    }

    /// Input tokens the vendor billed, when it reported a bill.
    fn input_tokens(&self) -> Option<u64> {
        self.events.iter().find_map(|event| match event {
            ProviderEvent::Usage(usage) => Some(usage.input_tokens),
            _ => None,
        })
    }
}

/// Sends `request` through `provider` and collects what came back, never
/// panicking on a refusal: a refusal is what these probes are here to read.
async fn through_wire(provider: &dyn Provider, request: ChatRequest) -> WireOutcome {
    let started = Instant::now();
    let mut events = Vec::new();
    let mut first_content = None;
    let stream = match provider.stream(request, CancellationToken::new()).await {
        Ok(stream) => stream,
        Err(error) => {
            return WireOutcome {
                events,
                refused: Some(error.to_string()),
                first_content,
                total: started.elapsed(),
            };
        }
    };
    futures::pin_mut!(stream);
    while let Some(event) = stream.next().await {
        if first_content.is_none()
            && matches!(
                event,
                ProviderEvent::TextDelta(_)
                    | ProviderEvent::ReasoningDelta(_)
                    | ProviderEvent::ToolCallStart { .. }
            )
        {
            first_content = Some(started.elapsed());
        }
        events.push(event);
    }
    let refused = events.iter().find_map(|event| match event {
        ProviderEvent::Failed(error) => Some(error.to_string()),
        _ => None,
    });

    WireOutcome { events, refused, first_content, total: started.elapsed() }
}

/// A request asking `model` for one word, carrying `responses` and offering
/// `tools`.
fn one_word(model: &str, responses: RequestOptions, tools: Vec<ToolDefinition>) -> ChatRequest {
    ChatRequest {
        model: model.to_owned(),
        system: Some("Answer with a single word.".to_owned()),
        messages: vec![Message::user(PROMPT)],
        tools,
        responses,
        ..ChatRequest::default()
    }
}

/// A tool no one-word answer has a reason to call, so a roster can ride a
/// request without the reply depending on it.
fn noop() -> ToolDefinition {
    ToolDefinition {
        name: "noop".to_owned(),
        description: "Does nothing. Never needed to answer.".to_owned(),
        schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
    }
}

/// `bash`'s own definition, as `Registry::with_builtins` advertises it.
fn bash() -> ToolDefinition {
    Registry::with_builtins()
        .definitions()
        .into_iter()
        .find(|tool| tool.name == "bash")
        .expect("bash is a builtin")
}

/// `fragment` as a [`RequestOptions::body`] — the wire's own spelling, which
/// is what `responses_ladder::body` builds from a config table key by key.
fn body(fragment: Value) -> RequestOptions {
    RequestOptions {
        body: fragment.as_object().cloned().expect("a body fragment is an object"),
        ..RequestOptions::default()
    }
}

/// Merges `fragment` into `base`, objects member by member.
fn merge(base: &mut Value, fragment: &Value) {
    match (base, fragment) {
        (Value::Object(base), Value::Object(fragment)) => {
            for (key, value) in fragment {
                merge(base.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
        (base, fragment) => *base = fragment.clone(),
    }
}

/// The terminal response object in an SSE body, and every event's `type` in
/// order.
fn sse_read(body: &str) -> (Option<Value>, Vec<Value>) {
    let mut terminal = None;
    let mut events = Vec::new();
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<Value>(data.trim()) else {
            continue;
        };
        if matches!(
            event["type"].as_str(),
            Some("response.completed" | "response.incomplete" | "response.failed")
        ) {
            terminal = Some(event["response"].clone());
        }
        events.push(event);
    }

    (terminal, events)
}

/// The `type` of every output item a response carried.
fn output_types(response: Option<&Value>) -> String {
    let types: Vec<&str> = response
        .and_then(|response| response["output"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|item| item["type"].as_str())
        .collect();

    if types.is_empty() { "none".to_owned() } else { types.join(", ") }
}

/// Every advertised tool as `type:name`, from a request or a response.
fn advertised(tools: Option<&Value>) -> String {
    let named: Vec<String> = tools
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|tool| {
            format!(
                "{}:{}",
                tool["type"].as_str().unwrap_or("?"),
                tool["name"].as_str().unwrap_or("-")
            )
        })
        .collect();

    if named.is_empty() { "none".to_owned() } else { named.join(", ") }
}

/// One request the [`Forwarder`] relayed.
#[derive(Clone)]
struct Exchange {
    /// The body exactly as the wire built it.
    from_wire: Value,
    /// The body as it left for the vendor, after the relay's edit.
    forwarded: Value,
    /// The vendor's status code.
    status: u16,
    /// The vendor's body, scrubbed of the credential it was asked with.
    body: String,
    /// From forwarding to the vendor's first body bytes.
    first_byte: Duration,
    /// From forwarding to the vendor's last body bytes.
    total: Duration,
}

impl Exchange {
    /// The terminal response object, when the vendor streamed one.
    fn response(&self) -> Option<Value> {
        sse_read(&self.body).0
    }

    /// `accepted`, or `rejected: "<body>"` — including a 200 whose stream
    /// ended in `response.failed` or an `error` event.
    fn verdict(&self) -> String {
        if self.status != 200 {
            return format!("rejected ({}): \"{}\"", self.status, quoted(&self.body));
        }
        let (terminal, events) = sse_read(&self.body);
        if let Some(error) = events.iter().find(|event| event["type"] == "error") {
            return format!("rejected in-stream: \"{}\"", quoted(&error.to_string()));
        }
        match terminal {
            Some(response) if response["status"] == "failed" => {
                format!("rejected in-stream: \"{}\"", quoted(&response["error"].to_string()))
            }
            Some(_) => "accepted".to_owned(),
            None => format!("no terminal frame: \"{}\"", quoted(&self.body)),
        }
    }

    /// Whether the wire built this one as a title request rather than a step.
    fn is_title(&self) -> bool {
        ganja_testkit::is_title_body(&self.from_wire)
    }
}

/// An edit the relay makes to a body before forwarding it.
type Edit = Arc<dyn Fn(&mut Map<String, Value>) + Send + Sync>;

/// A loopback relay between this build's wire and a vendor.
///
/// The wire is pointed at [`Forwarder::base_url`], builds its request exactly
/// as it would for the vendor, and the relay sends it on — every header but the
/// hop-by-hop ones, and the body after one optional [`Edit`] — then hands the
/// vendor's answer back whole. What it keeps is the pair of bodies and the
/// answer, which is the evidence no event the wire emits carries.
///
/// **The credential never leaves this process in a printed line.** The values
/// of the two headers that authenticate are remembered for exactly one purpose,
/// [`Forwarder::scrub`], which every body passes through before it is kept.
struct Forwarder {
    /// What the wire is pointed at.
    base_url: String,
    /// Every exchange, in the order they finished.
    exchanges: Arc<Mutex<Vec<Exchange>>>,
}

/// Headers the relay does not forward: they describe the hop to it, not the
/// request, and `accept-encoding` would hand back bytes the relay then decodes
/// under a header it no longer sends.
const HOP_BY_HOP: [&str; 6] =
    ["host", "content-length", "connection", "accept-encoding", "transfer-encoding", "keep-alive"];

/// Headers whose values authenticate, and so are scrubbed from every kept body.
const CREDENTIAL_HEADERS: [&str; 2] = ["authorization", "chatgpt-account-id"];

impl Forwarder {
    /// A relay to `upstream`, the base URL the wire would otherwise have used.
    async fn start(upstream: &'static str, edit: Option<Edit>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a loopback port");
        let base_url = format!("http://{}", listener.local_addr().expect("a bound address"));
        let exchanges = Arc::new(Mutex::new(Vec::new()));
        let client = reqwest::Client::new();

        let kept = Arc::clone(&exchanges);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let (kept, client, edit) = (Arc::clone(&kept), client.clone(), edit.clone());
                tokio::spawn(async move {
                    let Some((path, headers, raw)) = read_request(&mut socket).await else {
                        return;
                    };
                    let from_wire: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
                    let mut forwarded = from_wire.clone();
                    if let (Some(edit), Some(object)) = (&edit, forwarded.as_object_mut()) {
                        edit(object);
                    }
                    let secrets: Vec<String> = headers
                        .iter()
                        .filter(|(name, _)| CREDENTIAL_HEADERS.contains(&name.as_str()))
                        .map(|(_, value)| value.strip_prefix("Bearer ").unwrap_or(value).to_owned())
                        .filter(|value| !value.is_empty())
                        .collect();

                    let mut outbound = client.post(format!("{upstream}{path}"));
                    for (name, value) in &headers {
                        if !HOP_BY_HOP.contains(&name.as_str()) {
                            outbound = outbound.header(name, value);
                        }
                    }
                    let started = Instant::now();
                    let (status, content_type, body, first_byte) =
                        match outbound.body(forwarded.to_string()).send().await {
                            Ok(mut response) => {
                                let status = response.status();
                                let content_type = response
                                    .headers()
                                    .get("content-type")
                                    .and_then(|value| value.to_str().ok())
                                    .unwrap_or("application/json")
                                    .to_owned();
                                let mut bytes = Vec::new();
                                let mut first_byte = None;
                                while let Ok(Some(chunk)) = response.chunk().await {
                                    first_byte.get_or_insert_with(|| started.elapsed());
                                    bytes.extend_from_slice(&chunk);
                                }
                                (status, content_type, bytes, first_byte.unwrap_or_default())
                            }
                            Err(error) => (
                                reqwest::StatusCode::BAD_GATEWAY,
                                "text/plain".to_owned(),
                                format!("the relay could not reach {upstream}: {error}")
                                    .into_bytes(),
                                started.elapsed(),
                            ),
                        };
                    let total = started.elapsed();

                    let head = format!(
                        "HTTP/1.1 {} {}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        status.as_u16(),
                        status.canonical_reason().unwrap_or("Status"),
                        body.len(),
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                    let _ = socket.shutdown().await;

                    let body = scrub(&String::from_utf8_lossy(&body), &secrets);
                    kept.lock().expect("the exchange log is never poisoned").push(Exchange {
                        from_wire,
                        forwarded,
                        status: status.as_u16(),
                        body,
                        first_byte,
                        total,
                    });
                });
            }
        });

        Self { base_url, exchanges }
    }

    /// The seat's provider, pointed at this relay: `from_stored` in every
    /// respect but the base URL.
    fn seat(&self) -> ResponsesProvider {
        let login = Login::new().expect("a login client builds");

        ResponsesProvider::at(&self.base_url, Arc::new(login)).expect("a loopback base is allowed")
    }

    /// Every exchange so far.
    fn exchanges(&self) -> Vec<Exchange> {
        self.exchanges.lock().expect("the exchange log is never poisoned").clone()
    }

    /// Forgets every exchange, so the next probe reads only its own.
    fn forget(&self) {
        self.exchanges.lock().expect("the exchange log is never poisoned").clear();
    }
}

/// `text` with every one of `secrets` replaced, so a vendor that quotes a
/// credential back cannot put it in a printed row.
fn scrub(text: &str, secrets: &[String]) -> String {
    secrets.iter().fold(text.to_owned(), |text, secret| text.replace(secret.as_str(), "<redacted>"))
}

/// One HTTP/1.1 request off `socket`: the path, the lower-cased headers, and a
/// `content-length` body.
async fn read_request(socket: &mut TcpStream) -> Option<(String, Vec<(String, String)>, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    let head_end = loop {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.lines();
    let path = lines.next()?.split_whitespace().nth(1)?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect();
    let length: usize = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }

    Some((path, headers, body))
}

/// How a platform probe's echo is read back.
enum Echo {
    /// The response object's member at this JSON pointer, compared with what
    /// was sent.
    At(String, Value),
    /// The response's `tools`, which a hosted or custom advertisement comes
    /// back in with the vendor's defaults filled in.
    Tools,
    /// How many `response.output_text.delta` events carry `obfuscation` —
    /// the one key whose effect is on the stream rather than the object.
    Obfuscation,
    /// Nothing is echoed for this key on either id so far.
    Nothing,
}

/// One platform probe: a [`PLATFORM_ACCEPTED`] key (or a default this build
/// injects), the value the wire is handed, and the raw fragment its echo leg
/// sends.
struct PlatformProbe {
    /// The dotted key, as `set_keys` reports it, or `default:` and a name.
    key: &'static str,
    /// What was sent, for the row.
    sent: String,
    /// What the wire is handed.
    options: RequestOptions,
    /// Whether the request offers a tool roster, as several keys need.
    roster: Vec<ToolDefinition>,
    /// The fragment the raw leg merges into its baseline.
    raw: Value,
    /// How the echo is judged.
    echo: Echo,
}

impl PlatformProbe {
    /// A probe of a key that rides the configured body.
    fn body(key: &'static str, fragment: Value, echo: Echo) -> Self {
        Self {
            key,
            sent: fragment.to_string(),
            options: body(fragment.clone()),
            roster: Vec::new(),
            raw: fragment,
            echo,
        }
    }

    /// The same, offering the no-op roster a roster key needs to be kept.
    fn with_roster(mut self) -> Self {
        self.roster = vec![noop()];
        self
    }
}

/// Every platform probe, in the order the rows print.
///
/// Built offline and pinned offline by
/// `the_platform_probe_covers_every_key_tier_and_hosted_tool_this_build_accepts`,
/// so a key added to [`PLATFORM_ACCEPTED`] without a probe reddens the ordinary
/// suite rather than waiting for somebody to notice a missing row.
fn platform_probes() -> Vec<PlatformProbe> {
    let mut probes = Vec::new();

    // The two defaults this build injects on every platform request, sent
    // with no configuration at all: the whole body is the wire's.
    probes.push(PlatformProbe {
        key: "default:stream_options.include_obfuscation",
        sent: "(injected) {\"stream_options\":{\"include_obfuscation\":false}}".to_owned(),
        options: RequestOptions::default(),
        roster: Vec::new(),
        raw: json!({"stream_options": {"include_obfuscation": false}}),
        echo: Echo::Obfuscation,
    });
    probes.push(PlatformProbe {
        key: "default:tool_choice",
        sent: "(injected beside a roster) {\"tool_choice\":\"auto\"}".to_owned(),
        options: RequestOptions::default(),
        roster: vec![noop()],
        raw: json!({"tool_choice": "auto"}),
        echo: Echo::At("/tool_choice".to_owned(), json!("auto")),
    });

    for tier in PLATFORM_TIERS {
        probes.push(PlatformProbe {
            key: "service_tier",
            sent: format!("{{\"service_tier\":\"{tier}\"}}"),
            options: RequestOptions {
                service_tier: Some((*tier).to_owned()),
                ..RequestOptions::default()
            },
            roster: Vec::new(),
            raw: json!({"service_tier": tier}),
            echo: Echo::At("/service_tier".to_owned(), json!(tier)),
        });
    }

    probes.push(PlatformProbe::body(
        "reasoning.context",
        json!({"reasoning": {"context": "all_turns"}}),
        Echo::At("/reasoning/context".to_owned(), json!("all_turns")),
    ));
    probes.push(PlatformProbe {
        key: "reasoning.summary",
        sent: "{\"reasoning\":{\"summary\":\"concise\"}}".to_owned(),
        options: RequestOptions {
            reasoning_summary: Some("concise".to_owned()),
            ..RequestOptions::default()
        },
        roster: Vec::new(),
        raw: json!({"reasoning": {"summary": "concise"}}),
        echo: Echo::At("/reasoning/summary".to_owned(), json!("concise")),
    });
    // `pro` rather than `standard`: `standard` is every model's default on the
    // seat, so sending it could only ever read as "recognized".
    probes.push(PlatformProbe::body(
        "reasoning.mode",
        json!({"reasoning": {"mode": "pro"}}),
        Echo::At("/reasoning/mode".to_owned(), json!("pro")),
    ));
    probes.push(PlatformProbe::body(
        "text.verbosity",
        json!({"text": {"verbosity": "low"}}),
        Echo::At("/text/verbosity".to_owned(), json!("low")),
    ));
    probes.push(
        PlatformProbe::body(
            "parallel_tool_calls",
            json!({"parallel_tool_calls": false}),
            Echo::At("/parallel_tool_calls".to_owned(), json!(false)),
        )
        .with_roster(),
    );
    // `true`, the one value a config changes: `false` is already injected.
    probes.push(PlatformProbe::body(
        "stream_options.include_obfuscation",
        json!({"stream_options": {"include_obfuscation": true}}),
        Echo::Obfuscation,
    ));
    probes.push(
        PlatformProbe::body(
            "tool_choice",
            json!({"tool_choice": "required"}),
            Echo::At("/tool_choice".to_owned(), json!("required")),
        )
        .with_roster(),
    );

    let bash = bash();
    probes.push(PlatformProbe {
        key: "custom_tools",
        sent: "custom_tools = [\"bash\"] (custom beside its function twin)".to_owned(),
        options: RequestOptions {
            custom_tools: vec!["bash".to_owned()],
            ..RequestOptions::default()
        },
        roster: vec![bash.clone()],
        raw: json!({"tools": [
            function_entry(&bash),
            {"type": "custom", "name": bash.name, "description": bash.description},
        ]}),
        echo: Echo::Tools,
    });

    let vector_store = env::var("GANJA_PROBE_VECTOR_STORE")
        .unwrap_or_else(|_| "vs_ganja_probe_missing".to_owned());
    for entry in [
        json!({"type": "web_search"}),
        json!({"type": "image_generation"}),
        json!({"type": "file_search", "vector_store_ids": [vector_store]}),
        json!({"type": "code_interpreter", "container": {"type": "auto"}}),
        json!({
            "type": "mcp",
            "server_label": "deepwiki",
            "server_url": "https://mcp.deepwiki.com/mcp",
            "require_approval": "never",
        }),
    ] {
        probes.push(PlatformProbe {
            key: "server_tools",
            sent: entry.to_string(),
            options: RequestOptions {
                server_tools: vec![entry.as_object().cloned().expect("an entry is an object")],
                ..RequestOptions::default()
            },
            roster: Vec::new(),
            raw: json!({"tools": [entry]}),
            echo: Echo::Tools,
        });
    }

    for entry in PLATFORM_INCLUDE {
        probes.push(PlatformProbe {
            key: "include",
            sent: format!("{{\"include\":[\"{entry}\"]}}"),
            options: RequestOptions {
                include: vec![(*entry).to_owned()],
                ..RequestOptions::default()
            },
            roster: Vec::new(),
            raw: json!({"include": [entry]}),
            echo: Echo::Nothing,
        });
    }

    probes.push(PlatformProbe::body(
        "context_management",
        json!({"context_management": [{"type": "compaction", "compact_threshold": 1000}]}),
        Echo::At(
            "/context_management".to_owned(),
            json!([{"type": "compaction", "compact_threshold": 1000}]),
        ),
    ));
    for (key, value) in [
        ("client_metadata", json!({"probe": "ganja"})),
        ("max_output_tokens", json!(512)),
        ("prompt_cache_key", json!("ganja-probe-2026-09-16")),
        ("prompt_cache_retention", json!("24h")),
        ("prompt_cache_options", json!({"mode": "explicit", "ttl": "30m"})),
        ("temperature", json!(0.5)),
        ("top_p", json!(0.5)),
        ("truncation", json!("auto")),
        ("safety_identifier", json!("ganja-probe-user")),
        ("user", json!("ganja-probe-user")),
        ("metadata", json!({"probe": "ganja"})),
        ("moderation", json!({"model": "omni-moderation-latest"})),
    ] {
        probes.push(PlatformProbe::body(
            key,
            json!({ key: value.clone() }),
            Echo::At(format!("/{key}"), value),
        ));
    }
    probes.push(
        PlatformProbe::body(
            "max_tool_calls",
            json!({"max_tool_calls": 1}),
            Echo::At("/max_tool_calls".to_owned(), json!(1)),
        )
        .with_roster(),
    );
    // What `top_logprobs` comes back through has to be asked for beside it.
    probes.push(PlatformProbe {
        key: "top_logprobs",
        sent: "{\"top_logprobs\":2} + include message.output_text.logprobs".to_owned(),
        options: RequestOptions {
            include: vec!["message.output_text.logprobs".to_owned()],
            ..body(json!({"top_logprobs": 2}))
        },
        roster: Vec::new(),
        raw: json!({"top_logprobs": 2, "include": ["message.output_text.logprobs"]}),
        echo: Echo::At("/top_logprobs".to_owned(), json!(2)),
    });

    probes
}

/// The raw leg's answer: the status, the terminal object and the event list.
struct RawAnswer {
    /// The vendor's status code, or `0` when it could not be reached.
    status: u16,
    /// The body, for a refusal.
    body: String,
    /// The terminal response object.
    response: Option<Value>,
    /// Every streamed event.
    events: Vec<Value>,
}

/// `tool` as the raw leg advertises it: a Responses function entry.
fn function_entry(tool: &ToolDefinition) -> Value {
    json!({"type": "function", "name": tool.name, "description": tool.description, "parameters": tool.schema})
}

/// Sends the raw leg: a baseline one-word request with `fragment` merged in
/// and `roster` advertised, straight to the platform with `key`.
async fn raw_platform(
    client: &reqwest::Client,
    key: &str,
    model: &str,
    fragment: &Value,
    roster: &[ToolDefinition],
) -> RawAnswer {
    let mut request = json!({
        "model": model,
        "instructions": "Answer with a single word.",
        "input": PROMPT,
        "store": false,
        "stream": true,
    });
    if !roster.is_empty() {
        let tools: Vec<Value> = roster.iter().map(function_entry).collect();
        request["tools"] = Value::Array(tools);
    }
    merge(&mut request, fragment);

    match client
        .post(format!("{}/responses", openai::DEFAULT_BASE_URL))
        .bearer_auth(key)
        .json(&request)
        .send()
        .await
    {
        Ok(response) => {
            let status = response.status().as_u16();
            let body = scrub(&response.text().await.unwrap_or_default(), &[key.to_owned()]);
            let (response, events) = sse_read(&body);
            RawAnswer { status, body, response, events }
        }
        Err(error) => RawAnswer {
            status: 0,
            body: scrub(&error.to_string(), &[key.to_owned()]),
            response: None,
            events: Vec::new(),
        },
    }
}

/// The echo cell and the verdict for one platform probe.
fn judged(probe: &PlatformProbe, wire: &WireOutcome, raw: &RawAnswer) -> (String, &'static str) {
    if raw.status != 200 {
        let refusal = format!("raw leg {}: \"{}\"", raw.status, quoted(&raw.body));
        return (refusal, if wire.refused.is_some() { "rejected" } else { "CHECK BY HAND" });
    }
    let echo = match &probe.echo {
        Echo::At(pointer, sent) => {
            let found = raw.response.as_ref().and_then(|response| response.pointer(pointer));
            let verdict = if found == Some(sent) { "honored" } else { "recognized" };
            (cell(found), verdict)
        }
        Echo::Tools => {
            let tools = raw.response.as_ref().and_then(|response| response.get("tools"));
            (format!("tools: {}", cell(tools)), "recognized")
        }
        Echo::Obfuscation => {
            let deltas: Vec<&Value> = raw
                .events
                .iter()
                .filter(|event| event["type"] == "response.output_text.delta")
                .collect();
            let carrying = deltas.iter().filter(|event| event.get("obfuscation").is_some()).count();
            (format!("{carrying} of {} deltas carry obfuscation", deltas.len()), "recognized")
        }
        Echo::Nothing => ("no echo expected".to_owned(), "recognized"),
    };
    if wire.refused.is_some() {
        return (echo.0, "rejected");
    }

    echo
}

/// Probe 1: every key the platform list names, every tier, every hosted
/// tool type, and the two defaults this build injects.
///
/// **Needs** `GANJA_LIVE_TEST=1` and an exported `OPENAI_API_KEY` (a stored
/// platform key is not enough: the raw leg reads the exported one), with
/// `OPENAI_BASE_URL` unset. `GANJA_MODEL` moves the model from
/// [`PLATFORM_MODEL`]; `GANJA_PROBE_VECTOR_STORE` names a real vector store for
/// the `file_search` row, which otherwise measures what a missing store is
/// told.
///
/// Prints one markdown row per probe — key, what was sent, the wire's answer,
/// the raw leg's echo, and the verdict in the seat probe's vocabulary — which
/// is the report's table as-is. A row marked `CHECK BY HAND` is one where the
/// two legs disagreed. Every row costs one wire call and one raw call.
///
/// Recorded 2026-09-17 on `gpt-5.5`: the `scale` and `ultrafast` tiers and
/// `access_programs` were refused and left their lists; five more refusals
/// named the model rather than the key, and those keys stayed.
#[tokio::test]
#[ignore = "talks to OpenAI's platform; needs GANJA_LIVE_TEST=1 and an exported OPENAI_API_KEY"]
async fn probe_every_platform_option_this_build_sends() {
    let Some(key) = key("OPENAI_API_KEY") else {
        return;
    };
    let model = model_or(PLATFORM_MODEL);
    let provider = ResponsesProvider::from_env().expect("an exported key builds the platform wire");
    let client = reqwest::Client::new();
    let probes = platform_probes();

    let baseline = raw_platform(&client, &key, &model, &json!({}), &[]).await;
    let echoed = |name| cell(baseline.response.as_ref().and_then(|response| response.get(name)));
    eprintln!("## platform key set on {model} ({} probes)\n", probes.len());
    eprintln!(
        "baseline: HTTP {}; echo service_tier={} reasoning={} text={} parallel_tool_calls={} truncation={} temperature={} top_p={}\n",
        baseline.status,
        echoed("service_tier"),
        echoed("reasoning"),
        echoed("text"),
        echoed("parallel_tool_calls"),
        echoed("truncation"),
        echoed("temperature"),
        echoed("top_p"),
    );
    eprintln!("| key | sent | wire (this build's bytes) | echo (raw leg) | verdict |");
    eprintln!("|---|---|---|---|---|");

    for probe in &probes {
        tokio::time::sleep(BETWEEN_CALLS).await;
        let wire =
            through_wire(&provider, one_word(&model, probe.options.clone(), probe.roster.clone()))
                .await;
        tokio::time::sleep(BETWEEN_CALLS).await;
        let raw = raw_platform(&client, &key, &model, &probe.raw, &probe.roster).await;
        let (echo, verdict) = judged(probe, &wire, &raw);
        eprintln!(
            "| `{}` | `{}` | {} | {} | **{verdict}** |",
            probe.key,
            quoted(&probe.sent),
            wire.verdict(),
            echo
        );
    }
}

/// Probe 2: whether the platform serves `ultrafast` on
/// [`SOL`], and what `/fast on` there would be buying.
///
/// **Needs** `GANJA_LIVE_TEST=1` and `OPENAI_API_KEY`. Two rounds of
/// `default`, `priority` and `ultrafast`, interleaved so a slow minute does not
/// land on one tier: one sample each is what the seat probe had, and its one
/// `ultrafast` sample was the slowest of five. Prints the wire's answer, the
/// tier the terminal frame echoed, the time to first content and the total.
///
/// Recorded 2026-09-17: `ultrafast` was refused in both rounds (500,
/// `Invalid service_tier argument`), so `/fast on` over `openai` stays `priority`.
#[tokio::test]
#[ignore = "talks to OpenAI's platform; needs GANJA_LIVE_TEST=1 and OPENAI_API_KEY"]
async fn probe_what_the_platform_answers_to_each_tier_on_sol() {
    if key("OPENAI_API_KEY").is_none() {
        return;
    }
    let provider = ResponsesProvider::from_env().expect("an exported key builds the platform wire");

    eprintln!("## service_tier on {SOL}, platform\n");
    eprintln!("| round | sent | wire | echoed tier | first content | total |");
    eprintln!("|---|---|---|---|---|---|");
    for round in 1..=2 {
        for tier in ["default", "priority", "ultrafast"] {
            tokio::time::sleep(BETWEEN_CALLS).await;
            let options =
                RequestOptions { service_tier: Some(tier.to_owned()), ..RequestOptions::default() };
            let wire = through_wire(&provider, one_word(SOL, options, Vec::new())).await;
            eprintln!(
                "| {round} | `{tier}` | {} | {} | {} | {:.2?} |",
                wire.verdict(),
                wire.served_tier().unwrap_or_else(|| "absent".to_owned()),
                wire.first_content.map_or_else(|| "none".to_owned(), |at| format!("{at:.2?}")),
                wire.total
            );
        }
    }
}

/// Every text part and tool part a drained turn left, for the report.
fn parts(events: &[Event]) -> (String, Vec<PartBody>) {
    let mut text = String::new();
    let mut tools: Vec<PartBody> = Vec::new();
    for event in events {
        let Event::PartUpdated { part, .. } = event else {
            continue;
        };
        match &part.body {
            PartBody::Text { text: now } => text.clone_from(now),
            body @ PartBody::Tool { call_id, .. } => {
                let same = |seen: &PartBody| matches!(seen, PartBody::Tool { call_id: id, .. } if id == call_id);
                match tools.iter_mut().find(|seen| same(seen)) {
                    Some(seen) => *seen = body.clone(),
                    None => tools.push(body.clone()),
                }
            }
            _ => {}
        }
    }

    (text, tools)
}

/// How the turn in `events` finished: the reason, and the error when it failed.
fn finished(events: &[Event]) -> String {
    events
        .iter()
        .find_map(|event| match event {
            Event::MessageFinished { reason, error, .. } => Some(match error {
                Some(error) => format!("{reason:?}: \"{}\"", quoted(error)),
                None => format!("{reason:?}"),
            }),
            _ => None,
        })
        .unwrap_or_else(|| "no finish".to_owned())
}

/// Prints every step exchange a seat probe made, one row each, skipping the
/// title requests an engine makes on the side.
fn print_steps(exchanges: &[Exchange], extra: impl Fn(&Exchange) -> String) {
    for (index, exchange) in
        exchanges.iter().enumerate().filter(|(_, exchange)| !exchange.is_title())
    {
        eprintln!(
            "| {index} | {} | {} | {} | {:.2?} / {:.2?} |",
            exchange.verdict(),
            advertised(exchange.forwarded.get("tools")),
            extra(exchange),
            exchange.first_byte,
            exchange.total
        );
    }
}

/// Probe 3: `text.format` beside the default tool roster, on the seat,
/// wrapped exactly as `ganja run --json-schema` wraps it (`strict: true`).
///
/// **Needs** `GANJA_LIVE_TEST=1` and `ganja auth login chatgpt`.
/// `GANJA_MODEL` moves the model from `SUBSCRIPTION_DEFAULT`.
///
/// Two requests, so two questions come back apart:
/// the AC-31 schema as shipped — no `additionalProperties: false` — answers
/// "is a strict schema without it refused?", and the same schema with it set
/// answers "is `text.format` refused beside a roster at all?". Each is the
/// first step request of a `run --json-schema` turn as the wire builds it:
/// every builtin's definition on the roster, the seat's default fast tier,
/// and the format. **One request, not a turn**: a tool call that comes back is
/// printed as an output item and never run, so nothing the model asks for runs
/// on this machine — the engine's side of the scope (the format rides the
/// steps and nothing else) is pinned offline in `responses_options.rs`.
///
/// Recorded 2026-09-17 on `gpt-5.5`: the schema as shipped was refused (400,
/// `'additionalProperties' is required to be supplied and to be false.`) and
/// the closed one accepted, so `text.format` beside a roster is fine.
#[tokio::test]
#[ignore = "talks to the ChatGPT codex backend; needs GANJA_LIVE_TEST=1 and `ganja auth login chatgpt`"]
async fn probe_text_format_beside_the_default_roster_on_the_seat() {
    if !seated() {
        return;
    }
    let model = model_or(responses::SUBSCRIPTION_DEFAULT);
    let relay = Forwarder::start(responses::DEFAULT_BASE_URL, None).await;
    let shipped: Value = serde_json::from_str(AC31_SCHEMA).expect("the fixture is JSON");
    let mut closed = shipped.clone();
    closed["additionalProperties"] = json!(false);

    eprintln!("## text.format beside the default roster on {model}, seat\n");
    for (label, schema) in
        [("AC-31 schema as shipped", shipped), ("with additionalProperties: false", closed)]
    {
        relay.forget();
        let request = ChatRequest {
            model: model.clone(),
            system: Some("Answer the question in the required format.".to_owned()),
            messages: vec![Message::user("Is the sky blue on a clear day?")],
            tools: Registry::with_builtins().definitions(),
            responses: RequestOptions {
                service_tier: responses_ladder::fast_tier(responses::CHATGPT_ID, &model)
                    .map(str::to_owned),
                text_format: Some(json!({
                    "type": "json_schema",
                    "name": "ganja_run",
                    "schema": schema,
                    "strict": true,
                })),
                ..RequestOptions::default()
            },
            ..ChatRequest::default()
        };
        let wire = through_wire(&relay.seat(), request).await;
        let text: String = wire
            .events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect();

        eprintln!("### {label}\n");
        eprintln!("wire: {}; answer: \"{}\"\n", wire.verdict(), quoted(&text));
        eprintln!(
            "| exchange | vendor | advertised | echo text.format / output items | first byte / total |"
        );
        eprintln!("|---|---|---|---|---|");
        print_steps(&relay.exchanges(), |exchange| {
            let response = exchange.response();
            format!(
                "{} / {}",
                cell(response.as_ref().and_then(|response| response.pointer("/text/format"))),
                output_types(response.as_ref())
            )
        });
        eprintln!();
        tokio::time::sleep(BETWEEN_CALLS).await;
    }
}

/// Probe 4: `custom_tools = ["bash"]` on the seat — a custom
/// advertisement and its function twin under one name.
///
/// **Needs** `GANJA_LIVE_TEST=1` and `ganja auth login chatgpt`.
/// `GANJA_MODEL` moves the model from `SUBSCRIPTION_DEFAULT`.
///
/// The model is asked to run one exact command. A permission dialog is
/// answered through the engine's own reply command: **allowed once** when it
/// is `bash` asking for exactly that command and **rejected** otherwise, so
/// the most a dialog lets through on this machine is one `echo`. The read-only
/// tools a default ruleset runs unasked are on the roster as in any session;
/// the prompt gives the model no reason to reach for one.
///
/// Prints whether the request was accepted, what each step advertised, which
/// output item the backend produced (`custom_tool_call` or `function_call`),
/// and every tool part the turn stored with its `custom` flag — the flag is
/// the transcript's own record of which advertisement the call came back
/// under. Recorded 2026-09-17 on `gpt-5.5`: accepted, and the model called the
/// custom advertisement (the stored part carries `custom: true`).
#[tokio::test]
#[ignore = "talks to the ChatGPT codex backend; needs GANJA_LIVE_TEST=1 and `ganja auth login chatgpt`"]
async fn chatgpt_calls_bash_as_a_custom_tool() {
    /// The one command a dialog is allowed to run.
    const COMMAND: &str = "echo ganja-custom-probe";

    if !seated() {
        return;
    }
    let model = model_or(responses::SUBSCRIPTION_DEFAULT);
    let relay = Forwarder::start(responses::DEFAULT_BASE_URL, None).await;
    let table: ResponsesOptions =
        toml::from_str("custom_tools = [\"bash\"]\n").expect("the table decodes");
    let engine = Engine::new(
        Arc::new(relay.seat()),
        &model,
        Arc::new(Registry::with_builtins()),
        Permissions::default(),
    )
    .with_provider_options(BTreeMap::from([(responses::CHATGPT_ID.to_owned(), table)]));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine
        .send(prompt(&format!(
            "Use the bash tool to run exactly this command and nothing else: {COMMAND}\nThen reply with its output."
        )))
        .await
        .expect("an idle engine accepts a prompt");

    let mut seen = Vec::new();
    loop {
        let event = events.next().await.expect("the turn finishes before the stream ends");
        if let Event::PermissionRequested { id, tool, args, .. } = &event {
            let exact = tool == "bash" && args["command"].as_str().map(str::trim) == Some(COMMAND);
            let reply = if exact { PermissionReply::Once } else { PermissionReply::Reject };
            eprintln!("dialog: {tool} {} -> {reply:?}", quoted(&args.to_string()));
            engine
                .send(Command::ReplyPermission { id: id.clone(), reply })
                .await
                .expect("a reply is never refused");
        }
        let done = matches!(event, Event::MessageFinished { .. });
        seen.push(event);
        if done {
            break;
        }
    }
    let _ = engine.settle(SETTLE).await;
    let (text, tools) = parts(&seen);

    eprintln!("## custom_tools = [\"bash\"] on {model}, seat\n");
    eprintln!("turn: {}; answer: \"{}\"\n", finished(&seen), quoted(&text));
    eprintln!("| exchange | vendor | advertised | output items | first byte / total |");
    eprintln!("|---|---|---|---|---|");
    print_steps(&relay.exchanges(), |exchange| output_types(exchange.response().as_ref()));
    eprintln!("\n| stored tool part | custom | input |");
    eprintln!("|---|---|---|");
    for tool in &tools {
        if let PartBody::Tool { tool: name, custom, state, .. } = tool {
            let input =
                serde_json::to_value(state).ok().and_then(|state| state.get("input").cloned());
            eprintln!("| {name} | {custom} | {} |", cell(input.as_ref()));
        }
    }
}

/// Probe 5: whether `context_management` does anything on a transcript
/// long enough for a compaction threshold to be crossed.
///
/// **Needs** `GANJA_LIVE_TEST=1` and `ganja auth login chatgpt`.
/// `GANJA_MODEL` moves the model from `SUBSCRIPTION_DEFAULT`;
/// `GANJA_PROBE_TRANSCRIPT_CHARS` sizes the synthetic transcript (default
/// 200000 characters, roughly 50k tokens — raise it toward the model's window
/// to test "near its window" for real, at the seat's usage cost), and
/// `GANJA_PROBE_COMPACT_THRESHOLD` sets `compact_threshold` (default 20000,
/// below the default transcript).
///
/// The same request twice, without and then with the key. Prints, per
/// request, the vendor's answer, the billed input tokens, the
/// `context_management` echo and the output item
/// types, where a server-side compaction would show up as an item of its own.
/// Recorded 2026-09-17 on `gpt-5.5` at the default sizes: 56,437 input tokens
/// without the key and 56,629 with it, no compaction item and no echo, so the
/// key left the seat's accepted list.
#[tokio::test]
#[ignore = "talks to the ChatGPT codex backend; needs GANJA_LIVE_TEST=1 and `ganja auth login chatgpt`"]
async fn context_management_is_measured_on_a_long_transcript() {
    if !seated() {
        return;
    }
    let model = model_or(responses::SUBSCRIPTION_DEFAULT);
    let chars = knob("GANJA_PROBE_TRANSCRIPT_CHARS", 200_000);
    let threshold = knob("GANJA_PROBE_COMPACT_THRESHOLD", 20_000);
    let relay = Forwarder::start(responses::DEFAULT_BASE_URL, None).await;
    let provider = relay.seat();

    // Filler that no model can answer from: numbered, dull, and closed by the
    // same one-word ask every probe here ends in.
    let mut messages = Vec::new();
    let mut written = 0;
    let mut round = 0;
    while written < chars {
        round += 1;
        let filler: String = (0..40)
            .map(|line| {
                format!("Record {round}.{line}: the quick brown fox jumps over the lazy dog.\n")
            })
            .collect();
        written += filler.len();
        messages.push(Message::user(filler));
        let mut assistant = Message::assistant(&model);
        assistant.parts.push(Part::text(format!("Noted record {round}.")));
        messages.push(assistant);
    }
    messages.push(Message::user(PROMPT));

    eprintln!(
        "## context_management on {model}, seat ({written} transcript characters, threshold {threshold})\n"
    );
    eprintln!(
        "| request | vendor | input tokens | echo context_management | output items | total |"
    );
    eprintln!("|---|---|---|---|---|---|");
    for (label, options) in [
        ("without the key", RequestOptions::default()),
        (
            "with the key",
            body(
                json!({"context_management": [{"type": "compaction", "compact_threshold": threshold}]}),
            ),
        ),
    ] {
        relay.forget();
        let request =
            ChatRequest { messages: messages.clone(), ..one_word(&model, options, Vec::new()) };
        let wire = through_wire(&provider, request).await;
        let exchange = relay.exchanges().pop();
        let response = exchange.as_ref().and_then(Exchange::response);
        eprintln!(
            "| {label} | {} | {} | {} | {} | {:.2?} |",
            exchange.as_ref().map_or_else(|| wire.verdict(), Exchange::verdict),
            wire.input_tokens().map_or_else(|| "none".to_owned(), |tokens| tokens.to_string()),
            cell(response.as_ref().and_then(|response| response.get("context_management"))),
            output_types(response.as_ref()),
            wire.total
        );
        tokio::time::sleep(BETWEEN_CALLS).await;
    }
}

/// Probe 6: what the seat says to a tool-less request carrying a
/// `tool_choice` — the refusal `gated()` is built to prevent, measured rather
/// than assumed.
///
/// **Needs** `GANJA_LIVE_TEST=1` and `ganja auth login chatgpt`.
/// `GANJA_MODEL` moves the model from `SUBSCRIPTION_DEFAULT`.
///
/// The request is a compaction's shape — no roster, the resolved options'
/// `summary_view` — with `tool_choice = "required"` configured. The shipped
/// wire drops that key (the first column proves it did), so the relay puts it
/// back **ungated** before forwarding. `"auto"` rides the same path as a
/// control, so a refusal of `required` alone is told apart from a refusal of
/// any `tool_choice` beside no tools. Recorded 2026-09-17 on `gpt-5.5`:
/// `required` was refused (400, `Tool choice 'required' must be specified with
/// 'tools' parameter.`) and `auto` accepted.
#[tokio::test]
#[ignore = "talks to the ChatGPT codex backend; needs GANJA_LIVE_TEST=1 and `ganja auth login chatgpt`"]
async fn probe_tool_choice_on_a_tool_less_request_on_the_seat() {
    if !seated() {
        return;
    }
    let model = model_or(responses::SUBSCRIPTION_DEFAULT);

    eprintln!("## tool_choice on a tool-less request, ungated, on {model}, seat\n");
    eprintln!("| tool_choice | sent by the wire | forwarded | tools forwarded | vendor | total |");
    eprintln!("|---|---|---|---|---|---|");
    for choice in ["required", "auto"] {
        let edit: Edit = Arc::new(move |body: &mut Map<String, Value>| {
            body.insert("tool_choice".to_owned(), json!(choice));
        });
        let relay = Forwarder::start(responses::DEFAULT_BASE_URL, Some(edit)).await;
        let resolved = body(json!({"tool_choice": choice}));
        let request = ChatRequest {
            model: model.clone(),
            system: Some("Summarize the conversation so far in one sentence.".to_owned()),
            messages: vec![Message::user(PROMPT)],
            responses: resolved.summary_view(),
            ..ChatRequest::default()
        };
        let wire = through_wire(&relay.seat(), request).await;
        match relay.exchanges().pop() {
            Some(exchange) => eprintln!(
                "| `{choice}` | {} | {} | {} | {} | {:.2?} |",
                cell(exchange.from_wire.get("tool_choice")),
                cell(exchange.forwarded.get("tool_choice")),
                advertised(exchange.forwarded.get("tools")),
                exchange.verdict(),
                exchange.total
            ),
            None => {
                eprintln!("| `{choice}` | never reached the relay | | | {} | |", wire.verdict())
            }
        }
        tokio::time::sleep(BETWEEN_CALLS).await;
    }
}

/// Not a network test: the platform probe's rows cover exactly the names this
/// build would send the platform, so a key, tier, `include` value or hosted type
/// added to a list without a probe reddens here rather than going unmeasured.
#[test]
fn the_platform_probe_covers_every_key_tier_and_hosted_tool_this_build_accepts() {
    let probes = platform_probes();
    // Both ways, so a key that leaves the list stops being paid for here too.
    let keys: BTreeSet<&str> = probes.iter().map(|probe| probe.key).collect();
    let sent: BTreeSet<&str> = PLATFORM_ACCEPTED
        .iter()
        .copied()
        .chain(["default:stream_options.include_obfuscation", "default:tool_choice"])
        .collect();
    assert_eq!(keys, sent, "the probes send exactly the keys and defaults this build sends");

    let tiers: BTreeSet<&str> =
        probes.iter().filter_map(|probe| probe.options.service_tier.as_deref()).collect();
    assert_eq!(tiers, PLATFORM_TIERS.iter().copied().collect(), "every tier is sent once");

    let hosted: BTreeSet<String> = probes
        .iter()
        .flat_map(|probe| &probe.options.server_tools)
        .filter_map(|entry| entry.get("type").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let listed: BTreeSet<String> =
        PLATFORM_SERVER_TOOLS.iter().map(|kind| (*kind).to_owned()).collect();
    assert_eq!(hosted, listed, "every hosted type is sent once");

    let included: BTreeSet<&str> =
        probes.iter().flat_map(|probe| &probe.options.include).map(String::as_str).collect();
    for entry in PLATFORM_INCLUDE {
        assert!(included.contains(entry), "no platform probe asks for `{entry}`");
    }
}
