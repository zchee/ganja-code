//! The port policy, observed on real sockets: an explicit port is taken
//! exactly or refused, and no port means 4096 first with an OS-assigned
//! fallback — and whatever was bound, the handle reports the truth.

mod support;

use ganja_serve::{DEFAULT_PORT, ServeError};
use support::{base_url, engine, loopback_config};

async fn healthy(handle: &ganja_serve::Handle) {
    let health = reqwest::get(format!("{}/global/health", base_url(handle)))
        .await
        .expect("the reported address answers")
        .json::<serde_json::Value>()
        .await
        .expect("health is JSON");
    assert_eq!(health["healthy"], true);
}

#[tokio::test]
async fn an_explicit_port_that_is_taken_is_refused_rather_than_replaced() {
    // Hold a port, then ask for exactly it.
    let holder = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port exists");
    let taken = holder
        .local_addr()
        .expect("the holder has an address")
        .port();

    let mut config = loopback_config();
    config.port = Some(taken);
    let refused = ganja_serve::serve(engine(), config).await;

    assert!(
        matches!(refused, Err(ServeError::Bind { address, .. }) if address.port() == taken),
        "an explicit port is that port or nothing"
    );
    drop(holder);
}

#[tokio::test]
async fn an_os_assigned_port_is_reported_truthfully() {
    let handle = ganja_serve::serve(engine(), loopback_config())
        .await
        .expect("a loopback server comes up");

    assert_ne!(handle.address().port(), 0, "the truth, not the ask");
    healthy(&handle).await;

    handle.shutdown().await.expect("a clean stop");
}

/// Both halves of the no-port policy in one test, because they share the one
/// contended resource: with 4096 held, the server comes up elsewhere and says
/// where; with 4096 free, the server prefers it.
#[tokio::test]
async fn no_port_tries_4096_first_and_falls_back_when_it_is_taken() {
    let mut config = loopback_config();
    config.port = None;

    // Try to hold 4096 ourselves. Either way it ends held or the environment
    // already holds it — the fallback half is provable regardless.
    let holder = std::net::TcpListener::bind(("127.0.0.1", DEFAULT_PORT));

    let fallback = ganja_serve::serve(engine(), {
        let mut config = loopback_config();
        config.port = None;
        config
    })
    .await
    .expect("a taken 4096 is a fallback, not a failure");
    if holder.is_ok() {
        assert_ne!(
            fallback.address().port(),
            DEFAULT_PORT,
            "4096 is held, so the fallback lands elsewhere"
        );
    }
    healthy(&fallback).await;
    fallback.shutdown().await.expect("a clean stop");

    // With the holder gone, 4096 is preferred. Only provable when this test
    // owned 4096 a moment ago; an environment where something else holds it
    // has already proven the fallback half above.
    if let Ok(holder) = holder {
        drop(holder);
        let preferred = ganja_serve::serve(engine(), config)
            .await
            .expect("a free 4096 binds");
        assert_eq!(preferred.address().port(), DEFAULT_PORT);
        healthy(&preferred).await;
        preferred.shutdown().await.expect("a clean stop");
    }
}
