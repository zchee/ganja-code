//! The three security postures, pinned: a non-loopback bind with no password
//! is refused at startup, a request naming a directory this server does not
//! serve is `400`, and a configured password gates every route — `401` with
//! the Basic challenge, satisfied by the header or by `?auth_token=`.

mod support;

use base64::Engine as _;
use ganja_core::{permission::Permissions, tool::Registry};
use ganja_serve::{Credentials, ServeError};
use ganja_testkit::says;
use secrecy::SecretString;
use support::{base_url, loopback_config, scripted_engine};

fn engine() -> std::sync::Arc<ganja_core::Engine> {
    scripted_engine(
        vec![says("hi")],
        Registry::new(Vec::new()),
        Permissions::default(),
    )
}

#[tokio::test]
async fn a_non_loopback_bind_with_no_password_is_refused_at_startup() {
    let mut config = loopback_config();
    config.hostname = "0.0.0.0".to_owned();
    config.credentials = None;

    let refused = ganja_serve::serve(engine(), config).await;
    let error = match refused {
        Err(error) => error,
        Ok(handle) => panic!(
            "an unsecured non-loopback bind must not come up; it did, on {}",
            handle.address()
        ),
    };

    assert!(
        matches!(
            error,
            ServeError::UnsecuredNonLoopback { ref hostname } if hostname == "0.0.0.0"
        ),
        "the refusal names the posture: {error:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains("without a password") && message.contains("GANJA_SERVER_PASSWORD"),
        "the message says why and what to set: {message}"
    );
}

#[tokio::test]
async fn a_request_naming_another_directory_is_refused_with_400() {
    let handle = ganja_serve::serve(engine(), loopback_config())
        .await
        .expect("a loopback server comes up");
    let base = base_url(&handle);
    let served = std::env::current_dir()
        .expect("the working directory resolves")
        .display()
        .to_string();

    // The right directory — by header, by query, or unstated — is served.
    for request in [
        reqwest::Client::new().get(format!("{base}/global/health")),
        reqwest::Client::new()
            .get(format!("{base}/global/health"))
            .header(ganja_serve::DIRECTORY_HEADER, served.clone()),
        reqwest::Client::new().get(format!("{base}/global/health?directory={served}")),
    ] {
        let response = request.send().await.expect("the route answers");
        assert_eq!(response.status(), 200, "the served directory is served");
    }

    // Another directory — even a real one — is a refusal, never a silent
    // answer about the wrong worktree.
    let elsewhere = std::env::temp_dir().display().to_string();
    let by_header = reqwest::Client::new()
        .get(format!("{base}/global/health"))
        .header(ganja_serve::DIRECTORY_HEADER, elsewhere.clone())
        .send()
        .await
        .expect("the route answers");
    assert_eq!(by_header.status(), 400);
    let body = by_header
        .json::<serde_json::Value>()
        .await
        .expect("the refusal is tagged JSON");
    assert_eq!(body["type"], "invalid_request");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains(&elsewhere)),
        "the refusal names the directory it refused: {body}"
    );

    let by_query = reqwest::get(format!("{base}/global/health?directory={elsewhere}"))
        .await
        .expect("the route answers");
    assert_eq!(
        by_query.status(),
        400,
        "the query spelling is the same refusal"
    );

    handle.shutdown().await.expect("a clean stop");
}

#[tokio::test]
async fn a_configured_password_gates_every_route() {
    let mut config = loopback_config();
    config.credentials = Some(Credentials {
        username: "ganja".to_owned(),
        password: SecretString::from("hunter2"),
    });
    let handle = ganja_serve::serve(engine(), config)
        .await
        .expect("a loopback server comes up");
    let base = base_url(&handle);

    // No credential: the Basic challenge, empty-bodied.
    let refused = reqwest::get(format!("{base}/global/health"))
        .await
        .expect("the route answers");
    assert_eq!(refused.status(), 401);
    assert_eq!(
        refused
            .headers()
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok()),
        Some("Basic realm=\"Secure Area\"")
    );
    assert!(refused.bytes().await.expect("a body").is_empty());

    // The wrong password is the same refusal.
    let wrong = reqwest::Client::new()
        .get(format!("{base}/global/health"))
        .basic_auth("ganja", Some("hunter3"))
        .send()
        .await
        .expect("the route answers");
    assert_eq!(wrong.status(), 401);

    // The Basic header opens the door.
    let by_header = reqwest::Client::new()
        .get(format!("{base}/global/health"))
        .basic_auth("ganja", Some("hunter2"))
        .send()
        .await
        .expect("the route answers");
    assert_eq!(by_header.status(), 200);

    // So does the query escape hatch an EventSource is limited to; the event
    // stream is the route it exists for, so that is the one it is proven on.
    let token = base64::engine::general_purpose::STANDARD.encode("ganja:hunter2");
    let by_token = reqwest::get(format!("{base}/event?auth_token={token}"))
        .await
        .expect("the route answers");
    assert_eq!(by_token.status(), 200);
    assert_eq!(
        by_token
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    drop(by_token);

    handle.shutdown().await.expect("a clean stop");
}
