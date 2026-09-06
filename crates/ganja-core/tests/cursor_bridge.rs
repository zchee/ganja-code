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

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use ganja_core::Engine;
use ganja_core::permission::{Action, Permissions, Rule};
use ganja_core::protocol::{
    Command, Event, FinishReason, PartBody, PartId, PermissionReply, ToolState,
};
use ganja_core::provider::{CredentialSource, CursorProvider};
use ganja_core::tool::Registry;
use ganja_testkit::cursor_server::{CursorServer, Script, Step};
use ganja_testkit::{RecorderTool, drain, drain_answering};
use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio::time::timeout;

/// The model every seat here asks for; the mock listing serves it.
const MODEL: &str = "gpt-5.3-codex";

/// What the client writes only after the response has started.
const SECOND: &[u8] = b"second";

/// The token every wire here presents. A canary rather than a plausible
/// credential, though nothing in this file looks for it: the mock server never
/// reads the `authorization` header at all, because the point of handing the
/// wire a token explicitly is that no store is consulted.
///
/// That the cursor wire keeps a credential out of its renderings and its log is
/// checked in `crates/ganja-core/tests/secrets_env.rs`, whose own cursor arm
/// plants `CURSOR_CANARY` on a `CursorProvider::at` and drives both of this
/// wire's failure paths against an endpoint that quotes it back. It is a
/// separate binary because that drill mutates process-wide environment
/// variables and this one must not.
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

/// A Run whose request body is **not** chunked is refused readably, rather
/// than read forever as protobuf that never parses.
///
/// The mock de-chunks the request body by hand, because a body held open for a
/// whole turn has no length to declare. Nothing downstream of that checks it:
/// against a `content-length` body the first "size line" is protobuf,
/// `from_str_radix` fails, and the de-chunker returns nothing for the rest of
/// the connection — so every asking step in the script times out twenty seconds
/// apart and no message anywhere names the cause. That is a failure this suite
/// has already hit once, and one line at the head turns it into a sentence.
#[tokio::test]
async fn a_run_whose_body_is_not_chunked_is_refused_with_a_reason() {
    let server = CursorServer::start(Script::finished()).await;

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
        let server = CursorServer::start(Script::finished()).await;
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

/// An engine on `server`, holding `tools`, gated by `permissions`.
fn seated(server: &CursorServer, tools: Registry, permissions: Permissions) -> Engine {
    Engine::new(Arc::new(provider_at(server)), MODEL, Arc::new(tools), permissions)
}

/// A cursor provider pointed at `server`, presenting a token this file owns.
///
/// **No credential store is involved** (lead ruling, **Dv-11**): `at` takes its
/// credential explicitly, so this suite has no code path to `auth.json` at all
/// — structural, rather than an `XDG_DATA_HOME` redirect pointing away from it.
/// That is also what lets this be one binary holding eleven tests:
/// `ganja_testkit::redirect_xdg_data_home` is `unsafe` with a documented
/// one-test-per-binary invariant, and every other XDG-mutating binary in this
/// tree holds exactly one test.
fn provider_at(server: &CursorServer) -> CursorProvider {
    CursorProvider::at(server.base_url(), CredentialSource::key(TOKEN).expect("a non-blank token"))
        .expect("loopback may carry a token")
}

/// One prompt, with nothing attached.
fn prompt() -> Command {
    Command::SendPrompt {
        text: "what does this crate do".to_owned(),
        mentions: Vec::new(),
        skills: Vec::new(),
        session_mentions: Vec::new(),
        peers: Vec::new(),
    }
}

/// Rules that put an ask in front of `tool` and nothing else.
fn ask_for(tool: &str) -> Permissions {
    let mut permissions = Permissions::default();
    permissions.set_baseline(vec![Rule {
        permission: tool.to_owned(),
        pattern: "*".to_owned(),
        action: Action::Ask,
    }]);

    permissions
}

/// Rules that let `tool` run unasked, so a test about the bridge is not a test
/// about dialogs.
fn allow(tool: &str) -> Permissions {
    let mut permissions = Permissions::default();
    permissions.set_baseline(vec![Rule {
        permission: tool.to_owned(),
        pattern: "*".to_owned(),
        action: Action::Allow,
    }]);

    permissions
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
    let server = CursorServer::start(
        Script::new()
            .then(Step::Context)
            .then(Step::mcp(1, "lookup", &args))
            .then(Step::Text("found it".to_owned()))
            .then(Step::TurnEnded)
            .then(Step::EndStream),
    )
    .await;

    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "the answer");
    let engine = seated(&server, Registry::new(vec![tool]), allow("lookup"));
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts a prompt");
    let seen = drain(&mut events).await;

    let contexts = server.context_answers();
    let [roster] = contexts.as_slice() else {
        panic!("one turn asks for its context once, got {} of them", contexts.len());
    };
    let declared: Vec<&str> =
        roster.tools.iter().filter_map(|definition| definition.tool_name.as_deref()).collect();
    assert!(declared.contains(&"lookup"), "the roster the server was given names the tool");
    let entry = roster
        .tools
        .iter()
        .find(|definition| definition.tool_name.as_deref() == Some("lookup"))
        .expect("just found it");
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
    assert_eq!(recorded, vec![args], "the tool ran exactly once, with the server's own arguments");
    let [only] = recorded.as_slice() else {
        panic!("just asserted there is one");
    };
    assert_eq!(
        only["limit"],
        json!(2000),
        "an integral argument survives the double-only wire as an integer, not 2000.0",
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
    let server = CursorServer::start(
        Script::new()
            .then(Step::Context)
            .then(Step::mcp(1, "lookup", &args))
            .then(Step::Text("understood".to_owned()))
            .then(Step::TurnEnded)
            .then(Step::EndStream),
    )
    .await;

    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "the answer");
    let engine = seated(&server, Registry::new(vec![tool]), ask_for("lookup"));
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
    let server = CursorServer::start(
        Script::new()
            .then(Step::Context)
            .then(Step::mcp(1, "lookup", &args).approval_only())
            .then(Step::TurnEnded)
            .then(Step::EndStream),
    )
    .await;

    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "the answer");
    // Allowed, deliberately: under an `ask` rule an absent dialog could mean
    // the preflight was skipped *or* that the engine never got that far. With
    // the tool allowed, the only thing standing between this exec and a real
    // invocation is the flag.
    let engine = seated(&server, Registry::new(vec![tool]), allow("lookup"));
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

    let server = CursorServer::start(
        Script::new()
            .then(Step::Context)
            .then(Step::mcp(1, "lookup", &json!({"key": "alpha"})))
            .then(Step::TurnEnded)
            .then(Step::EndStream),
    )
    .await;

    let (tool, _calls) = RecorderTool::new("lookup", "lookup ran", "the answer");
    let engine = seated(&server, Registry::new(vec![tool]), ask_for("lookup"));
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

/// **Two execs in one batch**, which is what the recording actually saw the
/// server do (two `grep_args` within 5 ms) and what every sequential step in
/// this file cannot replay.
///
/// Both are written before either is answered, so both are genuinely in flight
/// and the client's answers are matched by `id` rather than by arrival order.
/// End to end this exercises the wire's gather window, the engine's `resolve`
/// fan-out and the transcript, over the real socket — the one measured server
/// behaviour this suite was built to be able to replay.
#[tokio::test]
async fn two_execs_the_server_sent_together_are_both_run_and_both_answered() {
    let first = json!({"key": "alpha"});
    let second = json!({"key": "beta"});
    let server = CursorServer::start(
        Script::new()
            .then(Step::Context)
            .then(Step::batch(vec![
                Step::mcp(1, "lookup", &first),
                Step::mcp(2, "lookup", &second),
            ]))
            .then(Step::TurnEnded)
            .then(Step::EndStream),
    )
    .await;

    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "the answer");
    let engine = seated(&server, Registry::new(vec![tool]), allow("lookup"));
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
    assert_eq!(
        seen.iter().filter(|event| matches!(event, Event::MessageFinished { .. })).count(),
        1,
        "and the batch is one step of one turn",
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
        let server = CursorServer::start(
            Script::new().then(Step::Context).then(ask).then(Step::TurnEnded).then(Step::EndStream),
        )
        .await;

        let engine = seated(&server, Registry::with_builtins(), ask_for("read"));
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
    let server = CursorServer::start(
        Script::new()
            .then(Step::Context)
            .then(Step::mcp(1, "nonesuch", &json!({})))
            .then(Step::mcp_from(2, "lookup", &json!({}), "somebody-else"))
            .then(Step::TurnEnded)
            .then(Step::EndStream),
    )
    .await;

    let (tool, calls) = RecorderTool::new("lookup", "lookup ran", "the answer");
    let engine = seated(&server, Registry::new(vec![tool]), allow("lookup"));
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
        Script::new()
            .then(Step::Context)
            .then(Step::mcp(1, "lookup", &json!({"key": "alpha"})))
            .then(Step::Text("found it".to_owned()))
            .then(Step::TurnEnded)
            .then(Step::EndStream),
        Script::new().then(Step::Text("A title".to_owned())).then(Step::EndStream),
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
        allow("lookup"),
        storage,
    );
    let mut events = engine.subscribe().await.expect("the first subscriber wins");

    engine.send(prompt()).await.expect("an idle engine accepts a prompt");
    let seen = drain(&mut events).await;
    assert!(engine.settle(SETTLE).await, "the turn's tail completes within the window");

    // The title is asked for from a **detached** `tokio::spawn` that the turn
    // does not await (`session.rs`'s `request_title`), so `settle` does not
    // cover it and a bare `runs() == 2` here races the task rather than
    // asserting anything. Waiting for the Run to appear is the honest shape:
    // the claim is that a second one opens, not that it has opened by the
    // instant the first turn's slot released.
    ganja_testkit::eventually(SETTLE, "the title one-shot to open its own Run", async || {
        (server.runs() == 2).then_some(())
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
    let rosters = server.declared_rosters();
    assert_eq!(rosters.len(), 2, "one opening frame per Run");
    assert!(!rosters[0].is_empty(), "the bridged turn declared its registry");
    assert!(rosters[1].is_empty(), "and the one-shot, carrying no tools, declared nothing");
}
