//! An OpenAI session becoming a turn, either of the vendor's two ids, against a
//! real socket.
//!
//! **The vendor picks the wire, and the id picks the backend.** Both of this
//! vendor's ids speak the Responses API — upstream's plugin routes every model
//! of that vendor through it without looking at the credential at all
//! (`plugin/provider/openai.ts:185`) — and since **D555** which *backend* a
//! request reaches, and what it carries beside the bearer, is settled by the id
//! the session was selected under rather than by whichever credential the
//! machine happens to hold. Two things depended on the wire being one: a stored
//! ChatGPT login had no consumer at all until the Responses provider landed,
//! and an API key could not run tools on the newest models, because chat
//! completions refused them live and named `/v1/responses` in the refusal.
//!
//! Told in phases, because ten different things have to be true at once and a
//! failure should still say which sentence broke:
//!
//! 1. **A whole subscription turn.** A stored ChatGPT credential drives a
//!    streamed reply through the ordinary engine — the request asserted whole,
//!    the events asserted as the engine published them, `store: false`
//!    included: the backend refuses a body without it.
//! 2. **The credential is read per request.** The stored credential is rotated
//!    between two turns and the *second* token is what the second request
//!    carries. A provider that captured its token at construction passes every
//!    other assertion here and fails this one.
//! 3. **A key rides the same wire, at the platform.** The same encoder, the
//!    same grammar coming back, a bearer — and **none** of the four headers the
//!    subscription request carries, because each of those is about borrowing
//!    somebody else's client registration.
//! 4. **The chat-completions encoder is unchanged.** It is no longer what an
//!    `openai` key gets, but it is still what grok and Copilot ride, so its
//!    body stays compared byte for byte against what this build has always
//!    sent.
//! 5. **The dispatch.** `openai` reaches the platform on its key and `chatgpt`
//!    the seat on its login — with both credentials present throughout, so
//!    neither answer can be a coincidence of what was missing.
//! 6. **The model each wire defaults to.** A seat's backend serves a narrower
//!    set than the platform, so a subscription session that named no model
//!    takes the seat's default rather than the catalog's — and a model somebody
//!    *did* name is answered or refused, never substituted.
//! 7. **No key under `openai`** is the startup failure it has always been, with
//!    nothing on the wire — and its sentence now names the seat's id as the
//!    other way out.
//! 8. **Nothing leaks.** No token reaches a rendering, an error or the store's
//!    own `Debug`.
//! 9. **An unsupported model costs nothing** — and the seat's list does not
//!    reach the platform, which phase 3 already took a turn on.
//! 10. **Thinking survives the request that produced it.** The one phase that
//!     needs two requests: `store: false` means the backend keeps nothing, so
//!     a turn's second step carries the first step's reasoning only because
//!     this build asked for it (`include`), kept it as a part, and handed it
//!     back in the item shape the pin asserts.
//!
//! Everything serves real bytes over loopback rather than mocking the client,
//! the way every other provider suite here works: what is asserted on is the
//! request that was actually built.
//!
//! One test, one binary, on purpose: it mutates `XDG_DATA_HOME`,
//! `OPENAI_API_KEY`, `OPENAI_BASE_URL`, `GANJA_PROVIDER` and `GANJA_MODEL`, and
//! a plain `cargo test` runs the tests inside a binary on parallel threads.

use std::env;
use std::sync::Arc;

use futures::StreamExt as _;
use ganja_core::auth::{self, AuthError, OauthCredential, RefreshOauth};
use ganja_core::config::Config;
use ganja_core::permission::Permissions;
use ganja_core::protocol::{Command, Event, PartBody, PartId, Role};
use ganja_core::provider::{
    ChatRequest, Provider as _, ProviderError, ProviderEvent, ResponsesProvider, openai, responses,
    select,
};
use ganja_core::tool::Registry;
use ganja_core::{Engine, catalog};
use ganja_testkit::responses_server::{Endpoint, responses_transcript, serve};
use ganja_testkit::{RecorderTool, drain, is_title_body};
use secrecy::SecretString;
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Where a Responses turn goes, under the endpoint's base URL.
const RESPONSES: &str = "/backend-api/codex/responses";

/// Where a chat-completions turn goes, under the same base URL.
const COMPLETIONS: &str = "/backend-api/codex/chat/completions";

/// The access token the first credential carries.
const FIRST_ACCESS: &str = "at-first-canary-AAAA";

/// The account that credential names.
const FIRST_ACCOUNT: &str = "acct_first_1111";

/// What a login stores beside the access token. Never sent by a turn.
const REFRESH: &str = "rt-stored-canary-BBBB";

/// The access token the credential is rotated to mid-test.
const SECOND_ACCESS: &str = "at-second-canary-CCCC";

/// The account that one names, so "the second token" and "the second account"
/// are two assertions rather than one.
const SECOND_ACCOUNT: &str = "acct_second_2222";

/// The API key the chat-completions half of the test authenticates with.
const KEY: &str = "sk-key-canary-DDDD";

/// The model the subscription phases ask for.
///
/// A real catalog row, so a turn that reaches the session layer has a context
/// window and a price to report — and one the ChatGPT backend serves (measured
/// 2026-09-16).
const SUBSCRIPTION_MODEL: &str = "gpt-5.5";

/// The model the key phases ask for.
///
/// Deliberately a *different* row, and deliberately the one the subscription
/// backend refuses (`codex.ts:289`). That is what makes it load-bearing here
/// rather than arbitrary: phase 3 takes a whole turn on it through a key, phase
/// 9 is refused it through a seat, and the pair is the proof that the seat's
/// allow-list gates one backend and not the other. It is also the model whose
/// live `400` — "To use function tools, use /v1/responses" — is why a key rides
/// this wire at all.
const KEY_MODEL: &str = "gpt-5.6";

/// Headers a subscription request carries and a key request must not.
///
/// Each exists because the codex backend is talked to as the Codex CLI, whose
/// client registration the stored access token was minted against; a key is the
/// caller's own credential against the platform, and upstream sends such a
/// request through the unwrapped `fetch` (`codex.ts:356`) with none of them.
const SUBSCRIPTION_HEADERS: [&str; 4] =
    ["chatgpt-account-id", "originator", "openai-beta", "user-agent"];

/// A whole chat-completions turn, for the phase that proves the key path.
fn completions_transcript() -> String {
    [
        r#"data: {"choices":[{"index":0,"delta":{"content":"Hello, world!"},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":9}}"#,
        "data: [DONE]",
    ]
    .join("\n\n")
        + "\n\n"
}

/// Puts a ChatGPT credential in the store, replacing whatever was there.
fn store(access: &str, account_id: &str) {
    let mut credential = OauthCredential::new(
        SecretString::from(REFRESH.to_owned()),
        SecretString::from(access.to_owned()),
        // Hours left, so nothing here ever asks a token endpoint: what this
        // suite is about is which token travels, not when one is renewed.
        auth::now_ms() + 86_400_000,
    );
    credential.account_id = Some(account_id.to_owned());

    auth::set_oauth(auth::openai::PROVIDER_ID, &credential).expect("the credential stores");
}

/// A renewal that must never run: every credential this suite stores is live,
/// so a call here is the provider renewing something that did not need it.
struct NeverRenews;

#[async_trait::async_trait]
impl RefreshOauth for NeverRenews {
    async fn refresh(
        &self,
        provider_id: &str,
        _credential: &OauthCredential,
    ) -> Result<OauthCredential, AuthError> {
        panic!("{provider_id} was renewed although its credential had hours left");
    }
}

/// One turn's worth of request, on the model the phase is about.
fn ask(model: &str) -> ChatRequest {
    ChatRequest {
        turn_start: 0,
        responses: Default::default(),
        effort_options: Default::default(),
        model: model.to_owned(),
        system: Some("be brief".to_owned()),
        messages: vec![ganja_core::protocol::Message::user("say hello")],
        tools: Vec::new(),
    }
}

/// The provider under test, pointed at `endpoint`.
fn responses(endpoint: &Endpoint) -> ResponsesProvider {
    ResponsesProvider::at(&endpoint.base_url, Arc::new(NeverRenews))
        .expect("loopback may carry a token")
}

/// Takes a whole turn and hands back what streamed.
///
/// The body is drained rather than dropped even where the caller only asserts
/// on the request: an unconsumed stream is a request that may never have been
/// sent.
async fn turn(provider: &dyn ganja_core::provider::Provider, model: &str) -> Vec<ProviderEvent> {
    let streamed: Vec<_> = provider
        .stream(ask(model), CancellationToken::new())
        .await
        .expect("the endpoint answered")
        .collect()
        .await;

    assert!(!streamed.is_empty(), "an answered turn streams something");
    streamed
}

/// The reply text a set of provider events spells.
fn replied(streamed: &[ProviderEvent]) -> String {
    streamed
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::TextDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

/// The bill those events reported, if any.
fn spent(streamed: &[ProviderEvent]) -> Option<ganja_core::protocol::Usage> {
    streamed.iter().find_map(|event| match event {
        ProviderEvent::Usage(usage) => Some(*usage),
        _ => None,
    })
}

/// A prompt command, as a frontend sends one.
fn prompt(text: &str) -> Command {
    Command::SendPrompt {
        text: text.to_owned(),
        mentions: Vec::new(),
        skills: Vec::new(),
        session_mentions: Vec::new(),
        peers: Vec::new(),
    }
}

#[tokio::test]
async fn either_openai_id_drives_a_responses_turn_against_the_backend_it_names() {
    let home = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        env::set_var("XDG_DATA_HOME", home.path());
        env::remove_var("OPENAI_API_KEY");
    }

    let endpoint = serve().await;

    // ---- 1. A whole turn, through the engine a frontend drives. -----------
    store(FIRST_ACCESS, FIRST_ACCOUNT);
    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "found it");
    let engine = Engine::new(
        Arc::new(responses(&endpoint)),
        SUBSCRIPTION_MODEL,
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    )
    .with_system_parts(Some("be brief".to_owned()), None);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt("say hello")).await.expect("an idle engine accepts");
    let seen = drain(&mut events).await;

    let sent = endpoint.only();
    assert_eq!(
        sent.path(),
        RESPONSES,
        "a subscription turn is a Responses request, not a chat-completions one"
    );
    assert_eq!(
        sent.header("authorization").as_deref(),
        Some(format!("Bearer {FIRST_ACCESS}").as_str()),
    );
    assert_eq!(
        sent.header("chatgpt-account-id").as_deref(),
        Some(FIRST_ACCOUNT),
        "the backend cannot tell which of a person's accounts to serve without it"
    );
    assert_eq!(sent.header("originator").as_deref(), Some("ganja-code"));
    assert_eq!(sent.header("openai-beta").as_deref(), Some("responses=experimental"));
    assert_eq!(
        sent.header("user-agent").as_deref(),
        Some(auth::device::GANJA_USER_AGENT),
        "the codex backend is told what this build is; the borrowed identity \
         is a per-host choice now, not every request's answer"
    );

    let body = sent.json();
    assert_eq!(body["model"], json!(SUBSCRIPTION_MODEL));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(
        body["store"],
        json!(false),
        "the backend answers a body without this `400 {{\"detail\":\"Store must \
         be set to false\"}}`, so every subscription turn depends on it: {body}"
    );
    assert_eq!(
        body["include"],
        json!(["reasoning.encrypted_content"]),
        "with the backend keeping nothing, this is the only way the next \
         request carries this one's thinking; phase 10 is that request: {body}"
    );
    assert_eq!(
        body["instructions"],
        json!("be brief"),
        "the system prompt is the Responses API's own field, not an input item"
    );
    assert_eq!(
        body["input"],
        json!([{"role": "user", "content": [{"type": "input_text", "text": "say hello"}]}]),
        "got {body}"
    );
    assert_eq!(
        body["tools"][0]["name"],
        json!("lookup"),
        "a real turn always offers tools, and the flat shape is what this API \
         reads: {body}"
    );
    assert_eq!(body["tools"][0]["type"], json!("function"));
    assert!(
        body["tools"][0]["function"].is_null(),
        "chat completions' nesting would leave the model offered nothing: {body}"
    );
    assert!(
        calls.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).is_empty(),
        "the transcript calls nothing, so nothing should have run"
    );

    // The turn as the frontend saw it, told apart by which part each fragment
    // grew: the reply is the text part's, and the summarized thought this API
    // streams beside it now has a part of its own rather than being dropped.
    let thoughts: Vec<&PartId> = seen
        .iter()
        .filter_map(|event| match event {
            Event::PartStarted { part, .. }
                if matches!(part.body, PartBody::ReasoningText { .. }) =>
            {
                Some(&part.id)
            }
            _ => None,
        })
        .collect();
    let streamed = |thinking: bool| -> String {
        seen.iter()
            .filter_map(|event| match event {
                Event::PartDelta { part_id, delta, .. }
                    if thoughts.contains(&part_id) == thinking =>
                {
                    Some(delta.as_str())
                }
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        streamed(false),
        "Hello, world!",
        "the reply, and only the reply — the thought is beside it, not mixed \
         into it: got {seen:?}"
    );
    assert_eq!(
        streamed(true),
        "Short is right.",
        "and the thought reaches the frontend rather than dying in the loop: \
         got {seen:?}"
    );

    let billed = seen.iter().find_map(|event| match event {
        Event::PartStarted { part, .. } => match &part.body {
            PartBody::StepFinish { usage } => Some(*usage),
            _ => None,
        },
        _ => None,
    });
    let billed = billed.expect("a finished step carries what it cost");
    assert_eq!(
        (billed.input_tokens, billed.cache_read_tokens),
        (26, 16),
        "42 prompt tokens of which the cache served 16 is 26 fresh, or the \
         cached half is billed twice: {billed:?}"
    );
    assert_eq!((billed.output_tokens, billed.reasoning_tokens), (9, 4));
    assert!(
        seen.iter().any(|event| matches!(event, Event::MessageStarted { message, .. }
                if message.role == Role::Assistant)),
        "the turn should have reached the event stream as a message: {seen:?}"
    );

    // ---- 2. The credential is read per request, not captured. -------------
    endpoint.forget();
    store(SECOND_ACCESS, SECOND_ACCOUNT);
    engine.send(prompt("again")).await.expect("an idle engine accepts");
    drain(&mut events).await;

    let sent = endpoint.only();
    assert_eq!(
        sent.header("authorization").as_deref(),
        Some(format!("Bearer {SECOND_ACCESS}").as_str()),
        "the same provider carried the token it was built with, so a login or \
         a renewal that happened mid-session would never be picked up"
    );
    assert_eq!(
        sent.header("chatgpt-account-id").as_deref(),
        Some(SECOND_ACCOUNT),
        "the account travels from the same read the token does"
    );

    // ---- 3. A key rides the same wire, at the platform. --------------------
    // The model here is the one the live pass met `400 "Function tools with
    // reasoning_effort are not supported for gpt-5.6 in /v1/chat/completions.
    // To use function tools, use /v1/responses…"` on. This is that turn taken
    // on the endpoint the refusal named.
    endpoint.forget();
    // SAFETY: as above.
    unsafe {
        env::set_var("OPENAI_API_KEY", KEY);
        env::set_var(openai::BASE_URL_ENV, &endpoint.base_url);
    }

    let keyed = ResponsesProvider::from_env().expect("an exported key builds a provider");
    let streamed = turn(&keyed, KEY_MODEL).await;

    let sent = endpoint.only();
    assert_eq!(
        sent.path(),
        RESPONSES,
        "a key session is a Responses request too — the vendor picks the wire, \
         not the credential (`plugin/provider/openai.ts:185`)"
    );
    assert_eq!(
        sent.header("authorization").as_deref(),
        Some(format!("Bearer {KEY}").as_str()),
        "the exported key, and nothing that had to be exchanged for it"
    );
    for absent in SUBSCRIPTION_HEADERS {
        assert_eq!(
            sent.header(absent),
            None,
            "`{absent}` is about borrowing somebody else's client registration \
             and travelled with an API key to a platform that never asked"
        );
    }

    let body = sent.json();
    assert_eq!(body["model"], json!(KEY_MODEL));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(
        body["store"],
        json!(false),
        "one encoder for both backends, so this is not a subscription special \
         case — upstream holds it as a route-level default: {body}"
    );
    assert_eq!(
        body["include"],
        json!(["reasoning.encrypted_content"]),
        "and its companion travels on both backends too, because upstream \
         attaches it to the model rather than to the credential: {body}"
    );
    assert_eq!(body["instructions"], json!("be brief"));
    assert_eq!(
        body["input"],
        json!([{"role": "user", "content": [{"type": "input_text", "text": "say hello"}]}]),
        "got {body}"
    );

    // And the grammar coming back is read the same way, which is the half a
    // request assertion cannot see.
    assert_eq!(replied(&streamed), "Hello, world!");
    let keyed_bill = spent(&streamed).expect("the terminal frame carries the bill");
    assert_eq!(
        (keyed_bill.input_tokens, keyed_bill.cache_read_tokens),
        (26, 16),
        "42 prompt tokens of which the cache served 16 is 26 fresh: {keyed_bill:?}"
    );

    // ---- 4. The chat-completions encoder is unchanged. ---------------------
    // No longer what an `openai` key gets — but still what grok and Copilot
    // ride, so the bytes stay pinned here rather than losing their only
    // spelled-out assertion to this move.
    endpoint.forget();
    endpoint.answers_turns_with(completions_transcript());

    let completions =
        openai::OpenAiProvider::from_env().expect("an exported key builds a provider");
    turn(&completions, KEY_MODEL).await;

    let sent = endpoint.only();
    assert_eq!(sent.path(), COMPLETIONS);
    assert_eq!(sent.header("authorization").as_deref(), Some(format!("Bearer {KEY}").as_str()));
    assert_eq!(
        sent.body,
        // What this build has always sent, spelled out rather than derived, so
        // that a change to the shared encoder has to be admitted here.
        concat!(
            r#"{"model":"gpt-5.6","stream":true,"stream_options":{"include_usage":true},"#,
            r#""messages":[{"role":"system","content":"be brief"},"#,
            r#"{"role":"user","content":"say hello"}]}"#,
        ),
        "the chat-completions request is not this lane's to change"
    );

    // ---- 5. The dispatch. --------------------------------------------------
    // **The id picks the backend, and no credential votes** (**D555**). Both
    // credentials are present for the whole of this phase, which is what makes
    // it an assertion rather than a coincidence: whichever id is named, the
    // other one's credential is sitting right there and does not travel. Both
    // wires answer on the same path, so what tells them apart is the bearer and
    // the headers rather than the URL.
    endpoint.forget();
    endpoint.answers_turns_with(responses_transcript());
    // SAFETY: as above. Named rather than defaulted, because an unset
    // `GANJA_PROVIDER` is the fake provider and would prove nothing.
    unsafe {
        env::set_var("GANJA_PROVIDER", openai::ID);
    }
    let chosen = select(&Config::default()).await.expect("a key is a session");
    assert_eq!(chosen.provider.id(), openai::ID, "the platform reports itself as the platform");
    turn(chosen.provider.as_ref(), KEY_MODEL).await;

    let sent = endpoint.only();
    assert_eq!(sent.path(), RESPONSES);
    assert_eq!(
        sent.header("authorization").as_deref(),
        Some(format!("Bearer {KEY}").as_str()),
        "a stored ChatGPT login must not take a session away from the id that \
         means the platform key"
    );
    for absent in SUBSCRIPTION_HEADERS {
        assert_eq!(sent.header(absent), None, "the key reached the platform");
    }

    // The seat's id, with the key still exported: the codex backend, the stored
    // login's bearer, and everything the seat wants. Before D555 the key won
    // here and this was a platform turn.
    endpoint.forget();
    // SAFETY: as above.
    unsafe {
        env::set_var("GANJA_PROVIDER", responses::CHATGPT_ID);
    }
    let chosen = select(&Config::default()).await.expect("a stored login is a session");
    assert_eq!(chosen.provider.id(), responses::CHATGPT_ID, "and the seat as the seat");
    turn(chosen.provider.as_ref(), SUBSCRIPTION_MODEL).await;
    let sent = endpoint.only();
    assert_eq!(sent.path(), RESPONSES, "the credential with no consumer now has one");
    assert_eq!(
        sent.header("authorization").as_deref(),
        Some(format!("Bearer {SECOND_ACCESS}").as_str()),
        "an exported key must not take a session away from the id that means \
         the subscription"
    );
    assert_eq!(
        sent.header("chatgpt-account-id").as_deref(),
        Some(SECOND_ACCOUNT),
        "the seat's headers are still there for the seat"
    );

    // SAFETY: as above. The key goes now, so that the defaults below are read
    // on a machine holding one credential each way round.
    unsafe {
        env::remove_var("OPENAI_API_KEY");
    }

    // ---- 6. The model each wire defaults to. -------------------------------
    // The catalog holds one default per vendor, and this vendor has two
    // backends with different offerings: the seat refuses `gpt-5.6` outright,
    // so a subscription session handed the vendor-wide default would be a seat
    // that cannot take a turn. The seat brings its own instead.
    endpoint.forget();
    // SAFETY: as above. `GANJA_MODEL` decides the model on its own tier, so it
    // has to be absent for a *default* to be what is observed at all.
    unsafe {
        env::remove_var("GANJA_MODEL");
    }
    let defaulted = select(&Config::default()).await.expect("a stored login is a session");
    assert_eq!(
        defaulted.model, SUBSCRIPTION_MODEL,
        "a ChatGPT seat that named no model takes the one its own backend \
         serves, not the catalog's per-vendor row"
    );
    // Honest about what this proves *today*: the two defaults currently name
    // the same model, so this compares equal whether or not the seat's default
    // is consulted at all. What holds the seam while that is true is the unit
    // test `a_backends_own_default_outranks_its_vendors_catalog_row`, which
    // feeds it a value no catalog carries. This assertion goes sharp the moment
    // they diverge, which is the commit that restores the newer row as the
    // catalog's — and it is the one that would catch a seat being handed it.
    assert!(
        catalog::default_model(openai::ID).is_some(),
        "the table still answers for this vendor, which is what the key wire \
         falls through to"
    );
    // **AC-0.7.** The other half of the alias, from the outside: the seat is
    // pinned to nothing here and still cataloged, so it keeps the sizing and
    // pricing that make auto-compaction work.
    assert_eq!(
        catalog::default_model(responses::CHATGPT_ID),
        None,
        "the row alias covers rows and stops short of defaults"
    );
    assert!(catalog::carries(responses::CHATGPT_ID), "and the rows themselves are reached");

    // And it is genuinely the seat's rather than a coincidence of the two
    // agreeing: a key session on the same vendor takes the catalog's.
    // SAFETY: as above.
    unsafe {
        env::set_var("OPENAI_API_KEY", KEY);
        env::set_var("GANJA_PROVIDER", openai::ID);
    }
    let defaulted = select(&Config::default()).await.expect("a key is a session");
    assert_eq!(
        defaulted.model,
        catalog::default_model(openai::ID).expect("openai has a pinned default"),
        "the platform serves whatever it sells, so the key wire's default is \
         the table's and no seat's list narrows it"
    );

    // A model somebody *named* is never substituted, on either wire: it is
    // answered, or refused with what the seat does serve. Silently swapping it
    // would answer a question nobody asked.
    // SAFETY: as above.
    unsafe {
        env::remove_var("OPENAI_API_KEY");
        env::set_var("GANJA_PROVIDER", responses::CHATGPT_ID);
        env::set_var("GANJA_MODEL", KEY_MODEL);
    }
    let named = select(&Config::default()).await.expect("a stored login is a session");
    assert_eq!(named.model, KEY_MODEL, "the seat's default must not overwrite an explicit choice");
    let Err(refused_model) =
        named.provider.stream(ask(&named.model), CancellationToken::new()).await
    else {
        panic!("the seat does not serve {KEY_MODEL}, so there is no turn to take");
    };
    assert!(
        refused_model.to_string().contains(KEY_MODEL),
        "the refusal names what was asked for: {refused_model}"
    );
    assert!(endpoint.seen().is_empty(), "and it costs no request to say so");
    // SAFETY: as above.
    unsafe {
        env::remove_var("GANJA_MODEL");
    }

    // ---- 7. Neither credential. --------------------------------------------
    // The startup failure it has always been, and since **D555** it is the
    // platform id that makes it: that arm reads a key and only a key, so the
    // sentence has to name the variable *and* the id somebody holding the other
    // credential should be selecting instead.
    endpoint.forget();
    assert!(
        auth::remove_credential(auth::openai::PROVIDER_ID).expect("the store is writable"),
        "there was a credential to remove"
    );
    // SAFETY: as above.
    unsafe {
        env::set_var("GANJA_PROVIDER", openai::ID);
    }
    let Err(refused) = select(&Config::default()).await else {
        panic!("a session with no credential at all is not a session");
    };
    let said = refused.to_string();
    assert!(
        said.contains(openai::API_KEY_ENV)
            && said.contains(&format!("ganja auth login {}", responses::CHATGPT_ID)),
        "the message has to name both ways out of this: {said}"
    );
    assert!(
        endpoint.seen().is_empty(),
        "a session that could not start must not have reached the wire"
    );

    // ---- 8. Nothing leaks. ------------------------------------------------
    // SAFETY: as above.
    unsafe {
        env::remove_var(openai::BASE_URL_ENV);
    }
    let provider = responses(&endpoint);
    // `expect_err` would need the success arm to render, and a boxed stream has
    // no `Debug`; the match is the same assertion said a way that compiles.
    let Err(refused_credential) =
        provider.stream(ask(SUBSCRIPTION_MODEL), CancellationToken::new()).await
    else {
        panic!("the credential was removed above, so there is no turn to take");
    };
    assert!(
        matches!(refused_credential, ProviderError::Auth(_)),
        "a store with no ChatGPT credential in it has refused a login, not a \
         socket: {refused_credential:?}"
    );

    let rendered =
        format!("{provider:?} {refused:?} {refused} {refused_credential:?} {refused_credential}");
    for secret in [FIRST_ACCESS, SECOND_ACCESS, REFRESH, KEY] {
        assert!(!rendered.contains(secret), "a credential reached a rendering: {rendered}");
    }

    // ---- 9. A model this seat cannot run is refused before a turn is spent. --
    // The same name phase 3 took a whole turn on as a key, which is the pair
    // that proves the seat's list gates one backend and not the other
    // (`codex.ts:281` returns the models unfiltered for a credential that is
    // not an OAuth one). The backend answers it `400 {"detail":"The 'gpt-5.6'
    // model is not supported when using Codex with a ChatGPT account."}` — a
    // round trip and somebody else's JSON to learn something `codex.ts:15` has
    // written down.
    let Err(unsupported) = provider.stream(ask(KEY_MODEL), CancellationToken::new()).await else {
        panic!("a model the backend refuses is not a turn to take");
    };
    let said = unsupported.to_string();

    assert!(
        said.contains(KEY_MODEL) && said.contains(SUBSCRIPTION_MODEL),
        "the refusal has to name both what was asked for and something that \
         would work: {said}"
    );
    assert!(
        endpoint.seen().is_empty(),
        "the whole point is that no request was spent finding this out"
    );
    // Ahead of the credential read, which is the ordering this asserts: the
    // store still has no ChatGPT credential in it, so a check that ran second
    // would have reported the missing login instead.
    assert!(
        !matches!(unsupported, ProviderError::Auth(_)),
        "the model was refused before the store was consulted: {unsupported:?}"
    );

    // ---- 10. Thinking survives the request that produced it. ---------------
    // The other half of `include`, and the only phase that needs two requests
    // to say anything: with `store: false` the backend keeps nothing, so a
    // turn's second step carries the first step's reasoning only if this build
    // kept it and handed it back. The shape is upstream's, asserted at
    // `packages/llm/test/tool-runtime.test.ts:596-605`.
    endpoint.forget();
    store(FIRST_ACCESS, FIRST_ACCOUNT);
    endpoint.answers_the_next_requests_with([sealed_then_calls(), a_closing_reply()]);

    let (tool, ran) = RecorderTool::new("lookup", "lookup ran", "found it");
    let engine = Engine::new(
        Arc::new(responses(&endpoint)),
        SUBSCRIPTION_MODEL,
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine.send(prompt("what is the weather")).await.expect("an idle engine accepts");
    let seen = drain(&mut events).await;

    assert_eq!(
        ran.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).len(),
        1,
        "the turn is two requests because the model called a tool between them"
    );
    let requests = endpoint.seen();
    let [first, second] = requests.as_slice() else {
        panic!("a tool call makes a turn two requests, got {}", requests.len());
    };

    let opening = first.json();
    assert!(
        opening["input"].as_array().is_some_and(|input| input.len() == 1),
        "the first request is the prompt and nothing else: {opening}"
    );

    let carried = second.json();
    assert_eq!(
        carried["include"],
        json!(["reasoning.encrypted_content"]),
        "the request that replays state asks for the next lot too: {carried}"
    );
    assert_eq!(
        carried["input"],
        json!([
            {"role": "user", "content": [{"type": "input_text", "text": "what is the weather"}]},
            // No `id`: under `store: false` there is no server-side item for
            // one to name, and no summary, because the readable half of a
            // thought has no part in this build.
            {"type": "reasoning", "summary": [], "encrypted_content": "sealed-state"},
            {
                "type": "function_call",
                "call_id": "call_1",
                "name": "lookup",
                "arguments": r#"{"city":"Paris"}"#,
            },
            {"type": "function_call_output", "call_id": "call_1", "output": "found it"},
        ]),
        "the model's own thinking has to come back before the call it \
         produced, or the second step reasons from evidence with no reasoning: \
         {carried}"
    );

    // The transcript half: the state is a part of the conversation, which is
    // what makes it survive a process rather than a loop iteration.
    let sealed: Vec<&PartBody> = seen
        .iter()
        .filter_map(|event| match event {
            Event::PartStarted { part, .. } => {
                matches!(part.body, PartBody::Reasoning { .. }).then_some(&part.body)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        sealed,
        vec![&PartBody::Reasoning {
            // The **seat's** id since **D555**, and the replay guard reads this
            // field: a sealed thought is handed back only to the backend that
            // minted it, and the platform is a different service, not a
            // different mood of the same one. The cost is stated where the
            // decision is: a transcript recorded before the split carries
            // `openai` and its thinking will not replay on the seat — the
            // ordinary degradation for state another wire sealed, logged at
            // debug and never fatal.
            provider: responses::CHATGPT_ID.to_owned(),
            item: "rs_1".to_owned(),
            encrypted: Some("sealed-state".to_owned()),
        }],
        "a frontend applying every event has to hold exactly what the next \
         request will carry, and this is now part of that: {seen:?}"
    );

    // ---- 11. Configured options reach the step, the summary and not the title.
    // **D563, AC-21.** The table installed the way a frontend installs it, and
    // the bytes read off the socket: the step carries everything, the title
    // carries none of the session's options, and the compaction carries the
    // tier and the body but not a roster key its empty `tools` could not honor.
    endpoint.forget();
    endpoint.answers_turns_with(a_closing_reply());
    let options: ganja_core::config::ResponsesOptions = toml::from_str(
        "service_tier = \"default\"\ntext = { verbosity = \"low\" }\ntool_choice = \"required\"\n",
    )
    .expect("the table decodes");
    let store_dir = tempfile::tempdir().expect("a temporary directory");
    let (tool, _) = RecorderTool::new("lookup", "lookup ran", "found it");
    let engine = Engine::persistent(
        Arc::new(responses(&endpoint)),
        SUBSCRIPTION_MODEL,
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
        ganja_core::Storage::open(store_dir.path().join("storage")),
    )
    .with_provider_options(std::collections::BTreeMap::from([(
        responses::CHATGPT_ID.to_owned(),
        options,
    )]));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine.send(prompt("what is the weather")).await.expect("an idle engine accepts");
    drain(&mut events).await;
    ganja_testkit::eventually(
        std::time::Duration::from_secs(5),
        "the first turn's title request",
        async || endpoint.seen().iter().any(|request| is_title_body(&request.json())).then_some(()),
    )
    .await;
    assert!(engine.settle(std::time::Duration::from_secs(10)).await);
    engine.send(Command::Compact).await.expect("an idle engine compacts");
    drain(&mut events).await;

    let bodies: Vec<serde_json::Value> =
        endpoint.seen().iter().map(ganja_testkit::responses_server::Recorded::json).collect();
    let [step, title, summary] = bodies.as_slice() else {
        panic!("a step, a title and a summary: {bodies:?}");
    };
    assert!(is_title_body(title), "the second request is the title: {title}");
    assert_eq!(step["service_tier"], json!("default"), "{step}");
    assert_eq!(step["text"], json!({"verbosity": "low"}), "{step}");
    assert_eq!(step["tool_choice"], json!("required"), "the step offers a tool: {step}");

    for key in ["service_tier", "text", "tool_choice"] {
        assert!(title.get(key).is_none(), "the title carries no `{key}`: {title}");
    }
    assert_eq!(
        title["stream_options"],
        json!({"include_obfuscation": false}),
        "and exactly the wire's own default beside today's body: {title}"
    );

    assert!(summary.get("tools").is_none(), "the summary offers nothing: {summary}");
    assert_eq!(summary["service_tier"], json!("default"), "{summary}");
    assert_eq!(summary["text"], json!({"verbosity": "low"}), "no `format` beside it: {summary}");
    assert!(summary.get("tool_choice").is_none(), "the wire dropped the roster key: {summary}");
}

/// A first reply: a thought the backend seals, then the call it led to.
///
/// The order and the `encrypted_content: null` on the opening item are the
/// API's own (`tool-runtime.test.ts:544-565`) — the state arrives when the
/// item closes, and a build reading the opening one would replay nothing.
fn sealed_then_calls() -> String {
    [
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1","encrypted_content":null}}"#,
        r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","summary_index":0,"delta":"Ask the tool."}"#,
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","encrypted_content":"sealed-state"}}"#,
        r#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"lookup","arguments":""}}"#,
        r#"data: {"type":"response.function_call_arguments.delta","item_id":"item_1","delta":"{\"city\":\"Paris\"}"}"#,
        r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"lookup","arguments":"{\"city\":\"Paris\"}"}}"#,
        r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":5}}}"#,
    ]
    .join("\n\n")
        + "\n\n"
}

/// A second reply: the answer, and no further calls, so the turn ends.
fn a_closing_reply() -> String {
    [
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_2"}}"#,
        r#"data: {"type":"response.output_text.delta","item_id":"msg_2","delta":"Rain."}"#,
        r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":12,"output_tokens":3}}}"#,
    ]
    .join("\n\n")
        + "\n\n"
}
