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
//! verdict on the streaming turn. A reply can be **gated** — written up to a
//! byte the test chooses, then held until the test says go — which is what
//! proves delivery is incremental: the first delta has to reach the session
//! while the rest of the body is deliberately unwritten. HTTP/1.1 on
//! loopback is deliberate — the framing is transport-agnostic, and h2
//! against the real endpoint is the `#[ignore]`d live test's job. What
//! HTTP/1.1 cannot honestly host is the Run RPC's full duplex: its request
//! body is a held-open chunked stream, so the fixture de-chunks exactly one
//! Connect envelope — the run request — and never waits for a body EOF a
//! live turn deliberately never sends; the ask-answer paths — the exec
//! channel's context ask and the kv channel's blob exchanges alike — are
//! unit-driven through in-memory channels in `cursor.rs`, where both
//! directions run without a transport to lie about.
//!
//! One test, one binary, on purpose: it mutates `XDG_DATA_HOME`, and a plain
//! `cargo test` runs the tests inside a binary on parallel threads.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use buffa::Message as _;
use futures::StreamExt as _;
use ganja_provider::auth::{self, AuthError, OauthCredential, RefreshOauth};
use ganja_provider::protocol::{FinishReason, Message};
use ganja_provider::provider::cursor::{CursorWire, history, proto};
use ganja_provider::provider::{
    ChatRequest, CursorProvider, Provider as _, ProviderError, ProviderEvent,
};
use ganja_provider::tool::ToolDefinition;
use secrecy::SecretString;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

/// The access token the seeded credential carries. Nothing may render it.
const ACCESS: &str = "at-cursor-canary-AAAA";

/// The refresh token stored beside it; never presented, because the
/// credential never expires within the test.
const REFRESH: &str = "rt-cursor-canary-BBBB";

/// The Connect EndStream flag, as the live probe recorded it.
const END_STREAM: u8 = 0b0000_0010;

/// How long an event that should already be decodable may take to arrive.
/// Generous because CI machines stall, and reached only when the wire has
/// regressed to buffering the whole body.
const PATIENCE: Duration = Duration::from_secs(10);

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
        self.head.lines().any(|line| line.trim().eq_ignore_ascii_case(&format!("{name}: {value}")))
    }

    /// The value of header `name`, when the request carried it.
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|line| {
            let (candidate, value) = line.split_once(':')?;
            candidate.trim().eq_ignore_ascii_case(name).then(|| value.trim().to_owned())
        })
    }
}

/// A pause in a reply: the body is written up to `after` bytes, then held
/// until `open` is notified. What it proves is that whatever the test read
/// in between was decoded from a body still in flight.
#[derive(Clone)]
struct Gate {
    after: usize,
    open: Arc<Notify>,
}

/// One canned answer: a status line, a content type, extra headers, raw
/// body bytes, and an optional mid-body pause.
#[derive(Clone)]
struct Reply {
    status: String,
    content_type: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    gate: Option<Gate>,
}

impl Reply {
    fn with(status: &str, content_type: &str, body: Vec<u8>) -> Self {
        Self {
            status: status.to_owned(),
            content_type: content_type.to_owned(),
            headers: Vec::new(),
            body,
            gate: None,
        }
    }

    fn ok(content_type: &str, body: Vec<u8>) -> Self {
        Self::with("200 OK", content_type, body)
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    fn gated(mut self, after: usize, open: Arc<Notify>) -> Self {
        self.gate = Some(Gate { after, open });
        self
    }

    /// The head on the wire, length-delimited because a protobuf body may
    /// hold any byte.
    fn head_bytes(&self) -> Vec<u8> {
        let mut rendered = format!(
            "HTTP/1.1 {}\r\nconnection: close\r\ncontent-type: {}\r\ncontent-length: {}\r\n",
            self.status,
            self.content_type,
            self.body.len()
        );
        for (name, value) in &self.headers {
            rendered.push_str(&format!("{name}: {value}\r\n"));
        }
        rendered.push_str("\r\n");

        rendered.into_bytes()
    }
}

struct State {
    seen: Mutex<Vec<Recorded>>,
    /// Answers served ahead of the sticky one, oldest first.
    queued: Mutex<VecDeque<Reply>>,
    /// What every request is answered with once the queue is empty.
    sticky: Mutex<Reply>,
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
        self.state.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// The one request the current phase made.
    fn only(&self) -> Recorded {
        let seen = self.seen();
        assert_eq!(seen.len(), 1, "one request per phase, and no retries");

        seen.into_iter().next().expect("just counted")
    }

    /// Forgets what has been served, so a phase counts only its own traffic.
    fn forget(&self) {
        self.state.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clear();
    }

    /// Sets what every request is answered with from now on.
    fn answers_with(&self, reply: Reply) {
        *self.state.sticky.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = reply;
    }

    /// Queues an answer served once, ahead of the sticky one — which is how
    /// a phase says "refuse the first attempt, answer the retry".
    fn answers_once(&self, reply: Reply) {
        self.state.queued.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push_back(reply);
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
            name.trim().eq_ignore_ascii_case("content-length").then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    let chunked =
        head.lines().any(|line| line.trim().eq_ignore_ascii_case("transfer-encoding: chunked"));

    let body = if chunked {
        read_enveloped_chunks(socket).await?
    } else {
        let mut body = vec![0_u8; length];
        if length > 0 && socket.read_exact(&mut body).await.is_err() {
            return None;
        }
        body
    };

    Some(Recorded { head, body })
}

/// De-chunks a streamed request body until one whole Connect envelope — the
/// run request — is buffered, then stops reading: the duplex body is held
/// open for exec answers and deliberately never ends while the turn is
/// open, so a fixture that read to EOF would hang on the wire's defining
/// feature.
async fn read_enveloped_chunks(socket: &mut tokio::net::TcpStream) -> Option<Vec<u8>> {
    let mut body = Vec::new();

    loop {
        if body.len() >= 5 {
            let declared = u32::from_be_bytes(body[1..5].try_into().ok()?) as usize;
            if body.len() >= 5 + declared {
                return Some(body);
            }
        }

        let mut line = Vec::new();
        let mut byte = [0_u8; 1];
        while !line.ends_with(b"\r\n") {
            match socket.read(&mut byte).await {
                Ok(0) | Err(_) => return None,
                Ok(_) => line.push(byte[0]),
            }
        }
        let size = usize::from_str_radix(String::from_utf8_lossy(&line).trim(), 16).ok()?;
        if size == 0 {
            // The client closed the body early; whatever arrived is the
            // record.
            return Some(body);
        }

        // The chunk's data and its trailing CRLF.
        let mut chunk = vec![0_u8; size + 2];
        if socket.read_exact(&mut chunk).await.is_err() {
            return None;
        }
        chunk.truncate(size);
        body.extend_from_slice(&chunk);
    }
}

async fn serve() -> Endpoint {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback is bindable");
    let address = listener.local_addr().expect("a bound socket has an address");
    let state = Arc::new(State {
        seen: Mutex::new(Vec::new()),
        queued: Mutex::new(VecDeque::new()),
        sticky: Mutex::new(Reply::ok("application/proto", Vec::new())),
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
                    .queued
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .pop_front()
                    .unwrap_or_else(|| {
                        state.sticky.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
                    });
                state.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(request);

                let _ = socket.write_all(&reply.head_bytes()).await;
                match &reply.gate {
                    None => {
                        let _ = socket.write_all(&reply.body).await;
                    }
                    Some(gate) => {
                        let split = gate.after.min(reply.body.len());
                        let _ = socket.write_all(&reply.body[..split]).await;
                        let _ = socket.flush().await;
                        gate.open.notified().await;
                        let _ = socket.write_all(&reply.body[split..]).await;
                    }
                }
                let _ = socket.flush().await;
            });
        }
    });

    Endpoint { base_url: format!("http://127.0.0.1:{}", address.port()), state, _server: server }
}

/// Wraps one message's bytes in the 5-byte Connect envelope, spelled by hand
/// so the suite's framing is independent of the code it drills.
fn frame(flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut framed = vec![flags];
    framed.extend_from_slice(&u32::try_from(payload.len()).expect("a test fits").to_be_bytes());
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

fn thinking_update(delta: &str) -> proto::Update {
    proto::Update {
        thinking_delta: buffa::MessageField::some(proto::ThinkingDelta::default().with_text(delta)),
        ..Default::default()
    }
}

fn turn_ended_update() -> proto::Update {
    proto::Update {
        turn_ended: buffa::MessageField::some(proto::TurnEnded::default()),
        ..Default::default()
    }
}

/// The body of a clean exchange the way a codex-family turn opens: thinking
/// ahead of the reply, then the turn marked over and the empty EndStream
/// verdict. No ask frames ride this body — answering one means writing to
/// the request body mid-response, the duplex half this fixture's HTTP/1.1
/// close-delimited replies cannot honestly host (hyper drops the unsent
/// request body once the whole response is in), so both ask channels are
/// unit-driven through in-memory channels in `cursor.rs`.
fn exchange_body() -> Vec<u8> {
    let mut body = update_frame(thinking_update("Weighing a greeting."));
    body.extend(update_frame(text_update("Hello")));
    body.extend(update_frame(text_update(" world")));
    body.extend(update_frame(turn_ended_update()));
    body.extend(frame(END_STREAM, b"{}"));

    body
}

/// What [`exchange_body`] decodes to, spelled once: the thinking surfaces
/// as reasoning.
fn exchange_events() -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ReasoningDelta("Weighing a greeting.".to_owned()),
        ProviderEvent::TextDelta("Hello".to_owned()),
        ProviderEvent::TextDelta(" world".to_owned()),
        ProviderEvent::Finish(FinishReason::Completed),
    ]
}

/// The one turn every phase asks for.
fn request() -> ChatRequest {
    ChatRequest {
        turn_start: 0,
        effort_options: Default::default(),
        model: "gpt-5.3-codex".to_owned(),
        system: Some("You are terse.".to_owned()),
        messages: vec![Message::user("say hi")],
        tools: Vec::new(),
    }
}

/// The roster the declaration phase offers.
fn declared() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "read".to_owned(),
            description: "Reads a file.".to_owned(),
            schema: serde_json::json!({ "type": "object" }),
        },
        ToolDefinition {
            name: "bash".to_owned(),
            description: "Runs a command.".to_owned(),
            schema: serde_json::json!({ "type": "object" }),
        },
    ]
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
    let request_id = recorded.header("x-request-id").expect("every request is stamped");
    assert_eq!(request_id.len(), 36, "a v4 uuid: {request_id}");

    if streaming {
        assert!(recorded.has_header("content-type", "application/connect+proto"));
        assert!(recorded.has_header("connect-protocol-version", "1"));
        assert!(
            recorded.has_header("transfer-encoding", "chunked"),
            "a duplex body has no length to declare: the run request opens it and \
             the exec answers keep it open"
        );
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

    let refused = CursorProvider::default()
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
            proto::ModelEntry::default().with_model_id("default").with_display_model_id("auto"),
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
    endpoint.answers_with(Reply::ok("application/connect+proto", exchange_body()));

    let one_shot = request();
    let events: Vec<ProviderEvent> = wire
        .stream(one_shot.clone(), CancellationToken::new())
        .await
        .expect("the exchange opens")
        .collect()
        .await;
    assert_eq!(events, exchange_events());

    let recorded = endpoint.only();
    assert_eq!(recorded.path(), "/agent.v1.AgentService/Run");
    assert_recorded_headers(&recorded, true);

    // The body is one enveloped run request whose bytes decode back to what
    // the turn asked: the framing, the model and the message all survive the
    // wire intact — and the system prompt never does, because its one inline
    // member is the allowlist-gated override the live server refused.
    assert_eq!(recorded.body[0], 0, "an ordinary data frame");
    let envelope =
        u32::from_be_bytes(recorded.body[1..5].try_into().expect("a whole prefix")) as usize;
    assert_eq!(envelope, recorded.body.len() - 5, "the envelope covers the body");
    let sent = proto::ClientMessage::decode_from_slice(&recorded.body[5..])
        .expect("the sent bytes are the client message");
    let run = sent.run_request.as_option().expect("a run request first");
    // **AC-4.** A one-shot has nothing before its newest run, so the state it
    // sends is present and empty — what this wire sent before history was
    // composed — under a conversation id all the same, and with no roster.
    let state = run.conversation_state.as_option().expect("the state is present");
    assert!(state.root_prompt_messages_json.is_empty(), "nothing to name on a first turn");
    assert!(state.turns.is_empty());
    assert_eq!(
        run.conversation_id.as_deref(),
        Some(history::derived(&one_shot.messages[0].id).as_str()),
        "the conversation id is derived from the first message"
    );
    assert!(!run.mcp_tools.is_set(), "no tools, no roster");
    let prompt = b"You are terse.";
    assert!(
        !recorded.body.windows(prompt.len()).any(|window| window == prompt),
        "the system prompt the turn carried must not reach cursor's wire"
    );
    assert_eq!(
        run.requested_model.as_option().and_then(|model| model.model_id.as_deref()),
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

    // ── The declared roster, on the wire ─────────────────────────────────
    // The same turn, offering tools: the run request that opens it carries
    // `mcp_tools = 4` with one definition per tool, and this is asserted
    // against the bytes a real socket received rather than against an
    // assembled message — the one place that proves the roster survives the
    // envelope, the chunked body and the transport.
    endpoint.forget();
    endpoint.answers_with(Reply::ok("application/connect+proto", exchange_body()));

    let offering = ChatRequest { tools: declared(), ..request() };
    let events: Vec<ProviderEvent> = wire
        .stream(offering, CancellationToken::new())
        .await
        .expect("the exchange opens")
        .collect()
        .await;
    assert_eq!(events, exchange_events(), "declaring a roster changes nothing about the reply");

    let recorded = endpoint.only();
    assert_eq!(recorded.body[0], 0, "an ordinary data frame");
    let sent = proto::ClientMessage::decode_from_slice(&recorded.body[5..])
        .expect("the sent bytes are the client message");
    let declaration = sent
        .run_request
        .as_option()
        .and_then(|run| run.mcp_tools.as_option())
        .expect("the roster rides the run request");
    assert_eq!(
        declaration.mcp_tools.iter().filter_map(|tool| tool.name.as_deref()).collect::<Vec<_>>(),
        vec!["read", "bash"],
        "one definition per tool, in the order the engine advertised them"
    );
    let read = &declaration.mcp_tools[0];
    assert_eq!(read.provider_identifier.as_deref(), Some("ganja"));
    assert_eq!(read.tool_name.as_deref(), Some("read"));
    assert_eq!(
        read.input_schema_json.as_deref(),
        Some(r#"{"type":"object"}"#),
        "the schema rides field 6"
    );
    assert!(!read.input_schema.is_set(), "and never field 3");

    // ── History on the wire ──────────────────────────────────────────────
    // **AC-10.** A second turn names the first: the run request a real socket
    // received carries three root ids and one turn id, each a raw sha256 the
    // client's own store holds the bytes of, a conversation id derived from
    // the first message, and the newest turn as the action. What never
    // reaches the socket is any text of the history — the ids are all the
    // wire carries, the bytes wait in the store for the server's kv gets.
    endpoint.forget();
    endpoint.answers_with(Reply::ok("application/connect+proto", exchange_body()));

    let mut replied = Message::assistant("gpt-5.3-codex");
    replied.parts.push(ganja_provider::protocol::Part::text("It parses TOML."));
    let two_turn = ChatRequest {
        messages: vec![Message::user("What does this crate do?"), replied, Message::user("How?")],
        turn_start: 2,
        ..request()
    };
    let composed = history::compose(&two_turn);
    let events: Vec<ProviderEvent> = wire
        .stream(two_turn.clone(), CancellationToken::new())
        .await
        .expect("the exchange opens")
        .collect()
        .await;
    assert_eq!(events, exchange_events(), "carrying history changes nothing about the reply");

    let recorded = endpoint.only();
    assert_eq!(recorded.body[0], 0, "an ordinary data frame");
    let sent = proto::ClientMessage::decode_from_slice(&recorded.body[5..])
        .expect("the sent bytes are the client message");
    let run = sent.run_request.as_option().expect("a run request first");
    let state = run.conversation_state.as_option().expect("the state is present");
    assert_eq!(state.root_prompt_messages_json.len(), 3, "the head, the prompt, the reply");
    assert!(state.root_prompt_messages_json.iter().all(|id| id.len() == 32), "raw sha256 ids");
    assert_eq!(state.turns.len(), 1, "one turn before this one");
    assert_eq!(state.turns[0].len(), 32);
    assert_eq!(
        state.root_prompt_messages_json, composed.root,
        "the ids on the wire are the ids the composition minted"
    );
    assert_eq!(state.turns, composed.turns);
    let turn = proto::ConversationTurn::decode_from_slice(&composed.blobs[&state.turns[0]])
        .expect("the turn id names a turn blob in the store the Run answers from");
    let agent = turn.agent_conversation_turn.as_option().expect("the agent arm");
    assert_eq!(agent.user_message.as_deref().map(<[u8]>::len), Some(32), "a user blob id");
    assert_eq!(agent.steps.len(), 1, "one step: the reply");
    assert_eq!(
        run.conversation_id.as_deref(),
        Some(history::derived(&two_turn.messages[0].id).as_str())
    );
    assert_eq!(
        run.action
            .as_option()
            .and_then(|action| action.user_message_action.as_option())
            .and_then(|action| action.user_message.as_option())
            .and_then(|message| message.text.as_deref()),
        Some("How?"),
        "the newest turn is the action"
    );
    assert!(!run.mcp_tools.is_set());
    for redacted in [
        b"You are terse.".as_slice(),
        b"What does this crate do?".as_slice(),
        b"It parses TOML.".as_slice(),
    ] {
        assert!(
            !recorded.body.windows(redacted.len()).any(|window| window == redacted),
            "history travels as ids, never as text: {}",
            String::from_utf8_lossy(redacted)
        );
    }

    // ── Delivery is incremental ──────────────────────────────────────────
    // The body pauses after the first frame, so the first delta can only
    // arrive if the wire decodes frames as they land. A wire that buffered
    // until the body ended would leave `next()` waiting on a connection the
    // server is deliberately holding open — which the timeout turns into a
    // readable failure instead of a hang.
    endpoint.forget();
    let open = Arc::new(Notify::new());
    let first_frame = update_frame(thinking_update("Weighing a greeting.")).len();
    endpoint.answers_with(
        Reply::ok("application/connect+proto", exchange_body())
            .gated(first_frame, Arc::clone(&open)),
    );

    let mut streamed =
        wire.stream(request(), CancellationToken::new()).await.expect("the exchange opens");
    let first = timeout(PATIENCE, streamed.next())
        .await
        .expect("the first delta must arrive while the body is still open");
    assert_eq!(
        first,
        Some(ProviderEvent::ReasoningDelta("Weighing a greeting.".to_owned())),
        "decoded from a body whose remainder is deliberately unwritten"
    );

    open.notify_one();
    let rest: Vec<ProviderEvent> = streamed.collect().await;
    assert_eq!(rest, exchange_events()[1..].to_vec());

    // ── A cancel mid-stream ──────────────────────────────────────────────
    // Same gate, but the person leaves: after the first delta the token
    // fires, and the stream ends with neither a Finish nor a Failed — the
    // engine is what reads that as Cancelled, and it cannot if a verdict
    // arrives.
    endpoint.forget();
    let open = Arc::new(Notify::new());
    endpoint.answers_with(
        Reply::ok("application/connect+proto", exchange_body())
            .gated(first_frame, Arc::clone(&open)),
    );

    let cancel = CancellationToken::new();
    let mut streamed = wire.stream(request(), cancel.clone()).await.expect("the exchange opens");
    let first = timeout(PATIENCE, streamed.next())
        .await
        .expect("the first delta arrives before the cancel");
    assert_eq!(first, Some(ProviderEvent::ReasoningDelta("Weighing a greeting.".to_owned())));

    cancel.cancel();
    let rest: Vec<ProviderEvent> = streamed.collect().await;
    assert!(rest.is_empty(), "a cancelled stream ends without a verdict: {rest:?}");
    // Lets the server task finish writing into whatever is left of the
    // socket, so nothing outlives the phase.
    open.notify_one();

    // ── The recorded refusal ─────────────────────────────────────────────
    // The live probe's exact exchange: a heartbeat, then the EndStream
    // verdict. Under incremental delivery the turn has opened by the time
    // the verdict lands, so the refusal arrives inside the stream — the
    // terminal Failed every wire reports an in-body death with — keeping
    // the provider's own vocabulary intact.
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

    let events: Vec<ProviderEvent> = wire
        .stream(request(), CancellationToken::new())
        .await
        .expect("the exchange opens on a 200")
        .collect()
        .await;
    assert!(
        matches!(
            events.as_slice(),
            [ProviderEvent::Failed(ProviderError::Status { status: 400, message })]
                if message.contains("invalid_argument")
        ),
        "{events:?}"
    );

    // ── A dead credential, in-body ───────────────────────────────────────
    endpoint.forget();
    endpoint.answers_with(Reply::ok(
        "application/connect+proto",
        frame(END_STREAM, br#"{"error":{"code":"unauthenticated","message":"token expired"}}"#),
    ));

    let events: Vec<ProviderEvent> = wire
        .stream(request(), CancellationToken::new())
        .await
        .expect("the exchange opens on a 200")
        .collect()
        .await;
    let [ProviderEvent::Failed(expired)] = events.as_slice() else {
        panic!("a dead credential fails the turn in-stream: {events:?}");
    };
    let rendered = expired.to_string();
    assert!(matches!(expired, ProviderError::Auth(_)), "{rendered}");
    assert!(
        rendered.contains("ganja auth login cursor"),
        "the in-body verdict names the same repair the startup refusal does: {rendered}"
    );

    // ── An HTTP refusal ──────────────────────────────────────────────────
    // The 415 the live probe drew by sending the wrong content type: a
    // status outside 2xx is the provider answering before the first byte of
    // a stream existed, so it refuses the turn's opening — and it is not a
    // status worth retrying, so exactly one request is made.
    endpoint.forget();
    endpoint.answers_with(Reply::with("415 Unsupported Media Type", "text/plain", Vec::new()));

    let unsupported = wire
        .stream(request(), CancellationToken::new())
        .await
        .err()
        .expect("a non-2xx answer refuses the turn");
    assert!(matches!(unsupported, ProviderError::Status { status: 415, .. }), "{unsupported:?}");
    assert_eq!(endpoint.seen().len(), 1, "a 415 is not worth a second try");

    // ── The retry the other wires ride ───────────────────────────────────
    // One transient refusal, then the answer: the shared driver replays the
    // request before the first byte, and what it replays is byte-identical
    // — the same body under the same x-request-id, because the replay is
    // the same request rather than a new one wearing a fresh stamp.
    endpoint.forget();
    endpoint.answers_once(
        Reply::with("503 Service Unavailable", "text/plain", b"try later".to_vec())
            // Zero seconds so the schedule is exercised without the test
            // waiting out a real backoff.
            .header("retry-after", "0"),
    );
    endpoint.answers_with(Reply::ok("application/connect+proto", exchange_body()));

    let events: Vec<ProviderEvent> = wire
        .stream(request(), CancellationToken::new())
        .await
        .expect("the retry answers")
        .collect()
        .await;
    assert_eq!(events.last(), Some(&ProviderEvent::Finish(FinishReason::Completed)), "{events:?}");

    let seen = endpoint.seen();
    assert_eq!(seen.len(), 2, "one refusal, one replay");
    assert_eq!(
        seen[0].header("x-request-id"),
        seen[1].header("x-request-id"),
        "the replay is the same request under the same stamp"
    );
    assert_eq!(seen[0].body, seen[1].body, "and carries the same bytes");

    // ── A refusal echoing the credential ─────────────────────────────────
    // A provider that quotes back the token it rejected is a real shape,
    // and the shared driver's redaction — which the one-shot wire's local
    // twin lacked a zeroize behind — is what keeps it out of the error.
    endpoint.forget();
    endpoint.answers_with(Reply::with(
        "401 Unauthorized",
        "text/plain",
        format!("bad token {ACCESS}, go away").into_bytes(),
    ));

    let rejected = wire
        .stream(request(), CancellationToken::new())
        .await
        .err()
        .expect("a 401 is not answerable");
    let rendered = format!("{rejected} / {rejected:?}");
    assert!(matches!(rejected, ProviderError::Status { status: 401, .. }), "{rendered}");
    assert!(!rendered.contains(ACCESS), "{rendered}");
    assert!(rendered.contains("[redacted]"), "the echo is masked rather than dropped: {rendered}");

    // ── An in-body verdict echoing the credential ────────────────────────
    // The same echo, arriving the way this wire's failures actually arrive:
    // inside a 200, as an EndStream verdict carrying the server's own text
    // verbatim. The HTTP refusal above is masked by `retry::refusal`, which
    // never sees this frame — so the mid-stream seam has to mask it too, or
    // the one wire whose failures are all in-body is the one wire that
    // reports a credential back to the screen.
    endpoint.forget();
    endpoint.answers_with(Reply::ok(
        "application/connect+proto",
        frame(
            END_STREAM,
            format!(
                r#"{{"error":{{"code":"unauthenticated","message":"bad token {ACCESS}, go away"}}}}"#
            )
            .as_bytes(),
        ),
    ));

    let events: Vec<ProviderEvent> = wire
        .stream(request(), CancellationToken::new())
        .await
        .expect("the exchange opens on a 200")
        .collect()
        .await;
    let [ProviderEvent::Failed(echoed)] = events.as_slice() else {
        panic!("an in-body verdict fails the turn in-stream: {events:?}");
    };
    let rendered = format!("{echoed} / {echoed:?}");
    assert!(!rendered.contains(ACCESS), "{rendered}");
    assert!(rendered.contains("[redacted]"), "the echo is masked rather than dropped: {rendered}");

    // ── A redirect is refused where it stands ────────────────────────────
    // A 3xx is an instruction to send the request — and its bearer token —
    // somewhere else. `.invalid` never resolves, so a followed redirect
    // would surface as a transport error; a Status 302 is the proof it was
    // refused unfollowed, the bound every wire's client is built with.
    endpoint.forget();
    endpoint.answers_with(
        Reply::with("302 Found", "text/plain", Vec::new())
            .header("location", "http://elsewhere.invalid/agent.v1.AgentService/Run"),
    );

    let redirected = wire
        .stream(request(), CancellationToken::new())
        .await
        .err()
        .expect("a 302 is not an answer");
    assert!(
        matches!(redirected, ProviderError::Status { status: 302, .. }),
        "a redirect must be refused where it stands, not followed: {redirected:?}"
    );
    assert_eq!(endpoint.seen().len(), 1, "nothing followed the redirect anywhere");
}
