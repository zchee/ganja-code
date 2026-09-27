//! Every surface this client drives, against a socket that answers real
//! bytes.
//!
//! Nothing here mocks the client: each test stands up a loopback server, lets
//! the client speak HTTP to it, and asserts both halves — what went out on the
//! wire, and what the typed answer became. The stub is what makes the
//! interesting half testable at all: a real server cannot be asked to send a
//! frame from a future version, and that refusal is the posture this crate is
//! built around.

mod support;

use std::time::{Duration, Instant};

use futures::StreamExt as _;
use ganja_client::sse::EvictedNotice;
use ganja_client::{Bounds, Client, ClientError, Credentials, PermissionReply, Prompt, SessionId};
use ganja_protocol::{Event, Message};
use support::{DEADLINE, HEALTHY, Reply, Stub, frame};

/// The session every fixture talks about.
fn session() -> SessionId {
    SessionId::from("ses_attached".to_owned())
}

/// One event, in the exact bytes serve would put on the wire for it.
fn started() -> (Event, String) {
    let event =
        Event::MessageStarted { session_id: session(), message: Message::user("what is in main") };
    let json = serde_json::to_string(&event).expect("an event serializes");

    (event, json)
}

/// Reads the stream to its end, under the deadline.
async fn drain(events: &mut ganja_client::Events) -> Vec<Result<Event, ClientError>> {
    let mut read = Vec::new();
    while let Some(item) = tokio::time::timeout(DEADLINE, events.next())
        .await
        .expect("the stream speaks within the deadline")
    {
        read.push(item);
    }

    read
}

// ---------------------------------------------------------------------------
// The REST surfaces.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_answers_what_the_server_says_it_is() {
    let stub = Stub::always(Reply::ok(HEALTHY)).await;

    let health = stub.client().health().await.expect("health answers");
    assert!(health.healthy);
    assert_eq!(health.version, "0.1.0");
    assert_eq!(
        health.session_id.as_str(),
        "01998ad0-0000-7000-8000-00000000d505",
        "and which session it is serving"
    );
    assert_eq!(stub.only_request().path, "/global/health");
}

#[tokio::test]
async fn creating_a_session_posts_and_answers_the_id_the_server_minted() {
    let stub = Stub::always(Reply::ok(r#"{"id":"ses_minted_over_there"}"#)).await;

    let created = stub.client().create_session().await.expect("the server mints one");
    assert_eq!(created.as_str(), "ses_minted_over_there");

    let request = stub.only_request();
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/session");
}

/// The listing is the engine's own `SessionInfo`, which carries fields this
/// crate deliberately does not name; reading it must not depend on them.
#[tokio::test]
async fn listing_sessions_reads_the_two_fields_a_continue_acts_on() {
    let stub = Stub::always(Reply::ok(
        r#"[{"id":"ses_root","version":1,"created":1,"updated":2,"usage":{},"context_tokens":0},
            {"id":"ses_child","version":1,"created":3,"updated":4,"parent":"ses_root"}]"#,
    ))
    .await;

    let sessions = stub.client().sessions().await.expect("the listing answers");
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].id.as_str(), "ses_root");
    assert_eq!(sessions[0].parent, None);
    assert_eq!(sessions[1].parent.as_ref().map(SessionId::as_str), Some("ses_root"));
    assert_eq!(stub.only_request().path, "/session");
}

#[tokio::test]
async fn a_prompt_goes_to_the_async_route_as_the_body_that_route_takes() {
    let stub = Stub::always(Reply::Accepted).await;

    stub.client()
        .prompt(&session(), &Prompt::new("look something up").as_agent(Some("build".to_owned())))
        .await
        .expect("the prompt is accepted");

    let request = stub.only_request();
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/session/ses_attached/prompt_async");
    let body: serde_json::Value =
        serde_json::from_str(&request.body).expect("the body is one JSON object");
    assert_eq!(body["text"], "look something up");
    assert_eq!(body["agent"], "build");
    // Absent rather than null: serve's body refuses unknown fields and takes
    // the switches as optional, so sending nothing is how nothing is switched.
    assert!(body.get("model").is_none(), "{body}");
    assert!(body.get("mentions").is_none(), "{body}");
}

#[tokio::test]
async fn the_pending_dialogs_are_read_whole() {
    let stub = Stub::always(Reply::ok(
        r#"[{"session_id":"ses_attached","id":"per_1","call_id":"call_1","tool":"bash",
             "title":"rm -rf /","args":{"command":"rm -rf /"}}]"#,
    ))
    .await;

    let pending = stub.client().permissions().await.expect("the pending list answers");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].tool, "bash");
    assert_eq!(pending[0].title, "rm -rf /");
    assert_eq!(pending[0].id.as_str(), "per_1");
    assert_eq!(pending[0].args["command"], "rm -rf /");
    // Serve omits the field when nothing outside the project is touched.
    assert!(pending[0].directories.is_empty());
    assert_eq!(stub.only_request().path, "/permission");
}

/// A field nobody declared is a server this build does not understand — the
/// same posture the event stream applies, applied to a body.
#[tokio::test]
async fn a_pending_dialog_carrying_a_field_nobody_declared_is_a_version_mismatch() {
    let stub = Stub::always(Reply::ok(
        r#"[{"session_id":"s","id":"per_1","call_id":"c","tool":"bash","title":"t",
             "args":{},"urgency":"high"}]"#,
    ))
    .await;

    let error = stub.client().permissions().await.expect_err("an undeclared field is refused");
    assert!(
        matches!(error, ClientError::Skew { .. }),
        "the posture is skew, not a shrug: {error:?}"
    );
    assert!(error.to_string().contains("different versions of ganja"), "{error}");
}

#[tokio::test]
async fn a_reply_names_the_dialog_and_the_decision() {
    let stub = Stub::always(Reply::Accepted).await;

    stub.client()
        .reply_permission(
            &ganja_client::PermissionId::from("per_7".to_owned()),
            PermissionReply::Reject,
        )
        .await
        .expect("the reply is accepted");

    let request = stub.only_request();
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/permission/per_7/reply");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&request.body).expect("one JSON object"),
        serde_json::json!({"response": "reject"})
    );
}

// ---------------------------------------------------------------------------
// The credential posture, which is serve's.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_configured_credential_travels_as_basic_auth_on_every_route() {
    let stub = Stub::always(Reply::ok(HEALTHY)).await;
    let client = Client::new(stub.address(), Some(Credentials::new("ganja", "hunter2")))
        .expect("the stub's address is usable");

    client.health().await.expect("health answers");

    let presented = stub.only_request().authorization.expect("the credential travelled");
    assert!(presented.starts_with("Basic "), "serve reads a Basic header: {presented}");
    // The wire carries base64(user:password), which is what serve decodes.
    assert_eq!(presented, "Basic Z2FuamE6aHVudGVyMg==");
}

#[tokio::test]
async fn a_server_that_refuses_the_credential_says_which_variable_secures_it() {
    let stub = Stub::always(Reply::Json { status: 401, body: String::new() }).await;

    let error = stub.client().health().await.expect_err("a 401 is not an answer");
    assert!(matches!(error, ClientError::Unauthorized { .. }), "{error:?}");
    let said = error.to_string();
    assert!(said.contains("GANJA_SERVER_PASSWORD"), "{said}");
    assert!(said.contains(stub.address()), "{said}");
}

/// The engine's refusals reach the caller as themselves: a session nothing
/// stored answers to is a 404, and a turn already streaming is a 409.
#[tokio::test]
async fn a_refused_route_carries_the_status_and_what_the_server_said() {
    for (status, message) in
        [(404u16, "no stored session named ses_attached"), (409, "a turn is already streaming")]
    {
        let stub = Stub::always(Reply::Json {
            status,
            body: serde_json::json!({"message": message}).to_string(),
        })
        .await;

        let error = stub
            .client()
            .prompt(&session(), &Prompt::new("hello"))
            .await
            .expect_err("a refusal is not an answer");
        match error {
            ClientError::Refused { status: carried, ref body, .. } => {
                assert_eq!(carried, status);
                assert!(body.contains(message), "{body}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(error.to_string().contains("prompt_async"), "{error}");
    }
}

#[tokio::test]
async fn a_server_that_is_not_there_names_the_address_that_was_tried() {
    // A port nothing is listening on: bound, read, and released, so the
    // address is real and dead rather than guessed at.
    let taken =
        tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.expect("a loopback port is bindable");
    let address = format!("http://{}", taken.local_addr().expect("the address reads"));
    drop(taken);

    let client = Client::new(&address, None).expect("the address is usable");
    let error = client.health().await.expect_err("nothing answers");
    assert!(matches!(error, ClientError::Transport { .. }), "{error:?}");
    assert!(error.to_string().contains(&address), "{error}");
}

// ---------------------------------------------------------------------------
// The event stream.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_stream_carries_events_and_swallows_the_frames_that_are_only_liveness() {
    let (event, json) = started();
    let stub = Stub::always(Reply::Stream {
        chunks: vec![
            frame("connected", "{}"),
            frame("heartbeat", "{}"),
            frame("message", &json),
            frame("heartbeat", "{}"),
        ],
    })
    .await;

    let client = stub.client();
    let mut events = client.events().await.expect("the stream opens");
    let read = drain(&mut events).await;

    assert_eq!(read.len(), 1, "only the engine's event survives: {read:?}");
    assert_eq!(read.into_iter().next().expect("just counted").expect("an event"), event);

    let request = stub.only_request();
    assert_eq!(request.path, "/event");
    assert_eq!(request.accept.as_deref(), Some("text/event-stream"));
}

/// The registration guarantee: `events` returns only once the connected frame
/// has been read, so a caller that prompts next cannot lose the turn's first
/// events between subscribing and asking.
#[tokio::test]
async fn the_stream_opens_only_once_the_server_has_said_hello() {
    let (_, json) = started();
    let stub = Stub::always(Reply::Stream {
        chunks: vec![frame("message", &json), frame("connected", "{}")],
    })
    .await;

    let client = stub.client();
    let error =
        client.events().await.expect_err("a stream that starts mid-conversation is refused");
    assert!(matches!(error, ClientError::Skew { .. }), "{error:?}");
    assert!(error.to_string().contains("connected"), "{error}");
}

/// The one thing worse than a torn transcript is a torn transcript that looks
/// whole: an eviction ends the stream with an error carrying the engine's own
/// account of it.
#[tokio::test]
async fn an_evicted_subscriber_ends_with_a_readable_error_rather_than_a_silent_stop() {
    let (event, json) = started();
    let notice = EvictedNotice {
        kind: "evicted".to_owned(),
        message: "this subscriber fell behind and was evicted; the events after its last one \
                  were never queued"
            .to_owned(),
    };
    let stub = Stub::always(Reply::Stream {
        chunks: vec![
            frame("connected", "{}"),
            frame("message", &json),
            frame("evicted", &serde_json::to_string(&notice).expect("the notice serializes")),
            // Nothing after an eviction is trustworthy, and nothing after it
            // is read.
            frame("message", &json),
        ],
    })
    .await;

    let client = stub.client();
    let mut events = client.events().await.expect("the stream opens");
    let read = drain(&mut events).await;

    assert_eq!(read.len(), 2, "the event, then the eviction: {read:?}");
    assert_eq!(read[0].as_ref().expect("the event"), &event);
    match read[1].as_ref().expect_err("an eviction is an error") {
        ClientError::Evicted { notice: said } => {
            assert!(said.contains("fell behind"), "{said}");
        }
        other => panic!("expected an eviction, got {other:?}"),
    }
}

/// The declared skew posture, on the surface it matters most: an event type
/// this build has no variant for is a version mismatch, not a mid-stream serde
/// message a script would have to guess at.
///
/// **The unknown name is deliberately one no release will ever mint.** An
/// earlier draft used a variant this build was about to gain in the same
/// engagement, which would have turned the assertion inside out the day it
/// landed — a test asserting a refusal, against a shape that parses. A name
/// from the protocol's own future is not a safe stand-in for one from
/// nobody's.
#[tokio::test]
async fn an_event_type_this_build_cannot_name_is_refused_as_a_version_mismatch() {
    let stub = Stub::always(Reply::Stream {
        chunks: vec![
            frame("connected", "{}"),
            frame(
                "message",
                r#"{"type":"telepathy_established","session_id":"ses_attached","id":"tel_1"}"#,
            ),
        ],
    })
    .await;

    let client = stub.client();
    let mut events = client.events().await.expect("the stream opens");
    let read = drain(&mut events).await;

    assert_eq!(read.len(), 1, "and then it stops: {read:?}");
    let error = read[0].as_ref().expect_err("an unknown type is an error");
    assert!(matches!(error, ClientError::Skew { .. }), "{error:?}");
    let said = error.to_string();
    assert!(said.contains("different versions of ganja"), "the refusal names the mismatch: {said}");
    assert!(said.contains("telepathy_established"), "and what it could not read: {said}");
}

/// An SSE frame outside the declared vocabulary is the same refusal by another
/// door — a server that grew a control frame is a server this build does not
/// understand.
#[tokio::test]
async fn a_frame_named_outside_the_vocabulary_ends_the_stream_readably() {
    let stub = Stub::always(Reply::Stream {
        chunks: vec![frame("connected", "{}"), frame("server.goodbye", "{}")],
    })
    .await;

    let client = stub.client();
    let mut events = client.events().await.expect("the stream opens");
    let read = drain(&mut events).await;

    assert_eq!(read.len(), 1);
    let said = read[0].as_ref().expect_err("an undeclared frame is an error").to_string();
    assert!(said.contains("server.goodbye"), "{said}");
    assert!(said.contains("different versions of ganja"), "{said}");
}

// ---------------------------------------------------------------------------
// The bounds: a server that stops answering is a named failure, and only that.
// ---------------------------------------------------------------------------

/// The bounds the tests that wait one out drive the client under: a read
/// bound a tenth of `READ_DEADLINE`, so each costs seconds rather than half
/// a minute. A loaded runner can only make such a bound end later, which
/// [`PAST_THE_BOUND`] leaves room for. `Client::new`'s own bounds are pinned
/// in `src/lib_tests.rs`.
const QUICK: Bounds = Bounds { connect: Duration::from_secs(10), read: Duration::from_secs(3) };

/// The bounds a stream kept alive by [`HEARTBEAT`] must outlive: five
/// heartbeats to a read, where serve's `READ_DEADLINE` holds three of its
/// own, so a test process stalled for up to four seconds on a loaded runner
/// still finds a heartbeat inside every read and does not fail a healthy
/// stream (nextest retries nothing).
const ROOMY: Bounds = Bounds { connect: Duration::from_secs(10), read: Duration::from_secs(5) };

/// The stub's heartbeat, and the pace of a trickle.
const HEARTBEAT: Duration = Duration::from_secs(1);

/// Longer than [`QUICK`]'s read bound, with room for a loaded runner: a wait
/// that outlasts this is a wait with no bound.
const PAST_THE_BOUND: Duration = Duration::from_secs(15);

/// A client for `stub`, under [`QUICK`].
fn quick(stub: &Stub) -> Client {
    Client::with_bounds(stub.address(), None, QUICK).expect("the stub's address is usable")
}

/// A client for `stub`, under [`ROOMY`].
fn roomy(stub: &Stub) -> Client {
    Client::with_bounds(stub.address(), None, ROOMY).expect("the stub's address is usable")
}

/// The stream serve opens: `connected`, then `beats` heartbeats a
/// [`HEARTBEAT`] apart, then `json` as one event, then nothing.
fn heartbeats_then(beats: usize, json: &str) -> Reply {
    let heartbeats = std::iter::repeat_with(|| (HEARTBEAT, frame("heartbeat", "{}"))).take(beats);

    Reply::Held {
        chunks: std::iter::once((Duration::ZERO, frame("connected", "{}")))
            .chain(heartbeats)
            .chain([(Duration::ZERO, frame("message", json))])
            .collect(),
    }
}

/// An error and its causes, joined as `anyhow`'s `{:#}` joins them — what
/// `ganja run --attach` prints for a turn that failed.
fn with_causes(error: &dyn std::error::Error) -> String {
    std::iter::successors(Some(error), |error| error.source())
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ")
}

/// A stream that said hello and then nothing — no heartbeat, no close — ends
/// with an error naming the server and the route, rather than holding its
/// reader forever. An attached run reads exactly this stream, and a stall
/// behind it used to be a run that never exited.
#[tokio::test]
async fn an_event_stream_that_goes_silent_ends_with_a_named_error_instead_of_waiting_forever() {
    let stub =
        Stub::always(Reply::Held { chunks: vec![(Duration::ZERO, frame("connected", "{}"))] })
            .await;

    let client = quick(&stub);
    let mut events = client.events().await.expect("the stream opens");
    let reading = Instant::now();
    let ended = tokio::time::timeout(PAST_THE_BOUND, events.next())
        .await
        .unwrap_or_else(|_| panic!("a stream silent for {PAST_THE_BOUND:?} was still waited on"))
        .expect("a silent stream ends with an error, not quietly");
    assert!(reading.elapsed() >= QUICK.read, "and not before the bound: {:?}", reading.elapsed());

    let error = ended.expect_err("silence is not an event");
    match &error {
        ClientError::Silent { address, bound } => {
            assert_eq!(address, stub.address(), "the error names the server");
            assert_eq!(*bound, QUICK.read, "and the bound that ended it");
        }
        other => panic!("the stream's own silence, not {other:?}"),
    }
    assert!(error.to_string().contains("GET /event"), "and the route: {error}");
    assert!(events.next().await.is_none(), "nothing follows the error");
}

/// The same silence before the stream's head as after it: a server that
/// takes `GET /event` and answers nothing does not hold the caller either.
#[tokio::test]
async fn an_event_stream_the_server_never_opens_is_the_same_named_silence() {
    let stub = Stub::always(Reply::Silent).await;

    let error = tokio::time::timeout(PAST_THE_BOUND, quick(&stub).events())
        .await
        .unwrap_or_else(|_| panic!("a stream unopened for {PAST_THE_BOUND:?} was still waited on"))
        .expect_err("silence is not a stream");

    assert!(matches!(error, ClientError::Silent { .. }), "{error:?}");
    assert_eq!(stub.only_request().path, "/event");
}

/// The stream's head and its `connected` frame answer inside one bound
/// together: serve writes `connected` the moment it has registered the
/// subscriber, so a server that is there meets the bound at once, and one
/// that trickles its hello a byte at a time cannot stretch it.
#[tokio::test]
async fn an_event_stream_that_trickles_its_hello_is_the_same_named_silence() {
    let stub = Stub::always(Reply::Trickled {
        every: HEARTBEAT,
        reply: Box::new(Reply::Stream { chunks: vec![frame("connected", "{}")] }),
    })
    .await;

    let error = tokio::time::timeout(PAST_THE_BOUND, quick(&stub).events())
        .await
        .unwrap_or_else(|_| panic!("a hello trickled for {PAST_THE_BOUND:?} was still waited on"))
        .expect_err("a hello that never finishes is not a stream");

    assert!(matches!(error, ClientError::Silent { .. }), "{error:?}");
}

/// The same bound on a read that never gets an answer at all: `health` is
/// the first call everything that attaches makes. Its causes, which `ganja
/// run` prints, say which route it was and that the bound ended it.
#[tokio::test]
async fn a_read_the_server_never_answers_is_a_transport_error_instead_of_a_hang() {
    let stub = Stub::always(Reply::Silent).await;

    let error = tokio::time::timeout(PAST_THE_BOUND, quick(&stub).health())
        .await
        .unwrap_or_else(|_| panic!("a read unanswered for {PAST_THE_BOUND:?} was still waited on"))
        .expect_err("silence is not an answer");

    match &error {
        ClientError::Transport { source, .. } => {
            assert!(source.is_timeout(), "the bound ended it: {source:?}");
        }
        other => panic!("a transport failure, not {other:?}"),
    }
    assert!(error.to_string().contains(stub.address()), "{error}");
    let said = with_causes(&error);
    assert!(said.contains("/global/health"), "the causes name the route: {said}");
    assert!(said.contains("timed out"), "and that it timed out: {said}");
    assert_eq!(stub.only_request().path, "/global/health");
}

/// A read's bound holds for its whole answer, not for each gap in it: a
/// server trickling a byte inside every bound would otherwise hold `health`,
/// the first call everything that attaches makes, as long as it liked.
#[tokio::test]
async fn a_read_the_server_trickles_is_bounded_as_a_whole_not_per_byte() {
    let stub =
        Stub::always(Reply::Trickled { every: HEARTBEAT, reply: Box::new(Reply::ok(HEALTHY)) })
            .await;

    let error = tokio::time::timeout(PAST_THE_BOUND, quick(&stub).health())
        .await
        .unwrap_or_else(|_| panic!("a read trickled for {PAST_THE_BOUND:?} was still waited on"))
        .expect_err("an answer that never finishes is not an answer");

    match &error {
        ClientError::Transport { source, .. } => {
            assert!(source.is_timeout(), "the bound ended it: {source:?}");
        }
        other => panic!("a transport failure, not {other:?}"),
    }
}

/// The bound is per read, not per stream: serve's heartbeat keeps an idle
/// stream alive however long it idles — a session waiting minutes on a
/// person's dialog sends nothing else — so a stream open for longer than the
/// bound in total, but never silent for longer than one heartbeat, still
/// delivers what comes next.
#[tokio::test]
async fn a_stream_that_keeps_heartbeating_outlives_the_read_bound() {
    let (event, json) = started();
    // One heartbeat past the bound.
    let stub = Stub::always(heartbeats_then(6, &json)).await;

    let client = roomy(&stub);
    let mut events = client.events().await.expect("the stream opens");
    let reading = Instant::now();
    let read = tokio::time::timeout(HEARTBEAT * 6 + DEADLINE, events.next())
        .await
        .expect("the event arrives once the heartbeats are through")
        .expect("the stream is still open");

    assert_eq!(read.expect("a heartbeat-kept stream is not a stall"), event);
    assert!(reading.elapsed() > ROOMY.read, "and it outlived the bound: {:?}", reading.elapsed());
}

/// A `POST` is not read-bounded: its handler may be running the session's
/// `UserPromptSubmit` hooks before it answers, each under a minute of its own
/// by default, and cutting that wait would fail a prompt the server was still
/// accepting.
#[tokio::test]
async fn a_prompt_the_server_takes_its_time_accepting_is_not_cut_by_the_read_bound() {
    let accepting = QUICK.read + Duration::from_secs(2);
    let stub =
        Stub::always(Reply::Late { after: accepting, reply: Box::new(Reply::Accepted) }).await;

    tokio::time::timeout(accepting + DEADLINE, quick(&stub).prompt(&session(), &Prompt::new("hi")))
        .await
        .expect("the prompt is answered once the server gets to it")
        .expect("a slow acceptance is still an acceptance");

    assert_eq!(stub.only_request().path, "/session/ses_attached/prompt_async");
}

/// The stream's bound counts the server's silence while somebody reads it,
/// not the caller's time elsewhere. An attached run opens the stream, then
/// waits on its prompt's acceptance — which may run `UserPromptSubmit` hooks
/// for longer than the bound — and reads nothing meanwhile; the heartbeats
/// serve wrote in that time are waiting when it comes back, and what follows
/// them still arrives.
#[tokio::test]
async fn a_stream_left_unread_while_a_slow_prompt_is_accepted_still_delivers_what_follows() {
    let (event, json) = started();
    let accepting = ROOMY.read + Duration::from_secs(2);
    let stub = Stub::answering(move |request| {
        if request.path != "/event" {
            return Reply::Late { after: accepting, reply: Box::new(Reply::Accepted) };
        }
        // The event comes a heartbeat after the prompt is accepted.
        heartbeats_then(8, &json)
    })
    .await;

    let client = roomy(&stub);
    let mut events = client.events().await.expect("the stream opens");
    let unread = Instant::now();
    tokio::time::timeout(accepting + DEADLINE, client.prompt(&session(), &Prompt::new("hi")))
        .await
        .expect("the prompt is answered once the server gets to it")
        .expect("a slow acceptance is still an acceptance");
    assert!(unread.elapsed() > ROOMY.read, "the stream sat unread past the bound");

    let read = tokio::time::timeout(DEADLINE, events.next())
        .await
        .expect("what follows the heartbeats arrives")
        .expect("the stream is still open");
    assert_eq!(read.expect("a stream nobody read during the prompt has not stalled"), event);
}
