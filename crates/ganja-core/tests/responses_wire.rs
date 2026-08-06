//! An OpenAI session becoming a turn, either credential, against a real socket.
//!
//! **The vendor picks the wire.** Everything filed under `openai` speaks the
//! Responses API — upstream's plugin routes every model of that vendor through
//! it without looking at the credential at all
//! (`plugin/provider/openai.ts:185`) — and what the credential picks is which
//! *backend* the request reaches and what it carries beside the bearer. Two
//! things depended on that: a stored ChatGPT login had no consumer at all until
//! the Responses provider landed, and an API key could not run tools on the
//! newest models, because chat completions refused them live and named
//! `/v1/responses` in the refusal.
//!
//! Told in phases, because nine different things have to be true at once and a
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
//! 5. **The dispatch.** A key wins over a stored login; a stored login serves
//!    where there is no key.
//! 6. **The model each wire defaults to.** A seat's backend serves a narrower
//!    set than the platform, so a subscription session that named no model
//!    takes the seat's default rather than the catalog's — and a model somebody
//!    *did* name is answered or refused, never substituted.
//! 7. **Neither credential** is the startup failure it has always been, with
//!    nothing on the wire.
//! 8. **Nothing leaks.** No token reaches a rendering, an error or the store's
//!    own `Debug`.
//! 9. **An unsupported model costs nothing** — and the seat's list does not
//!    reach the platform, which phase 3 already took a turn on.
//!
//! Everything serves real bytes over loopback rather than mocking the client,
//! the way every other provider suite here works: what is asserted on is the
//! request that was actually built.
//!
//! One test, one binary, on purpose: it mutates `XDG_DATA_HOME`,
//! `OPENAI_API_KEY`, `OPENAI_BASE_URL`, `GANJA_PROVIDER` and `GANJA_MODEL`, and
//! a plain `cargo test` runs the tests inside a binary on parallel threads.

use std::{
    env,
    sync::{Arc, Mutex},
};

use futures::StreamExt as _;
use ganja_core::{
    Engine,
    auth::{self, AuthError, OauthCredential, RefreshOauth},
    catalog,
    config::Config,
    permission::Permissions,
    protocol::{Command, Event, PartBody, Role},
    provider::{
        ChatRequest, Provider as _, ProviderError, ProviderEvent, ResponsesProvider, openai, select,
    },
    tool::Registry,
};
use ganja_testkit::{RecorderTool, drain};
use secrecy::SecretString;
use serde_json::json;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
};
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
/// window and a price to report — and one the ChatGPT backend actually serves
/// (`codex.ts:15`), which is a second requirement the live pass discovered the
/// hard way.
const SUBSCRIPTION_MODEL: &str = "gpt-5.4";

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
const SUBSCRIPTION_HEADERS: [&str; 4] = [
    "chatgpt-account-id",
    "originator",
    "openai-beta",
    "user-agent",
];

/// One request the endpoint was asked to serve.
#[derive(Clone)]
struct Recorded {
    /// Request line and headers, verbatim.
    head: String,
    /// The body, for a request that had one.
    body: String,
}

impl Recorded {
    /// The path asked for, which is what tells the two wires apart.
    fn path(&self) -> &str {
        self.head
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .split('?')
            .next()
            .unwrap_or_default()
    }

    /// The value of `name`, compared case-insensitively the way a header name
    /// is. [`None`] where the request did not carry it at all, which is a
    /// different answer from carrying it empty.
    fn header(&self, name: &str) -> Option<String> {
        let prefix = format!("{name}:");

        self.head.lines().find_map(|line| {
            let (found, value) = line.split_once(':')?;
            found
                .trim()
                .eq_ignore_ascii_case(prefix.trim_end_matches(':'))
                .then(|| value.trim().to_owned())
        })
    }

    /// The body as JSON, for the phases that assert on the whole request.
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("the body should be JSON ({error}): {}", self.body))
    }
}

/// Everything the server task and the test both hold.
struct State {
    seen: Mutex<Vec<Recorded>>,
    reply: Mutex<String>,
}

/// A loopback endpoint serving whatever the current phase set.
struct Endpoint {
    /// What a provider is pointed at.
    base_url: String,
    state: Arc<State>,
    /// Kept so the server outlives the test talking to it.
    _server: tokio::task::JoinHandle<()>,
}

impl Endpoint {
    /// Every request served so far, oldest first.
    fn seen(&self) -> Vec<Recorded> {
        self.state
            .seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The one request this phase produced.
    fn only(&self) -> Recorded {
        let seen = self.seen();
        let [request] = seen.as_slice() else {
            panic!("one turn is one request, got {}", seen.len());
        };

        request.clone()
    }

    /// Forgets what has been served, so a phase counts only its own traffic.
    fn forget(&self) {
        self.state
            .seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// Sets the event-stream body every turn is answered with from now on.
    fn answers_turns_with(&self, body: impl Into<String>) {
        *self
            .state
            .reply
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = body.into();
    }
}

/// Starts an endpoint that answers every connection for as long as the test
/// holds it.
async fn serve() -> Endpoint {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback is bindable");
    let address = listener
        .local_addr()
        .expect("a bound socket has an address");
    let state = Arc::new(State {
        seen: Mutex::new(Vec::new()),
        reply: Mutex::new(responses_transcript()),
    });

    let served = Arc::clone(&state);
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let state = Arc::clone(&served);

            tokio::spawn(async move {
                let Some(request) = read_request(&mut socket).await else {
                    return;
                };
                let body = state
                    .reply
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                state
                    .seen
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(request);

                let _ = socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nconnection: close\r\n\
                             content-type: text/event-stream\r\n\r\n{body}"
                        )
                        .as_bytes(),
                    )
                    .await;
                let _ = socket.flush().await;
                // Dropping the socket ends a close-delimited body.
            });
        }
    });

    Endpoint {
        base_url: format!("http://{address}/backend-api/codex"),
        state,
        _server: server,
    }
}

/// Reads one whole request: head to the blank line, then whatever
/// `content-length` promised.
async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<Recorded> {
    let mut buffer = Vec::new();
    let mut byte = [0_u8; 1];

    while !buffer.ends_with(b"\r\n\r\n") {
        match socket.read(&mut byte).await {
            Ok(0) | Err(_) => return None,
            Ok(_) => buffer.push(byte[0]),
        }
    }
    let head = String::from_utf8_lossy(&buffer).into_owned();

    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0_u8; length];
    if length > 0 && socket.read_exact(&mut body).await.is_err() {
        return None;
    }

    Some(Recorded {
        head,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// A whole Responses turn: a thought, two fragments of reply, and the bill.
fn responses_transcript() -> String {
    [
        r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5.6"}}"#,
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#,
        r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","summary_index":0,"delta":"Short is right."}"#,
        r#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_1"}}"#,
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"Hello, "}"#,
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"world!"}"#,
        r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":42,"input_tokens_details":{"cached_tokens":16},"output_tokens":9,"output_tokens_details":{"reasoning_tokens":4}}}}"#,
    ]
    .join("\n\n")
        + "\n\n"
}

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
    }
}

#[tokio::test]
async fn either_openai_credential_drives_a_responses_turn_against_the_backend_it_belongs_to() {
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
    .with_system(Some("be brief".to_owned()));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(prompt("say hello"))
        .await
        .expect("an idle engine accepts");
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
    assert_eq!(sent.header("originator").as_deref(), Some("opencode"));
    assert_eq!(
        sent.header("openai-beta").as_deref(),
        Some("responses=experimental")
    );
    assert_eq!(
        sent.header("user-agent").as_deref(),
        Some(auth::device::UPSTREAM_USER_AGENT),
        "one User-Agent for every request this build makes"
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
    assert!(
        body["include"].is_null(),
        "`reasoning.encrypted_content` is only worth asking for once a \
         transcript can hand it back, and no protocol part carries one: {body}"
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
        calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty(),
        "the transcript calls nothing, so nothing should have run"
    );

    // The turn as the frontend saw it: the reply text, and the bill with the
    // cached half taken back out of the prompt.
    let text: String = seen
        .iter()
        .filter_map(|event| match event {
            Event::PartDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        text, "Hello, world!",
        "the reply, and only the reply — the transcript also streams a \
         summarized thought, which no protocol part renders yet, so the engine \
         drops it rather than mixing it into the answer: got {seen:?}"
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
        seen.iter().any(
            |event| matches!(event, Event::MessageStarted { message, .. }
                if message.role == Role::Assistant)
        ),
        "the turn should have reached the event stream as a message: {seen:?}"
    );

    // ---- 2. The credential is read per request, not captured. -------------
    endpoint.forget();
    store(SECOND_ACCESS, SECOND_ACCOUNT);
    engine
        .send(prompt("again"))
        .await
        .expect("an idle engine accepts");
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
    assert_eq!(
        sent.header("authorization").as_deref(),
        Some(format!("Bearer {KEY}").as_str())
    );
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
    // A key outranks a stored login, exactly as `key_for` has always read the
    // two. Both are present here, which is the case that can only go one way —
    // and both wires now answer on the same path, so what tells them apart is
    // the bearer and the headers rather than the URL.
    endpoint.forget();
    endpoint.answers_turns_with(responses_transcript());
    // SAFETY: as above. Named rather than defaulted, because an unset
    // `GANJA_PROVIDER` is the fake provider and would prove nothing.
    unsafe {
        env::set_var("GANJA_PROVIDER", openai::ID);
    }
    let chosen = select(&Config::default()).expect("a key is a session");
    assert_eq!(chosen.provider.id(), openai::ID, "one vendor, either wire");
    turn(chosen.provider.as_ref(), KEY_MODEL).await;

    let sent = endpoint.only();
    assert_eq!(sent.path(), RESPONSES);
    assert_eq!(
        sent.header("authorization").as_deref(),
        Some(format!("Bearer {KEY}").as_str()),
        "a stored ChatGPT login must not take a session away from its API key"
    );
    for absent in SUBSCRIPTION_HEADERS {
        assert_eq!(sent.header(absent), None, "the key reached the platform");
    }

    // No key, a stored login: the codex backend, with everything it wants.
    endpoint.forget();
    // SAFETY: as above.
    unsafe {
        env::remove_var("OPENAI_API_KEY");
    }
    let chosen = select(&Config::default()).expect("a stored login is a session");
    assert_eq!(chosen.provider.id(), openai::ID);
    turn(chosen.provider.as_ref(), SUBSCRIPTION_MODEL).await;
    let sent = endpoint.only();
    assert_eq!(
        sent.path(),
        RESPONSES,
        "the credential with no consumer now has one"
    );
    assert_eq!(
        sent.header("authorization").as_deref(),
        Some(format!("Bearer {SECOND_ACCESS}").as_str()),
        "and it is the stored one that travels"
    );
    assert_eq!(
        sent.header("chatgpt-account-id").as_deref(),
        Some(SECOND_ACCOUNT),
        "the seat's headers are still there for the seat"
    );

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
    let defaulted = select(&Config::default()).expect("a stored login is a session");
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

    // And it is genuinely the seat's rather than a coincidence of the two
    // agreeing: a key session on the same vendor takes the catalog's.
    // SAFETY: as above.
    unsafe {
        env::set_var("OPENAI_API_KEY", KEY);
    }
    let defaulted = select(&Config::default()).expect("a key is a session");
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
        env::set_var("GANJA_MODEL", KEY_MODEL);
    }
    let named = select(&Config::default()).expect("a stored login is a session");
    assert_eq!(
        named.model, KEY_MODEL,
        "the seat's default must not overwrite an explicit choice"
    );
    let Err(refused_model) = named
        .provider
        .stream(ask(&named.model), CancellationToken::new())
        .await
    else {
        panic!("the seat does not serve {KEY_MODEL}, so there is no turn to take");
    };
    assert!(
        refused_model.to_string().contains(KEY_MODEL),
        "the refusal names what was asked for: {refused_model}"
    );
    assert!(
        endpoint.seen().is_empty(),
        "and it costs no request to say so"
    );
    // SAFETY: as above.
    unsafe {
        env::remove_var("GANJA_MODEL");
    }

    // ---- 7. Neither credential. --------------------------------------------
    // The startup failure it has always been, naming the variable and the
    // login.
    endpoint.forget();
    assert!(
        auth::remove_credential(auth::openai::PROVIDER_ID).expect("the store is writable"),
        "there was a credential to remove"
    );
    let Err(refused) = select(&Config::default()) else {
        panic!("a session with no credential at all is not a session");
    };
    let said = refused.to_string();
    assert!(
        said.contains(openai::API_KEY_ENV) && said.contains("ganja auth login"),
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
    let Err(refused_credential) = provider
        .stream(ask(SUBSCRIPTION_MODEL), CancellationToken::new())
        .await
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
        assert!(
            !rendered.contains(secret),
            "a credential reached a rendering: {rendered}"
        );
    }

    // ---- 9. A model this seat cannot run is refused before a turn is spent. --
    // The same name phase 3 took a whole turn on as a key, which is the pair
    // that proves the seat's list gates one backend and not the other
    // (`codex.ts:281` returns the models unfiltered for a credential that is
    // not an OAuth one). The backend answers it `400 {"detail":"The 'gpt-5.6'
    // model is not supported when using Codex with a ChatGPT account."}` — a
    // round trip and somebody else's JSON to learn something `codex.ts:15` has
    // written down.
    let Err(unsupported) = provider
        .stream(ask(KEY_MODEL), CancellationToken::new())
        .await
    else {
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
}
