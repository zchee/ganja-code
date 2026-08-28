//! The catalog against a real socket and a real cache directory.
//!
//! One test, because it is one narrative and because every step of it turns on
//! process-wide state — the environment that names the source and the cache
//! home, and the catalog table itself, which a refresh replaces for the whole
//! process. Splitting it would only mean two tests racing over both.
//!
//! Nothing here mocks the HTTP client: the request that is asserted on is the
//! request that was actually built and sent.

use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ganja_core::catalog;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// A catalog in the shape the endpoint publishes: providers keyed by id, each
/// holding models keyed by id, carrying rather more than this build reads and
/// rather less than it would like.
const PAYLOAD: &str = r#"{
  "fixture": {
    "id": "fixture",
    "name": "Fixture Inc",
    "env": ["FIXTURE_API_KEY"],
    "models": {
      "fixture-large": {
        "id": "fixture-large",
        "name": "Fixture Large",
        "family": "fixture",
        "release_date": "2026-02-14",
        "attachment": true,
        "reasoning": true,
        "temperature": false,
        "tool_call": true,
        "status": "beta",
        "modalities": { "input": ["text", "image"], "output": ["text"] },
        "experimental": { "modes": { "thinking": { "cost": { "input": 9, "output": 9 } } } },
        "a_key_this_build_has_never_heard_of": [1, 2, 3],
        "cost": { "input": 4.0, "output": 20.0, "cache_read": 0.4, "cache_write": 5.0 },
        "limit": { "context": 500000, "input": 400000, "output": 32000 }
      },
      "fixture-small": {
        "id": "fixture-small",
        "limit": { "context": 128000, "output": 8000 }
      }
    }
  }
}"#;

/// A loopback endpoint that answers every connection with the same catalog.
struct Endpoint {
    /// Base URL the catalog hangs `api.json` off.
    url: String,
    /// How many connections have been answered.
    served: Arc<AtomicUsize>,
    /// The request head of each, in order.
    requests: Arc<Mutex<Vec<String>>>,
    /// Kept so the server outlives the test talking to it.
    _server: tokio::task::JoinHandle<()>,
}

impl Endpoint {
    /// How many connections have been answered.
    fn served(&self) -> usize {
        self.served.load(Ordering::SeqCst)
    }

    /// The request head of the `index`-th connection.
    fn request(&self, index: usize) -> String {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(index)
            .cloned()
            .unwrap_or_else(|| panic!("no request {index} was made"))
    }
}

/// Serves `body` to every connection, recording what was asked for.
async fn serve(body: &'static str) -> Endpoint {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback is bindable");
    let url = format!("http://{}", listener.local_addr().expect("a bound socket has an address"));
    let served = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));

    let server = tokio::spawn({
        let served = Arc::clone(&served);
        let requests = Arc::clone(&requests);
        async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };

                let head = head(&mut socket).await;
                requests.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(head);

                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: \
                     {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
                served.fetch_add(1, Ordering::SeqCst);
            }
        }
    });

    Endpoint { url, served, requests, _server: server }
}

/// Reads a request up to the blank line that ends its head.
async fn head(socket: &mut TcpStream) -> String {
    let mut read = Vec::new();
    let mut buffer = [0_u8; 512];

    while !read.windows(4).any(|window| window == b"\r\n\r\n") {
        match socket.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(count) => read.extend_from_slice(&buffer[..count]),
        }
    }

    String::from_utf8_lossy(&read).into_owned()
}

/// Polls `probe` until it answers, so nothing here sleeps for a fixed guess.
async fn eventually<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    for _ in 0..200 {
        if let Some(value) = probe() {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    panic!("{what} never happened");
}

#[tokio::test]
async fn a_fetched_catalog_is_cached_verbatim_and_replaces_the_table() {
    let cache_home = tempfile::tempdir().expect("a temporary directory");
    let endpoint = serve(PAYLOAD).await;

    // SAFETY: this binary holds one test, so nothing else in the process is
    // reading the environment while it is being written.
    unsafe {
        std::env::set_var("XDG_CACHE_HOME", cache_home.path());
        std::env::set_var(catalog::MODELS_URL_ENV, &endpoint.url);
        std::env::remove_var(catalog::MODELS_PATH_ENV);
        std::env::remove_var(catalog::DISABLE_FETCH_ENV);
    }

    assert!(
        catalog::model("fixture-large").is_none(),
        "the compiled-in snapshot has never heard of the fixture"
    );

    // The startup call a frontend makes: adopt whatever is cached, then keep
    // it current. There is no cache yet, so the loop's first round fetches.
    let cancel = CancellationToken::new();
    catalog::spawn_refresh_loop(cancel.clone());

    let large =
        eventually("the fetched catalog to reach the table", || catalog::model("fixture-large"))
            .await;
    assert_eq!(endpoint.served(), 1, "the loop fetched exactly once");

    // What the endpoint was actually asked.
    let request = endpoint.request(0).to_ascii_lowercase();
    assert!(
        request.starts_with("get /api.json http/1.1\r\n"),
        "the catalog hangs off the source URL: {request}"
    );
    assert!(
        request.contains(&format!("user-agent: ganja-code/{}\r\n", env!("CARGO_PKG_VERSION"))),
        "the request names this build, by the project's name rather than the \
         binary's — one product name across every wire ganja speaks in its \
         own voice: {request}"
    );
    assert!(
        !request.contains("authorization:") && !request.contains("x-api-key:"),
        "a catalog request carries no credential: {request}"
    );

    // What the payload turned into. The fields this build reads are read; the
    // ones it has never heard of cost nothing; the ones the row left out take
    // their defaults.
    assert_eq!(large.provider_id, "fixture", "the provider is the outer key");
    assert_eq!(large.name, "Fixture Large");
    assert_eq!(large.context_window, 500_000);
    assert_eq!(large.max_output, 32_000);
    assert_eq!(large.input_limit, Some(400_000));
    assert_eq!(large.family.as_deref(), Some("fixture"));
    assert_eq!(large.release_date.as_deref(), Some("2026-02-14"));
    assert_eq!(large.status, catalog::ModelStatus::Beta);
    assert!((large.pricing.input - 4.0).abs() < f64::EPSILON);
    assert_eq!(large.pricing.cache_write, Some(5.0));

    let small = catalog::model("fixture-small").expect("a row with only limits is still a row");
    assert_eq!(small.name, "fixture-small", "an unnamed model is its id");
    assert_eq!(small.status, catalog::ModelStatus::Active);
    assert!(small.tool_call, "an absent tool_call means it takes tools");
    assert!(small.pricing.input.abs() < f64::EPSILON, "unpriced is free");

    assert!(
        catalog::model("claude-sonnet-5").is_none(),
        "a refresh replaces the table wholesale rather than merging into it"
    );

    // What landed on disk. A source other than the published one is cached
    // under a name of its own, so pointing a build at a mirror cannot have it
    // read a catalog fetched from somewhere else.
    let directory = cache_home.path().join("ganja");
    let cached: Vec<_> = fs::read_dir(&directory)
        .expect("the cache directory was created")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(cached.len(), 1, "one cache file, no leftovers: {cached:?}");
    assert!(
        cached[0].starts_with("models-") && cached[0].ends_with(".json"),
        "a custom source is cached under its own name: {cached:?}"
    );
    assert_eq!(
        fs::read_to_string(directory.join(&cached[0])).expect("the cache is readable"),
        PAYLOAD,
        "what is cached is the bytes that arrived, verbatim"
    );

    // A cache written moments ago is not fetched again, however often it is
    // asked for; forcing is what ignores that.
    assert!(
        !catalog::refresh(false).await.expect("nothing failed"),
        "a refresh inside the debounce has nothing to do"
    );
    assert_eq!(endpoint.served(), 1, "and did not touch the socket");

    assert!(
        catalog::refresh(true).await.expect("the fetch succeeds"),
        "a forced refresh ignores the debounce"
    );
    assert_eq!(endpoint.served(), 2);

    cancel.cancel();

    // And the cache alone, read back with nothing on the wire, is the same
    // catalog: this is the tier a frontend starts on the next time it runs.
    assert!(catalog::load_cached(), "the cache holds a catalog");
    assert!(catalog::model("fixture-large").is_some());
}
