//! The cursor wire, from the stored login to a decoded exchange, against a
//! real socket.
//!
//! Everything here serves real bytes over loopback rather than mocking the
//! client, the way every other wire suite in this build works: what is
//! asserted on is the request that was actually built — its path, its
//! headers, its Connect framing, and the protobuf inside — and the events a
//! canned response really decodes to. The response bodies are the shapes the
//! live probe recorded (`.omc/research/cursor/spike-wire-facts.md`): bare
//! protobuf on the unary listing, Connect frames with an in-body EndStream
//! verdict on the streaming turn. HTTP/1.1 on loopback is deliberate — the
//! framing is transport-agnostic, and h2 against the real endpoint is the
//! `#[ignore]`d live test's job.
//!
//! One test, one binary, on purpose: it mutates `XDG_DATA_HOME`, and a plain
//! `cargo test` runs the tests inside a binary on parallel threads.

use std::sync::{Arc, Mutex};

use buffa::Message as _;
use futures::StreamExt as _;
use ganja_provider::{
    auth::{self, AuthError, OauthCredential, RefreshOauth},
    protocol::{FinishReason, Message},
    provider::{
        ChatRequest, CursorProvider, Provider as _, ProviderError, ProviderEvent,
        cursor::{CursorWire, proto},
    },
};
use secrecy::SecretString;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;

/// The access token the seeded credential carries. Nothing may render it.
const ACCESS: &str = "at-cursor-canary-AAAA";

/// The refresh token stored beside it; never presented, because the
/// credential never expires within the test.
const REFRESH: &str = "rt-cursor-canary-BBBB";

/// The Connect EndStream flag, as the live probe recorded it.
const END_STREAM: u8 = 0b0000_0010;

/// A renewal that must never run: the seeded credential's expiry is far in
/// the future, so a call here means the wire re-decided what "expired"
/// means.
struct NeverRenews;

#[async_trait::async_trait]
impl RefreshOauth for NeverRenews {
    async fn refresh(
        &self,
        provider_id: &str,
        _credential: &OauthCredential,
    ) -> Result<OauthCredential, AuthError> {
        panic!("{provider_id} was renewed under a credential that was not due");
    }
}

/// One request the endpoint was asked to serve.
#[derive(Clone)]
struct Recorded {
    /// Request line and headers, verbatim.
    head: String,
    /// The raw body, which on the Run RPC is Connect-framed protobuf.
    body: Vec<u8>,
}

impl Recorded {
    /// The path asked for.
    fn path(&self) -> &str {
        self.head.split_whitespace().nth(1).unwrap_or_default()
    }

    /// Whether the request carried `name: value`, compared the way a header
    /// name is.
    fn has_header(&self, name: &str, value: &str) -> bool {
        self.head.lines().any(|line| {
            line.trim()
                .eq_ignore_ascii_case(&format!("{name}: {value}"))
        })
    }

    /// The value of header `name`, when the request carried it.
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|line| {
            let (candidate, value) = line.split_once(':')?;
            candidate
                .trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }
}

/// One canned answer: a status line, a content type, and raw body bytes.
#[derive(Clone)]
struct Reply {
    status: String,
    content_type: String,
    body: Vec<u8>,
}

impl Reply {
    fn ok(content_type: &str, body: Vec<u8>) -> Self {
        Self {
            status: "200 OK".to_owned(),
            content_type: content_type.to_owned(),
            body,
        }
    }

    /// The bytes on the wire, length-delimited because a protobuf body may
    /// hold any byte.
    fn bytes(&self) -> Vec<u8> {
        let mut rendered = format!(
            "HTTP/1.1 {}\r\nconnection: close\r\ncontent-type: {}\r\ncontent-length: {}\r\n\r\n",
            self.status,
            self.content_type,
            self.body.len()
        )
        .into_bytes();
        rendered.extend_from_slice(&self.body);

        rendered
    }
}

struct State {
    seen: Mutex<Vec<Recorded>>,
    reply: Mutex<Reply>,
}

/// A loopback endpoint serving whatever answer the current phase set.
struct Endpoint {
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

    /// The one request the current phase made.
    fn only(&self) -> Recorded {
        let seen = self.seen();
        assert_eq!(seen.len(), 1, "one request per phase, and no retries");

        seen.into_iter().next().expect("just counted")
    }

    /// Forgets what has been served, so a phase counts only its own traffic.
    fn forget(&self) {
        self.state
            .seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// Sets what every request is answered with from now on.
    fn answers_with(&self, reply: Reply) {
        *self
            .state
            .reply
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = reply;
    }
}

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

    Some(Recorded { head, body })
}

async fn serve() -> Endpoint {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback is bindable");
    let address = listener
        .local_addr()
        .expect("a bound socket has an address");
    let state = Arc::new(State {
        seen: Mutex::new(Vec::new()),
        reply: Mutex::new(Reply::ok("application/proto", Vec::new())),
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
                let reply = state
                    .reply
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                state
                    .seen
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(request);

                let _ = socket.write_all(&reply.bytes()).await;
                let _ = socket.flush().await;
            });
        }
    });

    Endpoint {
        base_url: format!("http://127.0.0.1:{}", address.port()),
        state,
        _server: server,
    }
}

/// Wraps one message's bytes in the 5-byte Connect envelope, spelled by hand
/// so the suite's framing is independent of the code it drills.
fn frame(flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut framed = vec![flags];
    framed.extend_from_slice(
        &u32::try_from(payload.len())
            .expect("a test fits")
            .to_be_bytes(),
    );
    framed.extend_from_slice(payload);

    framed
}

/// A server message carrying one update.
fn update_frame(update: proto::Update) -> Vec<u8> {
    let message = proto::ServerMessage {
        interaction_update: buffa::MessageField::some(update),
        ..Default::default()
    };

    frame(0, &message.encode_to_vec())
}

fn text_update(delta: &str) -> proto::Update {
    proto::Update {
        text_delta: buffa::MessageField::some(proto::TextDelta::default().with_text(delta)),
        ..Default::default()
    }
}

/// The one turn every phase asks for.
fn request() -> ChatRequest {
    ChatRequest {
        model: "gpt-5.3-codex".to_owned(),
        system: Some("You are terse.".to_owned()),
        messages: vec![Message::user("say hi")],
        tools: Vec::new(),
    }
}

/// The recorded headers every cursor request carries, asserted once per RPC
/// kind because the two differ only where the live probe measured them
/// differing.
fn assert_recorded_headers(recorded: &Recorded, streaming: bool) {
    assert!(
        recorded.has_header("authorization", &format!("Bearer {ACCESS}")),
        "the stored access token authenticates the request: {}",
        recorded.head.lines().next().unwrap_or_default()
    );
    assert!(recorded.has_header("x-cursor-client-version", "cli-2026.01.09-231024f"));
    assert!(recorded.has_header("x-cursor-client-type", "cli"));
    assert!(recorded.has_header("x-ghost-mode", "true"));
    assert!(recorded.has_header("te", "trailers"));
    let request_id = recorded
        .header("x-request-id")
        .expect("every request is stamped");
    assert_eq!(request_id.len(), 36, "a v4 uuid: {request_id}");

    if streaming {
        assert!(recorded.has_header("content-type", "application/connect+proto"));
        assert!(recorded.has_header("connect-protocol-version", "1"));
    } else {
        assert!(recorded.has_header("content-type", "application/proto"));
        assert_eq!(
            recorded.header("connect-protocol-version"),
            None,
            "the reference client omits it on unary RPCs and unary succeeded without it"
        );
    }
}

#[tokio::test]
async fn the_wire_speaks_the_recorded_connect_protocol() {
    // ── No login ─────────────────────────────────────────────────────────
    // Before anything is stored: the shipped provider refuses at the first
    // request, naming the login, and no socket is touched.
    let empty = tempfile::tempdir().expect("a temp directory");
    // SAFETY: this binary holds exactly one test, so nothing else in the
    // process is reading the environment concurrently.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", empty.path());
    }

    let refused = CursorProvider
        .stream(request(), CancellationToken::new())
        .await
        .err()
        .expect("a session with no login is refused, not sent");
    assert!(matches!(refused, ProviderError::Auth(_)), "{refused:?}");
    let rendered = refused.to_string();
    assert!(
        rendered.contains("ganja auth login cursor"),
        "the refusal names the repair: {rendered}"
    );

    // ── A stored login ───────────────────────────────────────────────────
    let home = tempfile::tempdir().expect("a temp directory");
    // SAFETY: as above — one test, one binary.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", home.path());
    }
    auth::set_oauth(
        auth::cursor::PROVIDER_ID,
        &OauthCredential::new(
            SecretString::from(REFRESH.to_owned()),
            SecretString::from(ACCESS.to_owned()),
            // Far enough out that the never-renewing refresher stays honest.
            u64::MAX / 2,
        ),
    )
    .expect("the credential stores");

    let endpoint = serve().await;
    let wire = CursorWire::at(&endpoint.base_url, Arc::new(NeverRenews))
        .expect("loopback may carry a token");

    // ── The model listing ────────────────────────────────────────────────
    // The canned answer mirrors the live one: bare protobuf, no framing,
    // the `default`/`auto` pair first.
    let listing = proto::GetUsableModelsResponse {
        models: vec![
            proto::ModelEntry::default()
                .with_model_id("default")
                .with_display_model_id("auto"),
            proto::ModelEntry::default()
                .with_model_id("gpt-5.3-codex")
                .with_display_model_id("gpt-5.3-codex")
                .with_display_name("Codex 5.3"),
        ],
        ..Default::default()
    }
    .encode_to_vec();
    endpoint.answers_with(Reply::ok("application/proto", listing));

    let models = wire.usable_models().await.expect("the listing decodes");
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].model_id.as_deref(), Some("default"));
    assert_eq!(models[1].display_name.as_deref(), Some("Codex 5.3"));

    let recorded = endpoint.only();
    assert_eq!(recorded.path(), "/agent.v1.AgentService/GetUsableModels");
    assert_recorded_headers(&recorded, false);
    assert!(
        recorded.body.is_empty(),
        "the fieldless request is the zero-byte body the live probe sent"
    );

    // ── One whole turn ───────────────────────────────────────────────────
    endpoint.forget();
    let mut exchange = update_frame(text_update("Hello"));
    exchange.extend(update_frame(text_update(" world")));
    exchange.extend(update_frame(proto::Update {
        turn_ended: buffa::MessageField::some(proto::TurnEnded::default()),
        ..Default::default()
    }));
    exchange.extend(frame(END_STREAM, b"{}"));
    endpoint.answers_with(Reply::ok("application/connect+proto", exchange));

    let events: Vec<ProviderEvent> = wire
        .stream(request(), CancellationToken::new())
        .await
        .expect("the exchange opens")
        .collect()
        .await;
    assert_eq!(
        events,
        vec![
            ProviderEvent::TextDelta("Hello".to_owned()),
            ProviderEvent::TextDelta(" world".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ]
    );

    let recorded = endpoint.only();
    assert_eq!(recorded.path(), "/agent.v1.AgentService/Run");
    assert_recorded_headers(&recorded, true);

    // The body is one enveloped run request whose bytes decode back to what
    // the turn asked: the framing, the model, the prompt and the message all
    // survive the wire intact.
    assert_eq!(recorded.body[0], 0, "an ordinary data frame");
    let declared =
        u32::from_be_bytes(recorded.body[1..5].try_into().expect("a whole prefix")) as usize;
    assert_eq!(
        declared,
        recorded.body.len() - 5,
        "the envelope covers the body"
    );
    let sent = proto::ClientMessage::decode_from_slice(&recorded.body[5..])
        .expect("the sent bytes are the client message");
    let run = sent.run_request.as_option().expect("a run request first");
    assert!(run.conversation_state.is_set());
    assert_eq!(run.custom_system_prompt.as_deref(), Some("You are terse."));
    assert_eq!(
        run.requested_model
            .as_option()
            .and_then(|model| model.model_id.as_deref()),
        Some("gpt-5.3-codex")
    );
    assert_eq!(
        run.action
            .as_option()
            .and_then(|action| action.user_message_action.as_option())
            .and_then(|action| action.user_message.as_option())
            .and_then(|message| message.text.as_deref()),
        Some("say hi")
    );

    // ── The recorded refusal ─────────────────────────────────────────────
    // The live probe's exact exchange: a heartbeat, then the EndStream
    // verdict. Nothing streamed, so the turn fails at its opening with the
    // provider's own vocabulary intact.
    endpoint.forget();
    let mut refusal = update_frame(proto::Update {
        heartbeat: buffa::MessageField::some(proto::Heartbeat::default()),
        ..Default::default()
    });
    refusal.extend(frame(
        END_STREAM,
        b"{\"error\":{\"code\":\"invalid_argument\",\"message\":\
           \"First message must be a run request or prewarm request\"}}",
    ));
    endpoint.answers_with(Reply::ok("application/connect+proto", refusal));

    let refused = wire
        .stream(request(), CancellationToken::new())
        .await
        .err()
        .expect("the recorded stream refuses");
    assert!(
        matches!(&refused, ProviderError::Status { status: 400, message }
            if message.contains("invalid_argument")),
        "{refused:?}"
    );

    // ── A dead credential, in-body ───────────────────────────────────────
    endpoint.forget();
    endpoint.answers_with(Reply::ok(
        "application/connect+proto",
        frame(
            END_STREAM,
            br#"{"error":{"code":"unauthenticated","message":"token expired"}}"#,
        ),
    ));

    let expired = wire
        .stream(request(), CancellationToken::new())
        .await
        .err()
        .expect("a dead credential refuses the turn");
    let rendered = expired.to_string();
    assert!(matches!(expired, ProviderError::Auth(_)), "{rendered}");
    assert!(
        rendered.contains("ganja auth login cursor"),
        "the in-body verdict names the same repair the startup refusal does: {rendered}"
    );

    // ── An HTTP refusal ──────────────────────────────────────────────────
    // The 415 the live probe drew by sending the wrong content type: a
    // status outside 2xx is the provider answering, reported as such.
    endpoint.forget();
    endpoint.answers_with(Reply {
        status: "415 Unsupported Media Type".to_owned(),
        content_type: "text/plain".to_owned(),
        body: Vec::new(),
    });

    let unsupported = wire
        .stream(request(), CancellationToken::new())
        .await
        .err()
        .expect("a non-2xx answer refuses the turn");
    assert!(
        matches!(unsupported, ProviderError::Status { status: 415, .. }),
        "{unsupported:?}"
    );
}
