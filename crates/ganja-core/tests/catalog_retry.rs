//! What a catalog fetch does when the endpoint answers badly.
//!
//! A refusal is retried, twice, and then reported — never turned into an empty
//! catalog. The table a failed refresh leaves behind is the one it started
//! with, which is the whole reason a refresh is allowed to fail quietly.
//!
//! One test, in its own binary: it points the whole process at a source of its
//! own.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use ganja_core::catalog::{self, CatalogError};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
};

/// The smallest thing that is still a catalog.
const PAYLOAD: &str =
    r#"{"fixture":{"models":{"fixture-only":{"limit":{"context":64000,"output":4000}}}}}"#;

/// Answers `script` in order, then repeats its last entry forever.
async fn serve(script: Vec<String>) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback is bindable");
    let url = format!(
        "http://{}",
        listener
            .local_addr()
            .expect("a bound socket has an address")
    );
    let served = Arc::new(AtomicUsize::new(0));
    let remaining = Arc::new(Mutex::new(script));

    tokio::spawn({
        let served = Arc::clone(&served);
        async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };

                // Drain the request first: a client that sees a reset instead
                // of an answer would report a transport failure where the test
                // means to serve a refusal.
                let mut buffer = [0_u8; 1024];
                let _ = socket.read(&mut buffer).await;

                let next = {
                    let mut remaining = remaining
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if remaining.len() > 1 {
                        remaining.remove(0)
                    } else {
                        remaining[0].clone()
                    }
                };

                let _ = socket.write_all(next.as_bytes()).await;
                let _ = socket.flush().await;
                served.fetch_add(1, Ordering::SeqCst);
            }
        }
    });

    (url, served)
}

/// A response with `body` behind `status`.
fn response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: \
         {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn a_refused_request_is_retried_and_then_reported() {
    let cache_home = tempfile::tempdir().expect("a temporary directory");
    let refused = response("503 Service Unavailable", "{}");
    let answered = response("200 OK", PAYLOAD);

    let (url, served) = serve(vec![
        refused.clone(),
        refused.clone(),
        answered,
        refused.clone(),
    ])
    .await;

    // SAFETY: this binary holds one test, so nothing else in the process is
    // reading the environment while it is being written.
    unsafe {
        std::env::set_var("XDG_CACHE_HOME", cache_home.path());
        std::env::set_var(catalog::MODELS_URL_ENV, &url);
        std::env::remove_var(catalog::MODELS_PATH_ENV);
        std::env::remove_var(catalog::DISABLE_FETCH_ENV);
    }

    // Two refusals inside one refresh are waited out rather than reported: the
    // third attempt is the one that answers.
    let started = Instant::now();
    assert!(
        catalog::refresh(true)
            .await
            .expect("the third attempt wins"),
        "a refresh that eventually succeeded replaced the table"
    );
    assert_eq!(
        served.load(Ordering::SeqCst),
        3,
        "two retries, then success"
    );
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(200),
        "the retries waited at all: {:?}",
        started.elapsed()
    );
    assert!(catalog::model("fixture-only").is_some());

    // From here the endpoint only ever refuses, and three attempts is where a
    // refresh gives up and says which status it gave up on.
    let error = catalog::refresh(true)
        .await
        .expect_err("an endpoint that only refuses cannot be fetched from");
    assert!(
        matches!(error, CatalogError::Status { status: 503 }),
        "the refusal is reported as itself: {error}"
    );
    assert_eq!(served.load(Ordering::SeqCst), 6, "three attempts, no more");

    // And the catalog that was already in hand is still in hand: a refresh
    // that failed leaves the table exactly as it found it.
    assert!(
        catalog::model("fixture-only").is_some(),
        "a failed refresh must not empty the table"
    );
}
