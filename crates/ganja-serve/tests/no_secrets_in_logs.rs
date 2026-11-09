//! A configured password must not come back out — not in a log line, not in
//! a `Debug` rendering, not in an error body — and neither may the
//! `auth_token` query value that carries it, which is why the serve layer
//! logs paths and methods and never a query string.
//!
//! Same drill as `ganja-core/tests/secrets_env.rs`: plant a canary, drive
//! real requests at a real socket, and search every byte that came out. The
//! capture is the **global** subscriber so that what handler tasks trace on
//! other threads is in the search space — and the assertion that a request
//! line arrived is what proves the space is not empty.
//!
//! One test, one binary: a global subscriber can be installed once per
//! process.

mod support;

use std::sync::Arc;

use base64::Engine as _;
use futures::future::BoxFuture;
use ganja_core::permission::Permissions;
use ganja_core::tool::Registry;
use ganja_serve::Credentials;
use ganja_serve::bootstrap::{Bootstrap, Cloned, Executor, Failure, Service, Stage};
use ganja_testkit::{LogCapture as Capture, says};
use secrecy::SecretString;
use support::{base_url, loopback_config, scripted_engine};

/// The password planted in the server. Nothing may render it.
const CANARY: &str = "pw-canary-XYZZY-73";
const ENCODED_USER: &str = "repo%2Duser%2Dcanary";
const DECODED_USER: &str = "repo-user-canary";
const ENCODED_PASSWORD: &str = "repo%2Dpassword%2Dcanary";
const DECODED_PASSWORD: &str = "repo-password-canary";
const EXECUTOR_DETAIL: &str = "bootstrap-executor-diagnostic";

struct Refused;

impl Executor for Refused {
    fn execute(
        &self,
        request: Bootstrap,
        _: tokio::time::Instant,
    ) -> BoxFuture<'static, Result<Cloned, Failure>> {
        Box::pin(async move {
            Err(Failure {
                stage: Stage::Dispatch,
                detail: format!(
                    "{EXECUTOR_DETAIL}: {CANARY} {} {ENCODED_USER} {DECODED_USER} {ENCODED_PASSWORD} {DECODED_PASSWORD}",
                    request.repository
                ),
            })
        })
    }
}

#[tokio::test]
async fn the_password_and_the_query_that_carries_it_reach_no_log_line() {
    let capture = Capture::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_max_level(tracing::level_filters::LevelFilter::TRACE)
            .with_ansi(false)
            .finish(),
    )
    .expect("this binary installs the only subscriber");

    let credentials =
        Credentials { username: "ganja".to_owned(), password: SecretString::from(CANARY) };
    // The Debug path: a credential rendered through `{:?}` shows a redaction.
    let debugged = format!("{credentials:?}");
    assert!(!debugged.contains(CANARY), "a Debug rendering leaked the password: {debugged}");

    let mut config = loopback_config();
    config.credentials = Some(credentials);
    let handle = ganja_serve::serve(
        scripted_engine(vec![says("hi")], Registry::new(Vec::new()), Permissions::default()),
        config,
    )
    .await
    .expect("a loopback server comes up");
    let base = base_url(&handle);

    // The credential travels as a query value — the one place a secret is
    // forced into a URL — and as a header; a wrong guess produces the error
    // path too, so every branch that could echo it has run.
    let token = base64::engine::general_purpose::STANDARD.encode(format!("ganja:{CANARY}"));
    let by_token = reqwest::get(format!("{base}/global/health?auth_token={token}"))
        .await
        .expect("the route answers");
    assert_eq!(by_token.status(), 200);

    let by_header = reqwest::Client::new()
        .get(format!("{base}/global/health"))
        .basic_auth("ganja", Some(CANARY))
        .send()
        .await
        .expect("the route answers");
    assert_eq!(by_header.status(), 200);

    let refused = reqwest::get(format!("{base}/global/health?auth_token=d3Jvbmc6Z3Vlc3M="))
        .await
        .expect("the route answers");
    assert_eq!(refused.status(), 401);
    let refusal_body = refused.bytes().await.expect("a body");
    assert!(refusal_body.is_empty(), "the 401 is empty-bodied, echoing nothing: {refusal_body:?}");

    handle.shutdown().await.expect("a clean stop");

    let service = Service::new(
        Credentials { username: "ganja".into(), password: CANARY.into() },
        std::env::current_dir().expect("cwd"),
        Arc::new(Refused),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let init = service.serve(listener).expect("init");
    let base = format!("http://{}", init.address());
    let repository = format!("https://{ENCODED_USER}:{ENCODED_PASSWORD}@example.test/repo");
    let body = serde_json::json!({"repository":repository, "reference":"main", "branch":"work", "author":{"name":"worker", "email":"worker@example.test"}, "deadline_secs":5});
    let response = reqwest::Client::new()
        .post(format!("{base}/bootstrap"))
        .basic_auth("ganja", Some(CANARY))
        .json(&body)
        .send()
        .await
        .expect("refused bootstrap");
    assert_eq!(response.status(), 409);
    let text = response.text().await.expect("body");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text).expect("failure JSON"),
        serde_json::json!({
            "status": "failed", "stage": "dispatch",
            "detail": "bootstrap failed; consult the server log", "cleanup_in_progress": false
        }),
        "executor details never enter the failure body"
    );
    for secret in [CANARY, ENCODED_USER, DECODED_USER, ENCODED_PASSWORD, DECODED_PASSWORD] {
        assert!(!text.contains(secret), "bootstrap body must omit every credential form");
    }
    assert!(!text.contains(EXECUTOR_DETAIL), "even non-credential stderr stays server-side");
    let malformed = reqwest::Client::new()
        .post(format!("{base}/bootstrap"))
        .basic_auth("ganja", Some(CANARY))
        .json(&serde_json::json!({CANARY: CANARY}))
        .send()
        .await
        .expect("invalid body");
    assert_eq!(malformed.status(), 400);
    assert!(!malformed.text().await.expect("body").contains(CANARY));
    init.shutdown().await.expect("init stops");

    let logged = capture.logged();
    // The search space is real: the request lines the serve layer writes are
    // in it, path and method and nothing more.
    assert!(
        logged.contains("/global/health"),
        "the capture holds the serve layer's own request lines: {logged}"
    );
    assert!(!logged.contains(CANARY), "the password reached a log line: {logged}");
    assert!(!logged.contains(&token), "the encoded credential reached a log line: {logged}");
    assert!(!logged.contains("auth_token"), "a query string reached a log line: {logged}");
    assert!(logged.contains(EXECUTOR_DETAIL), "redacted executor diagnostics remain observable");
    for secret in [ENCODED_USER, DECODED_USER, ENCODED_PASSWORD, DECODED_PASSWORD] {
        assert!(!logged.contains(secret), "repository userinfo reached a log line: {logged}");
    }
}
