use std::sync::Arc;

use futures::StreamExt as _;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{
    ACCOUNT_HEADER, ALLOWED_MODELS, Aliases, BETA, BETA_HEADER, Backend, Body,
    CHAT_COMPLETIONS_ONLY, CODEX_USER_AGENT, DEFAULT_BASE_URL, Frame, ID, Mapper as _, Mapping,
    OPENAI_CAP, ORIGINATOR, ORIGINATOR_HEADER, ResponsesProvider, SEAT_ROSTER,
    SUBSCRIPTION_DEFAULT, alias, generation, reauth, seals_reasoning, serves, summarized,
};
use crate::auth::{self, AuthError, OauthCredential, RefreshOauth};
use crate::catalog;
use crate::protocol::{FinishReason, Message, Part, PartBody, PartId, ToolState, Usage};
use crate::provider::{
    ChatRequest, CredentialSource, NO_RESULT, PROVIDERS, Presented, Provider as _, ProviderError,
    ProviderEvent, Resolved, openai, openrouter, replay, splice_effort,
};
use crate::tool::ToolDefinition;

/// A token no other value in this module could be mistaken for.
const ACCESS: &str = "at-responses-canary-7717";

/// The account the credential names.
const ACCOUNT: &str = "acct_2f7QpL9";

/// An API key no other value in this module could be mistaken for.
const KEY: &str = "sk-responses-key-canary-3131";

/// A model this backend serves (`codex.ts:15`).
const SERVED: &str = "gpt-5.4";

/// One it does not, and the one the live pass actually named
/// (`codex.ts:289`).
const REFUSED: &str = "gpt-5.6";

/// A renewal that must never run, for the cases about construction rather
/// than about a token endpoint.
struct NeverRenews;

#[async_trait::async_trait]
impl RefreshOauth for NeverRenews {
    async fn refresh(
        &self,
        provider_id: &str,
        _credential: &OauthCredential,
    ) -> Result<OauthCredential, AuthError> {
        panic!("{provider_id} was renewed by a test that only builds a provider");
    }
}

/// Runs a recorded transcript through the real splitter and mapper.
async fn events(transcript: &'static str) -> Vec<ProviderEvent> {
    replay(transcript, CancellationToken::new(), Mapping::default()).collect().await
}

/// The same, read by the mapper a gateway turn installs: nothing sealed,
/// and this vendor's own event spellings.
async fn gateway_events(transcript: &'static str) -> Vec<ProviderEvent> {
    replay(
        transcript,
        CancellationToken::new(),
        Mapping::for_backend(Backend::OpenRouter, Aliases::default()),
    )
    .collect()
    .await
}

/// The thinking a transcript streams, which is what the ✻ pane renders.
fn thinking(events: &[ProviderEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ReasoningDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

/// The reply text a transcript streams.
fn text(events: &[ProviderEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::TextDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

/// A resolved credential, the way one reaches [`ResponsesProvider::request`].
fn resolved(account_id: Option<&str>) -> Resolved {
    presenting(ACCESS, account_id)
}

/// The same, for a test that needs to say which secret travelled.
fn presenting(secret: &str, account_id: Option<&str>) -> Resolved {
    Resolved {
        presented: Presented::new(secret).expect("a non-blank credential"),
        account_id: account_id.map(str::to_owned),
    }
}

/// A subscription provider pointed somewhere a token may travel.
fn provider() -> ResponsesProvider {
    ResponsesProvider::at("http://127.0.0.1:8080/backend-api/codex", Arc::new(NeverRenews))
        .expect("loopback may carry a token")
}

/// The same wire against the platform, authenticated by a key.
///
/// Built through the private constructor rather than through
/// [`ResponsesProvider::from_env`] because that one reads the environment,
/// which is process-wide state a unit test must not mutate; what it would
/// add is the key lookup, and `credentials_env.rs` already owns that.
fn keyed() -> ResponsesProvider {
    ResponsesProvider::built(
        CredentialSource::Key(Presented::new(KEY).expect("a non-blank key")),
        "http://127.0.0.1:8080/v1".to_owned(),
        Backend::Platform,
    )
    .expect("loopback may carry a key")
}

/// A model the gateway publishes, in that vendor's own namespaced spelling.
const GATEWAY_MODEL: &str = "openai/gpt-5.4";

/// The same wire against the gateway, authenticated by that vendor's key.
fn routed() -> ResponsesProvider {
    ResponsesProvider::built(
        CredentialSource::Key(Presented::new(KEY).expect("a non-blank key")),
        "http://127.0.0.1:8080/api/v1".to_owned(),
        Backend::OpenRouter,
    )
    .expect("loopback may carry a key")
}

/// One turn's worth of request against the gateway.
fn gateway_ask() -> ChatRequest {
    ChatRequest { model: GATEWAY_MODEL.to_owned(), ..ask() }
}

/// One turn's worth of request, on a model this backend serves — anything
/// else is refused before a request is built at all.
fn ask() -> ChatRequest {
    ChatRequest {
        effort_options: Default::default(),
        model: SERVED.to_owned(),
        system: None,
        messages: vec![Message::user("hello")],
        tools: Vec::new(),
    }
}

#[test]
fn the_subscription_wire_is_the_same_vendor_as_the_key_one() {
    assert_eq!(ID, openai::ID, "one provider id, or a turn is priced wrong");
    assert!(PROVIDERS.contains(&ID), "a provider nothing can select is a provider nobody has");
    assert_eq!(ID, auth::openai::PROVIDER_ID, "and one credential to read");
    assert_eq!(
        format!("{DEFAULT_BASE_URL}/responses"),
        "https://chatgpt.com/backend-api/codex/responses",
        "codex.ts:12 — a ChatGPT token is minted for this backend and \
             api.openai.com refuses it"
    );
    assert_eq!(
        format!("{}/responses", Backend::Platform.default_base_url()),
        "https://api.openai.com/v1/responses",
        "the endpoint the live 400 named: \"To use function tools, use \
             /v1/responses\""
    );
}

/// Every header the codex backend uses to decide whether to serve a request
/// at all. Dropping any one of them is a turn that fails in production and
/// nowhere else, which is why this is asserted on the request rather than
/// on the code that builds it.
#[test]
fn every_subscription_request_names_the_account_the_originator_and_the_agent() {
    let built = provider().request(&resolved(Some(ACCOUNT)), &ask()).expect("the request builds");
    let headers = built.headers();
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());

    assert_eq!(built.url().as_str(), "http://127.0.0.1:8080/backend-api/codex/responses");
    assert_eq!(header("authorization"), Some(format!("Bearer {ACCESS}")).as_deref());
    assert_eq!(
        header(ACCOUNT_HEADER),
        Some(ACCOUNT),
        "codex.ts:406-408 — without it the backend cannot tell which of a \
             person's accounts to serve"
    );
    assert_eq!(header(ORIGINATOR_HEADER), Some(ORIGINATOR));
    assert_eq!(header(BETA_HEADER), Some(BETA));
    assert_eq!(
        header("user-agent"),
        Some(CODEX_USER_AGENT),
        "the name this host is told, which is this backend's own constant \
             rather than one shared with every other host"
    );

    // A credential naming no account still makes a request: most people
    // have exactly one, and `auth::openai` reads a token with no such claim
    // as a login that worked.
    let anonymous = provider().request(&resolved(None), &ask()).expect("the request builds");
    assert!(
        !anonymous.headers().contains_key(ACCOUNT_HEADER),
        "an account nobody named must not travel as an empty string"
    );
}

/// The other backend's request, which is the same body under a bearer and
/// **nothing else**.
///
/// Each of the four headers above exists because the subscription request
/// is a ChatGPT seat on the Codex CLI's registration; a key is the
/// caller's own credential against the platform, and
/// upstream sends such a request through the unwrapped `fetch`
/// (`codex.ts:356`), so it gains none of them. Asserted as absences because
/// that is the failure mode — a header added on a hunch travels with
/// somebody's API key to an endpoint that never asked for it.
#[test]
fn a_key_request_carries_the_bearer_and_none_of_the_subscription_headers() {
    // An account id on a key credential is impossible — `CredentialSource::Key`
    // resolves with `account_id: None` — but the header is skipped by
    // *backend* rather than by whether one was resolved, so handing it one
    // anyway proves the branch instead of the coincidence.
    let built =
        keyed().request(&presenting(KEY, Some(ACCOUNT)), &ask()).expect("the request builds");
    let headers = built.headers();

    assert_eq!(
        built.url().as_str(),
        "http://127.0.0.1:8080/v1/responses",
        "the platform's own Responses path, under whatever base URL points \
             at it"
    );
    assert_eq!(
        headers.get("authorization").and_then(|value| value.to_str().ok()),
        Some(format!("Bearer {KEY}")).as_deref()
    );
    for absent in [ACCOUNT_HEADER, ORIGINATOR_HEADER, BETA_HEADER, "user-agent"] {
        assert!(
            !headers.contains_key(absent),
            "`{absent}` belongs to the subscription backend and reached the \
                 platform: {headers:?}"
        );
    }
    assert_eq!(
        serde_json::to_value(Body::new(&ask(), Backend::Platform)).expect("the body serializes")["store"],
        json!(false),
        "one encoder, so `store: false` is not a subscription special case"
    );
}

/// The third backend's request: the same encoder pointed at another vendor,
/// under that vendor's own bearer and with **nothing** of this one's.
///
/// Spelled as absences for the reason the platform test above is: a field
/// carried over on the assumption that one Responses surface is every
/// Responses surface is exactly the failure this backend exists to prevent,
/// and each absence here is a row of `super::openrouter`'s ledger.
#[test]
fn an_openrouter_request_carries_only_what_that_vendor_documents() {
    let provider = routed();
    let built = provider
        .request(&presenting(KEY, Some(ACCOUNT)), &gateway_ask())
        .expect("the request builds");
    let headers = built.headers();

    assert_eq!(
        built.url().as_str(),
        "http://127.0.0.1:8080/api/v1/responses",
        "the vendor's own Responses path, under whatever base URL points at it"
    );
    assert_eq!(
        headers.get("authorization").and_then(|value| value.to_str().ok()),
        Some(format!("Bearer {KEY}")).as_deref(),
        "the reference asks for a bearer and a content type, and this is the \
             half that is a credential"
    );
    for absent in [ACCOUNT_HEADER, ORIGINATOR_HEADER, BETA_HEADER, "user-agent"] {
        assert!(
            !headers.contains_key(absent),
            "`{absent}` belongs to a ChatGPT seat on the Codex CLI's \
                 registration and reached a different vendor entirely: {headers:?}"
        );
    }

    let body = serde_json::to_value(Body::new(&gateway_ask(), Backend::OpenRouter))
        .expect("the body serializes");
    assert_eq!(
        body["store"],
        json!(false),
        "the reference rejects `store: true` outright, so `false` is the only \
             value a stateless API takes"
    );
    assert!(
        body.get("include").is_none(),
        "the reference documents no `include` parameter at all: {body}"
    );
    assert!(
        body.get("previous_response_id").is_none(),
        "the other half of the same rejection, and a field this encoder has \
             never had: {body}"
    );
    assert_eq!(
        body["model"],
        json!(GATEWAY_MODEL),
        "the id passes through \
             verbatim, namespace and all — it is the vendor's own spelling"
    );
}

/// The config-named backend's request: the entry's own headers travel
/// beside the bearer, nothing of the subscription's does, and the body
/// asks the endpoint for nothing this build cannot vouch it documents —
/// [`super::openrouter`]'s posture, applied to a vendor never met at all.
#[test]
fn a_config_named_request_carries_its_headers_and_asks_for_nothing_sealed() {
    let mut declared = reqwest::header::HeaderMap::new();
    declared.insert("x-custom", "1".parse().expect("a header value"));
    let provider = ResponsesProvider::built(
        CredentialSource::Key(Presented::new(KEY).expect("a non-blank key")),
        "http://127.0.0.1:8080/v1".to_owned(),
        Backend::Compat,
    )
    .expect("loopback may carry a key")
    .with_headers(declared);

    let built =
        provider.request(&presenting(KEY, Some(ACCOUNT)), &ask()).expect("the request builds");
    let headers = built.headers();

    assert_eq!(
        built.url().as_str(),
        "http://127.0.0.1:8080/v1/responses",
        "the endpoint the entry named, under the wire's own path"
    );
    assert_eq!(
        headers.get("x-custom").and_then(|value| value.to_str().ok()),
        Some("1"),
        "the entry's own headers travel"
    );
    assert_eq!(
        headers.get("authorization").and_then(|value| value.to_str().ok()),
        Some(format!("Bearer {KEY}")).as_deref()
    );
    for absent in [ACCOUNT_HEADER, ORIGINATOR_HEADER, BETA_HEADER, "user-agent"] {
        assert!(
            !headers.contains_key(absent),
            "`{absent}` belongs to the subscription backend and reached an \
                 endpoint a config named: {headers:?}"
        );
    }

    // `ask()`'s model is one `seals_reasoning` recognizes, so each absence
    // below is the backend refusing, not the model failing to qualify.
    let body =
        serde_json::to_value(Body::new(&ask(), Backend::Compat)).expect("the body serializes");
    assert_eq!(
        body["store"],
        json!(false),
        "one encoder, so the stateless posture is not somebody's special case"
    );
    assert!(
        body.get("include").is_none(),
        "nothing sealed is asked of an endpoint this build has never met: {body}"
    );
    let untouched = serde_json::Map::new();
    assert_eq!(
        summarized(&untouched, SERVED, Backend::Compat),
        untouched,
        "and no default is written into somebody else's `reasoning` object"
    );
}

/// The vendor's chat-completions-only fact is not pre-applied to an
/// endpoint that merely borrowed the vendor's wire: what a config-named
/// server serves under any model name is its own to answer for, and the
/// refusal's advice (`--model openai/…`) would name a provider such a
/// session is not on.
#[test]
fn a_config_named_endpoint_is_not_held_to_the_vendors_model_facts() {
    let compat = ResponsesProvider::built(
        CredentialSource::Key(Presented::new(KEY).expect("a non-blank key")),
        "http://127.0.0.1:8080/v1".to_owned(),
        Backend::Compat,
    )
    .expect("loopback may carry a key");
    let alias = CHAT_COMPLETIONS_ONLY[0];

    assert!(
        compat.refuses(alias).is_none(),
        "pre-refusing would guess about an endpoint this build has never met"
    );
    assert!(
        keyed().refuses(alias).is_some(),
        "while the vendor's own backend still refuses the alias it measured"
    );
}

/// Asking for sealed reasoning and replaying it are one feature, and this
/// backend does neither: the vendor documents no `include` to ask with and
/// no way to hand the state back. Half of the pairing would be worse than
/// neither — a request that asked and never replayed spends a field every
/// turn for nothing, and one that replayed unasked is a guess whose failure
/// lands on the *second* request of every reasoning turn.
///
/// The same transcript on the platform backend is the control: what differs
/// is the backend and nothing else about the fixture.
#[test]
fn an_openrouter_turn_neither_asks_for_sealed_reasoning_nor_replays_it() {
    let mut assistant = Message::assistant("gpt");
    assistant.parts.push(Part::text("thinking about it"));
    assistant.parts.push(Part::reasoning(
        openrouter::ID,
        "rs_gateway",
        Some("sealed-by-the-gateway".to_owned()),
    ));

    let mut request = gateway_ask();
    request.messages.push(assistant);

    let body = serde_json::to_value(Body::new(&request, Backend::OpenRouter))
        .expect("the body serializes");
    assert!(body.get("include").is_none(), "nothing was asked for: {body}");
    assert!(
        !body["input"]
            .as_array()
            .expect("input is a list")
            .iter()
            .any(|item| item["type"] == json!("reasoning")),
        "and nothing is handed back: {body}"
    );

    // The control. A model whose id `seals_reasoning` recognizes, on the
    // backend whose vendor documents the pairing, does both — so what the
    // assertions above prove is the backend and not the fixture.
    let mut owned = Message::assistant("gpt");
    owned.parts.push(Part::reasoning(ID, "rs_1", Some("sealed".to_owned())));
    let mut control = ask();
    control.messages.push(owned);

    let platform =
        serde_json::to_value(Body::new(&control, Backend::Platform)).expect("the body serializes");
    assert_eq!(platform["include"], json!(["reasoning.encrypted_content"]));
    assert!(
        platform["input"]
            .as_array()
            .expect("input is a list")
            .iter()
            .any(|item| item["type"] == json!("reasoning")),
        "{platform}"
    );
}

/// The reading half of the same decision. A mapper that recorded state this
/// build will never hand back would mint the row-that-can-never-do-anything
/// [`super::sealed`]'s own doc refuses, one layer further out.
#[test]
fn the_backend_that_replays_sealed_state_is_the_one_that_records_it() {
    for backend in [Backend::Codex, Backend::Platform] {
        assert!(
            Mapping::for_backend(backend, Aliases::default()).seals && backend.replays_reasoning(),
            "{backend:?} documents the pairing, so both halves are on"
        );
    }
    for backend in [Backend::OpenRouter, Backend::Compat] {
        assert!(
            !Mapping::for_backend(backend, Aliases::default()).seals
                && !backend.replays_reasoning(),
            "{backend:?}: both halves are off together, or the transcript \
                 fills with state nothing will ever send"
        );
    }
}

/// `reasoning.summary: "auto"` is two of this vendor's decisions in one
/// field — [`seals_reasoning`] is a rule about *its* model ids, and `"auto"`
/// is what *its* CLI sends — so a gateway fronting mostly other people's
/// models gets neither. What an effort put there still travels, because the
/// reference does document `reasoning` with effort levels.
#[test]
fn an_openrouter_request_defaults_no_summary_and_still_carries_an_effort() {
    let bare = summarized(&serde_json::Map::new(), "openai/gpt-5.4", Backend::OpenRouter);
    assert!(
        bare.is_empty(),
        "an id that merely *contains* this vendor's model family is not this \
             vendor's model: {bare:?}"
    );

    let mut request = gateway_ask();
    request.effort_options =
        json!({"reasoning": {"effort": "high"}}).as_object().cloned().expect("object fixture");
    let own = Body::new(&request, Backend::OpenRouter);
    let options = summarized(&request.effort_options, &request.model, Backend::OpenRouter);
    let body =
        serde_json::to_value(splice_effort(&options, &own)).expect("a spliced body serializes");

    assert_eq!(
        body["reasoning"],
        json!({"effort": "high"}),
        "the effort's own object, and not a summary nobody asked for"
    );
}

/// A gateway row as the catalog holds it once [`crate::effort::roster`] has
/// run over it, which is what `/effort`, `run --effort` and the config seed
/// all read.
fn gateway_row() -> catalog::ModelInfo {
    let mut row = catalog::ModelInfo {
        id: GATEWAY_MODEL.to_owned(),
        provider_id: openrouter::ID.to_owned(),
        name: "GPT-5.4".to_owned(),
        context_window: 1_050_000,
        max_output: 128_000,
        input_limit: None,
        pricing: catalog::Pricing { input: 2.5, output: 15.0, cache_read: 0.25, cache_write: None },
        family: None,
        release_date: None,
        tool_call: true,
        status: catalog::ModelStatus::Active,
        reasoning: true,
        reasoning_options: None,
        npm: None,
        variants: std::collections::BTreeMap::new(),
    };
    row.variants = crate::effort::roster(&row);

    row
}

/// The whole of R1, end to end on the one seam that matters: the roster the
/// catalog row carries is what a person is offered, and the option map each
/// entry holds is what the request body ends up carrying — exactly
/// `reasoning.effort`, and none of the two fields this vendor's ledger
/// drops.
#[test]
fn every_effort_this_gateway_offers_travels_as_the_one_field_it_documents() {
    let row = gateway_row();
    assert_eq!(
        row.variants.keys().map(String::as_str).collect::<Vec<_>>(),
        ["high", "low", "medium", "minimal"],
        "the reference's own four levels reach the chooser"
    );

    for (name, options) in &row.variants {
        let mut request = gateway_ask();
        request.effort_options = options.clone();

        let own = Body::new(&request, Backend::OpenRouter);
        let spliced = summarized(&request.effort_options, &request.model, Backend::OpenRouter);
        let body =
            serde_json::to_value(splice_effort(&spliced, &own)).expect("a spliced body serializes");

        assert_eq!(
            body["reasoning"],
            json!({"effort": name}),
            "`{name}` has to reach the wire as the reference's own object"
        );
        assert!(
            body.get("include").is_none() && body["reasoning"].get("summary").is_none(),
            "the effort door is not the way the dropped fields come back: {body}"
        );
    }

    // The standing posture, re-pinned beside them: no effort selected is no
    // `reasoning` key at all.
    let bare = serde_json::to_value(Body::new(&gateway_ask(), Backend::OpenRouter))
        .expect("the body serializes");
    assert!(bare.get("reasoning").is_none(), "got {bare}");
}

/// The splice order at this wire's send site: an effort adds what the body
/// does not carry — `reasoning` is the catalog's use of it — and loses
/// every key the wire itself writes; `store: false` in particular is what
/// the ChatGPT backend requires, so no catalog row may unmake it.
#[test]
fn an_effort_adds_reasoning_but_cannot_claim_store() {
    let mut request = ask();
    request.effort_options = json!({
        "reasoning": {"effort": "high"},
        "store": true,
        "model": "someone-elses",
    })
    .as_object()
    .cloned()
    .expect("the fixture options are an object");

    let own = Body::new(&request, Backend::Platform);
    let body = serde_json::to_value(splice_effort(&request.effort_options, &own))
        .expect("a spliced body serializes");

    assert_eq!(
        body["reasoning"],
        json!({"effort": "high"}),
        "a key the wire does not write arrives verbatim"
    );
    assert_eq!(body["store"], json!(false), "a key the wire writes resolves to the wire");
    assert_eq!(body["model"], json!(SERVED));
}

/// Readable thinking exists only downstream of asking for it: the body
/// carries `reasoning.summary: "auto"` for a model that reasons, the
/// default fills absence only, and a model that does not reason gets no
/// `reasoning` field to be refused over.
#[test]
fn a_reasoning_model_is_asked_to_show_its_thinking() {
    let cases: Vec<(&str, serde_json::Map<String, serde_json::Value>, serde_json::Value)> = vec![
        ("bare request asks for auto", serde_json::Map::new(), json!({"summary": "auto"})),
        (
            "an effort's own keys survive beside the default",
            json!({"reasoning": {"effort": "high"}}).as_object().cloned().expect("object fixture"),
            json!({"effort": "high", "summary": "auto"}),
        ),
        (
            "a summary somebody spelled out is theirs",
            json!({"reasoning": {"summary": "detailed"}})
                .as_object()
                .cloned()
                .expect("object fixture"),
            json!({"summary": "detailed"}),
        ),
    ];
    for (name, options, expected) in cases {
        let mut request = ask();
        request.effort_options = options;
        let own = Body::new(&request, Backend::Platform);
        let options = summarized(&request.effort_options, &request.model, Backend::Platform);
        let body =
            serde_json::to_value(splice_effort(&options, &own)).expect("a spliced body serializes");
        assert_eq!(body["reasoning"], expected, "{name}");
    }

    let mut request = ask();
    request.model = "gpt-5-chat".to_owned();
    let own = Body::new(&request, Backend::Platform);
    let options = summarized(&request.effort_options, &request.model, Backend::Platform);
    let body =
        serde_json::to_value(splice_effort(&options, &own)).expect("a spliced body serializes");
    assert!(
        body.get("reasoning").is_none(),
        "a model that does not reason is asked nothing about reasoning"
    );
}

/// The allow-list is a ChatGPT seat's product decision, and applying it to
/// a key would be somebody else's catalog deciding what an API key may ask
/// for. Upstream scopes it the same way and on the same condition:
/// `codex.ts:281` returns `provider.models` unfiltered whenever the
/// credential is not an OAuth one.
///
/// The model is the one the live pass proved the seat refuses, so this
/// says the two backends answer the same name differently.
#[test]
fn the_seats_allow_list_gates_the_subscription_backend_and_not_the_platform() {
    assert!(
        provider().refuses(REFUSED).is_some(),
        "{REFUSED} is `codex.ts:289`'s own arm and the seat cannot run it"
    );
    assert!(
        keyed().refuses(REFUSED).is_none(),
        "a key session held to a seat's allow-list cannot reach the models \
             it is paying for, which is the whole reason this wire moved"
    );

    // Both directions of the scoping, so removing the backend check fails
    // rather than merely widening what is served.
    assert!(provider().refuses(SERVED).is_none() && keyed().refuses(SERVED).is_none());
}

/// The one refusal that is not about a seat.
///
/// Upstream hides `gpt-5-chat-latest` from the OpenAI catalog outright,
/// with the reason in a comment at `plugin/provider/openai.ts:164-171`: the
/// plugin sends every OpenAI model through Responses and that alias is
/// chat-completions-only. It is therefore refused on **both** backends —
/// the vendor has no wire left that could serve it — and the message says
/// so rather than pointing at an API key that would not help.
#[test]
fn a_chat_completions_only_model_is_refused_on_both_backends() {
    // Named literally as well as iterated: a list this test only reads out
    // of would agree with itself however it was edited, and this is the one
    // string `plugin/provider/openai.ts:166` actually disables.
    assert!(
        CHAT_COMPLETIONS_ONLY.contains(&"gpt-5-chat-latest"),
        "the alias upstream hides is what this list is for"
    );

    for alias in CHAT_COMPLETIONS_ONLY {
        for (backend, provider) in [("subscription", provider()), ("key", keyed())] {
            let refused = provider
                .refuses(alias)
                .unwrap_or_else(|| panic!("{backend}: {alias} has no wire here to serve it"))
                .to_string();

            assert!(
                refused.contains(alias) && refused.contains("Responses"),
                "{backend}: the refusal has to say why there is no wire for \
                     it: {refused}"
            );
            assert!(
                !refused.contains(openai::API_KEY_ENV),
                "{backend}: a key is not the way out of this one, and \
                     offering it sends somebody to buy nothing: {refused}"
            );
        }
    }

    // The catalog this build compiles in carries no such row, which is
    // exactly why the refusal lives at the wire: `ganja models --refresh`
    // replaces that table with upstream's own file, and this list has to
    // keep applying to whatever it brings.
    assert!(
        CHAT_COMPLETIONS_ONLY.iter().all(|alias| catalog::model(alias).is_none()),
        "a row the snapshot now carries wants deciding here as well as at \
             the wire"
    );
}

/// The endpoint is not exempt from the rule every other provider's is held
/// to just because the credential arrived as a token rather than as a key.
#[test]
fn an_access_token_may_not_be_sent_anywhere_a_key_could_not_be() {
    let refused =
        ResponsesProvider::at("http://chatgpt.com/backend-api/codex", Arc::new(NeverRenews))
            .expect_err("plain http to a public host puts the token on the wire in the clear");

    assert!(matches!(refused, ProviderError::Transport(_)), "{refused:?}");
    assert!(
        ResponsesProvider::at("http://127.0.0.1:8080", Arc::new(NeverRenews)).is_ok(),
        "loopback never reaches a network, which is what a test depends on"
    );
}

#[test]
fn a_provider_never_renders_its_credential() {
    let provider = ResponsesProvider::at(
        "http://ganja:at-url-canary-9999@127.0.0.1:8080/backend-api/codex",
        Arc::new(NeverRenews),
    )
    .expect("loopback may carry a token");
    let rendered = format!("{provider:?}");

    assert!(
        !rendered.contains("at-url-canary-9999") && !rendered.contains("ganja:"),
        "a provider leaked its endpoint's userinfo: {rendered}"
    );
    assert!(
        rendered.contains("Oauth") && rendered.contains(ID),
        "a provider renders as which provider it is: {rendered}"
    );
    assert!(
        rendered.contains("127.0.0.1:8080"),
        "the endpoint is what tells one provider from another: {rendered}"
    );
}

/// A subscription session that names no model does **not** take the
/// catalog's default, and this is the half of that statement this module
/// owns.
///
/// The catalog holds one default per vendor, which is one too few here: a
/// row the platform sells and the seat does not — `gpt-5.6` is exactly that
/// — would hand a ChatGPT login a model its backend refuses outright, which
/// is a session that cannot take a single turn. So the seat brings its own,
/// and the obligations on it are both directions at once: the backend has
/// to serve it, and the catalog has to be able to size and price it.
///
/// The other half — that `select` actually reaches for this rather than for
/// the catalog — is `responses_wire.rs`'s, because it takes an environment
/// and a store to observe.
#[test]
fn a_subscription_session_that_names_no_model_gets_one_the_seat_can_run() {
    let info =
        catalog::model(SUBSCRIPTION_DEFAULT).expect("the subscription default is in the table");

    assert_eq!(info.provider_id, ID);
    assert!(info.context_window > 0 && info.max_output > 0);
    assert!(
        serves(SUBSCRIPTION_DEFAULT),
        "a default this backend refuses is a seat that cannot take a turn"
    );
    assert!(
        ALLOWED_MODELS.contains(&SUBSCRIPTION_DEFAULT),
        "it is named outright by `codex.ts:15` rather than admitted by the \
             generation rule, which is what keeps it from moving under us when \
             the rule does"
    );
}

/// The obligation [`SEAT_ROSTER`] carries: an offer this backend would
/// then refuse is a listing that lies, and the two halves of the roster
/// reach [`serves`] by different routes — two are named by
/// [`ALLOWED_MODELS`], three are admitted by the generation rule — so the
/// pin has to be asserted over the whole list rather than over either.
#[test]
fn every_model_the_seat_offers_is_one_the_seat_serves() {
    for offered in SEAT_ROSTER {
        assert!(serves(offered), "the roster offers `{offered}`, which this backend refuses");
    }
}

/// The other half of **D476**: the pin narrows what is *offered*, never
/// what is *servable*. Somebody who types `--model openai/gpt-5.4` on a
/// seat still takes their turn, although no listing volunteered it — which
/// is why the roster is a separate constant rather than a shorter
/// [`ALLOWED_MODELS`].
#[test]
fn a_model_the_roster_leaves_out_is_still_one_an_explicit_request_may_name() {
    for unoffered in ["gpt-5.4", "gpt-5.4-mini"] {
        assert!(!SEAT_ROSTER.contains(&unoffered), "`{unoffered}` is deliberately unoffered");
        assert!(
            serves(unoffered),
            "and deliberately still servable: the pin is an offer, not a gate"
        );
    }
}

/// The split that makes "display-only" a fact about this build rather
/// than a sentence in a doc comment (bead `pwe`), and the wire where both
/// halves are visible at once: this API *does* take reasoning back, so the
/// body below has to carry the sealed item and not the readable one.
///
/// A single moved match arm is all it would take to send both, and the
/// failure would be invisible — the request would still be accepted, and
/// the model would simply be handed the same thought twice, once in a
/// form it never asked for.
#[test]
fn a_transcript_held_thought_is_absent_from_the_body_while_the_sealed_half_travels() {
    const THOUGHT: &str = "the-user-is-probably-testing-me";
    const SEALED: &str = "sealed-blob-0001";

    let mut turn = Message::assistant(SERVED);
    turn.parts.push(Part::reasoning_text(THOUGHT));
    turn.parts.push(Part::text("Hello!"));
    turn.parts.push(Part::reasoning(ID, "rs_1", Some(SEALED.to_owned())));

    let request =
        ChatRequest { messages: vec![Message::user("hi"), turn, Message::user("again")], ..ask() };
    let body = serde_json::to_string(&Body::new(&request, Backend::Platform))
        .expect("the body serializes");

    assert!(
        !body.contains(THOUGHT),
        "the thought reached the wire; nothing sends readable reasoning: {body}"
    );
    assert!(
        body.contains(SEALED),
        "the *sealed* half is what this API asked to have handed back, and \
             a build that dropped it starts every reasoning turn from nothing \
             — this test must fail if the split is collapsed either way: {body}"
    );
    assert!(body.contains("Hello!"), "the reply still has to be sent: {body}");
}

/// The field the live pass died on. A body without it is answered
/// `400 {"detail":"Store must be set to false"}`, which is every
/// subscription turn this build could take — so it is asserted on the
/// serialized body rather than on the struct that produces it.
///
/// Its companion is here too: with the backend keeping nothing, `include`
/// is the only reason a second request can carry the first one's thinking.
#[test]
fn every_body_tells_the_backend_to_keep_nothing_and_to_seal_what_it_thought() {
    let body =
        serde_json::to_value(Body::new(&ask(), Backend::Platform)).expect("the body serializes");

    assert_eq!(
        body["store"],
        json!(false),
        "without this the backend refuses the turn outright: got {body}"
    );
    assert_eq!(
        body["include"],
        json!(["reasoning.encrypted_content"]),
        "`store: false` without this is a reasoning model whose every turn \
             starts from nothing: got {body}"
    );
}

/// Upstream attaches the include to the *model*, so this build asks the
/// same question of both backends rather than of the credential. The
/// exclusions are the two OpenAI models that do not reason — and
/// `gpt-5.5-pro`, which reads like one and is not spelled like one, is
/// deliberately still asked, because that is what the pin's literal does.
#[test]
fn asking_for_sealed_reasoning_is_a_question_about_the_model() {
    for reasons in
        ["gpt-5.4", "gpt-5.5", "gpt-5.6", "gpt-5.3-codex-spark", "GPT-5.4-MINI", "gpt-5.5-pro"]
    {
        assert!(seals_reasoning(reasons), "{reasons} reasons");
    }
    for plain in ["gpt-5-chat-latest", "gpt-5-pro", "gpt-4.1", "o3", "claude"] {
        assert!(!seals_reasoning(plain), "{plain} has nothing to seal");
    }

    let plain = ChatRequest { model: "gpt-4.1".to_owned(), ..ask() };
    let body =
        serde_json::to_value(Body::new(&plain, Backend::Platform)).expect("the body serializes");
    assert!(
        body["include"].is_null(),
        "a body that asks for nothing omits the field entirely, the way \
             upstream's does: got {body}"
    );
    assert_eq!(body["store"], json!(false), "and still keeps nothing");
}

/// A step's sealed thinking goes back in the shape the pin's second
/// request carries — before the calls it produced, with an empty summary
/// and **no** item id, because under `store: false` there is no
/// server-side item for one to name
/// (`packages/llm/test/tool-runtime.test.ts:599-604`).
#[test]
fn a_sealed_thought_is_replayed_before_the_calls_it_produced() {
    let mut assistant = Message::assistant("gpt-test");
    assistant.parts.push(Part { id: PartId::ascending(), body: PartBody::StepStart });
    assistant.parts.push(Part::reasoning(ID, "rs_1", Some("sealed-state".to_owned())));
    assistant.parts.push(tool_part(
        "call_read",
        "read",
        completed(json!({"filePath": "src/main.rs"}), "fn main() {}"),
    ));

    let request = ChatRequest {
        effort_options: Default::default(),
        model: SERVED.to_owned(),
        system: None,
        messages: vec![Message::user("read src/main.rs"), assistant],
        tools: Vec::new(),
    };
    let body =
        serde_json::to_value(Body::new(&request, Backend::Platform)).expect("the body serializes");

    assert_eq!(
        body["input"],
        json!([
            {"role": "user", "content": [{"type": "input_text", "text": "read src/main.rs"}]},
            {"type": "reasoning", "summary": [], "encrypted_content": "sealed-state"},
            {
                "type": "function_call",
                "call_id": "call_read",
                "name": "read",
                "arguments": r#"{"filePath":"src/main.rs"}"#,
            },
            {
                "type": "function_call_output",
                "call_id": "call_read",
                "output": "fn main() {}",
            },
        ]),
        "got {body}"
    );
}

/// The exact item shapes the Responses API documents for attachments,
/// pinned so a drift in the encoder is a red test rather than a 400 from
/// the vendor: an image rides `input_image` as a base64 data URL, a PDF
/// rides `input_file` with the mentioned path as its `filename`, and a
/// file part carrying no content is a reference the engine already
/// resolved, encoding nothing.
#[test]
fn an_attachment_becomes_the_input_item_its_mime_names() {
    let mut user = Message::user("what are these");
    user.parts.push(Part {
        id: PartId::ascending(),
        body: PartBody::File {
            path: "shot.png".to_owned(),
            mime: "image/png".to_owned(),
            start: None,
            end: None,
            content: Some("aW1n".to_owned()),
        },
    });
    user.parts.push(Part {
        id: PartId::ascending(),
        body: PartBody::File {
            path: "docs/paper.pdf".to_owned(),
            mime: "application/pdf".to_owned(),
            start: None,
            end: None,
            content: Some("cGRm".to_owned()),
        },
    });
    user.parts.push(Part::file("notes.md", "text/plain"));

    let request = ChatRequest {
        effort_options: Default::default(),
        model: SERVED.to_owned(),
        system: None,
        messages: vec![user],
        tools: Vec::new(),
    };
    let body =
        serde_json::to_value(Body::new(&request, Backend::Platform)).expect("the body serializes");

    assert_eq!(
        body["input"],
        json!([{
            "role": "user",
            "content": [
                {"type": "input_text", "text": "what are these"},
                {"type": "input_image", "image_url": "data:image/png;base64,aW1n"},
                {
                    "type": "input_file",
                    "filename": "docs/paper.pdf",
                    "file_data": "data:application/pdf;base64,cGRm",
                },
            ],
        }]),
        "got {body}"
    );
}

/// The three reasoning parts a request must **not** put on the wire, each
/// for its own reason, and each pinned because sending one is a refused
/// request rather than a degraded one.
#[test]
fn reasoning_with_nothing_to_replay_never_reaches_the_wire() {
    let mut assistant = Message::assistant("gpt-test");
    assistant.parts.push(Part { id: PartId::ascending(), body: PartBody::StepStart });
    // 1. State this build does not hold — an item that arrived without
    //    any, or a stored record that would not decode. Upstream drops it
    //    at `openai-responses.ts:451` and so does this.
    assistant.parts.push(Part::reasoning(ID, "rs_lost", None));
    // 2. Another wire's state, which means nothing to this one.
    assistant.parts.push(Part::reasoning(
        "anthropic",
        "th_1",
        Some("someone-elses-state".to_owned()),
    ));
    // 3. The same item twice, which is one item said twice.
    assistant.parts.push(Part::reasoning(ID, "rs_1", Some("sealed-state".to_owned())));
    assistant.parts.push(Part::reasoning(ID, "rs_1", Some("sealed-state".to_owned())));

    let request = ChatRequest {
        effort_options: Default::default(),
        model: SERVED.to_owned(),
        system: None,
        messages: vec![Message::user("hello"), assistant],
        tools: Vec::new(),
    };
    let body =
        serde_json::to_value(Body::new(&request, Backend::Platform)).expect("the body serializes");

    assert_eq!(
        body["input"],
        json!([
            {"role": "user", "content": [{"type": "input_text", "text": "hello"}]},
            {"type": "reasoning", "summary": [], "encrypted_content": "sealed-state"},
        ]),
        "one item survives all three rules, and it is the one this wire \
             sealed itself: got {body}"
    );
}

/// Two summary blocks on one stream stay two thoughts: the part
/// boundary the provider announces between them becomes a break, so they
/// cannot glue into "PlanningDesigning" downstream — and the boundary
/// ahead of the first block says nothing, because there is nothing yet
/// to break from.
#[tokio::test]
async fn a_second_summary_part_breaks_the_thought_before_it() {
    let seen = events(concat!(
            r#"data: {"type":"response.reasoning_summary_part.added","item_id":"rs_1","summary_index":0}"#,
            "\n\n",
            r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"Planning"}"#,
            "\n\n",
            r#"data: {"type":"response.reasoning_summary_part.added","item_id":"rs_1","summary_index":1}"#,
            "\n\n",
            r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"Designing"}"#,
            "\n\n",
            r#"data: {"type":"response.completed","response":{}}"#,
            "\n\n",
        ))
        .await;

    let thoughts: Vec<&ProviderEvent> = seen
        .iter()
        .filter(|event| {
            matches!(event, ProviderEvent::ReasoningDelta(_) | ProviderEvent::ReasoningBreak)
        })
        .collect();
    assert_eq!(
        thoughts,
        vec![
            &ProviderEvent::ReasoningDelta("Planning".to_owned()),
            &ProviderEvent::ReasoningBreak,
            &ProviderEvent::ReasoningDelta("Designing".to_owned()),
        ],
        "got {seen:?}"
    );
}

/// The spelling Gemini serves over the gateway: thinking streams as
/// `response.reasoning_text.delta` and its blocks close with `.done`,
/// which is the only boundary that stream carries — mapped, the two
/// thoughts stay two thoughts; unmapped, the pane stayed empty
/// (2026-08-25).
#[tokio::test]
async fn a_reasoning_text_stream_breaks_at_its_own_block_close() {
    let seen = events(concat!(
        r#"data: {"type":"response.reasoning_text.delta","item_id":"rs_1","delta":"Weighing"}"#,
        "\n\n",
        r#"data: {"type":"response.reasoning_text.done","item_id":"rs_1"}"#,
        "\n\n",
        r#"data: {"type":"response.reasoning_text.delta","item_id":"rs_2","delta":"Steeping"}"#,
        "\n\n",
        r#"data: {"type":"response.completed","response":{}}"#,
        "\n\n",
    ))
    .await;

    let thoughts: Vec<&ProviderEvent> = seen
        .iter()
        .filter(|event| {
            matches!(event, ProviderEvent::ReasoningDelta(_) | ProviderEvent::ReasoningBreak)
        })
        .collect();
    assert_eq!(
        thoughts,
        vec![
            &ProviderEvent::ReasoningDelta("Weighing".to_owned()),
            &ProviderEvent::ReasoningBreak,
            &ProviderEvent::ReasoningDelta("Steeping".to_owned()),
        ],
        "got {seen:?}"
    );
}

/// The receiving half: the state arrives on the item's *closing* frame,
/// the opening one having carried `encrypted_content: null`
/// (`tool-runtime.test.ts:544-553`).
#[tokio::test]
async fn a_reasoning_item_is_taken_when_it_closes_and_only_if_it_was_sealed() {
    let seen = events(concat!(
            r#"data: {"type":"response.output_item.added","item":{"type":"reasoning","id":"rs_1","encrypted_content":null}}"#,
            "\n\n",
            r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"Short is right."}"#,
            "\n\n",
            r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","encrypted_content":"sealed-state"}}"#,
            "\n\n",
            r#"data: {"type":"response.completed","response":{}}"#,
            "\n\n",
        ))
        .await;

    assert!(
        seen.contains(&ProviderEvent::ReasoningState {
            item: "rs_1".to_owned(),
            encrypted: "sealed-state".to_owned(),
        }),
        "the sealed state has to reach the loop or nothing can replay it: {seen:?}"
    );
    assert_eq!(
        seen.iter().filter(|event| matches!(event, ProviderEvent::ReasoningState { .. })).count(),
        1,
        "the item opened once and closed once: {seen:?}"
    );
    assert!(
        seen.contains(&ProviderEvent::ReasoningDelta("Short is right.".to_owned())),
        "the readable half is unchanged by any of this: {seen:?}"
    );
}

/// The gateway's own event name reaches the pane the vendor's does.
///
/// Unmapped it reached the debug log instead, so a reasoning turn over
/// OpenRouter streamed a reply with no thinking under it — the one symptom
/// this arm exists to remove.
#[tokio::test]
async fn a_gateways_reasoning_delta_is_read_as_thinking() {
    let seen = gateway_events(concat!(
            r#"data: {"type":"response.reasoning.delta","item_id":"rs_1","delta":"First, the year. "}"#,
            "\n\n",
            r#"data: {"type":"response.reasoning.delta","item_id":"rs_1","delta":"Then the difference."}"#,
            "\n\n",
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"Yes."}"#,
            "\n\n",
            r#"data: {"type":"response.completed","response":{}}"#,
            "\n\n",
        ))
        .await;

    assert_eq!(thinking(&seen), "First, the year. Then the difference.");
    assert_eq!(text(&seen), "Yes.");
    assert!(
        !seen.iter().any(|event| matches!(event, ProviderEvent::ReasoningState { .. })),
        "readable thinking is not sealed state, and this backend records \
             none of the latter: {seen:?}"
    );
}

/// Pre-mortem 1: a gateway relaying one vendor's stream through its own
/// normalization can carry both spellings of the same event. The first one
/// to say anything wins for the whole response, because a pane that renders
/// one train of thought twice is worse than one that renders the shorter of
/// the two.
#[tokio::test]
async fn both_reasoning_spellings_on_one_stream_render_one_train_of_thought() {
    let seen = gateway_events(concat!(
            r#"data: {"type":"response.reasoning.delta","item_id":"rs_1","delta":"Thinking"}"#,
            "\n\n",
            r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"Thinking"}"#,
            "\n\n",
            r#"data: {"type":"response.reasoning.delta","item_id":"rs_1","delta":" it through."}"#,
            "\n\n",
            r#"data: {"type":"response.completed","response":{}}"#,
            "\n\n",
        ))
        .await;

    assert_eq!(thinking(&seen), "Thinking it through.");
    assert_eq!(
        seen.iter().filter(|event| matches!(event, ProviderEvent::ReasoningDelta(_))).count(),
        2,
        "the relayed copy is dropped rather than appended: {seen:?}"
    );

    // The latch is about *content*: an empty fragment under a spelling the
    // stream then abandons must not lock the other one out.
    let recovered = gateway_events(concat!(
        r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":""}"#,
        "\n\n",
        r#"data: {"type":"response.reasoning.delta","item_id":"rs_1","delta":"Still heard."}"#,
        "\n\n",
        r#"data: {"type":"response.completed","response":{}}"#,
        "\n\n",
    ))
    .await;
    assert_eq!(thinking(&recovered), "Still heard.");
}

/// The vendor documents a `summary` array on the settled reasoning item and
/// no parameter to ask for one, so on a turn that streamed nothing readable
/// the closing frame is the only place thinking exists.
#[tokio::test]
async fn a_summary_that_was_never_streamed_still_reaches_the_pane() {
    let seen = gateway_events(concat!(
        r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","#,
        r#""encrypted_content":"sealed","summary":["First the year","Then the difference"]}}"#,
        "\n\n",
        r#"data: {"type":"response.completed","response":{}}"#,
        "\n\n",
    ))
    .await;

    let thoughts: Vec<&ProviderEvent> = seen
        .iter()
        .filter(|event| {
            matches!(event, ProviderEvent::ReasoningDelta(_) | ProviderEvent::ReasoningBreak)
        })
        .collect();
    assert_eq!(
        thoughts,
        vec![
            &ProviderEvent::ReasoningDelta("First the year".to_owned()),
            &ProviderEvent::ReasoningBreak,
            &ProviderEvent::ReasoningDelta("Then the difference".to_owned()),
        ],
        "each block is a thought of its own: {seen:?}"
    );
    assert!(
        !seen.iter().any(|event| matches!(event, ProviderEvent::ReasoningState { .. })),
        "a summary is not state, and this backend still seals nothing: {seen:?}"
    );

    // A stream that streamed its thinking is already served: the same item
    // closing with the same words must not say them again.
    let streamed = gateway_events(concat!(
        r#"data: {"type":"response.reasoning.delta","item_id":"rs_1","delta":"First the year"}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","#,
        r#""summary":["First the year"]}}"#,
        "\n\n",
        r#"data: {"type":"response.completed","response":{}}"#,
        "\n\n",
    ))
    .await;
    assert_eq!(thinking(&streamed), "First the year");

    // And the other vendor's shape of the same field, which arrives as
    // blocks rather than as strings, is read too — a summary that closed a
    // turn nobody asked a summary of is thinking either way.
    let blocked = events(concat!(
        r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","#,
        r#""encrypted_content":"sealed","summary":[{"type":"summary_text","text":"Blocked."}]}}"#,
        "\n\n",
        r#"data: {"type":"response.completed","response":{}}"#,
        "\n\n",
    ))
    .await;
    assert_eq!(thinking(&blocked), "Blocked.");
}

/// An item with nothing to replay produces no part at all: there is no
/// sending it back, and a row that can never do anything on every turn is
/// worse than none (deviation:
/// a-reasoning-item-without-state-is-not-recorded).
#[tokio::test]
async fn a_reasoning_item_that_was_never_sealed_leaves_no_trace() {
    for (item, transcript) in [
        (
            "a null state",
            concat!(
                r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","encrypted_content":null}}"#,
                "\n\n",
                r#"data: {"type":"response.completed","response":{}}"#,
                "\n\n",
            ),
        ),
        (
            "no state field at all",
            concat!(
                r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1"}}"#,
                "\n\n",
                r#"data: {"type":"response.completed","response":{}}"#,
                "\n\n",
            ),
        ),
        (
            "an empty state",
            concat!(
                r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","encrypted_content":""}}"#,
                "\n\n",
                r#"data: {"type":"response.completed","response":{}}"#,
                "\n\n",
            ),
        ),
        (
            // No id is no item: upstream requires a non-empty one
            // (`openai-responses.ts:572-573`).
            "state on an item with no id",
            concat!(
                r#"data: {"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"sealed-state"}}"#,
                "\n\n",
                r#"data: {"type":"response.completed","response":{}}"#,
                "\n\n",
            ),
        ),
    ] {
        let seen = events(transcript).await;

        assert!(
            !seen.iter().any(|event| matches!(event, ProviderEvent::ReasoningState { .. })),
            "{item} has no state to replay: {seen:?}"
        );
        assert!(
            seen.contains(&ProviderEvent::Finish(FinishReason::Completed)),
            "and the turn still ends normally: {seen:?}"
        );
    }
}

/// The backend serves a pinned list, and the first live ChatGPT turn met it
/// as `400 {"detail":"The 'gpt-5.6' model is not supported when using Codex
/// with a ChatGPT account."}`. Every arm of `codex.ts:281-292` that ganja
/// can express is here, in the order that makes it correct.
#[test]
fn the_backend_serves_a_pinned_list_and_the_order_of_the_rules_is_the_rule() {
    for served in ALLOWED_MODELS {
        assert!(serves(served), "codex.ts:15 names {served}");
    }
    // Three of those four are older than the floor, so a check that read
    // the generation rule first would refuse the models the list exists to
    // allow — including the one this build now defaults to.
    assert!(
        serves("gpt-5.4") && generation("gpt-5.4") == Some(5.4),
        "gpt-5.4 is not newer than 5.4 and is served anyway, which is what \
             makes the list order load-bearing"
    );

    for refused in ["gpt-5.6", "gpt-5.5-pro", "gpt-5.4-nano", "gpt-5.3-codex"] {
        assert!(!serves(refused), "{refused} is refused by the backend and has to be refused here");
    }

    // The forward hedge: a row the catalog gains later is reachable without
    // a code change, and anything that is not a `gpt-N.M` at all is not.
    assert!(serves("gpt-5.7") && serves("gpt-6.0-codex"));
    assert!(!serves("gpt-5") && !serves("o3") && !serves("claude-sonnet-5"));
    assert_eq!(generation("gpt-5.4-mini"), Some(5.4), "the halves are the id's");
    assert_eq!(generation("gpt-5"), None, "the fraction is required");
}

/// A refusal that only says no leaves a person guessing at a list they
/// cannot see. This one is what they read instead of the backend's JSON.
#[tokio::test]
async fn an_unsupported_model_is_refused_here_naming_what_the_seat_does_serve() {
    // The success arm is a boxed stream with no `Debug`, so `expect_err`
    // would not compile; the match is the same assertion said a way that
    // does, as elsewhere in this suite.
    let Err(refused) = provider()
        .stream(ChatRequest { model: REFUSED.to_owned(), ..ask() }, CancellationToken::new())
        .await
    else {
        panic!("a model this backend will not serve is not a turn to take");
    };
    let said = refused.to_string();

    assert!(said.contains(REFUSED), "it has to name what was asked for: {said}");
    for served in ALLOWED_MODELS {
        assert!(said.contains(served), "the served set is the part that is actionable: {said}");
    }
    assert!(
        said.contains(openai::API_KEY_ENV),
        "the other way out is a key, which reaches models a seat cannot: {said}"
    );
    // Refused before the credential is read at all: this provider has no
    // store behind it and would have panicked renewing one.
    assert!(
        matches!(refused, ProviderError::Transport(_)),
        "a request the provider declines to make, the way a bad base URL \
             is one: {refused:?}"
    );
}

#[test]
fn a_refused_credential_says_which_login_repairs_it() {
    for status in [401, 403] {
        let named = reauth(
            Backend::Codex,
            ProviderError::Status { status, message: "invalid token".to_owned() },
        );

        assert!(matches!(named, ProviderError::Auth(_)), "{named:?}");
        assert!(
            format!("{named}").contains("ganja auth login openai"),
            "the message is what a status bar shows: {named}"
        );
        assert!(
            !named.is_retryable(),
            "retrying a refused token is a storm against an identity provider"
        );

        // The same status against the platform is an API key being
        // refused, and `ganja auth login` does not mint one: sending
        // somebody through a browser flow would store a credential their
        // session cannot even reach while the key is exported.
        let keyed = reauth(
            Backend::Platform,
            ProviderError::Status { status, message: "invalid token".to_owned() },
        );
        assert!(
            matches!(keyed, ProviderError::Status { .. }),
            "the endpoint's own message is the honest one here: {keyed:?}"
        );
    }

    // Everything else is left as it was: a rate limit is not a login.
    let limited = reauth(
        Backend::Codex,
        ProviderError::Status { status: 429, message: "slow down".to_owned() },
    );
    assert!(
        matches!(limited, ProviderError::Status { status: 429, .. }) && limited.is_retryable(),
        "{limited:?}"
    );
}

#[test]
fn the_system_prompt_travels_as_instructions_and_the_turn_as_items() {
    let mut empty = Message::assistant("gpt");
    empty.parts.push(Part::text(""));

    let request = ChatRequest {
        effort_options: Default::default(),
        model: "gpt-test".to_owned(),
        system: Some("be brief".to_owned()),
        messages: vec![Message::user("hello"), empty, Message::user("again")],
        tools: Vec::new(),
    };

    assert_eq!(
        serde_json::to_value(Body::new(&request, Backend::Platform)).expect("the body serializes"),
        json!({
            "model": "gpt-test",
            "stream": true,
            // The backend answers a body without this
            // `400 {"detail":"Store must be set to false"}`.
            "store": false,
            "instructions": "be brief",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "hello"}]},
                {"role": "user", "content": [{"type": "input_text", "text": "again"}]},
            ],
        })
    );
}

/// The live field failure the alias exists for: a plugin-contributed MCP
/// server arrives namespaced `plugin:<name>:<server>` (**D473**), so its
/// tools are named like this — 69 characters, with colons besides, which
/// `meta/muse-spark-1.2` over openrouter refused as
/// ``\`name\` must be at most 64 characters, got 69``.
const REFUSED_NAME: &str = "mcp__plugin:mcp-gemini-search:mcp-gemini-search__deep_research_result";

/// [`a_tool`] under the name that got a live turn killed.
fn a_refused_tool() -> ToolDefinition {
    ToolDefinition { name: REFUSED_NAME.to_owned(), ..a_tool() }
}

/// Whether `name` is one this API's `^[a-zA-Z0-9_-]{1,64}$` accepts.
fn conforms(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= OPENAI_CAP
        && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

#[test]
fn a_tool_name_this_api_refuses_is_advertised_under_a_conforming_alias() {
    let request = ChatRequest {
        effort_options: Default::default(),
        model: "gpt-test".to_owned(),
        system: None,
        messages: vec![Message::user("research it")],
        tools: vec![a_refused_tool()],
    };

    let body = serde_json::to_value(Body::new(&request, Backend::OpenRouter))
        .expect("the body serializes");
    let advertised = body["tools"][0]["name"].as_str().expect("the tool is advertised");

    assert_ne!(advertised, REFUSED_NAME, "the refused name must not go out again");
    assert!(conforms(advertised), "{advertised} is still refusable");
}

/// The other half of the same seam. What the engine executes, what the
/// permission rules match and what the transcript records is the registry
/// name, so an alias the model calls back has to be undone before the
/// event leaves the wire.
#[test]
fn a_call_answering_with_the_alias_comes_back_out_under_the_registry_name() {
    let tools = vec![a_refused_tool()];
    let advertised = alias(REFUSED_NAME, OPENAI_CAP).into_owned();
    let mut mapping = Mapping::for_backend(Backend::OpenRouter, Aliases::of(&tools, OPENAI_CAP));
    let mut seen = Vec::new();

    mapping.frame(
        &Frame {
            event: None,
            data: json!({
                "type": "response.output_item.added",
                "item": {
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_1",
                    "name": advertised,
                },
            })
            .to_string(),
        },
        &mut seen,
    );

    assert_eq!(
        seen,
        vec![ProviderEvent::ToolCallStart {
            id: "call_1".to_owned(),
            name: REFUSED_NAME.to_owned(),
        }],
        "got {seen:?}"
    );
}

/// A call replayed on a later request has to name what that request's own
/// roster named, or the model is handed a trace citing a tool it was never
/// offered. Aliasing is deterministic, so both sides recompute it rather
/// than remembering anything across turns.
#[test]
fn a_completed_call_replays_under_the_same_alias_the_roster_advertises() {
    let mut assistant = Message::assistant("gpt-test");
    assistant.parts.push(tool_part(
        "call_1",
        REFUSED_NAME,
        completed(json!({"filePath": "src/main.rs"}), "a report"),
    ));

    let request = ChatRequest {
        effort_options: Default::default(),
        model: "gpt-test".to_owned(),
        system: None,
        messages: vec![Message::user("research it"), assistant],
        tools: vec![a_refused_tool()],
    };

    let body = serde_json::to_value(Body::new(&request, Backend::OpenRouter))
        .expect("the body serializes");
    let advertised = &body["tools"][0]["name"];
    let called = body["input"]
        .as_array()
        .expect("input is a list")
        .iter()
        .find(|item| item["type"] == "function_call")
        .expect("the replayed call is there");

    assert!(conforms(advertised.as_str().expect("a name")), "got {advertised}");
    assert_eq!(
        called["name"], *advertised,
        "the replayed call has to name exactly what the roster named: {body}"
    );
}

#[test]
fn a_request_advertises_the_tools_it_was_given() {
    let request = ChatRequest {
        effort_options: Default::default(),
        model: "gpt-test".to_owned(),
        system: None,
        messages: vec![Message::user("read src/main.rs")],
        tools: vec![a_tool()],
    };

    let body =
        serde_json::to_value(Body::new(&request, Backend::Platform)).expect("the body serializes");

    assert_eq!(
        body["tools"],
        json!([{
            // Flatter than chat completions', which nests these under
            // `function` — a tool advertised in the sibling's shape is one
            // this API ignores, so the model would be offered nothing.
            "type": "function",
            "name": "read",
            "description": "Reads a file from disk.",
            "parameters": {
                "type": "object",
                "properties": {"filePath": {"type": "string"}},
                "required": ["filePath"],
            },
        }]),
        "got {body}"
    );
}

/// Every tool-calling shape OpenRouter's reference publishes, held against
/// what this encoder sends (`api_reference/responses/tool-calling`, read
/// 2026-08-14).
///
/// Two fields in that reference are worth naming for what ganja does *not*
/// do with them:
///
/// - **`strict: null`** rides every tool definition it prints. `null` is
///   that field's absent value — the reference never sets it to a boolean
///   and documents no behavior for one — and ganja's schemas are generated
///   from the argument structs rather than written to the strict subset, so
///   a `true` would be a promise the roster cannot keep and a `null` is the
///   request that is already being sent. Omitted, deliberately.
/// - **`tool_choice: "auto"`** rides every one of its tool examples, and
///   *that* one ganja now sends — on this backend only, because the value
///   the API assumes in its absence is the one thing the reference does not
///   say, and the failure it would cause is a roster nothing ever calls.
#[test]
fn an_openrouter_tool_roster_is_the_shape_that_vendors_reference_documents() {
    let request = ChatRequest { tools: vec![a_tool()], ..gateway_ask() };
    let body = serde_json::to_value(Body::new(&request, Backend::OpenRouter))
        .expect("the body serializes");

    assert_eq!(
        body["tools"],
        json!([{
            "type": "function",
            "name": "read",
            "description": "Reads a file from disk.",
            "parameters": {
                "type": "object",
                "properties": {"filePath": {"type": "string"}},
                "required": ["filePath"],
            },
        }]),
        "the reference's own flat shape, minus the `strict: null` it prints: {body}"
    );
    assert!(
        body["tools"][0].get("strict").is_none(),
        "a null-valued field is an absent field, and a true one is a promise \
             a generated schema cannot keep: {body}"
    );
    assert_eq!(
        body["tool_choice"],
        json!("auto"),
        "the value every tool example in that reference sends: {body}"
    );

    // A turn with nothing to offer sends neither key: `tool_choice` beside
    // an absent roster is a choice about nothing.
    let bare = serde_json::to_value(Body::new(&gateway_ask(), Backend::OpenRouter))
        .expect("the body serializes");
    assert!(bare.get("tools").is_none() && bare.get("tool_choice").is_none(), "got {bare}");

    // And the two OpenAI backends are untouched: their request is the Codex
    // CLI's, which sends no `tool_choice` and has been served without one on
    // every turn this build has taken.
    for backend in [Backend::Codex, Backend::Platform] {
        let owned = serde_json::to_value(Body::new(
            &ChatRequest { tools: vec![a_tool()], ..ask() },
            backend,
        ))
        .expect("the body serializes");
        assert!(
            owned.get("tool_choice").is_none(),
            "{backend:?} gained a field its vendor never asked for: {owned}"
        );
    }
}

/// The gateway's own tools ride the same array, after the ones this build
/// runs (**D489**).
///
/// Two shapes in one list is the whole of what the reference's combined
/// example shows, and the absences are the opt-in: a provider nobody
/// configured sends none of these, and a request that carries them still
/// carries every function tool it had.
#[test]
fn a_configured_gateway_asks_for_its_own_tools_beside_this_builds() {
    let request = ChatRequest { tools: vec![a_tool()], ..gateway_ask() };

    let asked = Body::new(&request, Backend::OpenRouter)
        .serving(&["web_search".to_owned(), "datetime".to_owned()]);
    let body = serde_json::to_value(asked).expect("the body serializes");

    assert_eq!(
        body["tools"],
        json!([
            {
                "type": "function",
                "name": "read",
                "description": "Reads a file from disk.",
                "parameters": {
                    "type": "object",
                    "properties": {"filePath": {"type": "string"}},
                    "required": ["filePath"],
                },
            },
            {"type": "openrouter:web_search"},
            {"type": "openrouter:datetime"},
        ]),
        "the reference's own row shape, and this build's tools still first: {body}"
    );

    // Nothing configured, nothing asked for — on a request that is
    // otherwise identical, so what this proves is the config and not the
    // fixture.
    let unasked = serde_json::to_value(Body::new(&request, Backend::OpenRouter))
        .expect("the body serializes");
    assert_eq!(
        unasked["tools"].as_array().map(Vec::len),
        Some(1),
        "server tools bill per call, so a session that named none sends \
             none: {unasked}"
    );

    // And a session with no tools of its own still gets the gateway's.
    let alone = serde_json::to_value(
        Body::new(&gateway_ask(), Backend::OpenRouter).serving(&["shell".to_owned()]),
    )
    .expect("the body serializes");
    assert_eq!(alone["tools"], json!([{"type": "openrouter:shell"}]));
}

/// The other end of the same key: what a provider does with a roster is
/// decided by which backend it was built for, because these names are one
/// vendor's namespace.
#[test]
fn only_the_gateway_that_serves_server_tools_is_given_any() {
    let routed = routed().serving(vec!["web_search".to_owned()]);
    let body = serde_json::to_value(
        Body::new(&gateway_ask(), Backend::OpenRouter).serving(&["web_search".to_owned()]),
    )
    .expect("the body serializes");
    assert_eq!(body["tools"], json!([{"type": "openrouter:web_search"}]));
    // The provider kept them, which is what `request` will splice.
    assert_eq!(routed.server_tools, ["web_search"]);

    let elsewhere = keyed().serving(vec!["web_search".to_owned()]);
    assert!(
        elsewhere.server_tools.is_empty(),
        "one vendor's tool namespace must not reach another vendor's request"
    );
}

/// The roster is the reference's, and the config key is validated against
/// it — so the two halves of the opt-in cannot disagree.
#[test]
fn the_server_tool_roster_is_the_one_that_vendor_publishes() {
    assert_eq!(
        openrouter::SERVER_TOOLS,
        [
            "web_search",
            "datetime",
            "image_generation",
            "web_fetch",
            "apply_patch",
            "shell",
            "fusion",
            "advisor",
            "subagent",
            "experimental__search_models",
        ],
        "the roster table of `docs/guides/features/server-tools`, read 2026-08-14"
    );
    assert!(openrouter::serves_server_tool("web_search"));
    assert!(
        !openrouter::serves_server_tool("openrouter:web_search"),
        "a config names the half after the colon; the prefix is the wire's"
    );
}

/// The round trip the reference's multi-turn example shows: a
/// `function_call` item and a `function_call_output` that quotes its
/// `call_id`.
///
/// The reference's own note is what this pins — "Only `type`, `call_id`, and
/// `output` are required — `call_id` is what pairs the output with its
/// originating function_call" — so the optional `id` is absent here by
/// agreement rather than by omission.
#[test]
fn a_gateway_turn_replays_a_call_and_its_output_in_the_documented_pair() {
    let mut assistant = Message::assistant(GATEWAY_MODEL);
    assistant.parts.push(Part { id: PartId::ascending(), body: PartBody::StepStart });
    assistant.parts.push(tool_part(
        "call_xyz789",
        "read",
        completed(json!({"filePath": "src/main.rs"}), "fn main() {}"),
    ));

    let request = ChatRequest {
        messages: vec![Message::user("read src/main.rs"), assistant],
        tools: vec![a_tool()],
        ..gateway_ask()
    };
    let body = serde_json::to_value(Body::new(&request, Backend::OpenRouter))
        .expect("the body serializes");

    assert_eq!(
        body["input"],
        json!([
            {"role": "user", "content": [{"type": "input_text", "text": "read src/main.rs"}]},
            {
                "type": "function_call",
                "call_id": "call_xyz789",
                "name": "read",
                "arguments": r#"{"filePath":"src/main.rs"}"#,
            },
            {
                "type": "function_call_output",
                "call_id": "call_xyz789",
                "output": "fn main() {}",
            },
        ]),
        "got {body}"
    );
}

/// The streaming sequence that reference prints, frame for frame — including
/// the `response.function_call_arguments.done` its own example watches for
/// the finished arguments.
///
/// **Ganja terminates a call on `response.output_item.done`** and treats
/// that `.done` frame as the announcement it is: the arguments were already
/// accumulated from the deltas, and reading them again there would send the
/// model's JSON twice. The two frames arriving together must therefore
/// produce exactly one of each event, which is what this holds. If a live
/// turn ever shows this gateway ending a call *without* an
/// `output_item.done`, the fix is one arm here — recorded in
/// [`super::openrouter`]'s ledger rather than guessed at now.
#[tokio::test]
async fn the_references_own_streaming_tool_sequence_opens_fills_and_closes_once() {
    let seen = gateway_events(concat!(
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"#,
        r#""type":"function_call","id":"fc_abc123","call_id":"call_xyz789","#,
        r#""name":"read","arguments":""}}"#,
        "\n\n",
        r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_abc123","#,
        r#""delta":"{\"filePath\":\"src/main.rs\"}"}"#,
        "\n\n",
        r#"data: {"type":"response.function_call_arguments.done","item_id":"fc_abc123","#,
        r#""arguments":"{\"filePath\":\"src/main.rs\"}"}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"#,
        r#""type":"function_call","id":"fc_abc123","call_id":"call_xyz789","#,
        r#""name":"read","arguments":"{\"filePath\":\"src/main.rs\"}"}}"#,
        "\n\n",
        r#"data: {"type":"response.completed","response":{"usage":{"#,
        r#""input_tokens":45,"output_tokens":25}}}"#,
        "\n\n",
    ))
    .await;

    assert_eq!(
        seen.iter().filter(|event| !matches!(event, ProviderEvent::Usage(_))).collect::<Vec<_>>(),
        vec![
            &ProviderEvent::ToolCallStart { id: "call_xyz789".to_owned(), name: "read".to_owned() },
            &ProviderEvent::ToolCallDelta {
                id: "call_xyz789".to_owned(),
                json: r#"{"filePath":"src/main.rs"}"#.to_owned(),
            },
            &ProviderEvent::ToolCallEnd { id: "call_xyz789".to_owned() },
            &ProviderEvent::Finish(FinishReason::Completed),
        ],
        "got {seen:?}"
    );
}

/// A tool the gateway ran arrives as a row to render and **nothing to run**
/// (**D489**).
///
/// The three absences are the whole rule, and each of them is a way the
/// turn would break: a `ToolCallStart` would send the loop looking for a
/// tool no registry has, the dialog that call would raise would ask about
/// work already done, and a `Failed` would end a turn the gateway
/// completed.
#[tokio::test]
async fn a_gateway_run_tool_becomes_a_row_and_never_a_call_to_execute() {
    let seen = gateway_events(concat!(
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"#,
        r#""type":"openrouter:web_search","id":"or_1","status":"in_progress"}}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"#,
        r#""type":"openrouter:web_search","id":"or_1","status":"completed","#,
        r#""arguments":"{\"query\":\"rust 2024 edition\"}","#,
        r#""output":"3 results"}}"#,
        "\n\n",
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"It ships."}"#,
        "\n\n",
        r#"data: {"type":"response.completed","response":{}}"#,
        "\n\n",
    ))
    .await;

    assert!(
        seen.contains(&ProviderEvent::ServerTool {
            tool: "openrouter:web_search".to_owned(),
            input: json!({"query": "rust 2024 edition"}),
            output: "3 results".to_owned(),
        }),
        "the row has to carry the call and its answer: {seen:?}"
    );
    assert!(
        !seen.iter().any(|event| matches!(
            event,
            ProviderEvent::ToolCallStart { .. }
                | ProviderEvent::ToolCallDelta { .. }
                | ProviderEvent::ToolCallEnd { .. }
                | ProviderEvent::Failed(_)
        )),
        "nothing here is this build's to run, ask about, or die over: {seen:?}"
    );
    assert_eq!(
        text(&seen),
        "It ships.",
        "and the reply the gateway's own tool fed goes on arriving"
    );
    // Once: the opening frame announces structure, exactly as a reasoning
    // item's does, and a row minted there would be a claim about nothing.
    assert_eq!(
        seen.iter().filter(|event| matches!(event, ProviderEvent::ServerTool { .. })).count(),
        1,
        "{seen:?}"
    );
}

/// What the decode reads when the item is not shaped like the one tool the
/// reference spells out — which is nine of the ten, since the vendor
/// documents a different argument shape per tool.
#[tokio::test]
async fn a_gateway_tools_own_fields_are_shown_rather_than_guessed_at() {
    // `openrouter:shell`'s arguments mirror OpenAI's `shell_call.action`,
    // so they arrive under no `arguments` key at all.
    let seen = gateway_events(concat!(
        r#"data: {"type":"response.output_item.done","item":{"#,
        r#""type":"openrouter:shell","id":"or_2","status":"completed","#,
        r#""action":{"commands":["ls -la"]},"#,
        r#""output":[{"stdout":"total 0","stderr":"","outcome":{"type":"exit","exit_code":0}}]}}"#,
        "\n\n",
        r#"data: {"type":"response.completed","response":{}}"#,
        "\n\n",
    ))
    .await;

    assert!(
        seen.contains(&ProviderEvent::ServerTool {
            tool: "openrouter:shell".to_owned(),
            // The item minus its envelope, which is what arrived rather
            // than a shape assumed for it — and `output` is not in it,
            // because the row shows that separately.
            input: json!({"action": {"commands": ["ls -la"]}}),
            output: r#"[{"outcome":{"exit_code":0,"type":"exit"},"stderr":"","stdout":"total 0"}]"#
                .to_owned(),
        }),
        "got {seen:?}"
    );

    // An item this build has no idea about still ends nothing: the
    // skip-with-debug arm is the safety net a beta surface needs.
    let unknown = gateway_events(concat!(
        r#"data: {"type":"response.output_item.done","item":{"#,
        r#""type":"some_future_call","id":"x_1","payload":{"a":1}}}"#,
        "\n\n",
        r#"data: {"type":"response.some.future.event","item_id":"x_1"}"#,
        "\n\n",
        r#"data: {"type":"response.completed","response":{}}"#,
        "\n\n",
    ))
    .await;
    assert_eq!(
        unknown,
        vec![
            ProviderEvent::Usage(Usage::default()),
            ProviderEvent::Finish(FinishReason::Completed),
        ],
        "an unrecognized item is skipped, and the turn still completes: {unknown:?}"
    );
}

/// Parallel calls, which that reference has a section of its own for: two
/// items open in one response, their argument fragments interleave, and each
/// is answered under its own `call_id`.
#[tokio::test]
async fn parallel_calls_in_one_response_keep_their_own_identities() {
    let seen = gateway_events(concat!(
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"#,
            r#""type":"function_call","id":"fc_1","call_id":"call_read","name":"read","arguments":""}}"#,
            "\n\n",
            r#"data: {"type":"response.output_item.added","output_index":1,"item":{"#,
            r#""type":"function_call","id":"fc_2","call_id":"call_glob","name":"glob","arguments":""}}"#,
            "\n\n",
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_2","delta":"{\"pattern\":"}"#,
            "\n\n",
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"filePath\":\"a.rs\"}"}"#,
            "\n\n",
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_2","delta":"\"**/*.rs\"}"}"#,
            "\n\n",
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"#,
            r#""type":"function_call","id":"fc_1","call_id":"call_read","name":"read","arguments":"{}"}}"#,
            "\n\n",
            r#"data: {"type":"response.output_item.done","output_index":1,"item":{"#,
            r#""type":"function_call","id":"fc_2","call_id":"call_glob","name":"glob","arguments":"{}"}}"#,
            "\n\n",
            r#"data: {"type":"response.completed","response":{}}"#,
            "\n\n",
        ))
        .await;

    let arguments = |call: &str| {
        seen.iter()
            .filter_map(|event| match event {
                ProviderEvent::ToolCallDelta { id, json } if id == call => Some(json.as_str()),
                _ => None,
            })
            .collect::<String>()
    };

    assert_eq!(arguments("call_read"), r#"{"filePath":"a.rs"}"#);
    assert_eq!(
        arguments("call_glob"),
        r#"{"pattern":"**/*.rs"}"#,
        "a fragment keyed by the *item* id has to reach the call that item \
             opened, or two concurrent calls trade arguments: {seen:?}"
    );
    assert_eq!(
        seen.iter().filter(|event| matches!(event, ProviderEvent::ToolCallEnd { .. })).count(),
        2,
        "each call ends once and on its own frame: {seen:?}"
    );
}

/// A request offering `read`, which is what a session with a registry sends
/// on every turn.
fn a_tool() -> ToolDefinition {
    ToolDefinition {
        name: "read".to_owned(),
        description: "Reads a file from disk.".to_owned(),
        schema: json!({
            "type": "object",
            "properties": {"filePath": {"type": "string"}},
            "required": ["filePath"],
        }),
    }
}

/// A tool part carrying `state`, as an assistant message holds one.
fn tool_part(call_id: &str, tool: &str, state: ToolState) -> Part {
    Part {
        id: PartId::ascending(),
        body: PartBody::Tool { call_id: call_id.to_owned(), tool: tool.to_owned(), state },
    }
}

/// A call that ran and what it produced.
fn completed(input: serde_json::Value, output: &str) -> ToolState {
    ToolState::Completed {
        input,
        output: output.to_owned(),
        title: "src/main.rs".to_owned(),
        metadata: json!({}),
        started: 1,
        completed: 2,
    }
}

/// A turn that called tools reads back as items: what the model said, the
/// calls beside it, then every result — each its own entry, because this
/// API has no message that holds a call.
#[test]
fn a_finished_call_is_sent_back_as_a_call_item_and_an_output_item() {
    let mut assistant = Message::assistant("gpt-test");
    assistant.parts.push(Part { id: PartId::ascending(), body: PartBody::StepStart });
    assistant.parts.push(Part::text("Reading the file first."));
    assistant.parts.push(tool_part(
        "call_read",
        "read",
        completed(json!({"filePath": "src/main.rs"}), "fn main() {}"),
    ));
    assistant.parts.push(tool_part(
        "call_glob",
        "glob",
        ToolState::Error {
            input: json!({"pattern": "**/*.rs"}),
            error: "no such directory".to_owned(),
            started: 3,
            completed: 4,
        },
    ));

    let request = ChatRequest {
        effort_options: Default::default(),
        model: "gpt-test".to_owned(),
        system: None,
        messages: vec![Message::user("read src/main.rs"), assistant, Message::user("thanks")],
        tools: vec![a_tool()],
    };

    let body =
        serde_json::to_value(Body::new(&request, Backend::Platform)).expect("the body serializes");

    assert_eq!(
        body["input"],
        json!([
            {"role": "user", "content": [{"type": "input_text", "text": "read src/main.rs"}]},
            {
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Reading the file first."}],
            },
            {
                "type": "function_call",
                "call_id": "call_read",
                "name": "read",
                // Arguments travel as a string here too: the model streams
                // them as text and the API carries them as it got them.
                "arguments": r#"{"filePath":"src/main.rs"}"#,
            },
            {
                "type": "function_call",
                "call_id": "call_glob",
                "name": "glob",
                "arguments": r#"{"pattern":"**/*.rs"}"#,
            },
            {"type": "function_call_output", "call_id": "call_read", "output": "fn main() {}"},
            // A failure has nowhere to be flagged here either, so it
            // travels as the text the model reads.
            {
                "type": "function_call_output",
                "call_id": "call_glob",
                "output": "no such directory",
            },
            {"role": "user", "content": [{"type": "input_text", "text": "thanks"}]},
        ]),
        "got {body}"
    );
}

/// A turn that took two model requests reads back as two of them. The API
/// would accept one flattened run, but it would put everything the model
/// said ahead of every result, so a model re-reading its own trace would
/// find its reasoning shuffled out from under the evidence it reasoned
/// from.
#[test]
fn a_two_step_turn_is_sent_back_one_group_per_step() {
    let mut assistant = Message::assistant("gpt-test");

    for (text, call_id, tool, input, output) in [
        (
            "Reading.",
            "call_read",
            "read",
            json!({"filePath": "src/main.rs"}),
            "fn main() { let x = 1; }",
        ),
        (
            "Now editing.",
            "call_edit",
            "edit",
            json!({"filePath": "src/main.rs", "oldString": "1", "newString": "2"}),
            "1 replacement",
        ),
    ] {
        assistant.parts.push(Part { id: PartId::ascending(), body: PartBody::StepStart });
        assistant.parts.push(Part::text(text));
        assistant.parts.push(tool_part(call_id, tool, completed(input, output)));
    }

    let request = ChatRequest {
        effort_options: Default::default(),
        model: "gpt-test".to_owned(),
        system: None,
        messages: vec![Message::user("fix the bug"), assistant],
        tools: vec![a_tool()],
    };

    let body =
        serde_json::to_value(Body::new(&request, Backend::Platform)).expect("the body serializes");
    let wire = body["input"].to_string();
    let position = |needle: &str| wire.find(needle).expect("the wire holds it");

    assert!(
        position("Now editing.") > position("fn main() { let x = 1; }"),
        "what the model said in the second step must read as having been \
             said after the first step's result came back: {wire}"
    );
}

/// A turn cancelled while a tool was running leaves a call nobody answered,
/// and this API pairs a call with its output by `call_id`: a call with no
/// output is one the model is still waiting on. Dropping the call instead
/// would leave the reply talking about one that is not there.
#[test]
fn a_call_that_never_finished_is_answered_rather_than_left_dangling() {
    let mut assistant = Message::assistant("gpt-test");
    assistant.parts.push(tool_part("call_read", "read", ToolState::Pending { input: None }));

    let request = ChatRequest {
        effort_options: Default::default(),
        model: "gpt-test".to_owned(),
        system: None,
        messages: vec![Message::user("read src/main.rs"), assistant],
        tools: Vec::new(),
    };

    let body =
        serde_json::to_value(Body::new(&request, Backend::Platform)).expect("the body serializes");

    assert_eq!(
        body["input"][2],
        json!({
            "type": "function_call_output",
            "call_id": "call_read",
            // One spelling across both wires: the sibling's, imported.
            "output": NO_RESULT,
        }),
        "an unanswered call must not reach the API unanswered: {body}"
    );
}

/// The happy path: text, a summarized thought, and the bill the terminal
/// frame carries.
#[tokio::test]
async fn a_happy_path_transcript_maps_to_text_reasoning_and_a_bill() {
    let seen = events(concat!(
        "event: response.created\n",
        r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5.6"}}"#,
        "\n\n",
        "event: response.output_item.added\n",
        r#"data: {"type":"response.output_item.added","output_index":0,"#,
        r#""item":{"type":"reasoning","id":"rs_1"}}"#,
        "\n\n",
        "event: response.reasoning_summary_text.delta\n",
        r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","#,
        r#""summary_index":0,"delta":"A greeting is enough."}"#,
        "\n\n",
        "event: response.output_item.added\n",
        r#"data: {"type":"response.output_item.added","output_index":1,"#,
        r#""item":{"type":"message","id":"msg_1"}}"#,
        "\n\n",
        "event: response.output_text.delta\n",
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"Hello, "}"#,
        "\n\n",
        "event: response.output_text.delta\n",
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"world!"}"#,
        "\n\n",
        "event: response.completed\n",
        r#"data: {"type":"response.completed","response":{"usage":{"#,
        r#""input_tokens":42,"input_tokens_details":{"cached_tokens":16},"#,
        r#""output_tokens":9,"output_tokens_details":{"reasoning_tokens":4}}}}"#,
        "\n\n",
    ))
    .await;

    assert_eq!(text(&seen), "Hello, world!");
    assert!(
        seen.contains(&ProviderEvent::ReasoningDelta("A greeting is enough.".to_owned())),
        "a summarized thought should not be dropped, got {seen:?}"
    );
    assert_eq!(
        &seen[seen.len() - 2..],
        &[
            ProviderEvent::Usage(Usage {
                // 42 prompt tokens of which the cache served 16: 26 fresh.
                input_tokens: 26,
                output_tokens: 9,
                reasoning_tokens: 4,
                cache_read_tokens: 16,
                cache_write_tokens: 0,
            }),
            ProviderEvent::Finish(FinishReason::Completed),
        ],
        "got {seen:?}"
    );
}

/// This API reports the whole prompt as `input_tokens` and then says how
/// much of it the cache served and wrote; [`Usage`] keeps the three apart so
/// each can be billed at its own rate, a cache read costing a fraction of
/// fresh input. Handing every count through unchanged bills the cached part
/// twice.
#[tokio::test]
async fn a_cached_prompt_reports_only_its_fresh_half_as_input() {
    let cases = [
        (
            "a cached prompt bills only what the cache did not serve",
            concat!(
                r#"data: {"type":"response.completed","response":{"usage":{"#,
                r#""input_tokens":1000,"input_tokens_details":{"cached_tokens":800},"#,
                r#""output_tokens":20}}}"#,
                "\n\n",
            ),
            Usage {
                input_tokens: 200,
                output_tokens: 20,
                cache_read_tokens: 800,
                ..Usage::default()
            },
        ),
        (
            "a written cache entry is not fresh input either",
            concat!(
                r#"data: {"type":"response.completed","response":{"usage":{"#,
                r#""input_tokens":1000,"input_tokens_details":{"cached_tokens":600,"#,
                r#""cache_write_tokens":300},"output_tokens":20}}}"#,
                "\n\n",
            ),
            Usage {
                input_tokens: 100,
                output_tokens: 20,
                cache_read_tokens: 600,
                cache_write_tokens: 300,
                ..Usage::default()
            },
        ),
        (
            "an endpoint claiming more cached tokens than prompt tokens reads as \
                 nothing fresh rather than wrapping into a bill nobody owes",
            concat!(
                r#"data: {"type":"response.completed","response":{"usage":{"#,
                r#""input_tokens":100,"input_tokens_details":{"cached_tokens":900},"#,
                r#""output_tokens":5}}}"#,
                "\n\n",
            ),
            Usage { input_tokens: 0, output_tokens: 5, cache_read_tokens: 900, ..Usage::default() },
        ),
        (
            "a prompt nothing was cached for is fresh in full, thinking included \
                 in the output rather than counted beside it",
            concat!(
                r#"data: {"type":"response.completed","response":{"usage":{"#,
                r#""input_tokens":1000,"output_tokens":120,"#,
                r#""output_tokens_details":{"reasoning_tokens":100}}}}"#,
                "\n\n",
            ),
            Usage {
                input_tokens: 1_000,
                output_tokens: 120,
                reasoning_tokens: 100,
                ..Usage::default()
            },
        ),
    ];

    for (name, transcript, expected) in cases {
        let seen = events(transcript).await;

        assert!(seen.contains(&ProviderEvent::Usage(expected)), "{name}: got {seen:?}");
    }
}

/// The bill the corrected counts actually produce. Priced apart, a heavily
/// cached prompt costs a fraction of what the same tokens would fresh —
/// which is exactly the difference double-counting erases.
#[test]
fn a_cached_prompt_is_billed_once_rather_than_twice() {
    let model = catalog::model("gpt-5.6").expect("the catalog knows the model");
    let corrected = Usage { input_tokens: 200_000, cache_read_tokens: 800_000, ..Usage::default() };
    let doubled = Usage { input_tokens: 1_000_000, ..corrected };

    assert!(
        catalog::cost(&doubled, &model).total_usd
            > catalog::cost(&corrected, &model).total_usd * 3.0,
        "the uncorrected counts over-report by more than a factor of three, \
             which is the size of the error this pins"
    );
}

/// A call arrives across three event types, and the id that correlates them
/// is not the id its result has to quote back.
#[tokio::test]
async fn tool_calls_are_opened_filled_and_closed() {
    let seen = events(concat!(
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","#,
        r#""delta":"Reading the file first."}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.added","output_index":1,"item":{"#,
        r#""type":"function_call","id":"fc_1","call_id":"call_read","name":"read","#,
        r#""arguments":""}}"#,
        "\n\n",
        r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","#,
        r#""output_index":1,"delta":"{\"file"}"#,
        "\n\n",
        r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","#,
        r#""output_index":1,"delta":"Path\":\"src/main.rs\"}"}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.done","output_index":1,"item":{"#,
        r#""type":"function_call","id":"fc_1","call_id":"call_read","name":"read","#,
        r#""arguments":"{\"filePath\":\"src/main.rs\"}"}}"#,
        "\n\n",
        r#"data: {"type":"response.completed","response":{"usage":{"#,
        r#""input_tokens":10,"output_tokens":5}}}"#,
        "\n\n",
    ))
    .await;

    assert_eq!(text(&seen), "Reading the file first.");
    assert_eq!(
        seen.iter()
            .filter(|event| !matches!(event, ProviderEvent::TextDelta(_) | ProviderEvent::Usage(_)))
            .collect::<Vec<_>>(),
        vec![
            // The `call_id`, never the item id the deltas were keyed by:
            // this is the string a `function_call_output` has to quote.
            &ProviderEvent::ToolCallStart { id: "call_read".to_owned(), name: "read".to_owned() },
            &ProviderEvent::ToolCallDelta {
                id: "call_read".to_owned(),
                json: "{\"file".to_owned(),
            },
            &ProviderEvent::ToolCallDelta {
                id: "call_read".to_owned(),
                json: "Path\":\"src/main.rs\"}".to_owned(),
            },
            // This API *does* terminate a call, unlike chat completions.
            &ProviderEvent::ToolCallEnd { id: "call_read".to_owned() },
            &ProviderEvent::Finish(FinishReason::Completed),
        ],
        "got {seen:?}"
    );
}

/// The SSE decoder must tolerate anything: this stream carries a dozen
/// event types this build has no use for, and several more the API has not
/// invented yet.
#[tokio::test]
async fn an_unmapped_event_is_skipped_rather_than_ending_the_turn() {
    let seen = events(concat!(
        r#"data: {"type":"response.in_progress","response":{"id":"resp_1"}}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"#,
        r#""type":"web_search_call","id":"ws_1","status":"in_progress"}}"#,
        "\n\n",
        r#"data: {"type":"response.something.nobody.has.written.yet","delta":"x"}"#,
        "\n\n",
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"hi"}"#,
        "\n\n",
        "data: [DONE]\n\n",
        r#"data: {"type":"response.completed","response":{"usage":{"#,
        r#""input_tokens":1,"output_tokens":1}}}"#,
        "\n\n",
    ))
    .await;

    assert_eq!(text(&seen), "hi");
    assert_eq!(
        seen.last(),
        Some(&ProviderEvent::Finish(FinishReason::Completed)),
        "an unknown event is a log line, and `[DONE]` is not a parse \
             failure: {seen:?}"
    );
}

#[tokio::test]
async fn a_body_that_stops_mid_reply_fails_rather_than_completing() {
    let seen = events(concat!(
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","#,
        r#""delta":"The connection drops right"}"#,
        "\n\n",
    ))
    .await;

    assert_eq!(text(&seen), "The connection drops right");
    assert!(
        matches!(seen.last(), Some(ProviderEvent::Failed(ProviderError::Transport(_)))),
        "a dropped connection must never read as a finished turn, got {seen:?}"
    );
}

#[tokio::test]
async fn a_malformed_chunk_ends_the_turn_rather_than_being_skipped() {
    let seen = events(concat!(
        r#"data: {"type":"response.output_text.delta","item_id":"m","delta":"Hello"}"#,
        "\n\n",
        "data: {\"type\": not json at all\n\n",
        r#"data: {"type":"response.output_text.delta","item_id":"m","delta":" there"}"#,
        "\n\n",
    ))
    .await;

    assert_eq!(text(&seen), "Hello", "text before the break is kept");
    assert_eq!(seen.len(), 2, "nothing after the broken chunk is read, got {seen:?}");
    assert!(
        matches!(seen.last(), Some(ProviderEvent::Failed(ProviderError::Parse(_)))),
        "got {seen:?}"
    );
}

/// This API has two ways of saying a turn broke after the status was
/// already 200, and neither may read as a model that finished talking.
#[tokio::test]
async fn a_failed_response_and_an_error_chunk_both_end_the_turn_as_failures() {
    for transcript in [
        concat!(
            r#"data: {"type":"response.output_text.delta","item_id":"m","delta":"partial"}"#,
            "\n\n",
            r#"data: {"type":"response.failed","sequence_number":9,"response":{"#,
            r#""error":{"code":"server_error","message":"upstream capacity exceeded"}}}"#,
            "\n\n",
        ),
        concat!(
            r#"data: {"type":"response.output_text.delta","item_id":"m","delta":"partial"}"#,
            "\n\n",
            r#"data: {"type":"error","sequence_number":9,"code":"server_error","#,
            r#""message":"upstream capacity exceeded"}"#,
            "\n\n",
        ),
    ] {
        let seen = events(transcript).await;

        assert_eq!(text(&seen), "partial");
        assert_eq!(
            seen.last(),
            Some(&ProviderEvent::Failed(ProviderError::Status {
                status: 500,
                message: "upstream capacity exceeded".to_owned(),
            })),
            "got {seen:?}"
        );
    }
}

#[tokio::test]
async fn a_cancel_mid_transcript_ends_the_stream_without_a_verdict() {
    let cancel = CancellationToken::new();
    let mut stream = Box::pin(replay(
        concat!(
            r#"data: {"type":"response.output_text.delta","item_id":"m","delta":"one"}"#,
            "\n\n",
            r#"data: {"type":"response.output_text.delta","item_id":"m","delta":"two"}"#,
            "\n\n",
        ),
        cancel.clone(),
        Mapping::default(),
    ));

    assert_eq!(stream.next().await, Some(ProviderEvent::TextDelta("one".to_owned())));
    cancel.cancel();

    let rest: Vec<ProviderEvent> = stream.collect().await;
    assert!(rest.is_empty(), "a cancelled stream ends: {rest:?}");
}

#[tokio::test]
async fn a_turn_that_stopped_early_still_reports_what_it_spent() {
    let seen = events(concat!(
        r#"data: {"type":"response.output_text.delta","item_id":"m","delta":"as far as"}"#,
        "\n\n",
        r#"data: {"type":"response.incomplete","response":{"#,
        r#""incomplete_details":{"reason":"max_output_tokens"},"#,
        r#""usage":{"input_tokens":10,"output_tokens":128}}}"#,
        "\n\n",
    ))
    .await;

    assert_eq!(text(&seen), "as far as");
    assert_eq!(
        &seen[seen.len() - 2..],
        &[
            ProviderEvent::Usage(Usage {
                input_tokens: 10,
                output_tokens: 128,
                ..Usage::default()
            }),
            // A reply that stopped at the output ceiling is still a reply,
            // and the loop has no verdict between "done" and "broke".
            ProviderEvent::Finish(FinishReason::Completed),
        ],
        "got {seen:?}"
    );
}
