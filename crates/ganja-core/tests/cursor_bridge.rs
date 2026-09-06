//! The cursor tool bridge, end to end: a real [`Engine`] answering a real
//! cursor agent backend's `mcp_exec` asks over a real duplex socket (**D552**,
//! W4 of `.omc/plans/2026-09-04-cursor-tool-bridge.md`).
//!
//! The specification is the live recording at
//! `crates/ganja-provider/tests/fixtures/cursor-mcp-tools-probe.txt`, not this
//! file's prose: where the two disagree the recording is right. What it
//! measured, and what these tests replay, is that cursor's server offers and
//! calls a tool the client declared under `provider_identifier: "ganja"`, that
//! a 25-second hold on the run-level `client_heartbeat` alone survives, and
//! that a typed refusal keeps the turn alive and the model adapting.
//!
//! **Why this suite exists and `ganja-provider`'s own does not cover it.** An
//! `mcp_exec` is answered by the *engine* — the permission ladder, the tool
//! registry, the transcript — while the frames that carry it are the *wire*'s.
//! `ganja-provider` may not name `ganja-core` (`depgate.toml`), so the only
//! party that can drive both ends of one round trip is this crate. The
//! duplex-capable server the tests run against is
//! [`ganja_testkit::cursor_server`]; the transport assumption it rests on is
//! the first test below, measured rather than assumed.
//!
//! **On `depgate` and AC-17.** `cargo depgate check --config depgate.toml`
//! proves crate *reachability* — that `ganja-provider` cannot name the tool
//! registry, that nothing beneath the engine reaches the engine. It cannot
//! prove an *execution site*: a wire that reached a tool through some value
//! handed to it would satisfy every dependency rule in the file. The
//! invariant that a bridged call executes where every other tool call
//! executes — behind the same permission dialog, into the same transcript —
//! is what the AC-17 tests below assert, and nothing in `depgate.toml`
//! asserts it.
//!
//! **D553** (`.omc/plans/2026-09-07-cursor-history-blobs.md`, W3) added the
//! turn a server *drops*: a Run held for the engine whose request body the
//! server closes. The transport claim under it — that hyper fails the held
//! body's sender once the connection has FINed, so the keeper's next beat
//! drops the Run as `Reason::Closed` rather than the idle bound doing so ten
//! minutes later — is the pin
//! [`a_hangup_under_a_held_body_fails_the_keepers_next_beat`], measured
//! before the recovery tests build on it. The drop is always a
//! [`Step::Hangup`] the test releases, never a timeout.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::BoxStream;
use futures::{SinkExt as _, StreamExt as _};
use ganja_core::Engine;
use ganja_core::permission::{Action, Permissions, Rule};
use ganja_core::protocol::{
    Command, Event, FinishReason, Message, Part, PartBody, PartId, PermissionId, PermissionReply,
    ToolState,
};
use ganja_core::provider::cursor::proto;
use ganja_core::provider::{CredentialSource, CursorProvider};
use ganja_core::tool::Registry;
use ganja_testkit::cursor_server::{CursorServer, KvAnswer, Step, decoded, finished};
use ganja_testkit::{LogCapture, RecorderTool, drain, drain_answering};
use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio::time::timeout;

/// The model every seat here asks for.
const MODEL: &str = "gpt-5.3-codex";

/// The run-level heartbeat's cadence, restated: the wire's own constant is
/// crate-private, and the heartbeat test below already says the number in
/// its own voice.
const HEARTBEAT: Duration = Duration::from_secs(5);

/// Room for a slow machine to schedule the beat that lands on the bound.
const SCHEDULING: Duration = Duration::from_secs(2);

/// The keeper's own line for a held Run leaving the table — the line probe
/// 2 reads for the same fact.
const DROPPED: &str = "dropping a held run";

/// The clause `Reason::Closed` renders as, wherever the drop is named.
const CLOSED: &str = "cursor closed the request body under it";

/// What the client writes only after the response has started.
const SECOND: &[u8] = b"second";

/// The token every wire here presents. A canary rather than a plausible
/// credential, though nothing in this file looks for it: the mock server never
/// reads the `authorization` header at all, because the point of handing the
/// wire a token explicitly is that no store is consulted.
///
/// That the cursor wire keeps a credential out of its renderings and its log is
/// checked in `crates/ganja-core/tests/secrets_env.rs`'s cursor arm.
const TOKEN: &str = "at-cursor-bridge-canary-AAAA";

/// How long a byte that should already be in flight may take. Generous
/// because CI machines stall; reached only when the transport buffers.
const PATIENCE: Duration = Duration::from_secs(10);

/// How long a finished turn's tail — the title one-shot among it — may take.
const SETTLE: Duration = Duration::from_secs(20);

/// A request body the client keeps writing **after** the response headers and
/// a first response chunk have already arrived reaches the server.
///
/// Cursor's Run RPC is a duplex: the server generates by *asking* — a context
/// exec, kv blob gets and sets, and (since D552) `mcp_exec` calls whose answers
/// ganja writes back up the still-open request body. Every other test in this
/// file, and the mock server they drive, is worthless if hyper buffers or
/// closes that direction once a response is in flight.
///
/// So it is measured here, over the same stack the wire uses — reqwest's
/// `Body::wrap_stream` over a `futures::channel::mpsc` — against a hand-rolled
/// loopback endpoint that answers before it has read the whole request.
#[tokio::test]
async fn a_request_body_written_after_the_response_began_still_reaches_the_server() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback binds");
    let address = listener.local_addr().expect("the bound port is readable");
    let (report, seen) = tokio::sync::oneshot::channel::<Vec<u8>>();

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("the client connects");

        // The head only: everything after it is the chunked request body, and
        // reading it here is exactly what must not be required before answering.
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut byte).await.expect("the head arrives whole");
            head.push(byte[0]);
        }

        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\n\
                  content-type: application/connect+proto\r\n\
                  transfer-encoding: chunked\r\n\r\n",
            )
            .await
            .expect("the response head is written");
        socket.write_all(b"5\r\nfirst\r\n").await.expect("one response chunk is written");
        socket.flush().await.expect("and reaches the client");

        // Now — response open, one chunk delivered — read on for the half the
        // client has not written yet.
        let mut body = Vec::new();
        loop {
            let mut buffer = [0_u8; 1024];
            let read = socket.read(&mut buffer).await.expect("the socket stays readable");
            if read == 0 {
                break;
            }
            body.extend_from_slice(&buffer[..read]);
            if body.windows(SECOND.len()).any(|window| window == SECOND) {
                break;
            }
        }
        report.send(body).expect("the probe is still waiting");
    });

    let (mut writes, body) = futures::channel::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(4);
    writes.send(Ok(b"opening".to_vec())).await.expect("the body channel is open");

    let response = reqwest::Client::builder()
        .build()
        .expect("a default client builds")
        .post(format!("http://{address}/duplex"))
        .body(reqwest::Body::wrap_stream(body))
        .send()
        .await
        .expect("the endpoint answers before it has read the request body");
    assert!(response.status().is_success());

    let mut chunks = response.bytes_stream();
    let first = timeout(PATIENCE, chunks.next())
        .await
        .expect("the first response chunk is not buffered")
        .expect("the stream has a chunk")
        .expect("and it decodes");
    assert_eq!(&first[..], b"first", "the response is live before the request body has ended");

    // The whole question, in one line: this write happens strictly after the
    // response began.
    writes.send(Ok(SECOND.to_vec())).await.expect("the body channel is still open");

    let delivered = timeout(PATIENCE, seen)
        .await
        .expect("the server sees the late write within the patience window")
        .expect("the server task reports rather than panicking");
    assert!(
        delivered.windows(SECOND.len()).any(|window| window == SECOND),
        "hyper must keep sending the request body while the response is incomplete",
    );
}

/// A Run whose request body is **not** chunked is refused readably rather than
/// read forever as protobuf that never parses: the guard at the head of the
/// fixture's `serve_one`, whose comment says why a silent de-chunker is the
/// worse failure.
#[tokio::test]
async fn a_run_whose_body_is_not_chunked_is_refused_with_a_reason() {
    let server = CursorServer::start(finished()).await;

    // `body` over a sized value rather than a stream, which is what makes
    // reqwest declare a `content-length` instead of chunking.
    let refusal = reqwest::Client::builder()
        .build()
        .expect("a default client builds")
        .post(format!("{}/Run", server.base_url()))
        .body(vec![0_u8; 8])
        .send()
        .await
        .expect("the fixture answers rather than hanging");

    assert_eq!(refusal.status().as_u16(), 400, "an un-chunked Run body is a fixture misuse");
    let complaint = refusal.text().await.expect("the refusal carries its reason");
    assert!(
        complaint.contains("transfer-encoding: chunked"),
        "and the reason names what was missing, got {complaint:?}",
    );
}

/// A dropped [`CursorServer`] stops accepting.
///
/// The accept loop owns the listener, so nothing else can close it: without a
/// `Drop` the task runs until it errors, which is never. Under nextest that is
/// bounded by the process-per-test — but a plain `cargo test` of this binary
/// runs every test in one process, and each server would stay bound and
/// accepting for the life of it.
#[tokio::test]
async fn a_dropped_server_stops_accepting_connections() {
    let address = {
        let server = CursorServer::start(finished()).await;
        let address = server
            .base_url()
            .trim_start_matches("http://")
            .parse::<std::net::SocketAddr>()
            .expect("the base URL is host:port");
        // Still live while the server is: this is what the assertion below is
        // a change *from*, rather than an address that never worked.
        tokio::net::TcpStream::connect(address).await.expect("a live server accepts");

        address
    };

    // `abort` is a request, not a join, so the listener closes a moment later.
    ganja_testkit::eventually(
        PATIENCE,
        "the dropped server's port to stop accepting",
        async || tokio::net::TcpStream::connect(address).await.err().map(|_| ()),
    )
    .await;
}

/// **The transport pin under D553's recovery** (W3, step 9). A Run held open
/// for the engine is a request body the *keeper* beats on every
/// [`HEARTBEAT`]; when the server hangs up under it, the held Run must leave
/// the table as `Reason::Closed` on the keeper's next beat — not on the idle
/// bound, ten minutes on. That is a claim about hyper: that a `wrap_stream`
/// request body's receiver is dropped once the connection has FINed with the
/// exchange unfinished, so the sender's next write fails. Measured here,
/// over the stack the wire uses, before AC-16 builds on it.
///
/// The hangup is released once the dialog is up, because that is the proof
/// the Run is held ([`Step::Hangup`] says why nothing earlier is); the drop
/// is read off the keeper's own line, `dropping a held run … reason=Closed`,
/// the line probe 2 reads for the same fact. Two heartbeats is the bound the
/// plan states; the first is where the drop is expected, because the FIN
/// reaches hyper's read side while the response is mid-body and it closes
/// the connection there, well before any beat has to be written.
///
/// Refused afterwards, so the pin runs nothing and claims nothing about what
/// the resume finds — that is AC-16's.
#[tokio::test]
async fn a_hangup_under_a_held_body_fails_the_keepers_next_beat() {
    let (log, _guard) = capturing();
    let server = CursorServer::start(vec![
        Step::Context,
        Step::mcp(1, "lookup", &json!({"key": "alpha"})).no_wait(),
        Step::Hangup,
    ])
    .await;

    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "the answer");
    let engine = seated(&server, Registry::new(vec![tool]), rule("lookup", Action::Ask));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts a prompt");
    let (dialog, mut seen) = held_at_dialog(&mut events).await;
    assert!(
        log.logged().contains("holding a run open for the engine"),
        "the dialog is raised by a held Run, got:\n{}",
        log.logged(),
    );
    assert!(!log.logged().contains(DROPPED), "and nothing is dropped while the socket is up");

    let hung_up = tokio::time::Instant::now();
    let logged = hang_up_and_wait_for_drop(&server, &log, 1).await;
    let elapsed = hung_up.elapsed();

    let line = logged
        .lines()
        .find(|line| line.contains(DROPPED))
        .expect("the wait returned on the drop line");
    assert!(
        line.contains("reason=Closed"),
        "the drop names the closed body, not the idle bound or a cancel: {line}",
    );
    assert!(
        elapsed <= 2 * HEARTBEAT + SCHEDULING,
        "the keeper noticed within two beats, took {elapsed:?}",
    );

    engine
        .send(Command::ReplyPermission { id: dialog, reply: PermissionReply::Reject })
        .await
        .expect("a reply is never refused");
    seen.extend(drain(&mut events).await);
    assert!(calls.lock().expect("the call log is never poisoned").is_empty(), "refused, so unrun");
    assert!(
        matches!(seen.last(), Some(Event::MessageFinished { .. })),
        "and the turn ends rather than hanging on a body nobody holds",
    );
}

/// An engine on `server`, holding `tools`, gated by `permissions`.
fn seated(server: &CursorServer, tools: Registry, permissions: Permissions) -> Engine {
    Engine::new(Arc::new(provider_at(server)), MODEL, Arc::new(tools), permissions)
}

/// A cursor provider pointed at `server`, presenting a token this file owns.
///
/// **No credential store is involved** (lead ruling, **Dv-11**): `at` takes its
/// credential explicitly, so this suite has no code path to `auth.json` at all
/// — structural, rather than an `XDG_DATA_HOME` redirect pointing away from it.
/// That is also what lets this be one binary holding every test in this file:
/// `ganja_testkit::redirect_xdg_data_home` is `unsafe` with a documented
/// one-test-per-binary invariant, and every other XDG-mutating binary in this
/// tree holds exactly one test.
fn provider_at(server: &CursorServer) -> CursorProvider {
    CursorProvider::at(server.base_url(), CredentialSource::key(TOKEN).expect("a non-blank token"))
        .expect("loopback may carry a token")
}

/// One prompt, with nothing attached.
fn prompt() -> Command {
    saying("what does this crate do")
}

/// A prompt saying `text`, with nothing attached.
fn saying(text: &str) -> Command {
    Command::SendPrompt {
        text: text.to_owned(),
        mentions: Vec::new(),
        skills: Vec::new(),
        session_mentions: Vec::new(),
        peers: Vec::new(),
    }
}

/// One rule on `tool` and nothing else: an ask where the test is about the
/// dialog, an allow where a test about the bridge should not be one about
/// dialogs.
fn rule(tool: &str, action: Action) -> Permissions {
    rules(&[(tool, action)])
}

/// The same, one rule per named tool.
fn rules(rules: &[(&str, Action)]) -> Permissions {
    let mut permissions = Permissions::default();
    permissions.set_baseline(
        rules
            .iter()
            .map(|(tool, action)| Rule {
                permission: (*tool).to_owned(),
                pattern: "*".to_owned(),
                action: action.clone(),
            })
            .collect(),
    );

    permissions
}

/// A `tracing` capture at DEBUG for the calling thread — which, under the
/// current-thread runtime every test here runs on, is every task the engine
/// and the wire spawn: the keeper that drops a held Run logs from one, and
/// the recovery that reopens it logs from the turn.
///
/// Thread-local rather than global, because this binary holds many tests and
/// a plain `cargo test` runs them on parallel threads; the guard must live
/// as long as the test does.
fn capturing() -> (LogCapture, tracing::subscriber::DefaultGuard) {
    let capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);

    (capture, guard)
}

/// Reads events up to the dialog a bridged call raises, handing back its id
/// and everything seen. Once the dialog is up the Run is **held**: the wire
/// files the fold in the held-run table before it hands the engine the call
/// events that raise it, so this is the moment a test may hang up under it.
///
/// A turn that finishes before any dialog is a failure named here, not a
/// wait on a stream that never ends: the engine outlives its turns, so
/// `events` has no end to reach.
async fn held_at_dialog(events: &mut BoxStream<'static, Event>) -> (PermissionId, Vec<Event>) {
    let mut seen = Vec::new();
    loop {
        let event = events.next().await.expect("the dialog arrives before the stream ends");
        let waiting = match &event {
            Event::PermissionRequested { id, .. } => Some(id.clone()),
            _ => None,
        };
        let finished = matches!(event, Event::MessageFinished { .. });
        seen.push(event);
        if let Some(id) = waiting {
            return (id, seen);
        }
        assert!(
            !finished,
            "the turn finished before a bridged call raised its dialog: {:?}",
            seen.last()
        );
    }
}

/// Hangs up on `server` and waits for the keeper to drop the held Run — the
/// `nth` drop this log has seen — within the two heartbeats the transport
/// pin bounds it at, handing back the log so the caller can read the reason.
///
/// A wait on a line the keeper writes rather than a sleep: the drop lands on
/// the first beat after the FIN in practice, and a sleep sized for the bound
/// would spend the bound every time.
async fn hang_up_and_wait_for_drop(server: &CursorServer, log: &LogCapture, nth: usize) -> String {
    server.hang_up();

    ganja_testkit::eventually(
        2 * HEARTBEAT + SCHEDULING,
        "the keeper's beat to fail on the hung-up body",
        async || {
            let logged = log.logged();
            (logged.matches(DROPPED).count() >= nth).then_some(logged)
        },
    )
    .await
}

/// The text of one root entry as the composition renders it: a bare string
/// for the system head, the joined `text` items for everything else.
fn root_text(answer: &KvAnswer) -> String {
    let bytes = answer.data.as_deref().expect("a composed root id is found");
    let entry: serde_json::Value =
        serde_json::from_slice(bytes).expect("a root entry is the JSON the reference reads");
    match &entry["content"] {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) => {
            items.iter().filter_map(|item| item["text"].as_str()).collect::<Vec<_>>().join("")
        }
        other => panic!("a root entry's content is a string or a list, got {other}"),
    }
}

/// The answer recorded for `id`, which every composed id has exactly one of.
fn answer_for<'a>(answers: &'a [KvAnswer], id: &[u8]) -> &'a KvAnswer {
    let matching: Vec<&KvAnswer> = answers.iter().filter(|answer| answer.blob_id == id).collect();
    let [answer] = matching.as_slice() else {
        panic!("one get per composed id, got {} for {}", matching.len(), hex(id));
    };

    answer
}

/// A blob id's leading hex, the spelling the wire logs one under.
fn hex(id: &[u8]) -> String {
    id.iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}

/// The `Tool` parts a drained turn left in the transcript, as
/// `(call_id, state)`.
fn tool_parts(events: &[Event]) -> Vec<(String, ToolState)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::PartUpdated { part, .. } | Event::PartStarted { part, .. } => match &part.body {
                PartBody::Tool { call_id, state, .. } => Some((call_id.clone(), state.clone())),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// The assistant's text in a drained turn, as a frontend applying the stream
/// would hold it: one entry per `Text` part, in part order.
///
/// Streamed text opens as an **empty** [`Event::PartStarted`] and grows by
/// [`Event::PartDelta`] fragments addressed by part id, so reading the parts
/// alone finds every streamed part empty and would assert nothing. Applying
/// the deltas is what makes this a claim about what the turn actually said.
fn said(events: &[Event]) -> Vec<String> {
    let mut texts: BTreeMap<PartId, String> = BTreeMap::new();

    for event in events {
        match event {
            Event::PartStarted { part, .. } | Event::PartUpdated { part, .. } => {
                if let PartBody::Text { text } = &part.body {
                    texts.insert(part.id.clone(), text.clone());
                }
            }
            Event::PartDelta { part_id, delta, .. } => {
                if let Some(text) = texts.get_mut(part_id) {
                    text.push_str(delta);
                }
            }
            _ => {}
        }
    }

    texts.into_values().collect()
}

/// **AC-12.** The whole round trip, with nothing refusing anything: the roster
/// reaches the server on the context answer, the server calls one of its tools
/// with real arguments, the tool runs exactly once with those arguments, its
/// output goes back as `mcp_result.success`, and the turn finishes clean with
/// the call in the transcript.
#[tokio::test]
async fn a_declared_tool_the_server_calls_runs_once_and_answers_with_its_output() {
    // A string, an integer, and a nested object: the integer is deliberate.
    // `google.protobuf.Value` has only a `double` arm, so `40` goes out as
    // `40.0` and comes back an integer only because `value::decode` restores
    // the spelling for an integral double. `serde_json::Value`'s own equality
    // separates `Number(2000)` from `Number(2000.0)`, so the assertion below
    // fails if that restoration ever regresses — and it would fail *in the
    // tool*, as a real `read` refusing "invalid type: floating point" for a
    // limit the model spelled correctly.
    let args = json!({"key": "alpha", "limit": 2000, "nested": {"deep": [1, "two", true]}});
    let server = CursorServer::start(vec![
        Step::Context,
        Step::mcp(1, "lookup", &args),
        Step::Text("found it".to_owned()),
        Step::TurnEnded,
        Step::EndStream,
    ])
    .await;

    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "the answer");
    let engine = seated(&server, Registry::new(vec![tool]), rule("lookup", Action::Allow));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts a prompt");
    let seen = drain(&mut events).await;

    let contexts = server.context_answers();
    let [roster] = contexts.as_slice() else {
        panic!("one turn asks for its context once, got {} of them", contexts.len());
    };
    let entry = roster
        .tools
        .iter()
        .find(|definition| definition.tool_name.as_deref() == Some("lookup"))
        .expect("the roster the server was given names the tool");
    assert_eq!(
        entry.provider_identifier.as_deref(),
        Some("ganja"),
        "under the identifier the recording saw the server call back with",
    );
    assert!(
        entry.input_schema_json.as_deref().is_some_and(|schema| !schema.is_empty()),
        "declared on field 6 alone, which the recording measured is enough",
    );
    assert!(
        entry.input_schema.is_unset(),
        "and never on field 3, which is modelled and deliberately never filled",
    );

    let recorded = calls.lock().expect("the call log is never poisoned").clone();
    assert_eq!(
        recorded,
        vec![args],
        "the tool ran exactly once, with the server's own arguments — the integral one \
         surviving the double-only wire as an integer, not 2000.0",
    );

    let results = server.mcp_results();
    let [result] = results.as_slice() else {
        panic!("one exec is one answer, got {results:?}");
    };
    let success = result.success.as_option().expect("a tool that ran answers `success`");
    assert_ne!(success.is_error, Some(true), "a tool that ran and did not fail is not an error");
    let text: Vec<&str> = success
        .content
        .iter()
        .filter_map(|item| item.text.as_option().and_then(|content| content.text.as_deref()))
        .collect();
    assert_eq!(text, vec!["the answer"], "the tool's own output is what went back");

    assert!(
        matches!(seen.last(), Some(Event::MessageFinished { reason: FinishReason::Completed, .. })),
        "the turn ends in a finish, not a failure",
    );
    let parts = tool_parts(&seen);
    assert!(
        parts.iter().any(|(_, state)| matches!(state, ToolState::Completed { .. })),
        "a bridged call closes as a completed Tool part like any other, got {parts:?}",
    );
    // The AC's "and the text continues": what the server said *after* the tool
    // result is the proof the turn read on past the bridged exec rather than
    // ending on it.
    let continued = said(&seen);
    assert!(
        continued.iter().any(|line| line == "found it"),
        "the text the server sent after the tool result reached the transcript, got {continued:?}",
    );
}

/// **AC-13.** The same call behind an *ask* rule, refused at the dialog: the
/// tool never runs, the server is told `rejected` in the engine's own refusal
/// sentence, and the turn survives — a refusal is information, not an abort.
#[tokio::test]
async fn a_call_refused_at_the_dialog_never_runs_and_goes_back_as_rejected() {
    let args = json!({"key": "alpha"});
    let server = CursorServer::start(vec![
        Step::Context,
        Step::mcp(1, "lookup", &args),
        Step::Text("understood".to_owned()),
        Step::TurnEnded,
        Step::EndStream,
    ])
    .await;

    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "the answer");
    let engine = seated(&server, Registry::new(vec![tool]), rule("lookup", Action::Ask));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts a prompt");
    let seen = drain_answering(&engine, &mut events, PermissionReply::Reject).await;

    assert!(
        seen.iter().any(|event| matches!(event, Event::PermissionRequested { .. })),
        "a bridged call raises the dialog, which is the whole point of AC-13",
    );
    assert!(
        calls.lock().expect("the call log is never poisoned").is_empty(),
        "a rejected call must not run",
    );

    let results = server.mcp_results();
    let [result] = results.as_slice() else {
        panic!("one exec is one answer, got {results:?}");
    };
    let rejected =
        result.rejected.as_option().expect("a refusal answers `rejected`, never `error`");
    assert_eq!(
        rejected.reason.as_deref(),
        Some(ganja_core::tool::permission_text::REJECTED),
        "the server reads the same sentence the model does — the hoisted constant, D552's Dv-8",
    );
    assert!(result.success.is_unset(), "and never the failed-tool shape");

    assert!(
        matches!(seen.last(), Some(Event::MessageFinished { reason: FinishReason::Completed, .. })),
        "the turn survives its refusal",
    );

    // The transcript's own record of the same refusal: the part a
    // `ScriptedProvider` denial produces, asserted field by field rather than
    // by comparing two whole turns, because the ids and timestamps differ and
    // normalizing them would assert less than naming the two fields does.
    let parts = tool_parts(&seen);
    let error = parts
        .iter()
        .find_map(|(_, state)| match state {
            ToolState::Error { error, input, .. } => Some((error.clone(), input.clone())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("a refused call closes as an Error part, got {parts:?}"));
    assert_eq!(error.0, ganja_core::tool::permission_text::REJECTED);
    assert_eq!(error.1, args, "the arguments travel with it, as they do for every other refusal");
}

/// **AC-23** at engine level (**Dv-3**): an `mcp_args` exec carrying
/// `smart_mode_approval_only = 7` is a *preflight*, not a call. It is answered
/// `approved` and nothing runs — no tool, no dialog, no `Tool` part.
///
/// The wire proves the same thing over an in-memory duplex; this proves it
/// where a call would really have executed, which is the only place "nothing
/// executed" is a measurement rather than a property of the harness. The real
/// call the server sends *after* an approval is an ordinary exec and is gated
/// by the ordinary dialog — that is AC-13's, not this test's.
#[tokio::test]
async fn an_approval_preflight_is_approved_at_the_engine_without_running_anything() {
    let args = json!({"key": "alpha"});
    let server = CursorServer::start(vec![
        Step::Context,
        Step::mcp(1, "lookup", &args).approval_only(),
        Step::TurnEnded,
        Step::EndStream,
    ])
    .await;

    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "the answer");
    // Allowed, deliberately: under an `ask` rule an absent dialog could mean
    // the preflight was skipped *or* that the engine never got that far. With
    // the tool allowed, the only thing standing between this exec and a real
    // invocation is the flag.
    let engine = seated(&server, Registry::new(vec![tool]), rule("lookup", Action::Allow));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts a prompt");
    let seen = drain(&mut events).await;

    let results = server.mcp_results();
    let [result] = results.as_slice() else {
        panic!("one exec is one answer, got {results:?}");
    };
    assert!(result.approved.is_set(), "a preflight is answered `approved`, got {result:?}");
    assert!(result.success.is_unset(), "and never as a tool that ran");

    assert!(
        calls.lock().expect("the call log is never poisoned").is_empty(),
        "an approval preflight must not run the tool it names",
    );
    assert!(
        !seen.iter().any(|event| matches!(event, Event::PermissionRequested { .. })),
        "nor raise a dialog for a call that has not been made",
    );
    assert!(
        tool_parts(&seen).is_empty(),
        "nor leave a Tool part, which is what the transcript would show a call as",
    );
    assert!(
        matches!(seen.last(), Some(Event::MessageFinished { reason: FinishReason::Completed, .. })),
        "and the turn finishes normally",
    );
}

/// The run-level `client_heartbeat = 7` really crosses a socket while a turn is
/// held, which is what the recording's (b) measured a 25-second hold surviving
/// on.
///
/// Held at a **permission dialog**, because that is the hold a person actually
/// causes and the one with no upper bound: everything else a turn waits on is
/// bounded by something. The wire's own equivalent runs under
/// `tokio::time::pause`, which proves the cadence and not the socket; this is
/// the only place both are true at once, so it is worth the wall clock — and
/// the only test in this suite that costs any.
#[tokio::test]
async fn a_turn_held_at_a_dialog_keeps_beating_on_the_body_it_left_open() {
    /// Longer than one `client_heartbeat` interval (5 s) with room for a slow
    /// machine to schedule the first one.
    const HELD: Duration = Duration::from_millis(5_500);

    let server = CursorServer::start(vec![
        Step::Context,
        Step::mcp(1, "lookup", &json!({"key": "alpha"})),
        Step::TurnEnded,
        Step::EndStream,
    ])
    .await;

    let (tool, _calls) = RecorderTool::new("lookup", "lookup ran", "the answer");
    let engine = seated(&server, Registry::new(vec![tool]), rule("lookup", Action::Ask));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts a prompt");

    // Answered by hand rather than through `drain_answering`, because the whole
    // measurement is what happens *between* the dialog and the reply.
    let mut seen = Vec::new();
    let held = loop {
        let event = events.next().await.expect("the dialog arrives before the stream ends");
        let waiting = match &event {
            Event::PermissionRequested { id, .. } => Some(id.clone()),
            _ => None,
        };
        seen.push(event);
        if let Some(id) = waiting {
            break id;
        }
    };

    let before = server.heartbeats();
    tokio::time::sleep(HELD).await;
    let during = server.heartbeats();

    engine
        .send(Command::ReplyPermission { id: held, reply: PermissionReply::Once })
        .await
        .expect("a reply is never refused");
    seen.extend(drain(&mut events).await);

    assert!(
        during > before,
        "a run held {HELD:?} at a dialog should have beaten at least once on its \
         open request body, and went from {before} to {during}",
    );
    assert!(
        matches!(seen.last(), Some(Event::MessageFinished { reason: FinishReason::Completed, .. })),
        "and the held turn still finishes once the dialog is answered",
    );
}

/// **Two execs in one batch**: both are written before either is answered, so
/// both are genuinely in flight and the client's answers are matched by `id`
/// rather than by arrival order. End to end this exercises the wire's gather
/// window, the engine's `resolve` fan-out and the transcript, over the real
/// socket.
#[tokio::test]
async fn two_execs_the_server_sent_together_are_both_run_and_both_answered() {
    let first = json!({"key": "alpha"});
    let second = json!({"key": "beta"});
    let server = CursorServer::start(vec![
        Step::Context,
        Step::batch(vec![Step::mcp(1, "lookup", &first), Step::mcp(2, "lookup", &second)]),
        Step::TurnEnded,
        Step::EndStream,
    ])
    .await;

    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "the answer");
    let engine = seated(&server, Registry::new(vec![tool]), rule("lookup", Action::Allow));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts a prompt");
    let seen = drain(&mut events).await;

    // Both ran, with their own arguments. Sorted rather than compared in order:
    // two concurrent execs have no order to assert, and demanding one would be
    // asserting a scheduling detail rather than the behaviour.
    let mut recorded = calls.lock().expect("the call log is never poisoned").clone();
    recorded.sort_by_key(std::string::ToString::to_string);
    let mut wanted = vec![first, second];
    wanted.sort_by_key(std::string::ToString::to_string);
    assert_eq!(recorded, wanted, "each exec ran once, with the arguments it carried");

    let results = server.mcp_results();
    assert_eq!(results.len(), 2, "two execs are two answers, got {results:?}");
    for result in &results {
        let success = result.success.as_option().expect("a tool that ran answers `success`");
        assert_ne!(success.is_error, Some(true), "and neither failed");
    }

    // Two calls, two closed parts, one finish: a batch is one step of the
    // agent loop, not two turns.
    let completed = tool_parts(&seen)
        .into_iter()
        .filter(|(_, state)| matches!(state, ToolState::Completed { .. }))
        .map(|(call_id, _)| call_id)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(completed.len(), 2, "both calls close as completed Tool parts, got {completed:?}");
    assert!(
        matches!(seen.last(), Some(Event::MessageFinished { reason: FinishReason::Completed, .. })),
        "and the batch is one step of one turn, which finishes clean",
    );
}

/// **AC-17**, the execution-site invariant, on both paths.
///
/// A native `read_args` exec and an `mcp_exec` naming `read` are the *same*
/// tool reached two ways, and both must land behind the same dialog and in the
/// same transcript. `depgate` cannot say this — see the module header — so it
/// is asserted by running both.
#[tokio::test]
async fn both_paths_to_the_read_tool_pass_the_same_dialog_and_leave_the_same_record() {
    let file = ganja_testkit::temp_dir();
    let path = file.path().join("subject.txt");
    std::fs::write(&path, "the file's own content\n").expect("the fixture writes");
    let readable = path.to_string_lossy().into_owned();

    for native in [true, false] {
        let ask = if native {
            Step::read(1, &readable)
        } else {
            Step::mcp(1, "read", &json!({"filePath": readable}))
        };
        let server =
            CursorServer::start(vec![Step::Context, ask, Step::TurnEnded, Step::EndStream]).await;

        let engine = seated(&server, Registry::with_builtins(), rule("read", Action::Ask));
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        engine.send(prompt()).await.expect("an idle engine accepts a prompt");
        let seen = drain_answering(&engine, &mut events, PermissionReply::Once).await;

        assert!(
            seen.iter().any(|event| matches!(event, Event::PermissionRequested { .. })),
            "the {} path raises the dialog",
            if native { "native" } else { "mcp" },
        );

        let content = if native {
            let reads = server.read_results();
            let [result] = reads.as_slice() else {
                panic!("one read exec is one answer, got {reads:?}");
            };
            result
                .success
                .as_option()
                .expect("an allowed read succeeds")
                .content
                .clone()
                .unwrap_or_default()
        } else {
            let results = server.mcp_results();
            let [result] = results.as_slice() else {
                panic!("one mcp exec is one answer, got {results:?}");
            };
            result
                .success
                .as_option()
                .expect("an allowed read succeeds")
                .content
                .iter()
                .filter_map(|item| item.text.as_option().and_then(|text| text.text.clone()))
                .collect::<Vec<_>>()
                .join("")
        };
        assert!(
            content.contains("the file's own content"),
            "the file the dialog was answered for is what reached the server, got {content:?}",
        );

        let parts = tool_parts(&seen);
        assert!(
            parts.iter().any(|(_, state)| matches!(state, ToolState::Completed { .. })),
            "and it left a completed Tool part in the transcript, got {parts:?}",
        );
    }
}

/// **AC-14 and AC-22**, the two refusals that are the *server*'s mistake rather
/// than the person's: a tool nobody declared, and an exec addressed to somebody
/// else's server. Both are answered in the shape cursor's own vocabulary has
/// for them, and neither ends the turn.
#[tokio::test]
async fn an_undeclared_tool_and_a_foreign_server_are_each_refused_in_their_own_arm() {
    let server = CursorServer::start(vec![
        Step::Context,
        Step::mcp(1, "nonesuch", &json!({})),
        Step::mcp_from(2, "lookup", &json!({}), "somebody-else"),
        Step::TurnEnded,
        Step::EndStream,
    ])
    .await;

    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "the answer");
    let engine = seated(&server, Registry::new(vec![tool]), rule("lookup", Action::Allow));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts a prompt");
    let seen = drain(&mut events).await;

    let results = server.mcp_results();
    let [unknown, foreign] = results.as_slice() else {
        panic!("two execs are two answers, got {results:?}");
    };

    let missing =
        unknown.tool_not_found.as_option().expect("an undeclared name is `tool_not_found`");
    assert_eq!(missing.name.as_deref(), Some("nonesuch"));

    // "What the roster really holds" is asserted against the roster this Run
    // actually declared rather than against a hand-written list: the engine
    // installs residents of its own over the registry a test hands it, so a
    // literal here would be an assertion about `recompose_tools` wearing an
    // assertion about the refusal.
    let rosters = server.declared_rosters();
    let [declared] = rosters.as_slice() else {
        panic!("one Run declared one roster, got {} of them", rosters.len());
    };
    let names: Vec<String> =
        declared.iter().filter_map(|definition| definition.tool_name.clone()).collect();
    assert_eq!(
        missing.available_tools, names,
        "the refusal lists exactly the roster it was given, so the model can pick again",
    );
    assert!(names.contains(&"lookup".to_owned()), "which is not vacuously empty");
    assert!(!names.contains(&"nonesuch".to_owned()), "and does not hold what was asked for");

    assert!(
        foreign.server_not_found.is_set(),
        "an exec addressed elsewhere is `server_not_found`, got {foreign:?}",
    );
    assert!(
        calls.lock().expect("the call log is never poisoned").is_empty(),
        "neither refusal ran anything",
    );
    assert!(
        matches!(seen.last(), Some(Event::MessageFinished { reason: FinishReason::Completed, .. })),
        "and the turn survives both",
    );
}

/// The title one-shot a finished turn earns opens its **own** Run and does not
/// disturb the bridged one.
///
/// `ganja run` builds the wire twice per invocation for exactly this reason,
/// which is why the roster is declared per *wire* and never per session
/// (the recording's incidental findings). A one-shot carries no tools, so its
/// Run declares no roster — and the turn that did keeps its own.
#[tokio::test]
async fn the_title_one_shot_opens_its_own_run_and_leaves_the_bridged_turn_alone() {
    let server = CursorServer::with_scripts(vec![
        vec![
            Step::Context,
            Step::mcp(1, "lookup", &json!({"key": "alpha"})),
            Step::Text("found it".to_owned()),
            Step::TurnEnded,
            Step::EndStream,
        ],
        vec![Step::Text("A title".to_owned()), Step::EndStream],
    ])
    .await;

    // A store of this suite's own rather than `Homes`, whose `store()` finds
    // the directory a run of the *shipped binary* created and so is the fixture
    // for a subprocess test, not an in-process engine.
    let data = ganja_testkit::temp_dir();
    let storage = ganja_core::Storage::open(data.path().join("storage"));
    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "the answer");
    let provider = provider_at(&server);
    // Persistent, because the title one-shot is something a *stored* session
    // earns: a storeless engine has nothing to title and would open one Run.
    let engine = Engine::persistent(
        Arc::new(provider),
        MODEL,
        Arc::new(Registry::new(vec![tool])),
        rule("lookup", Action::Allow),
        storage,
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts a prompt");
    let seen = drain(&mut events).await;
    assert!(engine.settle(SETTLE).await, "the turn's tail completes within the window");

    // The title is asked for from a **detached** `tokio::spawn` that the turn
    // does not await (`session.rs`'s `request_title`), so `settle` does not
    // cover it and a bare `declared_rosters().len() == 2` here races the task
    // rather than asserting anything. Waiting for the second Run's opening
    // frame is the honest shape: the claim is that a second one opens, not
    // that it has opened by the instant the first turn's slot released.
    let rosters =
        ganja_testkit::eventually(SETTLE, "the title one-shot to open its own Run", async || {
            let rosters = server.declared_rosters();
            (rosters.len() == 2).then_some(rosters)
        })
        .await;

    assert_eq!(
        calls.lock().expect("the call log is never poisoned").len(),
        1,
        "the bridged call ran once, whatever else opened a socket",
    );
    assert!(
        matches!(seen.last(), Some(Event::MessageFinished { reason: FinishReason::Completed, .. })),
        "and its turn completed",
    );
    assert!(!rosters[0].is_empty(), "the bridged turn declared its registry");
    assert!(rosters[1].is_empty(), "and the one-shot, carrying no tools, declared nothing");

    // **AC-17 (D553).** The one-shot's request composes nothing before its
    // one message — an empty state, set rather than absent — and carries a
    // conversation id of its own, minted from that message rather than
    // borrowed from the turn it titles.
    let runs = server.run_requests();
    let [turn, one_shot] = runs.as_slice() else {
        panic!("two Runs opened, so two opening frames were recorded, got {}", runs.len());
    };
    let state = one_shot
        .conversation_state
        .as_option()
        .expect("the composed state rides every run request, empty or not");
    assert!(
        state.root_prompt_messages_json.is_empty() && state.turns.is_empty(),
        "a one-shot has no history to compose, got {} root entries and {} turns",
        state.root_prompt_messages_json.len(),
        state.turns.len(),
    );
    let conversation = one_shot
        .conversation_id
        .as_deref()
        .expect("a conversation id rides every run request that has a message");
    assert_eq!(conversation.len(), 36, "v4-shaped, as the reference's is: {conversation:?}");
    assert!(turn.conversation_id.is_some(), "the bridged turn's request carries one too");
    assert_ne!(
        one_shot.conversation_id, turn.conversation_id,
        "derived from each request's own first message, so the two Runs never share one",
    );
}

/// **AC-18 (D553).** A resumed session's first request carries the history
/// it stored, and the Run it opens serves that history to the server's gets:
/// `[u1, a1, u2]` seeded through the engine's own storage composes a system
/// head, `u1` and `a1` as root entries and `u1`'s turn with `a1` as its step,
/// every blob answered found — and the `a1` root entry answers with `a1`'s
/// text.
///
/// Seeded rather than played, because the composition is a property of the
/// *request* and a stored session is where a request with history comes
/// from; the newest message is the prompt, so the action is a user message
/// and not a resume.
#[tokio::test]
async fn a_resumed_session_opens_its_run_over_the_history_it_stored() {
    let server = CursorServer::start(vec![
        Step::Context,
        Step::KvGetComposed,
        Step::Text("By hand.".to_owned()),
        Step::TurnEnded,
        Step::EndStream,
    ])
    .await;

    let data = ganja_testkit::temp_dir();
    let storage = ganja_core::Storage::open(data.path().join("storage"));
    let session = ganja_testkit::seed_session(&storage, 0);
    let asked = "What does this crate do?";
    let answered = "It parses TOML.";
    let u1 = Message::user(asked);
    let mut a1 = Message::assistant(MODEL);
    a1.parts.push(Part::text(answered));
    a1.complete();
    ganja_testkit::seed_message(&storage, &session, &u1);
    ganja_testkit::seed_message(&storage, &session, &a1);

    let (tool, _calls) = RecorderTool::new("lookup", "lookup ran", "the answer");
    let engine = Engine::persistent(
        Arc::new(provider_at(&server)),
        MODEL,
        Arc::new(Registry::new(vec![tool])),
        rule("lookup", Action::Allow),
        storage,
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");
    engine.resume(&session).await.expect("the seeded session loads");

    engine.send(saying("How?")).await.expect("an idle engine accepts a prompt");
    let seen = drain(&mut events).await;
    assert!(
        matches!(seen.last(), Some(Event::MessageFinished { reason: FinishReason::Completed, .. })),
        "the turn over history finishes clean, got {:?}",
        seen.last(),
    );

    let runs = server.run_requests();
    let run = runs.first().expect("the turn opened a Run");
    let state = run.conversation_state.as_option().expect("the composed state rides the request");
    // The system head rides the root only when there is a system prompt, and
    // the same prompt travels to the server on the context answer's
    // `cloud_rule`, filtered empty the same way — so the two spellings are
    // held to each other rather than to a guess about what this engine
    // assembly says: a bare engine says nothing, and composes no head.
    let contexts = server.context_answers();
    let [context] = contexts.as_slice() else {
        panic!("one turn asks for its context once, got {} of them", contexts.len());
    };
    let head = usize::from(context.cloud_rule.is_some());
    assert_eq!(
        state.root_prompt_messages_json.len(),
        head + 2,
        "u1 and a1 as root entries, behind a head exactly when a system prompt travelled \
         (cloud_rule = {:?})",
        context.cloud_rule,
    );
    assert_eq!(state.turns.len(), 1, "u1's turn, with a1 as its step");
    assert!(run.conversation_id.is_some(), "and a conversation id, derived from u1");
    let action = run.action.as_option().expect("a request whose newest message is a prompt acts");
    assert!(action.user_message_action.is_set(), "as a user message, not a resume");
    assert!(action.resume_action.is_unset());

    let answers = server.kv_answers();
    assert!(
        answers.iter().all(|answer| answer.data.is_some()),
        "every composed id the server asked for is found, got {answers:?}",
    );
    let roots: Vec<(String, String)> = state
        .root_prompt_messages_json
        .iter()
        .map(|id| {
            let answer = answer_for(&answers, id);
            let entry: serde_json::Value =
                serde_json::from_slice(answer.data.as_deref().expect("found"))
                    .expect("a root entry is JSON");
            (entry["role"].as_str().unwrap_or_default().to_owned(), root_text(answer))
        })
        .collect();
    if head == 1 {
        assert_eq!(roots[0].0, "system", "the head first");
        assert_eq!(Some(roots[0].1.as_str()), context.cloud_rule.as_deref(), "carrying the prompt");
    }
    assert_eq!(roots[head], ("user".to_owned(), asked.to_owned()), "then u1");
    assert_eq!(
        roots[head + 1],
        ("assistant".to_owned(), answered.to_owned()),
        "and the a1 root entry answers with a1's text",
    );

    // The turn blob names u1's message and a1's step, and both were asked for
    // and found — the walk the live server makes.
    let turn = decoded::<proto::ConversationTurn>(
        answer_for(&answers, &state.turns[0]).data.as_deref().expect("found"),
    )
    .expect("a turn blob decodes")
    .agent_conversation_turn
    .into_option()
    .expect("holding the agent turn");
    let user = decoded::<proto::UserMessage>(
        answer_for(&answers, turn.user_message.as_deref().expect("a turn names its user message"))
            .data
            .as_deref()
            .expect("found"),
    )
    .expect("a user-message blob decodes");
    assert_eq!(user.text.as_deref(), Some(asked));
    assert_eq!(user.message_id.as_deref().map(str::len), Some(36), "under a derived v4-shaped id");
    let [step] = turn.steps.as_slice() else {
        panic!("one reply is one step, got {}", turn.steps.len());
    };
    let step = decoded::<proto::ConversationStep>(
        answer_for(&answers, step).data.as_deref().expect("found"),
    )
    .expect("a step blob decodes");
    assert_eq!(
        step.assistant_message.as_option().and_then(|message| message.text.as_deref()),
        Some(answered),
        "the step is a1's text",
    );
    assert_eq!(answers.len(), head + 2 + 1 + 2, "the roots, one turn, its message and its step");
}

/// **AC-16 (D553).** A bridged call whose Run the server hung up under is
/// recovered, not failed: the held Run leaves the table as `Reason::Closed`
/// on the keeper's next beat (the pin above), and when the tool's result
/// comes back the engine's next step opens a **second** Run under
/// `resume_action` whose state carries the whole conversation — the call and
/// its result included — served to the server's gets from the fresh Run's
/// own store. The tool ran exactly once; nothing was answered on the dead
/// body; the turn finishes on the second Run's text.
///
/// The call is held at a dialog so the drop provably precedes the result:
/// an allowed tool answers within a millisecond of the exec, which would
/// race the hangup rather than follow it. Probe 2 holds the very same dialog
/// for the very same reason.
#[tokio::test]
async fn a_bridged_call_whose_run_hung_up_is_recovered_on_a_fresh_run_carrying_its_result() {
    let (log, _guard) = capturing();
    let args = json!({"key": "alpha"});
    let server = CursorServer::with_scripts(vec![
        vec![Step::Context, Step::mcp(1, "lookup", &args).no_wait(), Step::Hangup],
        vec![
            Step::Context,
            Step::KvGetComposed,
            Step::Text("recovered".to_owned()),
            Step::TurnEnded,
            Step::EndStream,
        ],
    ])
    .await;

    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "the answer");
    let engine = seated(&server, Registry::new(vec![tool]), rule("lookup", Action::Ask));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts a prompt");
    let (dialog, mut seen) = held_at_dialog(&mut events).await;
    let logged = hang_up_and_wait_for_drop(&server, &log, 1).await;
    assert!(logged.contains("reason=Closed"), "the pin's drop, again: {logged}");

    engine
        .send(Command::ReplyPermission { id: dialog, reply: PermissionReply::Once })
        .await
        .expect("a reply is never refused");
    seen.extend(drain(&mut events).await);

    // The verdict first, so that a build without the recovery arm fails
    // here, naming the sentence it failed by.
    let finish = seen.last();
    assert!(
        matches!(finish, Some(Event::MessageFinished { reason: FinishReason::Completed, .. })),
        "a resume against a hung-up Run reopens one rather than failing by name, got {finish:?}",
    );
    assert!(
        !seen.iter().any(|event| matches!(
            event,
            Event::MessageFinished { reason: FinishReason::Failed, .. }
        )),
        "and no Failed was published along the way",
    );
    assert_eq!(
        calls.lock().expect("the call log is never poisoned").as_slice(),
        &[args],
        "the tool ran exactly once — a recovery replays nothing",
    );

    let runs = server.run_requests();
    let [first, second] = runs.as_slice() else {
        panic!("the drop and the recovery are two Runs, got {}", runs.len());
    };
    let action = second.action.as_option().expect("the second Run acts");
    assert!(
        action.resume_action.is_set() && action.user_message_action.is_unset(),
        "the request whose newest message is the assistant's goes out as a resume",
    );
    let state = second.conversation_state.as_option().expect("over the composed state");
    assert!(!state.root_prompt_messages_json.is_empty(), "which is not empty");
    assert_eq!(
        second.conversation_id, first.conversation_id,
        "one turn, one conversation: both Runs derive it from the same opening message",
    );
    assert!(
        first
            .conversation_state
            .as_option()
            .is_some_and(|state| state.root_prompt_messages_json.is_empty()),
        "where the first Run, a first turn, composed nothing",
    );

    // What the second Run served: every root id found and JSON, the call and
    // its result among them; the turn blob found and decoding.
    let answers = server.kv_answers();
    let texts: Vec<String> = state
        .root_prompt_messages_json
        .iter()
        .map(|id| root_text(answer_for(&answers, id)))
        .collect();
    assert!(
        texts.iter().any(|text| text.contains("[Tool Call] lookup")),
        "the assistant entry carries the call, got {texts:?}",
    );
    assert!(
        texts.iter().any(|text| text.starts_with("[Tool Result]") && text.contains("the answer")),
        "and the result entry carries the tool's own output, got {texts:?}",
    );
    for id in &state.turns {
        let bytes = answer_for(&answers, id).data.as_deref().expect("a turn id is found");
        assert!(decoded::<proto::ConversationTurn>(bytes).is_some(), "and decodes");
    }
    assert!(
        server.mcp_results().is_empty(),
        "the result reached the server as history, never as an answer on the dead body",
    );

    // The transcript: one call, closed completed, then the second Run's text.
    let parts = tool_parts(&seen);
    let ids: std::collections::BTreeSet<&String> = parts.iter().map(|(id, _)| id).collect();
    assert_eq!(ids.len(), 1, "exactly one Tool part, got {parts:?}");
    assert!(
        parts.iter().any(|(_, state)| matches!(state, ToolState::Completed { .. })),
        "closed as completed, got {parts:?}",
    );
    assert!(
        said(&seen).iter().any(|line| line == "recovered"),
        "and the second Run's text is what the turn ends on, got {:?}",
        said(&seen),
    );

    let logged = log.logged();
    assert!(
        logged.contains("recovering a cursor turn on a fresh run") && logged.contains(CLOSED),
        "the recovery is a log line naming the drop's reason, got:\n{logged}",
    );
}

/// **AC-16b (D553), the cap end to end.** The recovered Run is hung up under
/// too: the engine runs the second tool, and its next request finds a key
/// that was already reopened once — so the turn ends in a `Failed` naming
/// the cap, **exactly two** Runs were opened, and each tool ran exactly once.
/// This is the paid loop bounded where it would otherwise run.
#[tokio::test]
async fn a_second_hangup_of_one_turn_is_not_reopened_and_the_turn_fails_by_name() {
    let (log, _guard) = capturing();
    let server = CursorServer::with_scripts(vec![
        vec![Step::Context, Step::mcp(1, "first", &json!({"n": 1})).no_wait(), Step::Hangup],
        vec![Step::Context, Step::mcp(2, "second", &json!({"n": 2})).no_wait(), Step::Hangup],
    ])
    .await;

    let (first, first_calls) = RecorderTool::new("first", "first ran", "one");
    let (second, second_calls) = RecorderTool::new("second", "second ran", "two");
    let engine = seated(
        &server,
        Registry::new(vec![first, second]),
        rules(&[("first", Action::Ask), ("second", Action::Ask)]),
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts a prompt");
    let (dialog, mut seen) = held_at_dialog(&mut events).await;
    hang_up_and_wait_for_drop(&server, &log, 1).await;
    engine
        .send(Command::ReplyPermission { id: dialog, reply: PermissionReply::Once })
        .await
        .expect("a reply is never refused");

    // The recovery opened the second Run, whose exec raises the second dialog
    // — a held Run again, under the same key.
    let (dialog, more) = held_at_dialog(&mut events).await;
    seen.extend(more);
    hang_up_and_wait_for_drop(&server, &log, 2).await;
    engine
        .send(Command::ReplyPermission { id: dialog, reply: PermissionReply::Once })
        .await
        .expect("a reply is never refused");
    seen.extend(drain(&mut events).await);

    let Some(Event::MessageFinished { reason, error, .. }) = seen.last() else {
        panic!("a drained turn ends in a finish");
    };
    assert_eq!(*reason, FinishReason::Failed, "the second reopening is refused, got {error:?}");
    let error = error.as_deref().expect("a failed finish names why");
    assert!(
        error.contains("already reopened once") && error.contains(CLOSED),
        "the cap's own sentence, with the second drop's reason: {error}",
    );

    let runs = server.run_requests();
    assert_eq!(runs.len(), 2, "exactly two Runs: the drop and the one recovery");
    assert!(
        runs[1].action.as_option().is_some_and(|action| action.resume_action.is_set()),
        "the second under resume_action",
    );
    assert_eq!(first_calls.lock().expect("never poisoned").len(), 1, "the first tool ran once");
    assert_eq!(second_calls.lock().expect("never poisoned").len(), 1, "and so did the second");
    assert!(server.mcp_results().is_empty(), "neither answered on a body the server had closed");
}
