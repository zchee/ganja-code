//! The socket address form (**D505**), against a listener that answers real
//! bytes over a Unix socket.
//!
//! **Contract-level, not end to end.** The far end here is `support`'s stub,
//! answering the bodies `ganja-serve` puts on the wire, so what these pin is
//! this side: that a call crosses the socket, what it sends, and how it reads
//! the answer. `ganja-serve/tests/uds.rs` pins the server end against a real
//! engine, and `ganja-cli/tests/uds.rs` the two processes together (AC-9);
//! nothing here claims anything about serve's behavior beyond the shape of
//! its bodies.

#![cfg(unix)]

mod support;

use ganja_client::ClientError;
use support::{Reply, Stub};

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

// ---------------------------------------------------------------------------
// Refusals read the way every other refusal does.
// ---------------------------------------------------------------------------

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
