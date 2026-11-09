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
use ganja_client::{
    Bootstrap, BootstrapAuthor, BootstrapOutcome, BootstrapStatus, Bounds, Client, ClientError,
    Credentials, EventId, HeaderMap, HeaderValue, PermissionReply, Prompt, Resumable, Resumed,
    SessionId,
};
use ganja_protocol::{Event, Message};
use support::{DEADLINE, EPOCH, HEALTHY, Reply, Stub, frame, frame_at};

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

/// A server behind a proxy that mounts it under a path is attached to by
/// that path, and every route is spelled after it rather than in its place.
#[tokio::test]
async fn an_address_with_a_path_reaches_each_route_under_that_path() {
    let stub = Stub::always(Reply::ok(HEALTHY)).await;
    let client = Client::new(&format!("{}/behind/a/proxy/", stub.address()), None)
        .expect("an address with a path is usable");

    client.health().await.expect("health answers under the path");
    assert_eq!(stub.only_request().path, "/behind/a/proxy/global/health");
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

#[tokio::test]
async fn messages_and_shell_follow_the_existing_wire_shapes() {
    let message = Message::user("stored prompt");
    let body = serde_json::to_string(&vec![message.clone()]).expect("message json");
    let stub = Stub::answering(move |request| match request.method.as_str() {
        "GET" => Reply::ok(body.clone()),
        "POST" => Reply::Accepted,
        other => panic!("unexpected method {other}"),
    })
    .await;
    assert_eq!(stub.client().messages(&session()).await.expect("messages"), vec![message]);
    stub.client().shell(&session(), "printf hello").await.expect("shell accepted");
    let requests = stub.received();
    assert_eq!(requests[0].path, "/session/ses_attached/message");
    assert_eq!(requests[1].path, "/session/ses_attached/shell");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&requests[1].body).expect("body"),
        serde_json::json!({"command":"printf hello"})
    );
}

#[tokio::test]
async fn an_unknown_stored_message_field_is_version_skew() {
    let mut message = serde_json::to_value(Message::user("stored")).expect("message");
    message["unknown"] = true.into();
    let stub = Stub::always(Reply::ok(serde_json::json!([message]).to_string())).await;
    assert!(matches!(stub.client().messages(&session()).await, Err(ClientError::Skew { .. })));
}

#[tokio::test]
async fn an_idle_status_after_an_ambiguous_post_never_authorizes_resending() {
    let stub = Stub::answering(|request| {
        if request.method == "POST" { Reply::Silent } else { Reply::ok(r#"{"status":"idle"}"#) }
    })
    .await;
    let client = Client::with_bounds(
        stub.address(),
        None,
        Bounds { connect: Duration::from_secs(1), read: Duration::from_millis(100) },
    )
    .expect("client");
    assert_eq!(
        client.bootstrap(&bootstrap(), Duration::from_secs(5)).await.expect("classified ambiguity"),
        BootstrapOutcome::AmbiguousStartup
    );
    assert_eq!(stub.received().iter().filter(|request| request.method == "POST").count(), 1);
}

#[tokio::test]
async fn holding_an_established_stream_keeps_its_cursor_until_explicit_invalidation() {
    let (_, json) = started();
    let stub = Stub::always(Reply::Held {
        chunks: vec![
            (Duration::ZERO, connected(0, 0, "fresh")),
            (Duration::ZERO, frame_at("message", &json, 1)),
        ],
    })
    .await;
    let resumable = Resumable::new(stub.client());
    resumable.next().await.expect("item").expect("event");
    resumable.hold().await;
    assert_eq!(resumable.cursor().await, Some(id(1)));
    assert!(resumable.next().await.is_none());
    resumable.invalidate().await;
    assert_eq!(resumable.cursor().await, None);
    resumable.release();
    let fresh = resumable.connect().await.expect("fresh connection").expect("released");
    assert_eq!(fresh.replay, ganja_client::sse::Replay::Fresh);
    assert!(!stub.received().last().expect("request").headers.contains_key("last-event-id"));
}

#[tokio::test]
async fn a_cut_after_fresh_registration_replays_from_that_head() {
    let (_, json) = started();
    let stub = Stub::answering(move |request| {
        if request.headers.contains_key("last-event-id") {
            assert_eq!(request.headers["last-event-id"], format!("{EPOCH}-5"));
            Reply::Stream { chunks: vec![connected(5, 6, "from"), frame_at("message", &json, 6)] }
        } else {
            Reply::Stream { chunks: vec![connected(5, 5, "fresh")] }
        }
    })
    .await;
    let resumed = Resumable::new(stub.client());
    resumed.connect().await.expect("registration").expect("not held");
    assert_eq!(resumed.cursor().await, Some(id(5)));
    tokio::time::timeout(DEADLINE, resumed.next())
        .await
        .expect("reconnect")
        .expect("item")
        .expect("event");
    assert_eq!(resumed.cursor().await, Some(id(6)));
}

#[derive(Debug)]
struct TestConnectPolicy {
    allowed: std::sync::atomic::AtomicBool,
    released: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    outcomes: std::sync::Mutex<Vec<ganja_client::AttemptOutcome>>,
}

impl TestConnectPolicy {
    fn new(allowed: bool) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            allowed: std::sync::atomic::AtomicBool::new(allowed),
            released: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            outcomes: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn outcomes(&self) -> Vec<ganja_client::AttemptOutcome> {
        self.outcomes.lock().expect("outcome mutex").clone()
    }
}

struct TestPermit(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl Drop for TestPermit {
    fn drop(&mut self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl ganja_client::ConnectPolicy for TestConnectPolicy {
    fn admit(&self) -> futures::future::BoxFuture<'_, Option<ganja_client::ConnectionPermit>> {
        Box::pin(async {
            self.allowed.load(std::sync::atomic::Ordering::SeqCst).then(|| {
                Box::new(TestPermit(self.released.clone())) as ganja_client::ConnectionPermit
            })
        })
    }

    fn observe(&self, outcome: ganja_client::AttemptOutcome) {
        self.outcomes.lock().expect("outcome mutex").push(outcome);
    }
}

#[tokio::test]
async fn refused_admission_sends_no_request_and_reports_no_attempt() {
    let stub = Stub::always(Reply::Silent).await;
    let policy = TestConnectPolicy::new(false);
    let resumable = Resumable::new(stub.client()).with_policy(policy.clone());
    assert!(resumable.connect().await.expect("admission refused").is_none());
    assert!(resumable.next().await.is_none());
    assert!(stub.received().is_empty());
    assert!(policy.outcomes().is_empty());
}

#[tokio::test]
async fn every_reconnect_is_admitted_and_reports_exactly_one_outcome() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ganja_client::AttemptOutcome;
    let calls = AtomicUsize::new(0);
    let (_, json) = started();
    let stub = Stub::answering(move |_| match calls.fetch_add(1, Ordering::SeqCst) {
        0 => Reply::Json { status: 503, body: "shed".to_owned() },
        1 => Reply::Stream { chunks: Vec::new() },
        _ => Reply::Held {
            chunks: vec![
                (Duration::ZERO, connected(0, 0, "fresh")),
                (Duration::ZERO, frame_at("message", &json, 1)),
            ],
        },
    })
    .await;
    let policy = TestConnectPolicy::new(true);
    let resumable = Resumable::new(stub.client()).with_policy(policy.clone());
    tokio::time::timeout(DEADLINE, resumable.next())
        .await
        .expect("bounded retry")
        .expect("item")
        .expect("event");
    let outcomes = policy.outcomes();
    assert_eq!(outcomes.len(), 3);
    assert_eq!(outcomes[0], AttemptOutcome::Shed);
    assert_eq!(outcomes[1], AttemptOutcome::Transport);
    assert!(
        matches!(&outcomes[2], AttemptOutcome::Connected(ack) if ack.replay == ganja_client::sse::Replay::Fresh)
    );
    assert_eq!(
        policy.released.load(Ordering::SeqCst),
        2,
        "successful permit lives with the connection"
    );
    resumable.hold().await;
    assert_eq!(policy.released.load(Ordering::SeqCst), 3);
    assert_eq!(policy.outcomes().len(), 3, "closing an acknowledged stream is not another attempt");
}

#[tokio::test]
async fn a_hold_classifies_a_cancelled_attempt_before_acknowledging() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let stub = Stub::answering(move |_| {
        tx.send(()).expect("request receiver");
        Reply::Silent
    })
    .await;
    let policy = TestConnectPolicy::new(true);
    let resumable = std::sync::Arc::new(Resumable::new(stub.client()).with_policy(policy.clone()));
    let reader = tokio::spawn({
        let resumable = resumable.clone();
        async move { resumable.connect().await }
    });
    rx.recv().await.expect("attempt began");
    resumable.hold().await;
    assert_eq!(policy.outcomes(), vec![ganja_client::AttemptOutcome::Cancelled]);
    assert_eq!(policy.released.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(reader.await.expect("joined").expect("held").is_none());
}

#[tokio::test]
async fn a_connection_timeout_is_reported_once() {
    let stub = Stub::always(Reply::Silent).await;
    let policy = TestConnectPolicy::new(true);
    let client = Client::with_bounds(
        stub.address(),
        None,
        Bounds { connect: Duration::from_secs(1), read: Duration::from_millis(100) },
    )
    .expect("client");
    let resumable = Resumable::new(client).with_policy(policy.clone());
    assert!(matches!(resumable.connect().await, Err(ClientError::Silent { .. })));
    assert_eq!(policy.outcomes(), vec![ganja_client::AttemptOutcome::TimedOut]);
}

#[tokio::test]
async fn hold_stops_an_established_heartbeat_only_stream() {
    let stub = Stub::always(Reply::Held {
        chunks: vec![
            (Duration::ZERO, connected(0, 0, "fresh")),
            (Duration::ZERO, frame_at("heartbeat", "{}", 0)),
        ],
    })
    .await;
    let policy = TestConnectPolicy::new(true);
    let resumable = std::sync::Arc::new(Resumable::new(stub.client()).with_policy(policy.clone()));
    resumable.connect().await.expect("connected");
    let reader = tokio::spawn({
        let resumable = resumable.clone();
        async move { resumable.next().await }
    });
    tokio::task::yield_now().await;
    tokio::time::timeout(Duration::from_secs(2), resumable.hold())
        .await
        .expect("hold does not await conversation data");
    assert!(reader.await.expect("joined").is_none());
    assert_eq!(policy.released.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(policy.outcomes().len(), 1);
}

#[derive(Debug, Default)]
struct WaitingPolicy(tokio::sync::Notify);

impl ganja_client::ConnectPolicy for WaitingPolicy {
    fn admit(&self) -> futures::future::BoxFuture<'_, Option<ganja_client::ConnectionPermit>> {
        Box::pin(async {
            self.0.notify_one();
            futures::future::pending().await
        })
    }

    fn observe(&self, _outcome: ganja_client::AttemptOutcome) {
        panic!("no attempt began while admission was waiting");
    }
}

#[tokio::test]
async fn hold_cancels_waiting_admission_without_needing_the_stream_door() {
    let stub = Stub::always(Reply::Silent).await;
    let policy = std::sync::Arc::new(WaitingPolicy::default());
    let resumable = std::sync::Arc::new(Resumable::new(stub.client()).with_policy(policy.clone()));
    let reader = tokio::spawn({
        let resumable = resumable.clone();
        async move { resumable.connect().await }
    });
    policy.0.notified().await;
    tokio::time::timeout(Duration::from_secs(2), resumable.hold())
        .await
        .expect("hold bypasses blocked admission");
    assert!(reader.await.expect("joined").expect("held").is_none());
    assert!(stub.received().is_empty());
}

#[tokio::test]
async fn http_refusal_and_invalid_response_are_each_observed_once() {
    use ganja_client::AttemptOutcome;
    for (reply, expected) in [
        (
            Reply::Json { status: 401, body: String::new() },
            AttemptOutcome::Http { status: 401, from_serve: false },
        ),
        (
            Reply::Stream { chunks: vec![frame_at("unknown", "{}", 0)] },
            AttemptOutcome::InvalidResponse,
        ),
    ] {
        let stub = Stub::always(reply).await;
        let policy = TestConnectPolicy::new(true);
        let resumable = Resumable::new(stub.client()).with_policy(policy.clone());
        assert!(resumable.connect().await.is_err());
        assert_eq!(policy.outcomes(), vec![expected]);
        assert_eq!(policy.released.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn http_errors_and_attempts_preserve_valid_server_provenance() {
    use ganja_client::AttemptOutcome;
    for (status, epoch, expected) in [
        (409, Some(EPOCH), true),
        (503, None, false),
        (404, None, false),
        (409, Some("malformed"), false),
        (409, Some("01998ad0-0000-4000-8000-00000000d505"), false),
        (503, Some(EPOCH), true),
        (401, Some(EPOCH), true),
    ] {
        let headers = epoch
            .map(|epoch| vec![("ganja-serve-epoch".to_owned(), epoch.to_owned())])
            .unwrap_or_default();
        let stub = Stub::always(Reply::JsonWithHeaders {
            status,
            body: r#"{"message":"refused"}"#.to_owned(),
            headers,
        })
        .await;
        let error = stub.client().abort(&session()).await.expect_err("refusal");
        match error {
            ClientError::Refused { from_serve, .. }
            | ClientError::Unauthorized { from_serve, .. } => assert_eq!(from_serve, expected),
            other => panic!("unexpected error {other:?}"),
        }
        let policy = TestConnectPolicy::new(true);
        let resumed = Resumable::new(stub.client()).with_policy(policy.clone());
        assert!(resumed.connect().await.is_err());
        let outcome = if status == 503 && !expected {
            AttemptOutcome::Shed
        } else {
            AttemptOutcome::Http { status, from_serve: expected }
        };
        assert_eq!(policy.outcomes(), vec![outcome]);
    }
}

#[tokio::test]
async fn bootstrap_failures_preserve_cleanup_in_progress_and_default_it_when_absent() {
    for cleanup in [None, Some(false), Some(true)] {
        let mut body =
            serde_json::json!({"status":"failed","stage":"deadline","detail":"budget expired"});
        if let Some(cleanup) = cleanup {
            body["cleanup_in_progress"] = cleanup.into();
        }
        let stub = Stub::always(Reply::Json { status: 409, body: body.to_string() }).await;
        assert!(
            matches!(stub.client().bootstrap_status().await.expect("status"), BootstrapStatus::Failed { cleanup_in_progress, .. } if cleanup_in_progress == cleanup.unwrap_or(false))
        );
        assert!(
            matches!(stub.client().bootstrap(&bootstrap(), Duration::from_secs(5)).await.expect("outcome"), BootstrapOutcome::Failed { cleanup_in_progress, .. } if cleanup_in_progress == cleanup.unwrap_or(false))
        );
    }
}

fn id(seq: u64) -> EventId {
    format!("{EPOCH}-{seq}").parse().expect("a fixture cursor")
}

fn connected(seq: u64, head: u64, replay: &str) -> String {
    frame_at("connected", &format!(r#"{{"head":"{EPOCH}-{head}","replay":"{replay}"}}"#), seq)
}

#[tokio::test]
async fn a_resume_cursor_is_sent_and_advances_only_when_an_event_is_yielded() {
    let (_, json) = started();
    let stub = Stub::always(Reply::Stream {
        chunks: vec![
            connected(2, 4, "from"),
            frame_at("message", &json, 3),
            frame_at("heartbeat", "{}", 3),
            frame_at("message", &json, 4),
        ],
    })
    .await;
    let mut events = stub.client().events_from(Some(id(2))).await.expect("resumed");
    assert_eq!(events.cursor(), Some(&id(2)));
    assert_eq!(events.connected().head, id(4));
    assert_eq!(stub.only_request().headers["last-event-id"], id(2).to_string());
    events.next().await.expect("one event").expect("valid event");
    assert_eq!(events.cursor(), Some(&id(3)), "buffered and control frames cannot commit a cursor");
    events.next().await.expect("second event").expect("valid event");
    assert_eq!(events.cursor(), Some(&id(4)));
}

#[tokio::test]
async fn malformed_or_missing_frame_ids_and_unknown_connected_fields_are_skew() {
    for bytes in [
        "event: connected\ndata: {}\n\n".to_owned(),
        "id: not-a-cursor\nevent: connected\ndata: {}\n\n".to_owned(),
        frame_at(
            "connected",
            &format!(r#"{{"head":"{EPOCH}-0","replay":"fresh","extra":true}}"#),
            0,
        ),
    ] {
        let stub = Stub::always(Reply::Stream { chunks: vec![bytes] }).await;
        assert!(matches!(stub.client().events().await, Err(ClientError::Skew { .. })));
    }
}

#[tokio::test]
async fn a_severed_stream_reconnects_after_the_last_yielded_event() {
    let (event, json) = started();
    let stub = Stub::answering(move |request| {
        match request.headers.get("last-event-id").map(String::as_str) {
            None => Reply::Stream {
                chunks: vec![connected(0, 0, "fresh"), frame_at("message", &json, 1)],
            },
            Some(cursor) if cursor == format!("{EPOCH}-1") => Reply::Stream {
                chunks: vec![connected(1, 2, "from"), frame_at("message", &json, 2)],
            },
            other => panic!("unexpected cursor: {other:?}"),
        }
    })
    .await;
    let resumed = Resumable::new(stub.client());
    for seq in 1..=2 {
        assert_eq!(
            tokio::time::timeout(DEADLINE, resumed.next())
                .await
                .expect("bounded reconnect")
                .expect("item")
                .expect("valid event"),
            Resumed::Event(Box::new(event.clone()))
        );
        assert_eq!(resumed.cursor().await, Some(id(seq)));
    }
    assert_eq!(stub.received().len(), 2);
}

#[tokio::test]
async fn a_gap_discards_the_tail_and_the_next_connection_is_fresh() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = AtomicUsize::new(0);
    let (event, json) = started();
    let stub = Stub::answering(move |request| match calls.fetch_add(1, Ordering::SeqCst) {
        0 => {
            Reply::Stream { chunks: vec![connected(0, 0, "fresh"), frame_at("message", &json, 1)] }
        }
        1 => {
            assert_eq!(request.headers.get("last-event-id"), Some(&format!("{EPOCH}-1")));
            Reply::Stream { chunks: vec![connected(1, 9, "gap"), frame_at("message", &json, 8)] }
        }
        2 => {
            assert!(!request.headers.contains_key("last-event-id"));
            Reply::Stream { chunks: vec![connected(9, 9, "fresh"), frame_at("message", &json, 10)] }
        }
        _ => panic!("unexpected reconnect"),
    })
    .await;
    let resumed = Resumable::new(stub.client());
    assert_eq!(
        resumed.next().await.expect("item").expect("event"),
        Resumed::Event(Box::new(event.clone()))
    );
    assert_eq!(resumed.next().await.expect("item").expect("gap"), Resumed::Gap { head: id(9) });
    assert_eq!(resumed.cursor().await, None);
    assert_eq!(
        resumed.next().await.expect("item").expect("fresh event"),
        Resumed::Event(Box::new(event))
    );
    assert_eq!(resumed.cursor().await, Some(id(10)));
}

#[tokio::test]
async fn hold_acknowledges_an_inflight_connect_without_waiting_for_its_network_deadline() {
    let (seen, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let stub = Stub::answering(move |_| {
        seen.send(()).expect("receiver exists");
        Reply::Silent
    })
    .await;
    let resumed = std::sync::Arc::new(Resumable::new(stub.client()));
    let reader = tokio::spawn({
        let resumed = resumed.clone();
        async move { resumed.next().await }
    });
    requests.recv().await.expect("connect started");
    tokio::time::timeout(Duration::from_secs(2), resumed.hold())
        .await
        .expect("hold has its own control path");
    assert!(reader.await.expect("reader joined").is_none());
    assert!(resumed.next().await.is_none(), "held consumer cannot reconnect");
    assert_eq!(stub.received().len(), 1);
    resumed.release();
    let reader = tokio::spawn({
        let resumed = resumed.clone();
        async move { resumed.next().await }
    });
    requests.recv().await.expect("release permits a new attempt");
    resumed.hold().await;
    assert!(reader.await.expect("reader joined").is_none());
}

#[tokio::test]
async fn fixed_headers_apply_to_reads_posts_and_streams() {
    let stub = Stub::answering(|request| match request.path.as_str() {
        "/event" => Reply::Stream { chunks: vec![connected(0, 0, "fresh")] },
        "/session/ses_attached/abort" => Reply::ok("true"),
        _ => Reply::ok(HEALTHY),
    })
    .await;
    let mut headers = HeaderMap::new();
    headers.insert("x-worker-target", HeaderValue::from_static("worker-one"));
    let client = stub.client().with_headers(headers);
    client.health().await.expect("health");
    assert!(client.abort(&session()).await.expect("boolean acknowledgement"));
    client.events().await.expect("stream");
    assert!(
        stub.received().iter().all(|request| request.headers["x-worker-target"] == "worker-one")
    );
}

fn bootstrap() -> Bootstrap {
    Bootstrap {
        repository: "https://example.com/repo.git".to_owned(),
        reference: "main".to_owned(),
        branch: "worker-one".to_owned(),
        author: BootstrapAuthor {
            name: "Worker".to_owned(),
            email: "worker@example.com".to_owned(),
        },
    }
}

#[tokio::test]
async fn bootstrap_sends_its_server_budget_and_decodes_the_result() {
    let stub =
        Stub::always(Reply::ok(r#"{"status":"cloned","head":"abc","branch":"worker-one"}"#)).await;
    assert_eq!(
        stub.client().bootstrap(&bootstrap(), Duration::from_secs(30)).await.expect("bootstrap"),
        BootstrapOutcome::Cloned { head: "abc".to_owned(), branch: "worker-one".to_owned() }
    );
    let request = stub.only_request();
    assert_eq!((request.method.as_str(), request.path.as_str()), ("POST", "/bootstrap"));
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("json body");
    assert_eq!(body["deadline_secs"], 30);
    assert_eq!(body["author"]["name"], "Worker");
}

#[tokio::test]
async fn bootstrap_reconciles_a_timed_out_post_without_sending_it_again() {
    let stub = Stub::answering(|request| {
        if request.method == "POST" {
            Reply::Silent
        } else {
            Reply::ok(r#"{"status":"cloned","head":"abc","branch":"worker-one"}"#)
        }
    })
    .await;
    let client = Client::with_bounds(
        stub.address(),
        None,
        Bounds { connect: Duration::from_secs(1), read: Duration::from_millis(100) },
    )
    .expect("client");
    assert!(matches!(
        client.bootstrap(&bootstrap(), Duration::from_secs(5)).await.expect("reconciled"),
        BootstrapOutcome::Cloned { .. }
    ));
    let received = stub.received();
    assert_eq!(received.iter().filter(|request| request.method == "POST").count(), 1);
    assert_eq!(received.iter().filter(|request| request.method == "GET").count(), 1);
}

#[tokio::test]
async fn bootstrap_status_and_abort_refuse_unknown_shapes() {
    let stub = Stub::always(Reply::ok(r#"{"status":"idle","unknown":1}"#)).await;
    assert!(matches!(stub.client().bootstrap_status().await, Err(ClientError::Skew { .. })));
    assert!(matches!(stub.client().abort(&session()).await, Err(ClientError::Skew { .. })));
    let stub = Stub::always(Reply::Json { status: 404, body: String::new() }).await;
    assert_eq!(
        stub.client().bootstrap_status().await.expect("serve handoff"),
        BootstrapStatus::AlreadyServing
    );
}
