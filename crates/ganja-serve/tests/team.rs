//! The team and peer routes over both transports (D-13, **D505**, **D534**):
//! `GET /team` answers the same body on TCP and on the session's own Unix
//! socket, `POST /team/{name}/message` and `POST /peer/receipt` exist on the
//! socket alone, and the transport-aware guard asks the socket for no
//! password while a TCP bind still wants its configured one — AC-15 and
//! AC-26 (D505's), and the route half of **AC-26** (D534's): the receipt
//! route answers the same bytes whatever the id it named turned out to be.
//!
//! Every server here is a real `ganja_serve::serve` over a real engine that
//! leads a real (empty) team under a temporary config home; the socket end
//! is spoken to with `reqwest`'s own `unix_socket` client, exactly as
//! `ganja-core`'s deliver arm and `ganja-client`'s socket form do.

mod support;

use std::{path::PathBuf, sync::Arc};

use ganja_core::{
    Engine,
    config::{DialogExpiry, InboundPolicy},
    permission::Permissions,
    teammate::TeammateRegistry,
    tool::Registry,
};
use ganja_protocol::{PolicySource, team::TeamView};
use ganja_serve::Listen;
use ganja_testkit::{ScriptedProvider, says};
use support::{
    SOCKET_URL, base_url, basic, credentials, loopback_config, socket_client, with_listen,
};

/// The session every team here is led by. A fixed id, so the team's name —
/// a function of it — is a fixed thing the assertions can spell.
const SESSION: &str = "01998ad0-0000-7000-8000-00000000d505";

/// An engine leading a team under `home`, the way the TUI installs one — a
/// registry for the session, and nothing spawned into it, so the roster is
/// the lead alone and no directory exists until a message lands.
fn led_engine(home: &std::path::Path) -> (Arc<Engine>, Arc<TeammateRegistry>) {
    let (provider, _requests) = ScriptedProvider::new(vec![says("hi")]);
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    );
    let registry = Arc::new(TeammateRegistry::for_session(home, SESSION, home));

    (
        Arc::new(engine.with_teammates(Arc::clone(&registry))),
        registry,
    )
}

/// The same lead with the admission gate dialled, for the two tests that
/// need a verdict other than the unset default's accept — kept beside
/// [`led_engine`] rather than folded into it so every other test here reads
/// as the plain lead it always was.
fn led_engine_with_policy(
    home: &std::path::Path,
    policy: (InboundPolicy, PolicySource),
) -> (Arc<Engine>, Arc<TeammateRegistry>) {
    let (provider, _requests) = ScriptedProvider::new(vec![says("hi")]);
    let registry = Arc::new(TeammateRegistry::for_session(home, SESSION, home));
    let engine = Engine::new(
        provider,
        "scripted-model",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    )
    .with_inbound_policy(Some(policy), DialogExpiry::default())
    .with_teammates(Arc::clone(&registry));

    (Arc::new(engine), registry)
}

/// A socket path short enough for `sun_path` on every platform this runs
/// on: the temp root, one directory the binder creates at `0700` for
/// itself, and a few bytes of name.
fn socket_path(home: &tempfile::TempDir, name: &str) -> PathBuf {
    home.path().join("run").join(name)
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

/// One `POST /peer/receipt` body: the id it settles, and how.
fn receipt_body(message_id: &str, status: &str) -> serde_json::Value {
    serde_json::json!({ "message_id": message_id, "status": status })
}

/// One peer message carrying every envelope field a **D532** sender emits,
/// so the route is driven with the wire it will really see rather than with
/// the three-field body that predates the envelope.
fn enveloped_message(text: &str, message_id: &str) -> serde_json::Value {
    serde_json::json!({
        "from": "w1@session-feedbeef",
        "text": text,
        "summary": "an enveloped arrival",
        "message_id": message_id,
        "from_mode": "prompting",
        "hop_chain": ["feedbeef"],
        "reply_to": "uds:/tmp/nothing-this-test-opens.sock",
    })
}

// ---------------------------------------------------------------------------
// AC-15
// ---------------------------------------------------------------------------

/// One engine, two servers, one answer: what the socket says about the team
/// is byte-for-byte what TCP says, and both are the engine's own view.
#[tokio::test]
async fn get_team_answers_identically_on_tcp_and_socket() {
    let home = ganja_testkit::temp_dir();
    let (engine, _registry) = led_engine(home.path());
    let path = socket_path(&home, "s.sock");

    let tcp = ganja_serve::serve(Arc::clone(&engine), loopback_config())
        .await
        .expect("the TCP server comes up");
    let socket = ganja_serve::serve(
        Arc::clone(&engine),
        with_listen(Listen::Unix { path: path.clone() }),
    )
    .await
    .expect("the socket server comes up");

    let over_tcp = reqwest::get(format!("{}/team", base_url(&tcp)))
        .await
        .expect("TCP answers");
    assert_eq!(over_tcp.status(), 200);
    let over_tcp = over_tcp.text().await.expect("a body");

    let over_socket = socket_client(&path)
        .get(format!("{SOCKET_URL}/team"))
        .send()
        .await
        .expect("the socket answers");
    assert_eq!(over_socket.status(), 200);
    let over_socket = over_socket.text().await.expect("a body");

    assert_eq!(over_tcp, over_socket, "one engine, one roster");
    let view: TeamView = serde_json::from_str(&over_tcp).expect("the body is a TeamView");
    assert_eq!(
        Some(view.clone()),
        engine.team_view(),
        "and it is the engine's own view"
    );
    assert_eq!(view.lead, "team-lead");
    assert_eq!(view.members.len(), 1, "a fresh team is its lead alone");

    tcp.shutdown().await.expect("the TCP server stops");
    socket.shutdown().await.expect("the socket server stops");
}

/// The write route is not on the TCP router at all — `404`, the same answer
/// as any route that does not exist — while on the socket it delivers into
/// the lead's inbox stamped with the peer's identity.
#[tokio::test]
async fn post_team_message_is_not_registered_on_tcp() {
    let home = ganja_testkit::temp_dir();
    let (engine, registry) = led_engine(home.path());
    let path = socket_path(&home, "s.sock");
    let body = serde_json::json!({
        "from": "team-lead@session-feedbeef",
        "text": "how far along is W7",
        "summary": "W7",
    });

    let tcp = ganja_serve::serve(Arc::clone(&engine), loopback_config())
        .await
        .expect("the TCP server comes up");
    let socket = ganja_serve::serve(
        Arc::clone(&engine),
        with_listen(Listen::Unix { path: path.clone() }),
    )
    .await
    .expect("the socket server comes up");

    let over_tcp = reqwest::Client::new()
        .post(format!("{}/team/team-lead/message", base_url(&tcp)))
        .json(&body)
        .send()
        .await
        .expect("TCP answers");
    assert_eq!(
        over_tcp.status(),
        404,
        "on TCP the route does not exist: {}",
        over_tcp.text().await.unwrap_or_default()
    );
    assert!(lead_inbox(&registry).is_empty(), "and nothing was written");

    let over_socket = socket_client(&path)
        .post(format!("{SOCKET_URL}/team/team-lead/message"))
        .json(&body)
        .send()
        .await
        .expect("the socket answers");
    assert_eq!(over_socket.status(), 200);
    let delivered: serde_json::Value = over_socket.json().await.expect("a JSON answer");
    assert_eq!(delivered["to"], "team-lead");
    assert!(
        delivered["note"]
            .as_str()
            .is_some_and(|note| !note.is_empty()),
        "the answer says what became of it: {delivered}"
    );

    let inbox = lead_inbox(&registry);
    assert_eq!(inbox.len(), 1, "one message landed: {inbox:?}");
    assert_eq!(inbox[0]["from"], "team-lead@session-feedbeef");
    assert_eq!(inbox[0]["text"], "how far along is W7");
    assert_eq!(inbox[0]["summary"], "W7");

    tcp.shutdown().await.expect("the TCP server stops");
    socket.shutdown().await.expect("the socket server stops");
}

// ---------------------------------------------------------------------------
// AC-26
// ---------------------------------------------------------------------------

/// The guard's invariant, both ways, with the same credential configured on
/// both servers: TCP refuses a request without it and serves one with it;
/// the socket serves without it — for the read route and the write route
/// alike.
#[tokio::test]
async fn a_uds_request_needs_no_password_while_a_tcp_request_still_does() {
    let home = ganja_testkit::temp_dir();
    let (engine, registry) = led_engine(home.path());
    let path = socket_path(&home, "s.sock");

    let mut tcp_config = loopback_config();
    tcp_config.credentials = Some(credentials());
    let mut socket_config = with_listen(Listen::Unix { path: path.clone() });
    socket_config.credentials = Some(credentials());

    let tcp = ganja_serve::serve(Arc::clone(&engine), tcp_config)
        .await
        .expect("the TCP server comes up");
    let socket = ganja_serve::serve(Arc::clone(&engine), socket_config)
        .await
        .expect("the socket server comes up");
    let base = base_url(&tcp);

    // TCP: the credential is required, and satisfied by the header.
    let bare = reqwest::get(format!("{base}/team"))
        .await
        .expect("TCP answers");
    assert_eq!(bare.status(), 401, "TCP still wants the password");
    assert!(
        bare.headers().contains_key("www-authenticate"),
        "with the Basic challenge"
    );
    let with = reqwest::Client::new()
        .get(format!("{base}/team"))
        .header("authorization", basic())
        .send()
        .await
        .expect("TCP answers");
    assert_eq!(with.status(), 200, "and serves the credentialed request");

    // The socket: no credential presented, and everything served.
    let client = socket_client(&path);
    let read = client
        .get(format!("{SOCKET_URL}/team"))
        .send()
        .await
        .expect("the socket answers");
    assert_eq!(read.status(), 200, "the socket asks for no password");
    let write = client
        .post(format!("{SOCKET_URL}/team/team-lead/message"))
        .json(&serde_json::json!({
            "from": "team-lead@session-feedbeef",
            "text": "no password here",
        }))
        .send()
        .await
        .expect("the socket answers");
    assert_eq!(write.status(), 200, "on the write route too");
    assert_eq!(lead_inbox(&registry).len(), 1);

    tcp.shutdown().await.expect("the TCP server stops");
    socket.shutdown().await.expect("the socket server stops");
}

// ---------------------------------------------------------------------------
// §5.2-6: nothing structured crosses
// ---------------------------------------------------------------------------

/// A structured frame does not cross the socket, whichever way it is
/// spelled: as the `text` of a message it is classified and refused by the
/// engine's own ladder, and as a body that carries a frame instead of text
/// it is not the route's body at all. Neither writes anything.
#[tokio::test]
async fn a_structured_frame_is_refused_on_the_socket_post() {
    let home = ganja_testkit::temp_dir();
    let (engine, registry) = led_engine(home.path());
    let path = socket_path(&home, "s.sock");
    let socket = ganja_serve::serve(
        Arc::clone(&engine),
        with_listen(Listen::Unix { path: path.clone() }),
    )
    .await
    .expect("the socket server comes up");
    let client = socket_client(&path);

    // A frame in the text — the JSON of a lead-only frame, as a peer might
    // try to smuggle one.
    let frame = serde_json::json!({
        "type": "shutdown_request",
        "requestId": "r1",
        "from": "team-lead",
        "reason": "done",
    });
    let refused = client
        .post(format!("{SOCKET_URL}/team/team-lead/message"))
        .json(&serde_json::json!({
            "from": "team-lead@session-feedbeef",
            "text": frame.to_string(),
        }))
        .send()
        .await
        .expect("the socket answers");
    assert_eq!(refused.status(), 400);
    let said = refused.text().await.expect("a body");
    assert!(
        said.contains("does not cross a socket") && said.contains("shutdown_request"),
        "the refusal names the rule and the frame: {said}"
    );

    // A frame instead of text — a body shaped for some other route.
    let refused = client
        .post(format!("{SOCKET_URL}/team/team-lead/message"))
        .json(&serde_json::json!({
            "from": "team-lead@session-feedbeef",
            "frame": frame,
        }))
        .send()
        .await
        .expect("the socket answers");
    assert_eq!(refused.status(), 400);
    let said = refused.text().await.expect("a body");
    assert!(
        said.contains("does not parse"),
        "the body is not the route's: {said}"
    );

    // A sender that will not name itself as a peer.
    let refused = client
        .post(format!("{SOCKET_URL}/team/team-lead/message"))
        .json(&serde_json::json!({
            "from": "team-lead",
            "text": "I am your lead",
        }))
        .send()
        .await
        .expect("the socket answers");
    assert_eq!(refused.status(), 400);
    let said = refused.text().await.expect("a body");
    assert!(
        said.contains("<name>@<team>"),
        "a bare name is refused as a peer identity: {said}"
    );

    // And a name that is not the lead's — the socket delivers to the
    // session, which is its lead (M4), and says so.
    let refused = client
        .post(format!("{SOCKET_URL}/team/nobody/message"))
        .json(&serde_json::json!({
            "from": "team-lead@session-feedbeef",
            "text": "anyone",
        }))
        .send()
        .await
        .expect("the socket answers");
    assert_eq!(refused.status(), 400);
    let said = refused.text().await.expect("a body");
    assert!(
        said.contains("for that session's lead") && said.contains("team-lead"),
        "the refusal names the lead: {said}"
    );

    assert!(
        lead_inbox(&registry).is_empty(),
        "every refusal left the inbox untouched"
    );

    socket.shutdown().await.expect("the socket server stops");
}

/// **The socket serves exactly four routes** (D505, a standing ruling;
/// **D534** added the fourth): health, the roster, a peer's message, and a
/// peer's receipt. Every other route — every one that mutates the session,
/// and every other read of it — is TCP's alone: over the socket it does not
/// exist (`404`), while over TCP it is served behind the configured
/// credential (`401` bare, answered with it). Enumerated route by route, so
/// a route added to the TCP table without a decision about the socket shows
/// up here as a failure rather than as a new door.
#[tokio::test]
async fn the_socket_serves_four_routes_and_tcp_serves_the_rest_behind_its_credential() {
    let home = ganja_testkit::temp_dir();
    let (engine, _registry) = led_engine(home.path());
    let path = socket_path(&home, "s.sock");
    let mut tcp_config = loopback_config();
    tcp_config.credentials = Some(credentials());
    let mut socket_config = with_listen(Listen::Unix { path: path.clone() });
    socket_config.credentials = Some(credentials());
    let tcp = ganja_serve::serve(Arc::clone(&engine), tcp_config)
        .await
        .expect("the TCP server comes up");
    let socket = ganja_serve::serve(Arc::clone(&engine), socket_config)
        .await
        .expect("the socket server comes up");
    let base = base_url(&tcp);
    let client = socket_client(&path);
    let session = engine.session_id();
    let session = session.as_str();

    // Every route the TCP table serves that the socket must not, with a
    // body that parses where one is taken, so a TCP answer is the route's
    // own and never a `400` about the payload.
    let tcp_only: Vec<(&str, String, Option<serde_json::Value>)> = vec![
        ("GET", "/config".to_owned(), None),
        ("GET", "/path".to_owned(), None),
        ("GET", "/agent".to_owned(), None),
        ("GET", "/command".to_owned(), None),
        ("GET", "/session".to_owned(), None),
        ("GET", format!("/session/{session}"), None),
        ("GET", format!("/session/{session}/message"), None),
        (
            "POST",
            format!("/session/{session}/message"),
            Some(serde_json::json!({"text": "hi"})),
        ),
        (
            "POST",
            format!("/session/{session}/prompt_async"),
            Some(serde_json::json!({"text": "hi"})),
        ),
        ("POST", format!("/session/{session}/abort"), None),
        ("POST", format!("/session/{session}/summarize"), None),
        (
            "POST",
            format!("/session/{session}/command"),
            Some(serde_json::json!({"name": "init"})),
        ),
        (
            "POST",
            format!("/session/{session}/shell"),
            Some(serde_json::json!({"command": "true"})),
        ),
        ("POST", format!("/session/{session}/revert"), None),
        ("POST", format!("/session/{session}/unrevert"), None),
        (
            "POST",
            format!("/session/{session}/agent"),
            Some(serde_json::json!({"name": "build"})),
        ),
        (
            "POST",
            format!("/session/{session}/model"),
            Some(serde_json::json!({"model": "scripted-model"})),
        ),
        ("POST", "/session".to_owned(), None),
        ("GET", "/permission".to_owned(), None),
        (
            "POST",
            "/permission/per_nobody/reply".to_owned(),
            Some(serde_json::json!({"response": "once"})),
        ),
        ("GET", "/event".to_owned(), None),
    ];

    for (method, route, body) in &tcp_only {
        // Over the socket: not there. Credential or no credential — none is
        // presented, and none would help.
        let mut request = client.request(
            reqwest::Method::from_bytes(method.as_bytes()).expect("a method"),
            format!("{SOCKET_URL}{route}"),
        );
        if let Some(body) = body {
            request = request.json(body);
        }
        let over_socket = request.send().await.expect("the socket answers");
        assert_eq!(
            over_socket.status(),
            404,
            "{method} {route} does not exist on the socket: {}",
            over_socket.text().await.unwrap_or_default()
        );
    }

    // Over TCP: gated, and served. The gate is asserted on every route; that
    // the route is *there* is asserted on the ones whose answer cannot start
    // a turn or run a command in this test's own working directory — the
    // point is the table, and the table is the same for all of them.
    for (method, route, _) in &tcp_only {
        let bare = reqwest::Client::new()
            .request(
                reqwest::Method::from_bytes(method.as_bytes()).expect("a method"),
                format!("{base}{route}"),
            )
            .send()
            .await
            .expect("TCP answers");
        assert_eq!(
            bare.status(),
            401,
            "{method} {route} on TCP wants the credential"
        );
    }
    // The stored-session reads are left out too: this engine is ephemeral,
    // and their honest answer over TCP is a `404` about the store.
    for (method, route, body) in tcp_only.iter().filter(|(method, route, _)| {
        matches!(*method, "GET") && route != "/event" && !route.starts_with("/session/")
            || route.ends_with("/abort")
            || route.ends_with("/agent")
            || route.ends_with("/model")
            || route.ends_with("/reply")
    }) {
        let mut request = reqwest::Client::new()
            .request(
                reqwest::Method::from_bytes(method.as_bytes()).expect("a method"),
                format!("{base}{route}"),
            )
            .header("authorization", basic());
        if let Some(body) = body {
            request = request.json(body);
        }
        let served = request.send().await.expect("TCP answers");
        assert_ne!(
            served.status(),
            404,
            "{method} {route} is served over TCP: {}",
            served.text().await.unwrap_or_default()
        );
    }

    // And the two routes the socket serves that no other test here counts,
    // without a credential — the two `/team` routes are AC-26's own test,
    // above.
    let health = client
        .get(format!("{SOCKET_URL}/global/health"))
        .send()
        .await
        .expect("the socket answers");
    assert_eq!(health.status(), 200);

    let receipt = client
        .post(format!("{SOCKET_URL}/peer/receipt"))
        .json(&receipt_body(
            "01998ad0-0000-7000-8000-00000000d534",
            "delivered",
        ))
        .send()
        .await
        .expect("the socket answers");
    assert_eq!(
        receipt.status(),
        204,
        "the fourth route is there and takes no credential: {}",
        receipt.text().await.unwrap_or_default()
    );

    tcp.shutdown().await.expect("the TCP server stops");
    socket.shutdown().await.expect("the socket server stops");
}

/// `GET /global/health` names the session the server is serving, on the
/// socket as on TCP — the one fact that maps a socket file, named by a
/// prefix of an id, back to the session it belongs to (**D505**).
#[tokio::test]
async fn health_names_the_session_the_socket_serves() {
    let home = ganja_testkit::temp_dir();
    let (engine, _registry) = led_engine(home.path());
    let path = socket_path(&home, "s.sock");
    let socket = ganja_serve::serve(
        Arc::clone(&engine),
        with_listen(Listen::Unix { path: path.clone() }),
    )
    .await
    .expect("the socket server comes up");

    let health: serde_json::Value = socket_client(&path)
        .get(format!("{SOCKET_URL}/global/health"))
        .send()
        .await
        .expect("the socket answers")
        .json()
        .await
        .expect("health is JSON");
    assert_eq!(health["healthy"], true);
    assert_eq!(
        health["session_id"],
        engine.session_id().as_str(),
        "the id is the engine's current slot: {health}"
    );

    socket.shutdown().await.expect("the socket server stops");
}

/// A session leading no team has nothing to show and nowhere to deliver:
/// `404` on both routes, on both transports.
#[tokio::test]
async fn a_session_leading_no_team_answers_not_found_on_both_routes() {
    let home = ganja_testkit::temp_dir();
    let engine = support::engine();
    let path = socket_path(&home, "s.sock");
    let tcp = ganja_serve::serve(Arc::clone(&engine), loopback_config())
        .await
        .expect("the TCP server comes up");
    let socket = ganja_serve::serve(
        Arc::clone(&engine),
        with_listen(Listen::Unix { path: path.clone() }),
    )
    .await
    .expect("the socket server comes up");

    let over_tcp = reqwest::get(format!("{}/team", base_url(&tcp)))
        .await
        .expect("TCP answers");
    assert_eq!(over_tcp.status(), 404);
    let client = socket_client(&path);
    let over_socket = client
        .get(format!("{SOCKET_URL}/team"))
        .send()
        .await
        .expect("the socket answers");
    assert_eq!(over_socket.status(), 404);
    let posted = client
        .post(format!("{SOCKET_URL}/team/team-lead/message"))
        .json(&serde_json::json!({
            "from": "team-lead@session-feedbeef",
            "text": "anyone",
        }))
        .send()
        .await
        .expect("the socket answers");
    assert_eq!(posted.status(), 404);
    assert!(
        posted
            .text()
            .await
            .expect("a body")
            .contains("leads no team")
    );

    tcp.shutdown().await.expect("the TCP server stops");
    socket.shutdown().await.expect("the socket server stops");
}

// ---------------------------------------------------------------------------
// D534 — the receipt route (**AC-26**'s route half)
// ---------------------------------------------------------------------------

/// The receipt route is the message route's mirror: not on the TCP table at
/// all — `404`, the same answer as any route that does not exist — and on
/// the socket answered without a credential.
#[tokio::test]
async fn post_peer_receipt_is_not_registered_on_tcp() {
    let home = ganja_testkit::temp_dir();
    let (engine, _registry) = led_engine(home.path());
    let path = socket_path(&home, "s.sock");
    let body = receipt_body("01998ad0-0000-7000-8000-00000000d534", "delivered");

    let tcp = ganja_serve::serve(Arc::clone(&engine), loopback_config())
        .await
        .expect("the TCP server comes up");
    let socket = ganja_serve::serve(
        Arc::clone(&engine),
        with_listen(Listen::Unix { path: path.clone() }),
    )
    .await
    .expect("the socket server comes up");

    let over_tcp = reqwest::Client::new()
        .post(format!("{}/peer/receipt", base_url(&tcp)))
        .json(&body)
        .send()
        .await
        .expect("TCP answers");
    assert_eq!(
        over_tcp.status(),
        404,
        "on TCP the route does not exist: {}",
        over_tcp.text().await.unwrap_or_default()
    );

    let over_socket = socket_client(&path)
        .post(format!("{SOCKET_URL}/peer/receipt"))
        .json(&body)
        .send()
        .await
        .expect("the socket answers");
    assert_eq!(over_socket.status(), 204);
    assert!(
        over_socket.bytes().await.expect("a body").is_empty(),
        "and says nothing at all"
    );

    tcp.shutdown().await.expect("the TCP server stops");
    socket.shutdown().await.expect("the socket server stops");
}

/// **AC-26, the route half.** Whatever a receipt named, the wire says the
/// same thing: an id this session never minted, the same id posted again, an
/// id that a first terminal already settled, and a second terminal for it
/// all answer one status and one (empty) body. A distinct answer for any of
/// them would let any same-uid process enumerate which settlements a session
/// is waiting on, which is the argument `socket_routes`' own doc keeps.
///
/// The fourth case — an id this session really is holding open — is not
/// drivable from this crate: an entry is registered only by the sending
/// side's own `send_message` path, which `Engine` exposes no seam to reach,
/// so it is asserted where it can be, in core's own suite. What this crate
/// can pin, and does, is that the route has nothing to branch on at all:
/// `Engine::apply_receipt` answers `()`, and `routes_tests.rs` asserts the
/// handler's table is what its argument says it is.
#[tokio::test]
async fn a_receipt_answers_the_same_bytes_whatever_id_it_named() {
    let home = ganja_testkit::temp_dir();
    let (engine, _registry) = led_engine(home.path());
    let path = socket_path(&home, "s.sock");
    let socket = ganja_serve::serve(
        Arc::clone(&engine),
        with_listen(Listen::Unix { path: path.clone() }),
    )
    .await
    .expect("the socket server comes up");
    let client = socket_client(&path);
    let known = "01998ad0-0000-7000-8000-0000000000a1";

    let mut answers = Vec::new();
    for (case, body) in [
        (
            "an id this session never minted",
            receipt_body(known, "delivered"),
        ),
        (
            "the very same receipt again",
            receipt_body(known, "delivered"),
        ),
        (
            "a second terminal for that id",
            receipt_body(known, "denied"),
        ),
        (
            "another id nothing is waiting on",
            receipt_body("01998ad0-0000-7000-8000-0000000000a2", "expired"),
        ),
    ] {
        let answered = client
            .post(format!("{SOCKET_URL}/peer/receipt"))
            .json(&body)
            .send()
            .await
            .expect("the socket answers");
        answers.push((
            case,
            answered.status().as_u16(),
            answered.bytes().await.expect("a body").to_vec(),
        ));
    }

    let (first_case, first_status, first_body) = answers[0].clone();
    for (case, status, body) in &answers[1..] {
        assert_eq!(
            (*status, body),
            (first_status, &first_body),
            "{case} answers what {first_case} answered"
        );
    }
    assert_eq!(first_status, 204, "and that answer is `204`, said once");
    assert!(first_body.is_empty(), "with nothing in it");

    socket.shutdown().await.expect("the socket server stops");
}

/// A status this route does not carry is a body that does not parse — the
/// string `"held"` included, because ganja answers *held* synchronously on
/// the message route and never over this one. Shape predates policy here as
/// it does on the ladder.
#[tokio::test]
async fn a_receipt_naming_a_status_this_route_does_not_carry_is_refused() {
    let home = ganja_testkit::temp_dir();
    let (engine, _registry) = led_engine(home.path());
    let path = socket_path(&home, "s.sock");
    let socket = ganja_serve::serve(
        Arc::clone(&engine),
        with_listen(Listen::Unix { path: path.clone() }),
    )
    .await
    .expect("the socket server comes up");

    let refused = socket_client(&path)
        .post(format!("{SOCKET_URL}/peer/receipt"))
        .json(&receipt_body(
            "01998ad0-0000-7000-8000-0000000000b1",
            "held",
        ))
        .send()
        .await
        .expect("the socket answers");
    assert_eq!(refused.status(), 400);
    assert!(
        refused
            .text()
            .await
            .expect("a body")
            .contains("does not parse"),
        "and says so readably"
    );

    socket.shutdown().await.expect("the socket server stops");
}

// ---------------------------------------------------------------------------
// D532 — the envelope reaches the gate, and the answer keeps its shape
// ---------------------------------------------------------------------------

/// The four envelope fields are carried, not judged: a body bearing all of
/// them is admitted exactly as the three-field body beside it, and both
/// answers are the same bytes. What the envelope buys happens inside the
/// gate; what this route owes is to hand it over unchanged.
#[tokio::test]
async fn an_enveloped_body_and_a_bare_one_are_admitted_alike() {
    let enveloped_home = ganja_testkit::temp_dir();
    let bare_home = ganja_testkit::temp_dir();
    let (enveloped_engine, enveloped_registry) = led_engine(enveloped_home.path());
    let (bare_engine, bare_registry) = led_engine(bare_home.path());
    let enveloped_path = socket_path(&enveloped_home, "e.sock");
    let bare_path = socket_path(&bare_home, "b.sock");
    let enveloped_server = ganja_serve::serve(
        Arc::clone(&enveloped_engine),
        with_listen(Listen::Unix {
            path: enveloped_path.clone(),
        }),
    )
    .await
    .expect("the socket server comes up");
    let bare_server = ganja_serve::serve(
        Arc::clone(&bare_engine),
        with_listen(Listen::Unix {
            path: bare_path.clone(),
        }),
    )
    .await
    .expect("the socket server comes up");

    let enveloped = socket_client(&enveloped_path)
        .post(format!("{SOCKET_URL}/team/team-lead/message"))
        .json(&enveloped_message(
            "did the envelope cross",
            "01998ad0-0000-7000-8000-0000000000c1",
        ))
        .send()
        .await
        .expect("the socket answers");
    let bare = socket_client(&bare_path)
        .post(format!("{SOCKET_URL}/team/team-lead/message"))
        .json(&serde_json::json!({
            "from": "w1@session-feedbeef",
            "text": "did the envelope cross",
            "summary": "an enveloped arrival",
        }))
        .send()
        .await
        .expect("the socket answers");

    assert_eq!(enveloped.status(), 200);
    assert_eq!(bare.status(), enveloped.status());
    let enveloped_body = enveloped.bytes().await.expect("a body");
    let bare_body = bare.bytes().await.expect("a body");
    assert_eq!(
        enveloped_body,
        bare_body,
        "an old sender's body is answered like a new one's: {} vs {}",
        String::from_utf8_lossy(&enveloped_body),
        String::from_utf8_lossy(&bare_body),
    );
    assert!(
        !String::from_utf8_lossy(&enveloped_body).contains("\"held\""),
        "and neither answer carries the hold field: {}",
        String::from_utf8_lossy(&enveloped_body)
    );

    assert_eq!(lead_inbox(&enveloped_registry).len(), 1, "both landed");
    assert_eq!(lead_inbox(&bare_registry).len(), 1);

    enveloped_server
        .shutdown()
        .await
        .expect("the enveloped server stops");
    bare_server.shutdown().await.expect("the bare server stops");
}

/// A hold is the one answer that differs, and it differs by exactly one
/// typed field (**N2**): `held` carries the gate's own cause, while the
/// accept and the refuse beside it stay byte-identical to each other and
/// carry no such field at all — which is what keeps the enumeration channel
/// this table closes shut.
#[tokio::test]
async fn a_hold_answers_its_typed_cause_while_accept_and_refuse_stay_identical() {
    let accepting_home = ganja_testkit::temp_dir();
    let refusing_home = ganja_testkit::temp_dir();
    let holding_home = ganja_testkit::temp_dir();
    let (accepting, _) = led_engine(accepting_home.path());
    let (refusing, _) = led_engine_with_policy(
        refusing_home.path(),
        (InboundPolicy::Refuse, PolicySource::Global),
    );
    let (holding, holding_registry) = led_engine_with_policy(
        holding_home.path(),
        (InboundPolicy::Hold, PolicySource::Global),
    );
    let accepting_path = socket_path(&accepting_home, "a.sock");
    let refusing_path = socket_path(&refusing_home, "r.sock");
    let holding_path = socket_path(&holding_home, "h.sock");

    let mut servers = Vec::new();
    for (engine, path) in [
        (&accepting, &accepting_path),
        (&refusing, &refusing_path),
        (&holding, &holding_path),
    ] {
        servers.push(
            ganja_serve::serve(
                Arc::clone(engine),
                with_listen(Listen::Unix { path: path.clone() }),
            )
            .await
            .expect("the socket server comes up"),
        );
    }

    let body = enveloped_message("hold or not", "01998ad0-0000-7000-8000-0000000000d1");
    let mut answered = Vec::new();
    for path in [&accepting_path, &refusing_path, &holding_path] {
        let response = socket_client(path)
            .post(format!("{SOCKET_URL}/team/team-lead/message"))
            .json(&body)
            .send()
            .await
            .expect("the socket answers");
        answered.push((
            response.status().as_u16(),
            response.bytes().await.expect("a body").to_vec(),
        ));
    }
    let [
        (accept_status, accept_body),
        (refuse_status, refuse_body),
        (hold_status, hold_body),
    ] = <[_; 3]>::try_from(answered).expect("three answers");

    assert_eq!(accept_status, 200);
    assert_eq!(refuse_status, accept_status);
    assert_eq!(hold_status, accept_status, "a hold is not an error either");
    assert_eq!(
        refuse_body,
        accept_body,
        "the accept and the refuse are the same bytes: {} vs {}",
        String::from_utf8_lossy(&accept_body),
        String::from_utf8_lossy(&refuse_body),
    );
    assert!(
        !String::from_utf8_lossy(&accept_body).contains("\"held\""),
        "and neither of them carries the hold field: {}",
        String::from_utf8_lossy(&accept_body)
    );

    let held: serde_json::Value = serde_json::from_slice(&hold_body).expect("a JSON answer");
    assert_eq!(held["to"], "team-lead");
    assert_eq!(
        held["held"]["cause"],
        serde_json::json!({ "kind": "explicit", "source": "global" }),
        "the hold names its cause as a typed value: {held}"
    );
    assert!(
        held["note"]
            .as_str()
            .is_some_and(|note| note.contains("held for a person's review")),
        "and the prose beside it still says so: {held}"
    );
    assert!(
        lead_inbox(&holding_registry).is_empty(),
        "a held message reaches no inbox"
    );

    for server in servers {
        server.shutdown().await.expect("the socket server stops");
    }
}
