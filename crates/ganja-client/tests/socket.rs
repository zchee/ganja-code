//! The socket address form and the two team routes (**D505**), against a
//! listener that answers real bytes — over a Unix socket for the form that
//! is the point, and over loopback TCP for the routes every form drives.
//!
//! **Contract-level, not end to end.** The far end here is `support`'s stub,
//! answering the bodies `ganja-serve` puts on the wire, so what these pin is
//! this side: which route a call drives, what it sends, and how it reads the
//! answer. `ganja-serve/tests/team.rs` pins the server end against a real
//! engine, and `ganja-cli/tests/uds.rs` the two processes together (AC-9);
//! nothing here claims anything about serve's behavior beyond the shape of
//! its bodies.

#![cfg(unix)]

mod support;

use ganja_client::{ClientError, Delivered, TeamMessage};
use support::{Reply, Stub};

/// The `TeamView` a serve answers for a team that is its lead alone, in the
/// bytes serve writes.
const LEAD_ONLY: &str = r#"{"team":"session-feedbeef","lead":"team-lead","members":[{"name":"team-lead","agent_id":"team-lead@session-feedbeef","backend":"in-process","is_lead":true}]}"#;

// ---------------------------------------------------------------------------
// The socket form rides the same wire.
// ---------------------------------------------------------------------------

/// The one call everything that attaches opens with, over a socket: the
/// request crosses the socket, and the typed answer is what it always was.
#[tokio::test]
async fn health_rides_the_socket_form() {
    let stub = Stub::on_socket(|_| Reply::ok(r#"{"healthy":true,"version":"0.1.0","session_id":"01998ad0-0000-7000-8000-00000000d505"}"#)).await;

    let health = stub
        .client()
        .health()
        .await
        .expect("health answers over the socket");
    assert!(health.healthy);
    assert_eq!(health.version, "0.1.0");

    let request = stub.only_request();
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/global/health");
    assert_eq!(
        request.authorization, None,
        "a same-uid socket presents no credential"
    );
}

/// `GET /team` over the socket reads the roster whole, as a `TeamView`.
#[tokio::test]
async fn team_answers_the_roster_over_the_socket() {
    let stub = Stub::on_socket(|_| Reply::ok(LEAD_ONLY)).await;

    let team = stub.client().team().await.expect("the roster answers");
    assert_eq!(team.team, "session-feedbeef");
    assert_eq!(team.lead, "team-lead");
    assert_eq!(team.members.len(), 1);
    assert!(team.members[0].is_lead);
    assert_eq!(stub.only_request().path, "/team");
}

/// `POST /team/{name}/message` over the socket sends the three-field body
/// serve takes and reads its `Delivered` answer.
#[tokio::test]
async fn a_team_message_posts_the_wire_body_and_reads_what_became_of_it() {
    let stub = Stub::on_socket(|_| {
        Reply::ok(
            r#"{"to":"team-lead","note":"It is in that inbox and will be read on the next pass."}"#,
        )
    })
    .await;

    let delivered = stub
        .client()
        .send_team_message(
            "team-lead",
            &TeamMessage::new("team-lead@session-abcd1234", "how far along is W7").summarized("W7"),
        )
        .await
        .expect("the message is taken");
    assert_eq!(
        delivered,
        Delivered {
            to: "team-lead".to_owned(),
            note: "It is in that inbox and will be read on the next pass.".to_owned(),
        }
    );

    let request = stub.only_request();
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/team/team-lead/message");
    let body: serde_json::Value =
        serde_json::from_str(&request.body).expect("the body is one JSON object");
    assert_eq!(
        body,
        serde_json::json!({
            "from": "team-lead@session-abcd1234",
            "text": "how far along is W7",
            "summary": "W7",
        }),
        "exactly the three names serve's body declares"
    );
}

/// A message with nothing summarized sends no `summary` key at all — serve's
/// body takes the field as optional, and absent is how nothing is said.
#[tokio::test]
async fn an_unsummarized_message_sends_no_summary_key() {
    let stub = Stub::on_socket(|_| Reply::ok(r#"{"to":"w1","note":"landed"}"#)).await;

    stub.client()
        .send_team_message("w1", &TeamMessage::new("team-lead@session-abcd1234", "go"))
        .await
        .expect("the message is taken");

    let body: serde_json::Value =
        serde_json::from_str(&stub.only_request().body).expect("one JSON object");
    assert!(body.get("summary").is_none(), "{body}");
}

// ---------------------------------------------------------------------------
// Refusals read the way every other refusal does.
// ---------------------------------------------------------------------------

/// A server's refusal of a team message — a `400` for a frame, a `404` for a
/// TCP server that never registered the route — is [`ClientError::Refused`]
/// carrying the status and the server's own sentence, so a caller switches
/// on the status and a person reads the reason.
#[tokio::test]
async fn a_refused_team_message_carries_the_status_and_the_servers_sentence() {
    let stub = Stub::on_socket(|_| Reply::Json {
        status: 400,
        body: r#"{"type":"invalid_request","message":"A protocol frame does not cross a socket"}"#
            .to_owned(),
    })
    .await;

    let refused = stub
        .client()
        .send_team_message(
            "team-lead",
            &TeamMessage::new(
                "team-lead@session-abcd1234",
                "{\"type\":\"idle_notification\"}",
            ),
        )
        .await
        .expect_err("the server refused");
    match refused {
        ClientError::Refused {
            method,
            path,
            status,
            body,
        } => {
            assert_eq!(method, "POST");
            assert_eq!(path, "/team/team-lead/message");
            assert_eq!(status, 400);
            assert!(body.contains("does not cross a socket"), "{body}");
        }
        other => panic!("a refusal, not {other:?}"),
    }
}

/// A socket nothing listens at is a transport error naming the socket under
/// its `uds:` spelling — the address a `send_message` call would have
/// written — never a hang.
#[tokio::test]
async fn a_dead_socket_is_a_transport_error_naming_the_socket() {
    let path = std::env::temp_dir().join(format!("ganja-client-{}-dead.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let client = ganja_client::Client::on_socket(&path).expect("a path binds a client");

    let failed = tokio::time::timeout(support::DEADLINE, client.health())
        .await
        .expect("a dead socket answers within the deadline")
        .expect_err("nothing listens");
    match failed {
        ClientError::Transport { address, .. } => {
            assert_eq!(address, format!("uds:{}", path.display()));
        }
        other => panic!("a transport failure, not {other:?}"),
    }
}

/// A body past the client's cap is refused unread, whatever route answered
/// it: the far end of a socket is another process's word, and a listing
/// that walks every socket in a directory must not be made to buffer what a
/// hostile one sends. Declared oversize here, so not a byte of it is read.
#[tokio::test]
async fn an_oversized_answer_is_refused_unread() {
    let stub = Stub::on_socket(|_| Reply::Json {
        status: 200,
        body: "x".repeat(ganja_client::BODY_CAP + 1),
    })
    .await;

    let refused = tokio::time::timeout(support::DEADLINE, stub.client().health())
        .await
        .expect("an oversized answer is refused within the deadline")
        .expect_err("more than the cap");
    match refused {
        ClientError::Oversized { method, path, cap } => {
            assert_eq!(method, "GET");
            assert_eq!(path, "/global/health");
            assert_eq!(cap, ganja_client::BODY_CAP);
        }
        other => panic!("an oversize refusal, not {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The same routes over TCP: one client, two forms, one wire.
// ---------------------------------------------------------------------------

/// `GET /team` is declared on the TCP form too, since serve answers it on
/// both listeners; the request is the same bytes.
#[tokio::test]
async fn team_is_the_same_route_over_tcp() {
    let stub = Stub::always(Reply::ok(LEAD_ONLY)).await;

    let team = stub.client().team().await.expect("the roster answers");
    assert_eq!(team.lead, "team-lead");
    assert_eq!(stub.only_request().path, "/team");
}

/// A TCP server never registers the write route and answers `404`; the
/// client passes that on as a refusal rather than pre-empting the server —
/// which listener answers a route is the server's fact.
#[tokio::test]
async fn a_team_message_over_tcp_is_refused_by_the_server_not_the_client() {
    let stub = Stub::always(Reply::Json {
        status: 404,
        body: String::new(),
    })
    .await;

    let refused = stub
        .client()
        .send_team_message("team-lead", &TeamMessage::new("a@b", "hello"))
        .await
        .expect_err("the server has no such route");
    assert!(
        matches!(refused, ClientError::Refused { status: 404, .. }),
        "{refused:?}"
    );
    assert_eq!(
        stub.only_request().path,
        "/team/team-lead/message",
        "the client sent it all the same"
    );
}
