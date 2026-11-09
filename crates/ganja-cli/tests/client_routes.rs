//! Typed client operations against the served engine, not a wire stub.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use ganja_client::{BootstrapStatus, Client};
use ganja_core::Engine;
use ganja_core::provider::fake::FakeProvider;
use ganja_core::tool::Registry;
use ganja_permission::Permissions;
use ganja_protocol::{Event, Part};

#[tokio::test(flavor = "multi_thread")]
async fn typed_routes_read_the_real_abort_shell_messages_and_bootstrap_shapes() {
    let directory = ganja_testkit::temp_dir();
    let storage = ganja_core::Storage::open(directory.path().join("sessions"));
    let engine = Arc::new(Engine::persistent(
        Arc::new(FakeProvider::new("reply", Duration::ZERO)),
        "canned",
        Arc::new(Registry::new(Vec::new())),
        Permissions::default(),
        storage,
    ));
    let mut config = ganja_serve::ServeConfig::in_directory(directory.path().to_path_buf());
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
        messages
            .iter()
            .flat_map(|message| &message.parts)
            .filter_map(Part::as_text)
            .any(|text| text.contains("client-route-shell"))
    );
    handle.shutdown().await.expect("server stops");
}
