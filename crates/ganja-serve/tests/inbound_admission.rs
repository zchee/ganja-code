//! The admission gate's answers on the wire (**D523**): past the engine's
//! shape ladder, `POST /team/{name}/message` answers `200` whatever the
//! policy decided — an explicit refuse and every guard drop byte-identical
//! to an accept, a hold alone announced with its cause — while the ladder's
//! own refusals keep their statuses ahead of any policy. AC-8, AC-9, AC-10
//! and AC-11 at the HTTP layer, over a real bound Unix socket — plus the
//! observability leg: a hold and its settlement reach `GET /event`, so an
//! attached client learns of them without polling.
//!
//! Every server here is a real `ganja_serve::serve` over a real engine
//! leading a real (empty) team under a temporary home, spoken to with
//! `reqwest`'s own `unix_socket` client — `tests/team.rs`'s fixture with
//! the gate's policy dialled per test. No test imports the engine's note
//! constants (they are core's own): every byte-identity claim compares raw
//! responses, so what is pinned is the wire, never a constant equal to
//! itself. The two guard drops driven here are the two the wire can reach —
//! the bucket and the dedup window; a hop chain has no field on
//! [`ganja_core::SocketMessage`] to arrive in, so the hop drops are pinned
//! where synthetic chains exist to drive them, in core's own suite.

mod support;

use std::{path::PathBuf, sync::Arc};

use futures::StreamExt as _;
use ganja_core::{
    Engine,
    config::{DialogExpiry, InboundPolicy},
    permission::Permissions,
    teammate::TeammateRegistry,
    tool::Registry,
};
use ganja_protocol::{Command, HeldDecision, HoldCause, PolicySource};
use ganja_serve::Listen;
use ganja_testkit::{ScriptedProvider, says};
use support::{
    DEADLINE, Frame, SOCKET_URL, base_url, drain_frames, loopback_config, socket_client,
    with_listen,
};

/// The session every team here is led by — a fixed id, so the lead's name
/// in every answer is a fixed thing the byte comparisons can trust.
const SESSION: &str = "01998ad0-0000-7000-8000-0000000d0523";

/// The one peer identity these suites send as — shaped `<name>@<team>`, so
/// it passes the ladder and everything after it is the gate's to decide.
const PEER: &str = "w1@session-feedbeef";

/// An engine leading a team under `home` with the admission gate dialled to
/// `policy` — `tests/team.rs`'s lead with the one knob these suites turn.
/// [`None`] is the unset default: a prompting receiver, which accepts.
fn led_engine(
    home: &std::path::Path,
    policy: Option<(InboundPolicy, PolicySource)>,
) -> (Arc<Engine>, Arc<TeammateRegistry>) {
    let (provider, _requests) = ScriptedProvider::new(vec![says("hi")]);
    let registry = Arc::new(TeammateRegistry::for_session(home, SESSION, home));
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_inbound_policy(policy, DialogExpiry::default())
    .with_teammates(Arc::clone(&registry));

    (Arc::new(engine), registry)
}

/// A socket path short enough for `sun_path` on every platform this runs
/// on: the temp root, one directory the binder creates at `0700` for
/// itself, and a few bytes of name.
fn socket_path(home: &tempfile::TempDir, name: &str) -> PathBuf {
    home.path().join("run").join(name)
}

/// The engine at `path`, served — the socket bind every test here drives.
async fn socket_server(engine: &Arc<Engine>, path: &std::path::Path) -> ganja_serve::Handle {
    ganja_serve::serve(
        Arc::clone(engine),
        with_listen(Listen::Unix {
            path: path.to_owned(),
        }),
    )
    .await
    .expect("the socket server comes up")
}

/// The lead's inbox under `registry`, read as the JSON array §2.3 stores —
/// this crate does not link the team crate, and a test of the served route
/// should not need to; an inbox nothing has written is an absent file, and
/// empty.
fn lead_inbox(registry: &TeammateRegistry) -> Vec<serde_json::Value> {
    match std::fs::read(registry.lead_inbox()) {
        Ok(bytes) => serde_json::from_slice(&bytes).expect("an inbox is a JSON array"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("the inbox does not read: {error}"),
    }
}

/// A peer message carrying `text`, from the one identity every test sends
/// as.
fn message(text: &str) -> serde_json::Value {
    serde_json::json!({
        "from": PEER,
        "text": text,
        "summary": "inbound admission",
    })
}

/// One response, whole: status, headers and raw body — what the byte
/// comparisons compare and what the AC-8 evidence transcript renders.
struct Transcript {
    status: u16,
    version: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Transcript {
    async fn read(response: reqwest::Response) -> Self {
        let status = response.status().as_u16();
        let version = format!("{:?}", response.version());
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        let body = response.bytes().await.expect("a body").to_vec();

        Self {
            status,
            version,
            headers,
            body,
        }
    }

    /// The response as the raw HTTP a wire capture would show, titled — the
    /// AC-8 evidence pair is rendered through this.
    fn render(&self, title: &str) -> String {
        let mut lines = format!("--- {title} ---\n{} {}\n", self.version, self.status);
        for (name, value) in &self.headers {
            lines.push_str(&format!("{name}: {value}\n"));
        }
        lines.push('\n');
        lines.push_str(&String::from_utf8_lossy(&self.body));
        lines.push('\n');

        lines
    }
}

/// Posts `body` to the lead's message route through `client` and reads the
/// whole answer.
async fn post(client: &reqwest::Client, body: &serde_json::Value) -> Transcript {
    let response = client
        .post(format!("{SOCKET_URL}/team/team-lead/message"))
        .json(body)
        .send()
        .await
        .expect("the socket answers");

    Transcript::read(response).await
}

// ---------------------------------------------------------------------------
// AC-8
// ---------------------------------------------------------------------------

/// What policy decides, the wire does not say: an engine whose policy
/// refuses answers the same status and the same bytes as one that accepts,
/// and the difference lives where only the receiver can see it — the
/// accept wrote the lead's inbox, the refuse wrote nothing.
#[tokio::test]
async fn a_policy_refused_post_answers_byte_identically_to_an_accepted_one() {
    let accepting_home = ganja_testkit::temp_dir();
    let refusing_home = ganja_testkit::temp_dir();
    let (accepting, accepting_registry) = led_engine(accepting_home.path(), None);
    let (refusing, refusing_registry) = led_engine(
        refusing_home.path(),
        Some((InboundPolicy::Refuse, PolicySource::Global)),
    );
    let accepting_path = socket_path(&accepting_home, "a.sock");
    let refusing_path = socket_path(&refusing_home, "r.sock");
    let accepting_server = socket_server(&accepting, &accepting_path).await;
    let refusing_server = socket_server(&refusing, &refusing_path).await;

    let body = message("did W3 land");
    let accepted = post(&socket_client(&accepting_path), &body).await;
    let refused = post(&socket_client(&refusing_path), &body).await;

    assert_eq!(accepted.status, 200, "the accept is a plain 200");
    assert_eq!(
        refused.status, accepted.status,
        "and the refuse answers the same status"
    );
    assert_eq!(
        refused.body,
        accepted.body,
        "and the same bytes: accept {:?} vs refuse {:?}",
        String::from_utf8_lossy(&accepted.body),
        String::from_utf8_lossy(&refused.body),
    );

    assert_eq!(
        lead_inbox(&accepting_registry).len(),
        1,
        "the accepted message landed in the lead's inbox"
    );
    assert!(
        lead_inbox(&refusing_registry).is_empty(),
        "the refused one was never written"
    );

    // The raw pair, printed for the verify lane's evidence bundle — run
    // with `--no-capture` to read it off the terminal.
    eprintln!(
        "{}{}",
        accepted.render("AC-8 accept (policy unset, prompting receiver)"),
        refused.render("AC-8 refuse (cross_session_inbound: \"refuse\")"),
    );

    accepting_server
        .shutdown()
        .await
        .expect("the accepting server stops");
    refusing_server
        .shutdown()
        .await
        .expect("the refusing server stops");
}

// ---------------------------------------------------------------------------
// AC-9
// ---------------------------------------------------------------------------

/// A hold alone is announced: `200`, the note naming held and its cause —
/// while nothing reaches the inbox and the entry waits in the engine's held
/// list for a person to settle it.
#[tokio::test]
async fn a_held_post_answers_200_naming_the_hold_and_writes_no_inbox() {
    let home = ganja_testkit::temp_dir();
    let (engine, registry) = led_engine(
        home.path(),
        Some((InboundPolicy::Hold, PolicySource::Global)),
    );
    let path = socket_path(&home, "h.sock");
    let server = socket_server(&engine, &path).await;

    let held = post(&socket_client(&path), &message("hold this one")).await;

    assert_eq!(held.status, 200, "a hold is not an error");
    let delivered: serde_json::Value = serde_json::from_slice(&held.body).expect("a JSON answer");
    assert_eq!(delivered["to"], "team-lead");
    let note = delivered["note"].as_str().expect("a note");
    assert!(
        note.contains("held for a person's review"),
        "the note says held: {note}"
    );
    assert!(
        note.contains("an explicit hold policy from its global config"),
        "and names the cause: {note}"
    );
    assert!(
        note.contains("has not been delivered"),
        "and that nothing was delivered: {note}"
    );

    assert!(
        lead_inbox(&registry).is_empty(),
        "a held message never touches the inbox"
    );
    let held_list = engine.held_messages();
    assert_eq!(held_list.len(), 1, "the entry is in the engine's held list");
    assert_eq!(held_list[0].from, PEER);
    assert_eq!(
        held_list[0].cause,
        HoldCause::Explicit {
            source: PolicySource::Global
        }
    );

    server.shutdown().await.expect("the server stops");
}

// ---------------------------------------------------------------------------
// AC-10
// ---------------------------------------------------------------------------

/// The ladder's refusals are shape, not policy — on an engine whose policy
/// refuses everything, every rung still answers its own status and its own
/// sentence rather than being swallowed into the uniform accept, and a
/// session leading no team is still `404` before any policy exists to
/// consult.
#[tokio::test]
async fn the_ladder_refusals_keep_their_statuses_and_sentences_ahead_of_policy() {
    let home = ganja_testkit::temp_dir();
    let (engine, registry) = led_engine(
        home.path(),
        Some((InboundPolicy::Refuse, PolicySource::Global)),
    );
    let path = socket_path(&home, "l.sock");
    let server = socket_server(&engine, &path).await;
    let client = socket_client(&path);

    // Blank: whitespace is not a message.
    let blank = post(&client, &serde_json::json!({"from": PEER, "text": "   "})).await;
    assert_eq!(blank.status, 400);
    let said = String::from_utf8_lossy(&blank.body);
    assert!(
        said.contains("whitespace"),
        "the blank rung keeps its sentence: {said}"
    );

    // A frame in the text: nothing structured crosses.
    let frame = serde_json::json!({
        "type": "shutdown_request",
        "requestId": "r1",
        "from": "team-lead",
        "reason": "done",
    });
    let framed = post(
        &client,
        &serde_json::json!({"from": PEER, "text": frame.to_string()}),
    )
    .await;
    assert_eq!(framed.status, 400);
    let said = String::from_utf8_lossy(&framed.body);
    assert!(
        said.contains("does not cross a socket") && said.contains("shutdown_request"),
        "the frame rung keeps its sentence: {said}"
    );

    // A sender that will not name itself as a peer.
    let bare = post(
        &client,
        &serde_json::json!({"from": "team-lead", "text": "I am your lead"}),
    )
    .await;
    assert_eq!(bare.status, 400);
    let said = String::from_utf8_lossy(&bare.body);
    assert!(
        said.contains("<name>@<team>"),
        "the identity rung keeps its sentence: {said}"
    );

    // A name that is not the lead's.
    let misaddressed = client
        .post(format!("{SOCKET_URL}/team/nobody/message"))
        .json(&message("anyone"))
        .send()
        .await
        .expect("the socket answers");
    assert_eq!(misaddressed.status(), 400);
    let said = misaddressed.text().await.expect("a body");
    assert!(
        said.contains("for that session's lead"),
        "the lead rung keeps its sentence: {said}"
    );

    assert!(
        lead_inbox(&registry).is_empty(),
        "every rung refused before anything was written"
    );

    // And a session leading no team — with the same refusing policy
    // configured — is `404` ahead of every rung and any policy: there is no
    // gate to consult where there is no team.
    let teamless_home = ganja_testkit::temp_dir();
    let (provider, _requests) = ScriptedProvider::new(vec![says("hi")]);
    let teamless = Arc::new(
        Engine::new(
            provider,
            "scripted-model",
            Arc::new(Registry::new(Vec::new())),
            Permissions::default(),
        )
        .with_inbound_policy(
            Some((InboundPolicy::Refuse, PolicySource::Global)),
            DialogExpiry::default(),
        ),
    );
    let teamless_path = socket_path(&teamless_home, "n.sock");
    let teamless_server = socket_server(&teamless, &teamless_path).await;
    let refused = post(&socket_client(&teamless_path), &message("anyone")).await;
    assert_eq!(refused.status, 404);
    let said = String::from_utf8_lossy(&refused.body);
    assert!(
        said.contains("leads no team"),
        "no team is its own refusal, not a silent accept: {said}"
    );

    server.shutdown().await.expect("the server stops");
    teamless_server
        .shutdown()
        .await
        .expect("the teamless server stops");
}

// ---------------------------------------------------------------------------
// AC-11
// ---------------------------------------------------------------------------

/// One sender's thirty-first message inside the refill window is dropped by
/// the rate guard — and the wire says exactly what it said for the thirty
/// accepts, while the inbox holds thirty entries, not thirty-one.
#[tokio::test]
async fn the_thirty_first_message_from_one_sender_answers_like_an_accept_and_is_dropped() {
    let home = ganja_testkit::temp_dir();
    let (engine, registry) = led_engine(home.path(), None);
    let path = socket_path(&home, "b.sock");
    let server = socket_server(&engine, &path).await;
    let client = socket_client(&path);

    // Thirty distinct bodies, so the dedup window cannot trip first and the
    // drop, when it comes, is the bucket's.
    let first = post(&client, &message("message 0")).await;
    assert_eq!(first.status, 200);
    for i in 1..30 {
        let admitted = post(&client, &message(&format!("message {i}"))).await;
        assert_eq!(admitted.status, 200);
        assert_eq!(
            admitted.body, first.body,
            "every admit answers the same bytes"
        );
    }
    assert_eq!(
        lead_inbox(&registry).len(),
        30,
        "thirty admitted, thirty written"
    );

    let dropped = post(&client, &message("message 30")).await;
    assert_eq!(dropped.status, first.status);
    assert_eq!(
        dropped.body, first.body,
        "the drop is indistinguishable on the wire"
    );
    assert_eq!(
        lead_inbox(&registry).len(),
        30,
        "and the thirty-first was never written"
    );

    server.shutdown().await.expect("the server stops");
}

/// The same body twice inside the dedup window: the second is dropped by
/// the duplicate guard, answering the first's exact bytes, and the inbox
/// holds the message once.
#[tokio::test]
async fn a_repeated_body_inside_the_window_answers_like_an_accept_and_lands_once() {
    let home = ganja_testkit::temp_dir();
    let (engine, registry) = led_engine(home.path(), None);
    let path = socket_path(&home, "d.sock");
    let server = socket_server(&engine, &path).await;
    let client = socket_client(&path);

    let first = post(&client, &message("did W3 land")).await;
    let repeat = post(&client, &message("did W3 land")).await;

    assert_eq!(first.status, 200);
    assert_eq!(repeat.status, first.status);
    assert_eq!(
        repeat.body, first.body,
        "the duplicate is indistinguishable on the wire"
    );
    assert_eq!(lead_inbox(&registry).len(), 1, "and it landed exactly once");

    server.shutdown().await.expect("the server stops");
}

// ---------------------------------------------------------------------------
// Observability: the hold and its settlement on the event stream
// ---------------------------------------------------------------------------

/// One open `GET /event` connection, read frame by frame — the reader
/// `tests/replay_identity.rs` drives a whole turn through, here spanning a
/// hold and its settlement.
struct SseReader {
    // `axum::body::Bytes` is the same `bytes::Bytes` reqwest yields; naming
    // it through axum keeps the bytes crate out of this manifest.
    stream: futures::stream::BoxStream<'static, reqwest::Result<axum::body::Bytes>>,
    buffer: Vec<u8>,
    frames: Vec<Frame>,
}

impl SseReader {
    fn new(response: reqwest::Response) -> Self {
        Self {
            stream: response.bytes_stream().boxed(),
            buffer: Vec::new(),
            frames: Vec::new(),
        }
    }

    /// Reads until `done` says the frames collected so far are enough.
    async fn read_until(&mut self, mut done: impl FnMut(&[Frame]) -> bool) -> &[Frame] {
        while !done(&self.frames) {
            let chunk = tokio::time::timeout(DEADLINE, self.stream.next())
                .await
                .expect("the stream should keep speaking within the deadline")
                .expect("the stream should not end before the test does")
                .expect("the transport should not fail");
            self.buffer.extend_from_slice(&chunk);
            self.frames.extend(drain_frames(&mut self.buffer));
        }

        &self.frames
    }
}

/// The first engine event of `wanted` type among the message frames.
fn first_event(frames: &[Frame], wanted: &str) -> Option<serde_json::Value> {
    frames
        .iter()
        .filter(|frame| frame.event == "message")
        .find_map(|frame| {
            serde_json::from_str::<serde_json::Value>(&frame.data)
                .ok()
                .filter(|value| value["type"] == wanted)
        })
}

/// A hold and its settlement reach the SSE stream: `peer_held` names the id
/// and the cause the moment the POST is held, and a person's deny arrives
/// as `peer_hold_settled` naming the same id — so an attached client can
/// review holds it never polled for. The stream is TCP's and the POST is
/// the socket's, one engine serving both, exactly as a lead session runs.
#[tokio::test]
async fn a_hold_and_its_settlement_reach_the_event_stream() {
    let home = ganja_testkit::temp_dir();
    let (engine, registry) = led_engine(
        home.path(),
        Some((InboundPolicy::Hold, PolicySource::Global)),
    );
    let path = socket_path(&home, "e.sock");
    let socket = socket_server(&engine, &path).await;
    let tcp = ganja_serve::serve(Arc::clone(&engine), loopback_config())
        .await
        .expect("the TCP server comes up");

    // The stream first: registration precedes the response, so a reader
    // that has the connected frame cannot miss what the POST publishes.
    let response = reqwest::get(format!("{}/event", base_url(&tcp)))
        .await
        .expect("the event stream answers");
    assert_eq!(response.status(), 200);
    let mut reader = SseReader::new(response);
    reader
        .read_until(|frames| frames.iter().any(|frame| frame.event == "connected"))
        .await;

    let held = post(&socket_client(&path), &message("hold and watch")).await;
    assert_eq!(held.status, 200);

    let frames = reader
        .read_until(|frames| first_event(frames, "peer_held").is_some())
        .await;
    let held_event = first_event(frames, "peer_held").expect("just read");
    assert_eq!(held_event["session_id"], engine.session_id().as_str());
    assert_eq!(held_event["from"], PEER);
    assert_eq!(held_event["cause"]["kind"], "explicit");
    assert_eq!(held_event["cause"]["source"], "global");
    assert!(
        held_event["expires_in_ms"].is_null(),
        "an explicit hold installs no timer: {held_event}"
    );

    // The wire id names the engine's own hold — settle exactly that one.
    let held_list = engine.held_messages();
    assert_eq!(held_list.len(), 1);
    assert_eq!(
        serde_json::to_value(&held_list[0].id).expect("an id serializes"),
        held_event["id"],
        "the event names the entry the engine holds"
    );
    engine
        .send(Command::SettleHeld {
            id: held_list[0].id.clone(),
            decision: HeldDecision::Deny,
        })
        .await
        .expect("the settle is taken");

    let frames = reader
        .read_until(|frames| first_event(frames, "peer_hold_settled").is_some())
        .await;
    let settled = first_event(frames, "peer_hold_settled").expect("just read");
    assert_eq!(
        settled["id"], held_event["id"],
        "the settlement names the hold"
    );
    assert_eq!(settled["outcome"], "denied");

    assert!(
        engine.held_messages().is_empty(),
        "the hold left the engine's list"
    );
    assert!(
        lead_inbox(&registry).is_empty(),
        "and a denied socket-door hold wrote nothing"
    );

    tcp.shutdown().await.expect("the TCP server stops");
    socket.shutdown().await.expect("the socket server stops");
}
