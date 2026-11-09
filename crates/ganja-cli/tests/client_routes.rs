//! Typed client operations against the served engine, not a wire stub.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use ganja_client::{BootstrapStatus, Client};
use ganja_core::Engine;
use ganja_core::provider::fake::FakeProvider;
use ganja_core::tool::Registry;
use ganja_permission::Permissions;
use ganja_protocol::{Event, PartBody, ToolState};

#[tokio::test]
async fn client_owned_headers_override_fixed_headers_for_fresh_and_resumed_connections() {
    let directory = ganja_testkit::temp_dir();
    let engine = Arc::new(Engine::new(
        Arc::new(FakeProvider::new("reply", Duration::ZERO)),
        "canned",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
    ));
    let mut config = ganja_serve::ServeConfig::in_directory(directory.path().to_path_buf());
    config.credentials = Some(ganja_serve::Credentials {
        username: "ganja".into(),
        password: "header-precedence-test".into(),
    });
    config.listen = ganja_serve::Listen::Tcp { hostname: "127.0.0.1".into(), port: Some(0) };
    let handle = ganja_serve::serve(engine, config).await.expect("serve");
    let mut headers = ganja_client::HeaderMap::new();
    headers.append("authorization", ganja_client::HeaderValue::from_static("Basic stale"));
    headers.append("authorization", ganja_client::HeaderValue::from_static("Basic also-stale"));
    headers.append("last-event-id", ganja_client::HeaderValue::from_static("stale-cursor"));
    headers.append("last-event-id", ganja_client::HeaderValue::from_static("another-stale-cursor"));
    let client = Client::new(
        &format!("http://{}", handle.address().tcp().expect("tcp address")),
        Some(ganja_client::Credentials::new("ganja", "header-precedence-test")),
    )
    .expect("client")
    .with_headers(headers);
    client.health().await.expect("explicit credentials override every fixed authorization value");
    let fresh = client.events().await.expect("fixed cursor does not change a fresh connection");
    assert_eq!(fresh.connected().replay, ganja_client::sse::Replay::Fresh);
    let cursor = fresh.cursor().expect("fresh head").clone();
    drop(fresh);
    let resumed = client.events_from(Some(cursor.clone())).await.expect("explicit cursor wins");
    assert_eq!(resumed.connected().replay, ganja_client::sse::Replay::From);
    assert_eq!(resumed.cursor(), Some(&cursor));
    drop(resumed);
    handle.shutdown().await.expect("server stops");
}

#[tokio::test(flavor = "multi_thread")]
async fn typed_routes_read_the_real_abort_shell_messages_and_bootstrap_shapes() {
    let directory = ganja_testkit::temp_dir();
    let storage = ganja_core::Storage::open(directory.path().join("sessions"));
    let engine = Arc::new(Engine::persistent(
        Arc::new(FakeProvider::new("reply", Duration::ZERO)),
        "canned",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage.clone(),
    ));
    let mut config = ganja_serve::ServeConfig::in_directory(directory.path().to_path_buf());
    config.storage = Some(storage);
    config.listen = ganja_serve::Listen::Tcp { hostname: "127.0.0.1".to_owned(), port: Some(0) };
    let handle = ganja_serve::serve(engine, config).await.expect("server starts");
    let client =
        Client::new(&format!("http://{}", handle.address().tcp().expect("tcp address")), None)
            .expect("client");
    let session = client.health().await.expect("health").session_id;
    assert!(client.abort(&session).await.expect("200 boolean acknowledgement"));
    assert_eq!(
        client.bootstrap_status().await.expect("serve has handed off"),
        BootstrapStatus::AlreadyServing
    );
    let mut events = client.events().await.expect("registered before shell");
    client.shell(&session, "printf client-route-shell").await.expect("204 acceptance");
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(event) = events.next().await {
            if matches!(event.expect("event"), Event::MessageFinished { .. }) {
                return;
            }
        }
        panic!("stream ended before shell finished");
    })
    .await
    .expect("shell finishes");
    let messages = client.messages(&session).await.expect("stored transcript");
    assert!(
        messages.iter().flat_map(|message| &message.parts).any(|part| {
            matches!(&part.body, PartBody::Tool { state: ToolState::Completed { output, .. }, .. }
                if output.contains("client-route-shell"))
        }),
        "the stored shell result must preserve its output: {messages:#?}"
    );
    handle.shutdown().await.expect("server stops");
}
