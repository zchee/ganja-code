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

use std::{
    io,
    sync::{Arc, Mutex},
};

use base64::Engine as _;
use ganja_core::{permission::Permissions, tool::Registry};
use ganja_serve::Credentials;
use ganja_testkit::says;
use secrecy::SecretString;
use support::{base_url, loopback_config, scripted_engine};
use tracing_subscriber::fmt::MakeWriter;

/// The password planted in the server. Nothing may render it.
const CANARY: &str = "pw-canary-XYZZY-73";

/// A `tracing` writer a test can read back.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn logged(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("the log is never poisoned")).into_owned()
    }
}

impl io::Write for Capture {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("the log is never poisoned")
            .extend_from_slice(buffer);

        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
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

    let credentials = Credentials {
        username: "ganja".to_owned(),
        password: SecretString::from(CANARY),
    };
    // The Debug path: a credential rendered through `{:?}` shows a redaction.
    let debugged = format!("{credentials:?}");
    assert!(
        !debugged.contains(CANARY),
        "a Debug rendering leaked the password: {debugged}"
    );

    let mut config = loopback_config();
    config.credentials = Some(credentials);
    let handle = ganja_serve::serve(
        scripted_engine(
            vec![says("hi")],
            Registry::new(Vec::new()),
            Permissions::default(),
        ),
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
    assert!(
        refusal_body.is_empty(),
        "the 401 is empty-bodied, echoing nothing: {refusal_body:?}"
    );

    handle.shutdown().await.expect("a clean stop");

    let logged = capture.logged();
    // The search space is real: the request lines the serve layer writes are
    // in it, path and method and nothing more.
    assert!(
        logged.contains("/global/health"),
        "the capture holds the serve layer's own request lines: {logged}"
    );
    assert!(
        !logged.contains(CANARY),
        "the password reached a log line: {logged}"
    );
    assert!(
        !logged.contains(&token),
        "the encoded credential reached a log line: {logged}"
    );
    assert!(
        !logged.contains("auth_token"),
        "a query string reached a log line: {logged}"
    );
}
