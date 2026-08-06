//! A ChatGPT subscription becoming a turn, against a real socket.
//!
//! The credential a `ganja auth login openai` stores had no consumer until the
//! Responses provider landed: `GANJA_PROVIDER=openai` with no API key died at
//! startup naming `OPENAI_API_KEY`. This is what proves it now answers, and
//! that the path a key takes is the one it always was.
//!
//! Told in phases, because six different things have to be true at once and a
//! failure should still say which sentence broke:
//!
//! 1. **A whole turn.** A stored ChatGPT credential drives a streamed reply
//!    through the ordinary engine — the request asserted whole, the events
//!    asserted as the engine published them, `store: false` included: the
//!    backend refuses a body without it.
//! 2. **The credential is read per request.** The stored credential is rotated
//!    between two turns and the *second* token is what the second request
//!    carries. A provider that captured its token at construction passes every
//!    other assertion here and fails this one.
//! 3. **The key path is untouched.** The same socket, an API key, and a
//!    chat-completions request compared byte for byte against what this build
//!    has always sent.
//! 4. **The dispatch, all three ways.** A key wins over a stored login, a
//!    stored login serves where there is no key, and neither is the startup
//!    failure it has always been.
//! 5. **Nothing leaks.** No token reaches a rendering, an error or the store's
//!    own `Debug`.
//! 6. **An unsupported model costs nothing.** This backend serves a pinned
//!    list, and a name outside it is refused here — before the credential is
//!    read and before anything reaches the socket — rather than after a round
//!    trip spent on the backend's own JSON.
//!
//! Everything serves real bytes over loopback rather than mocking the client,
//! the way every other provider suite here works: what is asserted on is the
//! request that was actually built.
//!
//! One test, one binary, on purpose: it mutates `XDG_DATA_HOME`,
//! `OPENAI_API_KEY` and `OPENAI_BASE_URL`, and a plain `cargo test` runs the
//! tests inside a binary on parallel threads.

use std::{
    env,
    sync::{Arc, Mutex},
};

use futures::StreamExt as _;
use ganja_core::{
    Engine,
    auth::{self, AuthError, OauthCredential, RefreshOauth},
    config::Config,
    permission::Permissions,
    protocol::{Command, Event, PartBody, Role},
    provider::{ChatRequest, Provider as _, ProviderError, ResponsesProvider, openai, select},
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
/// Deliberately a *different* row, and deliberately one the subscription
/// backend refuses (`codex.ts:289`): it keeps the chat-completions literal in
/// phase 3 the bytes it has always been, and it is what phase 6 asks for to
/// prove the two wires answer the same name differently.
const KEY_MODEL: &str = "gpt-5.6";

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

/// Takes a whole turn, for the phases that assert on the request rather than
/// on what streamed back. The body is drained rather than dropped: an
/// unconsumed stream is a request that may never have been sent.
async fn turn(provider: &dyn ganja_core::provider::Provider, model: &str) {
    let streamed: Vec<_> = provider
        .stream(ask(model), CancellationToken::new())
        .await
        .expect("the endpoint answered")
        .collect()
        .await;

    assert!(!streamed.is_empty(), "an answered turn streams something");
}

/// A prompt command, as a frontend sends one.
fn prompt(text: &str) -> Command {
    Command::SendPrompt {
        text: text.to_owned(),
        mentions: Vec::new(),
    }
}

#[tokio::test]
async fn a_chatgpt_credential_drives_a_responses_turn_and_a_key_still_drives_chat_completions() {
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

    // ---- 3. The key path is byte-identical. -------------------------------
    endpoint.forget();
    endpoint.answers_turns_with(completions_transcript());
    // SAFETY: as above.
    unsafe {
        env::set_var("OPENAI_API_KEY", KEY);
        env::set_var(openai::BASE_URL_ENV, &endpoint.base_url);
    }

    let keyed = openai::OpenAiProvider::from_env().expect("an exported key builds a provider");
    turn(&keyed, KEY_MODEL).await;

    let sent = endpoint.only();
    assert_eq!(sent.path(), COMPLETIONS);
    assert_eq!(
        sent.header("authorization").as_deref(),
        Some(format!("Bearer {KEY}").as_str())
    );
    for absent in ["chatgpt-account-id", "originator", "openai-beta"] {
        assert_eq!(
            sent.header(absent),
            None,
            "the key path must not have grown a header from the other wire"
        );
    }
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

    // ---- 4. The dispatch, all three ways. ---------------------------------
    // A key outranks a stored login, exactly as `key_for` has always read the
    // two. Both are present here, which is the case that can only go one way.
    endpoint.forget();
    // SAFETY: as above. Named rather than defaulted, because an unset
    // `GANJA_PROVIDER` is the fake provider and would prove nothing.
    unsafe {
        env::set_var("GANJA_PROVIDER", openai::ID);
    }
    let chosen = select(&Config::default()).expect("a key is a session");
    assert_eq!(chosen.provider.id(), openai::ID, "one vendor, either wire");
    turn(chosen.provider.as_ref(), KEY_MODEL).await;
    assert_eq!(
        endpoint.only().path(),
        COMPLETIONS,
        "a stored ChatGPT login must not take a session away from its API key"
    );

    // No key, a stored login: the subscription wire.
    endpoint.forget();
    endpoint.answers_turns_with(responses_transcript());
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

    // Neither: the startup failure it has always been, naming the variable and
    // the login.
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

    // ---- 5. Nothing leaks. ------------------------------------------------
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

    // ---- 6. A model this seat cannot run is refused before a turn is spent. --
    // The same name phase 3 sent successfully as a key. The backend answers it
    // `400 {"detail":"The 'gpt-5.6' model is not supported when using Codex
    // with a ChatGPT account."}` — a round trip and somebody else's JSON to
    // learn something `codex.ts:15` has written down.
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
