//! An OAuth credential becoming a request, against a real socket.
//!
//! An API key is captured once and every request presents the same one. An
//! access token is not: it expires under a session, it rotates when it is
//! renewed, and the one on disk may have been replaced by another turn since
//! the provider was built. So the credential is resolved *per request, before
//! the request is built* — the position upstream's `fetch` override occupies
//! (`plugin/xai.ts:477-528`, `plugin/openai/codex.ts:341-395`) — and this suite
//! is what pins that it really is resolved there and not captured at
//! construction.
//!
//! Everything here serves real bytes over loopback rather than mocking the
//! client, the way every other provider suite in this build works: what is
//! asserted on is the request that was actually built and the header it
//! actually carried. Both endpoints a grok turn touches — the chat API and the
//! token endpoint — are the same socket on different paths, so "how many times
//! was the token endpoint asked" is a count this suite can take rather than
//! infer.
//!
//! Four of the phases are about a mistake that would be a defect rather than a
//! wrong choice:
//!
//! - a refused refresh token is not retryable, or one expired grant becomes a
//!   retry storm against an identity provider;
//! - a token endpoint that could not be reached *is* retryable, or a dropped
//!   network sends someone through a login they did not need;
//! - concurrent requests renew once, or the second exchange presents a refresh
//!   token the first has already spent — xAI's rotate — and gets refused;
//! - `expires: 0` means *never expires* and must cost zero token-endpoint
//!   requests, because an implementation that asks "is `expires` in the past"
//!   instead of "does this credential have a deadline" renews forever.
//!
//! One test, one binary, on purpose: it mutates `XDG_DATA_HOME`, and a plain
//! `cargo test` runs the tests inside a binary on parallel threads.

use std::{
    collections::HashMap,
    env,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::StreamExt as _;
use ganja_core::{
    auth::{self, OauthCredential, grok::Refresh},
    protocol::Message,
    provider::{ChatRequest, GrokProvider, Provider as _, ProviderError, ProviderEvent},
};
use secrecy::{ExposeSecret as _, SecretString};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;

/// Where the chat request goes, under the endpoint's base URL.
const CHAT: &str = "/v1/chat/completions";

/// Where a renewal goes.
const TOKEN: &str = "/oauth2/token";

/// The access token a live credential carries. Nothing may render it.
const LIVE_ACCESS: &str = "at-live-canary-AAAA";

/// The refresh token stored beside it. It is *sent* to the token endpoint, so
/// it has one more way out than the access token does.
const REFRESH: &str = "rt-stored-canary-BBBB";

/// What the token endpoint hands back. Distinct from [`LIVE_ACCESS`] so that
/// "the request carried the new token" is a different assertion from "the
/// request carried a token".
const RENEWED_ACCESS: &str = "at-renewed-canary-CCCC";

/// The rotated refresh token a renewal stores in place of [`REFRESH`].
const ROTATED: &str = "rt-rotated-canary-DDDD";

/// The access token of a credential that never expires.
const ETERNAL_ACCESS: &str = "at-eternal-canary-EEEE";

/// How long the token endpoint takes to answer when a phase needs the window
/// held open. Real time, because what is being proved is that callers arriving
/// while a renewal is in flight join it rather than starting their own.
const RENEWAL: Duration = Duration::from_millis(150);

/// Callers that discover the same expiry at the same moment.
const CALLERS: usize = 8;

/// One request the endpoint was asked to serve.
#[derive(Clone)]
struct Recorded {
    /// Request line and headers, verbatim.
    head: String,
    /// The body, for a request that had one.
    body: String,
}

impl Recorded {
    /// The path asked for, which is what tells the two endpoints apart.
    fn path(&self) -> &str {
        self.head
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .split('?')
            .next()
            .unwrap_or_default()
    }

    /// Whether the request carried `name: value`, compared case-insensitively
    /// the way a header name is.
    fn has_header(&self, name: &str, value: &str) -> bool {
        self.head.lines().any(|line| {
            line.trim()
                .eq_ignore_ascii_case(&format!("{name}: {value}"))
        })
    }

    /// Whether the request carried `name` at all, whatever its value.
    fn has_header_named(&self, name: &str) -> bool {
        let prefix = format!("{name}:");

        self.head
            .lines()
            .any(|line| line.trim().to_ascii_lowercase().starts_with(&prefix))
    }

    /// The form fields a token request presented.
    fn form(&self) -> HashMap<String, String> {
        url::form_urlencoded::parse(self.body.as_bytes())
            .map(|(field, value)| (field.into_owned(), value.into_owned()))
            .collect()
    }
}

/// One canned answer.
#[derive(Clone)]
struct Reply {
    /// Status line, e.g. `200 OK`.
    status: String,
    /// What the body is.
    content_type: String,
    /// The body itself.
    body: String,
    /// How long to take before answering.
    delay: Duration,
}

impl Reply {
    fn ok(content_type: &str, body: impl Into<String>) -> Self {
        Self {
            status: "200 OK".to_owned(),
            content_type: content_type.to_owned(),
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    fn refusing(status: &str, body: impl Into<String>) -> Self {
        Self {
            status: status.to_owned(),
            content_type: "application/json".to_owned(),
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    /// The same answer, taking `delay` to arrive.
    fn slowly(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// The bytes on the wire. Close-delimited, because that is how an
    /// event-stream body ends and it costs the JSON answers nothing.
    fn bytes(&self) -> Vec<u8> {
        format!(
            "HTTP/1.1 {}\r\nconnection: close\r\ncontent-type: {}\r\n\r\n{}",
            self.status, self.content_type, self.body
        )
        .into_bytes()
    }
}

/// Everything the server task and the test both hold.
struct State {
    seen: Mutex<Vec<Recorded>>,
    chat: Mutex<Reply>,
    token: Mutex<Reply>,
}

/// A loopback endpoint serving both halves of a grok turn.
struct Endpoint {
    /// What the provider is pointed at.
    api_base: String,
    /// What a renewal is pointed at.
    token_url: String,
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

    /// How many requests reached `path`.
    fn count(&self, path: &str) -> usize {
        self.seen()
            .iter()
            .filter(|request| request.path() == path)
            .count()
    }

    /// Forgets what has been served, so a phase counts only its own traffic.
    fn forget(&self) {
        self.state
            .seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// Sets what the token endpoint answers with from now on.
    fn answers_renewals_with(&self, reply: Reply) {
        *self
            .state
            .token
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = reply;
    }

    /// Sets what the chat endpoint answers with from now on.
    fn answers_turns_with(&self, reply: Reply) {
        *self
            .state
            .chat
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = reply;
    }
}

/// Starts an endpoint that answers every connection, concurrently, for as long
/// as the test holds it.
///
/// A connection per task rather than a queue of canned answers, because one
/// phase deliberately has eight requests in the air at once and a server that
/// served them one at a time would be proving something about the server.
async fn serve() -> Endpoint {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback is bindable");
    let address = listener
        .local_addr()
        .expect("a bound socket has an address");
    let state = Arc::new(State {
        seen: Mutex::new(Vec::new()),
        chat: Mutex::new(Reply::ok(
            "text/event-stream",
            include_str!("../../ganja-provider/tests/fixtures/openai_happy_path.sse"),
        )),
        token: Mutex::new(Reply::ok(
            "application/json",
            format!(
                r#"{{"access_token":"{RENEWED_ACCESS}","refresh_token":"{ROTATED}",
                    "expires_in":3600,"token_type":"Bearer"}}"#
            ),
        )),
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
                let reply = {
                    let chosen = if request.path() == TOKEN {
                        &state.token
                    } else {
                        &state.chat
                    };
                    chosen
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone()
                };
                state
                    .seen
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(request);

                if !reply.delay.is_zero() {
                    tokio::time::sleep(reply.delay).await;
                }
                let _ = socket.write_all(&reply.bytes()).await;
                let _ = socket.flush().await;
                // Dropping the socket ends a close-delimited body.
            });
        }
    });

    Endpoint {
        api_base: format!("http://{address}/v1"),
        token_url: format!("http://{address}{TOKEN}"),
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

/// Puts `credential` in the store as grok's, replacing whatever was there.
fn store(access: &str, refresh: &str, expires: u64) {
    auth::set_oauth(
        auth::grok::PROVIDER_ID,
        &OauthCredential::new(
            SecretString::from(refresh.to_owned()),
            SecretString::from(access.to_owned()),
            expires,
        ),
    )
    .expect("the credential stores");
}

/// A credential that is spent, so the next request has to renew it.
fn spent() -> u64 {
    // Inside the skew window as well as past, which is the condition
    // `needs_refresh` actually applies.
    auth::now_ms()
}

/// A credential with hours left on it.
fn live() -> u64 {
    auth::now_ms() + 86_400_000
}

/// One turn's worth of request.
fn ask() -> ChatRequest {
    ChatRequest {
        variant_options: Default::default(),
        model: "grok-4.3".to_owned(),
        system: None,
        messages: vec![Message::user("hello")],
        tools: Vec::new(),
    }
}

/// Takes a whole turn and returns what streamed.
async fn turn(provider: &GrokProvider) -> Vec<ProviderEvent> {
    provider
        .stream(ask(), CancellationToken::new())
        .await
        .expect("the endpoint answered")
        .collect()
        .await
}

#[tokio::test]
async fn an_oauth_credential_is_resolved_afresh_for_every_request_and_renewed_once() {
    let home = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        env::set_var("XDG_DATA_HOME", home.path());
    }

    let endpoint = serve().await;
    let provider = Arc::new(
        GrokProvider::at(
            &endpoint.api_base,
            Arc::new(Refresh::at(&endpoint.token_url).expect("a client builds")),
        )
        .expect("loopback may carry a token"),
    );

    // A credential that is still good is presented as it stands, and the token
    // endpoint is not troubled at all.
    store(LIVE_ACCESS, REFRESH, live());
    let streamed = turn(&provider).await;

    assert!(
        streamed
            .iter()
            .any(|event| matches!(event, ProviderEvent::TextDelta(_))),
        "the turn should have streamed a reply: {streamed:?}"
    );
    assert_eq!(
        endpoint.count(TOKEN),
        0,
        "a credential that is not due must not be renewed"
    );
    let sent = endpoint.seen();
    let [chat] = sent.as_slice() else {
        panic!("one turn is one request, got {}", sent.len());
    };
    assert_eq!(chat.path(), CHAT, "the base URL override is what was used");
    assert!(
        chat.has_header("authorization", &format!("Bearer {LIVE_ACCESS}")),
        "an access token travels the way xAI wants it: {}",
        chat.head
    );
    assert!(
        !chat.has_header_named("x-api-key"),
        "an OAuth provider has no API key to send: {}",
        chat.head
    );

    // A credential past its skew is renewed *before* the request is built, and
    // what goes on the wire is the token that renewal produced.
    endpoint.forget();
    store(LIVE_ACCESS, REFRESH, spent());
    turn(&provider).await;

    let sent = endpoint.seen();
    let [renewal, chat] = sent.as_slice() else {
        panic!("a renewal and then a turn, got {}", sent.len());
    };
    assert_eq!(
        renewal.path(),
        TOKEN,
        "renewing after the request was built would put a spent token on the wire"
    );
    assert_eq!(chat.path(), CHAT);
    assert_eq!(
        renewal.form().get("refresh_token").map(String::as_str),
        Some(REFRESH),
        "the renewal presents what was stored"
    );
    assert!(
        chat.has_header("authorization", &format!("Bearer {RENEWED_ACCESS}")),
        "the request carried the token it replaced, not the one it renewed: {}",
        chat.head
    );

    let stored = auth::oauth_for(auth::grok::PROVIDER_ID)
        .expect("the store reads")
        .expect("the credential is there");
    assert_eq!(stored.access.expose_secret(), RENEWED_ACCESS);
    assert_eq!(
        stored.refresh.expose_secret(),
        ROTATED,
        "xAI rotates, and the next process must not present the spent one"
    );

    // Everyone who meets the same expiry at the same moment renews once. With
    // a rotating refresh token the second exchange would present one the first
    // has already spent, and the provider would be right to refuse it.
    endpoint.forget();
    endpoint.answers_renewals_with(
        Reply::ok(
            "application/json",
            format!(
                r#"{{"access_token":"{RENEWED_ACCESS}","refresh_token":"{ROTATED}",
                    "expires_in":3600}}"#
            ),
        )
        .slowly(RENEWAL),
    );
    store(LIVE_ACCESS, REFRESH, spent());

    let concurrent: Vec<_> = (0..CALLERS)
        .map(|_| {
            let provider = Arc::clone(&provider);
            tokio::spawn(async move { turn(&provider).await })
        })
        .collect();
    for caller in concurrent {
        caller.await.expect("no caller panicked");
    }

    assert_eq!(
        endpoint.count(TOKEN),
        1,
        "{CALLERS} callers spent the rotating refresh token more than once"
    );
    assert_eq!(endpoint.count(CHAT), CALLERS);
    assert!(
        endpoint
            .seen()
            .iter()
            .filter(|request| request.path() == CHAT)
            .all(|request| request
                .has_header("authorization", &format!("Bearer {RENEWED_ACCESS}"))),
        "every caller should have got the one renewal's token"
    );

    // A credential that never expires has no deadline to be past, and asking a
    // token endpoint about it is how an implementation that reads `expires: 0`
    // as "expired in 1970" loops forever.
    endpoint.forget();
    endpoint.answers_renewals_with(Reply::refusing(
        "500 Internal Server Error",
        r#"{"error":"this endpoint should not have been asked"}"#,
    ));
    store(ETERNAL_ACCESS, REFRESH, 0);
    for _ in 0..4 {
        turn(&provider).await;
    }

    assert_eq!(
        endpoint.count(TOKEN),
        0,
        "`expires: 0` is upstream's never-expires, not a deadline in the past"
    );
    assert_eq!(endpoint.count(CHAT), 4);
    assert!(
        endpoint
            .seen()
            .iter()
            .all(|request| request.has_header("authorization", &format!("Bearer {ETERNAL_ACCESS}"))),
        "the credential that needs no renewal is the one that should travel"
    );

    // A token endpoint that refuses is a dead credential: only a new login
    // fixes it, and retrying it is a storm against an identity provider.
    endpoint.forget();
    endpoint.answers_renewals_with(Reply::refusing(
        "401 Unauthorized",
        // The shape that makes this worth testing: the endpoint quotes the
        // token it refused.
        format!(
            r#"{{"error":"invalid_grant","error_description":"refresh token {REFRESH} is spent"}}"#
        ),
    ));
    store(LIVE_ACCESS, REFRESH, spent());
    let Err(refused) = provider.stream(ask(), CancellationToken::new()).await else {
        panic!("a refused renewal is not a turn");
    };

    assert!(
        matches!(refused, ProviderError::Auth(_)),
        "a dead refresh token is not a transport failure: {refused:?}"
    );
    assert!(
        !refused.is_retryable(),
        "retrying a refusal is what turns one expired grant into a storm"
    );
    assert!(
        format!("{refused}").contains("ganja auth login grok"),
        "the message is what a status bar shows: {refused}"
    );
    assert_eq!(
        endpoint.count(CHAT),
        0,
        "a credential that could not be produced must not become a request"
    );

    // A token endpoint that could not be reached has refused nothing. The
    // stored credential is untouched and trying again is the answer.
    let unreachable = GrokProvider::at(
        &endpoint.api_base,
        Arc::new(Refresh::at(dead_endpoint().await).expect("a client builds")),
    )
    .expect("loopback may carry a token");
    endpoint.forget();
    store(LIVE_ACCESS, REFRESH, spent());
    let Err(unavailable) = unreachable.stream(ask(), CancellationToken::new()).await else {
        panic!("a renewal that never happened is not a turn");
    };

    assert!(
        matches!(unavailable, ProviderError::Transport(_)),
        "an endpoint that never answered has refused nothing: {unavailable:?}"
    );
    assert!(
        unavailable.is_retryable(),
        "trying again is exactly what fixes a renewal that could not be reached"
    );

    // The last way out: the provider itself refuses the token and quotes it
    // back. An access token gets the masking an API key gets, or a 401 body
    // puts a live credential in the transcript and in the log.
    endpoint.forget();
    endpoint.answers_turns_with(Reply::refusing(
        "401 Unauthorized",
        format!(r#"{{"error":{{"message":"token {LIVE_ACCESS} is not valid"}}}}"#),
    ));
    store(LIVE_ACCESS, REFRESH, live());
    let Err(rejected) = provider.stream(ask(), CancellationToken::new()).await else {
        panic!("a 401 is not answerable");
    };

    assert!(
        matches!(rejected, ProviderError::Status { status: 401, .. }),
        "{rejected:?}"
    );
    assert!(
        format!("{rejected}").contains("[redacted]"),
        "the quoted token should be masked rather than dropped: {rejected}"
    );

    // Nothing any phase produced may carry a token, including the two the
    // store still holds and the provider's own rendering.
    let rendered = format!(
        "{provider:?} {unreachable:?} {refused} {refused:?} {rejected} {rejected:?} {unavailable} {unavailable:?}"
    );
    for secret in [
        LIVE_ACCESS,
        REFRESH,
        RENEWED_ACCESS,
        ROTATED,
        ETERNAL_ACCESS,
    ] {
        assert!(
            !rendered.contains(secret),
            "a token reached a rendering: {rendered}"
        );
    }
}

/// An address nothing is listening on.
///
/// Bound and released rather than guessed, so the port is one the kernel just
/// confirmed was free instead of one that might belong to something else.
async fn dead_endpoint() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback is bindable");
    let address = listener
        .local_addr()
        .expect("a bound socket has an address");
    drop(listener);

    format!("http://{address}{TOKEN}")
}
