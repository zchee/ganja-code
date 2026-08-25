//! A GitHub Copilot subscription becoming a turn, against a real socket.
//!
//! The credential a `ganja auth login github-copilot` stores had no consumer
//! until this provider landed: there was no `GANJA_PROVIDER` value that would
//! reach it at all. This is what proves it answers now, and that it answers the
//! way the live spike against `api.githubcopilot.com` measured rather than the
//! way a port might reasonably have guessed.
//!
//! Told in phases, because several different things have to be true at once and
//! a failure should still say which sentence broke:
//!
//! 1. **A whole turn.** A stored Copilot credential drives a streamed reply
//!    through the ordinary engine — the request asserted whole, all four
//!    headers, the events asserted as the engine published them.
//! 2. **The token travels verbatim.** The bearer is compared byte for byte
//!    against what the login stored. There is no `copilot_internal/v2/token`
//!    exchange anywhere in the pin and the spike confirmed the raw `gho_` token
//!    is accepted, so anything that transformed it — an exchange, a prefix, a
//!    trim — is a regression this phase exists to redden on.
//! 3. **A `length` finish is a completed turn.** A reasoning model given a
//!    small budget answers `200` with empty content and `finish_reason:
//!    "length"`. That is a model that ran out of room, not a credential that
//!    was refused, and the engine has to publish it as a turn that finished.
//! 4. **The deployment comes from the login.** An enterprise credential points
//!    the provider at `copilot-api.{domain}` and a public one at
//!    `api.githubcopilot.com`, both derived from what is stored beside the
//!    token rather than from anything a session says.
//! 5. **The credential is read per request.** The stored token is replaced
//!    between two turns and the *second* one is what the second request
//!    carries. A provider that captured its token at construction passes every
//!    other assertion here and fails this one.
//! 6. **Selection, and nothing leaking.** `GANJA_PROVIDER=github-copilot`
//!    resolves to this provider and a model the catalog can size; no token
//!    reaches a rendering or an error the endpoint echoed back.
//!
//! Everything serves real bytes over loopback rather than mocking the client,
//! the way every other provider suite here works: what is asserted on is the
//! request that was actually built.
//!
//! One test, one binary, on purpose: it mutates `XDG_DATA_HOME` and
//! `GANJA_PROVIDER`, and a plain `cargo test` runs the tests inside a binary on
//! parallel threads.

use std::{
    env,
    sync::{Arc, Mutex},
};

use ganja_core::{
    Engine,
    auth::{self, OauthCredential},
    config::Config,
    permission::Permissions,
    protocol::{Command, Event, FinishReason, PartBody, Role},
    provider::{CopilotProvider, copilot, select},
    tool::Registry,
};
use ganja_testkit::{RecorderTool, drain};
use secrecy::SecretString;
use serde_json::json;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
};

/// Where a Copilot turn goes, under the endpoint's base URL.
const COMPLETIONS: &str = "/chat/completions";

/// The GitHub token the login stored. A `gho_` prefix because that is what a
/// device flow against github.com returns, and the whole point of this suite is
/// that the prefix is still on it when it reaches the wire.
const TOKEN: &str = "gho_copilot-wire-canary-AAAA";

/// The token the store is moved to mid-suite, so "the token that travels" and
/// "the token that was stored first" are two different strings.
const SECOND_TOKEN: &str = "gho_copilot-wire-canary-BBBB";

/// The enterprise deployment one phase logs in against.
const ENTERPRISE: &str = "https://company.ghe.com/";

/// The model every phase asks for. A real catalog row, so a turn that reaches
/// the session layer has a context window to compact against.
const MODEL: &str = "claude-sonnet-4.6";

/// One request the endpoint was asked to serve.
#[derive(Clone)]
struct Recorded {
    /// Request line and headers, verbatim.
    head: String,
    /// The body, for a request that had one.
    body: String,
}

impl Recorded {
    /// The path asked for.
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
        self.head.lines().find_map(|line| {
            let (found, value) = line.split_once(':')?;
            found
                .trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }

    /// The body as JSON, for the phase that asserts on the whole request.
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
        reply: Mutex::new(transcript()),
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
        base_url: format!("http://{address}"),
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

/// A whole chat-completions turn, as `api.githubcopilot.com` streams one.
fn transcript() -> String {
    [
        r#"data: {"choices":[{"index":0,"delta":{"content":"Hello, "}}]}"#,
        r#"data: {"choices":[{"index":0,"delta":{"content":"world!"},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":9,"prompt_tokens_details":{"cached_tokens":16}}}"#,
        "data: [DONE]",
    ]
    .join("\n\n")
        + "\n\n"
}

/// What a reasoning model with a budget too small to answer in sends back: a
/// `200`, no content at all, and `length` as the reason it stopped.
///
/// Measured against the live endpoint, and the shape a naive port gets wrong —
/// an empty reply is easy to mistake for a request that was not authorised.
fn out_of_room_transcript() -> String {
    [
        r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"length"}],"usage":{"prompt_tokens":40,"completion_tokens":2048,"completion_tokens_details":{"reasoning_tokens":2048}}}"#,
        "data: [DONE]",
    ]
    .join("\n\n")
        + "\n\n"
}

/// Puts a Copilot credential in the store the way a completed login does,
/// replacing whatever was there.
///
/// Built through [`auth::copilot::credential_from`] rather than by hand, so
/// that this suite is asserting on the record the login actually writes —
/// including the `expires: 0` that means *never*, which is what keeps every
/// phase here from touching a renewal.
fn store(token: &str, deployment: &auth::copilot::Deployment) {
    let credential = auth::copilot::credential_from(
        &auth::device::Tokens {
            access: SecretString::from(token.to_owned()),
            refresh: None,
            expires_in: None,
        },
        deployment,
    );

    auth::set_oauth(copilot::ID, &credential).expect("the credential stores");
}

/// The stored credential's access token, for the phases that compare the wire
/// against the store rather than against a constant.
fn stored_token() -> String {
    use secrecy::ExposeSecret as _;

    let credential: OauthCredential = auth::oauth_for(copilot::ID)
        .expect("the store reads")
        .expect("a login was stored");

    credential.access.expose_secret().to_owned()
}

/// A prompt command, as a frontend sends one.
fn prompt(text: &str) -> Command {
    Command::SendPrompt {
        text: text.to_owned(),
        mentions: Vec::new(),
        skills: Vec::new(),
        peers: Vec::new(),
    }
}

/// The provider under test, pointed at `endpoint`.
fn copilot_at(endpoint: &Endpoint) -> CopilotProvider {
    CopilotProvider::at(&endpoint.base_url).expect("loopback may carry a token")
}

#[tokio::test]
async fn a_copilot_subscription_drives_a_turn_with_the_headers_and_the_raw_token_it_was_measured_with()
 {
    let home = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        env::set_var("XDG_DATA_HOME", home.path());
    }

    let endpoint = serve().await;

    // ---- 1. A whole turn, through the engine a frontend drives. -----------
    store(TOKEN, &auth::copilot::Deployment::Public);
    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "found it");
    let engine = Engine::new(
        Arc::new(copilot_at(&endpoint)),
        MODEL,
        Arc::new(Registry::new(vec![tool])),
        Permissions::default(),
    )
    .with_system_parts(Some("be brief".to_owned()), None);
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine
        .send(prompt("say hello"))
        .await
        .expect("an idle engine accepts");
    let seen = drain(&mut events).await;

    let sent = endpoint.only();
    assert_eq!(
        sent.path(),
        COMPLETIONS,
        "a Copilot turn is a chat-completions request; this provider has no \
         second wire and must not have grown one"
    );

    // The four headers the live spike measured, together. Nothing here knows
    // which of them the endpoint would still serve a request without, so all
    // four are asserted and none of them is a value to adjust on a hunch.
    assert_eq!(
        sent.header("x-github-api-version").as_deref(),
        Some(auth::copilot::API_VERSION),
        "the endpoint is told which API version this request is written \
         against, or it is free to serve a different one"
    );
    assert_eq!(
        sent.header("x-github-api-version").as_deref(),
        Some("2026-06-01"),
        "the version as a literal too, so moving the date is a decision \
         somebody has to make on purpose"
    );
    assert_eq!(
        sent.header("openai-intent").as_deref(),
        Some("conversation-edits")
    );
    assert_eq!(sent.header("x-initiator").as_deref(), Some("user"));
    assert_eq!(
        sent.header("user-agent").as_deref(),
        Some(auth::device::UPSTREAM_USER_AGENT),
        "this host keeps the borrowed identity by decision, while the codex \
         backend now carries ganja's own — moving it belongs to its own \
         evidence, not to a tidy-up that unifies the two constants"
    );
    assert_eq!(
        sent.header("x-api-key"),
        None,
        "Copilot authenticates with a bearer; an `x-api-key` here would be \
         another provider's header on this one's request"
    );

    let body = sent.json();
    assert_eq!(body["model"], json!(MODEL));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(
        body["stream_options"],
        json!({"include_usage": true}),
        "without this the stream reports no token counts, which for a seat \
         with a quota is the only usage figure there is: {body}"
    );
    assert_eq!(
        body["messages"],
        json!([
            {"role": "system", "content": "be brief"},
            {"role": "user", "content": "say hello"},
        ]),
        "got {body}"
    );
    assert_eq!(
        body["tools"][0]["function"]["name"],
        json!("lookup"),
        "a real turn always offers tools, in chat completions' nested shape: \
         {body}"
    );
    assert!(
        calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty(),
        "the transcript calls nothing, so nothing should have run"
    );

    // The turn as the frontend saw it.
    let text: String = seen
        .iter()
        .filter_map(|event| match event {
            Event::PartDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello, world!");
    assert!(
        seen.iter().any(
            |event| matches!(event, Event::MessageStarted { message, .. }
                if message.role == Role::Assistant)
        ),
        "the turn should have reached the event stream as a message: {seen:?}"
    );

    let billed = seen
        .iter()
        .find_map(|event| match event {
            Event::PartStarted { part, .. } => match &part.body {
                PartBody::StepFinish { usage } => Some(*usage),
                _ => None,
            },
            _ => None,
        })
        .expect("a finished step carries what it cost");
    assert_eq!(
        (billed.input_tokens, billed.cache_read_tokens),
        (26, 16),
        "42 prompt tokens of which the cache served 16 is 26 fresh — the \
         counters are what a seat's quota is spent against, so they have to be \
         right even where the price is zero: {billed:?}"
    );
    assert_eq!(billed.output_tokens, 9);

    // ---- 2. The token travels verbatim. -----------------------------------
    // Compared against the store rather than against the constant, so this
    // reddens for a transformation applied on either side of the wire.
    assert_eq!(
        sent.header("authorization").as_deref(),
        Some(format!("Bearer {}", stored_token()).as_str()),
        "the GitHub OAuth token *is* the Copilot credential — there is no \
         `copilot_internal/v2/token` exchange in the pin and the live endpoint \
         accepts the raw token, so anything that exchanged, prefixed or \
         trimmed it before presenting it is a regression"
    );
    assert_eq!(
        sent.header("authorization").as_deref(),
        Some(format!("Bearer {TOKEN}").as_str()),
        "and the token the login stored is the one that arrived"
    );

    // ---- 3. `length` with nothing said is a finished turn. ----------------
    // A reasoning model with a budget too small to answer in. The endpoint
    // answered `200`; reading an empty reply as a refusal would tell somebody
    // to log in again over a turn that needed a bigger budget.
    endpoint.forget();
    endpoint.answers_turns_with(out_of_room_transcript());

    engine
        .send(prompt("think hard"))
        .await
        .expect("an idle engine accepts");
    let seen = drain(&mut events).await;

    let finished = seen
        .iter()
        .find_map(|event| match event {
            Event::MessageFinished { reason, error, .. } => Some((*reason, error.clone())),
            _ => None,
        })
        .expect("every turn ends with a finish");
    assert_eq!(
        finished,
        (FinishReason::Completed, None),
        "`finish_reason: \"length\"` with empty content is a model that ran \
         out of room — a completed turn, never an auth failure and never a \
         transport one: {seen:?}"
    );
    assert!(
        !seen.iter().any(|event| matches!(
            event,
            Event::MessageFinished {
                reason: FinishReason::Failed,
                ..
            }
        )),
        "nothing about an empty reply is a failure: {seen:?}"
    );

    // ---- 4. The deployment comes from the login, not from a session. ------
    // `from_stored` is what a real session builds, and the only thing it reads
    // from the store is which GitHub this login was against.
    store(TOKEN, &auth::copilot::Deployment::enterprise(ENTERPRISE));
    let enterprise = format!(
        "{:?}",
        CopilotProvider::from_stored().expect("a client builds")
    );
    assert!(
        enterprise.contains("https://copilot-api.company.ghe.com"),
        "an enterprise login must not send its turns to github.com: \
         {enterprise}"
    );

    store(TOKEN, &auth::copilot::Deployment::Public);
    let public = format!(
        "{:?}",
        CopilotProvider::from_stored().expect("a client builds")
    );
    assert!(
        public.contains(auth::copilot::DEFAULT_API_BASE),
        "and a public one goes to GitHub's own API base: {public}"
    );

    // ---- 5. The credential is read per request, not captured. -------------
    endpoint.forget();
    endpoint.answers_turns_with(transcript());
    store(SECOND_TOKEN, &auth::copilot::Deployment::Public);

    engine
        .send(prompt("again"))
        .await
        .expect("an idle engine accepts");
    drain(&mut events).await;

    assert_eq!(
        endpoint.only().header("authorization").as_deref(),
        Some(format!("Bearer {SECOND_TOKEN}").as_str()),
        "the provider carried the token it was built with, so a login that \
         happened mid-session would never be picked up"
    );

    // ---- 6. Selection, and nothing leaking. -------------------------------
    // SAFETY: as above. Named rather than defaulted, because an unset
    // `GANJA_PROVIDER` is the fake provider and would prove nothing.
    unsafe {
        env::set_var("GANJA_PROVIDER", copilot::ID);
    }
    let chosen = select(&Config::default()).expect("a stored login is a session");
    assert_eq!(chosen.provider.id(), copilot::ID);
    assert_eq!(
        chosen.model, "claude-opus-4.8",
        "a session that names no model gets the catalog's copilot pin — the \
         measured fixture above stays on the model the recording was taken with"
    );
    assert!(
        chosen.notice.is_none(),
        "a provider that was asked for by name was not defaulted to"
    );

    // A session with no credential at all is still built — grok's posture,
    // inherited — and fails at the request, with the message that names the
    // login rather than a second, earlier one.
    endpoint.forget();
    assert!(
        auth::remove_credential(copilot::ID).expect("the store is writable"),
        "there was a credential to remove"
    );
    let chosen = select(&Config::default())
        .expect("construction does not read a token, so it cannot refuse one");
    let asked = chosen
        .provider
        .stream(
            ganja_core::provider::ChatRequest {
                effort_options: Default::default(),
                model: MODEL.to_owned(),
                system: None,
                messages: vec![ganja_core::protocol::Message::user("hello")],
                tools: Vec::new(),
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
    // Not `expect_err`: the success arm is a boxed stream, which has no
    // `Debug` to render.
    let Err(refused) = asked else {
        panic!("a turn with no credential stored is not a turn");
    };
    let said = refused.to_string();
    assert!(
        said.contains(&format!("ganja auth login {}", copilot::ID)),
        "the message is what a status bar shows, and only a login fixes this: \
         {said}"
    );
    assert!(
        endpoint.seen().is_empty(),
        "a turn with no credential must not have reached the wire"
    );

    // Nothing that was rendered along the way may hold the token — not the
    // provider, not the selection, not the refusal.
    store(TOKEN, &auth::copilot::Deployment::Public);
    let rendered = format!(
        "{said} {:?} {:?} {enterprise} {public}",
        select(&Config::default()).expect("a stored login is a session"),
        CopilotProvider::from_stored().expect("a client builds"),
    );
    for secret in [TOKEN, SECOND_TOKEN] {
        assert!(
            !rendered.contains(secret),
            "a token reached a rendering: {rendered}"
        );
    }

    // SAFETY: as above.
    unsafe {
        env::remove_var("GANJA_PROVIDER");
    }
}
