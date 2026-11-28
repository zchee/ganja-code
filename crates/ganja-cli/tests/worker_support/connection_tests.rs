use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ganja_client::{Bootstrap, BootstrapAuthor, BootstrapOutcome};
use reqwest::header::{CONNECTION, HeaderValue};
use reqwest::{Method, StatusCode, Version};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};

use super::worker_support::{GitServer, WorkerProcess};

fn bootstrap(server: &GitServer) -> Bootstrap {
    Bootstrap {
        repository: server.url.clone(),
        reference: "main".to_owned(),
        branch: "workers/connection-policy".to_owned(),
        author: BootstrapAuthor {
            name: "fixture".to_owned(),
            email: "fixture@example.invalid".to_owned(),
        },
    }
}

async fn replacement_announced(worker: &WorkerProcess) {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut poll = tokio::time::interval(Duration::from_millis(10));
        loop {
            poll.tick().await;
            if worker.announced_ports().len() == 2 {
                break;
            }
        }
    })
    .await
    .expect("replacement server announces readiness");
}

#[tokio::test]
async fn every_initializer_response_tells_http_one_peers_to_close() {
    let server = GitServer::start(false).await;
    let worker = WorkerProcess::start(&server).await;
    let client = reqwest::Client::builder()
        .http1_only()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("HTTP/1 client");
    let cases = BTreeMap::from([
        ("readiness", (Method::GET, "/readyz", "", false, StatusCode::OK)),
        ("readiness with body", (Method::GET, "/readyz", "bad", false, StatusCode::BAD_REQUEST)),
        ("unauthorized", (Method::GET, "/bootstrap", "", false, StatusCode::UNAUTHORIZED)),
        ("idle status", (Method::GET, "/bootstrap", "", true, StatusCode::OK)),
        ("status with body", (Method::GET, "/bootstrap", "bad", true, StatusCode::BAD_REQUEST)),
        ("invalid body", (Method::POST, "/bootstrap", "{", true, StatusCode::BAD_REQUEST)),
        (
            "wrong directory",
            (
                Method::GET,
                "/bootstrap?directory=/missing-worker-directory",
                "",
                true,
                StatusCode::BAD_REQUEST,
            ),
        ),
        ("missing route", (Method::GET, "/missing", "", true, StatusCode::NOT_FOUND)),
        ("wrong method", (Method::DELETE, "/bootstrap", "", true, StatusCode::METHOD_NOT_ALLOWED)),
        (
            "failed checkout",
            (
                Method::POST,
                "/bootstrap",
                r#"{"repository":"file:///refused","reference":"main","branch":"work","author":{"name":"fixture","email":"fixture@example.invalid"},"deadline_secs":15}"#,
                true,
                StatusCode::CONFLICT,
            ),
        ),
    ]);
    let mut missing = Vec::new();
    for (name, (method, path, body, authorized, status)) in cases {
        for version in [Version::HTTP_10, Version::HTTP_11] {
            for connection in [None, Some("keep-alive"), Some("close")] {
                let mut request = client
                    .request(method.clone(), format!("{}{path}", worker.address))
                    .version(version)
                    .body(body);
                if authorized {
                    request = request.basic_auth("ganja", Some("synthetic-worker-password"));
                }
                if let Some(value) = connection {
                    request = request.header(CONNECTION, value);
                }
                let response = request.send().await.expect("initializer response");
                assert_eq!(response.status(), status, "{name}: {version:?} {connection:?}");
                if response.headers().get(CONNECTION) != Some(&HeaderValue::from_static("close")) {
                    missing.push(format!("{name}: {version:?} {connection:?}"));
                }
                response.bytes().await.expect("complete initializer response body");
            }
        }
    }
    drop(worker);
    server.close().await;
    assert!(missing.is_empty(), "initializer responses missing Connection: close: {missing:?}");
}

#[tokio::test]
async fn the_successful_bootstrap_response_tells_http_one_peers_to_close() {
    let server = GitServer::start(false).await;
    let worker = WorkerProcess::start(&server).await;
    let request = bootstrap(&server);
    let response = reqwest::Client::builder()
        .http1_only()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("HTTP/1 client")
        .post(format!("{}/bootstrap", worker.address))
        .basic_auth("ganja", Some("synthetic-worker-password"))
        .json(&serde_json::json!({
            "repository": request.repository,
            "reference": request.reference,
            "branch": request.branch,
            "author": {"name": request.author.name, "email": request.author.email},
            "deadline_secs": 15,
        }))
        .send()
        .await
        .expect("successful bootstrap response");
    assert_eq!(response.status(), StatusCode::OK);
    let close = response.headers().get(CONNECTION).cloned();
    let body: serde_json::Value = response.json().await.expect("complete bootstrap body");
    assert_eq!(body["status"], "cloned");
    replacement_announced(&worker).await;
    drop(worker);
    server.close().await;
    assert_eq!(close, Some(HeaderValue::from_static("close")), "bootstrap closes its connection");
}

// This transparent relay preserves HTTP bytes but withholds the upstream EOF
// until the client closes or attempts another request. A delayed TCP FIN must
// not be needed to retire an initializer connection from the client's pool.
struct ObservedConnections {
    address: String,
    accepted: Arc<Mutex<Vec<SocketAddr>>>,
    reused_after_close: Arc<AtomicUsize>,
    closed: watch::Receiver<usize>,
    task: JoinHandle<()>,
}

impl ObservedConnections {
    async fn start(worker: &WorkerProcess) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("relay listener");
        let address = format!("http://{}", listener.local_addr().expect("relay address"));
        let upstream =
            worker.address.strip_prefix("http://").expect("worker TCP address").to_owned();
        let accepted = Arc::new(Mutex::new(Vec::new()));
        let reused_after_close = Arc::new(AtomicUsize::new(0));
        let (closed_tx, closed) = watch::channel(0);
        let peers = Arc::clone(&accepted);
        let reused = Arc::clone(&reused_after_close);
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut downstream, peer) = accepted.expect("accept relay connection");
                        peers.lock().expect("peer log").push(peer);
                        let upstream = upstream.clone();
                        let reused = Arc::clone(&reused);
                        let closed = closed_tx.clone();
                        connections.spawn(async move {
                            let mut upstream = TcpStream::connect(upstream).await.expect("reach worker");
                            let (mut incoming, mut outgoing) = downstream.split();
                            let (mut replies, mut requests) = upstream.split();
                            let (eof, mut closed_upstream) = oneshot::channel();
                            let mut reported = false;
                            let send = async {
                                let mut bytes = [0; 8192];
                                loop {
                                    tokio::select! {
                                        result = incoming.read(&mut bytes) => {
                                            let size = result.expect("read client request");
                                            if size == 0 { break; }
                                            requests.write_all(&bytes[..size]).await.expect("relay request");
                                        }
                                        result = &mut closed_upstream => {
                                            result.expect("upstream EOF signal");
                                            closed.send_modify(|count| *count += 1);
                                            reported = true;
                                            if incoming.read(&mut bytes).await.expect("client retirement") != 0 {
                                                reused.fetch_add(1, Ordering::SeqCst);
                                            }
                                            break;
                                        }
                                    }
                                }
                            };
                            let receive = async {
                                tokio::io::copy(&mut replies, &mut outgoing).await.expect("relay response");
                                let _ = eof.send(());
                            };
                            tokio::join!(send, receive);
                            if !reported { closed.send_modify(|count| *count += 1); }
                        });
                    }
                    Some(result) = connections.join_next() => { result.expect("relay task"); }
                }
            }
        });
        Self { address, accepted, reused_after_close, closed, task }
    }

    fn client(&self) -> ganja_client::Client {
        ganja_client::Client::new(
            &self.address,
            Some(ganja_client::Credentials::new("ganja", "synthetic-worker-password")),
        )
        .expect("observed keep-alive client")
    }

    fn peers(&self) -> Vec<SocketAddr> {
        self.accepted.lock().expect("peer log").clone()
    }
}

impl Drop for ObservedConnections {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn consecutive_initializer_exchanges_use_different_tcp_connections() {
    let server = GitServer::start(false).await;
    let worker = WorkerProcess::start(&server).await;
    let observed = ObservedConnections::start(&worker).await;
    let client = observed.client();
    for _ in 0..2 {
        assert_eq!(
            client.bootstrap_status().await.expect("idle bootstrap status"),
            ganja_client::BootstrapStatus::Idle,
        );
    }
    let peers = observed.peers();
    drop(client);
    drop(observed);
    drop(worker);
    server.close().await;
    assert_eq!(peers.len(), 2, "two bootstrap exchanges must open two TCP connections: {peers:?}");
    assert_ne!(peers[0], peers[1], "client-side TCP addresses differ");
}

#[tokio::test]
async fn the_first_health_after_handoff_opens_a_connection_after_the_announcement() {
    let server = GitServer::start(false).await;
    let worker = WorkerProcess::start(&server).await;
    let mut observed = ObservedConnections::start(&worker).await;
    let client = observed.client();
    assert_eq!(
        client.bootstrap_status().await.expect("idle status"),
        ganja_client::BootstrapStatus::Idle
    );
    assert!(matches!(
        client.bootstrap(&bootstrap(&server), Duration::from_secs(15)).await.expect("checkout"),
        BootstrapOutcome::Cloned { .. }
    ));
    replacement_announced(&worker).await;
    let before = observed.peers();
    tokio::time::timeout(
        Duration::from_secs(5),
        observed.closed.wait_for(|count| *count >= before.len()),
    )
    .await
    .expect("initializer connections have ended upstream")
    .expect("relay observes initializer retirement");
    let health = tokio::time::timeout(Duration::from_secs(5), client.health()).await;
    let after = observed.peers();
    let stale = observed.reused_after_close.load(Ordering::SeqCst);
    drop(client);
    drop(observed);
    drop(worker);
    server.close().await;
    assert_eq!(
        stale, 0,
        "first health must not reuse an initializer connection despite a withheld FIN"
    );
    assert!(health.expect("bounded first health").expect("first health without retry").healthy);
    assert_eq!(
        after.len(),
        before.len() + 1,
        "first health opens its connection after announcement"
    );
    assert!(
        !before.contains(after.last().expect("health connection")),
        "health has a new local TCP port"
    );
}

#[tokio::test]
async fn an_http_two_prior_knowledge_client_reaches_health_after_handoff() {
    let server = GitServer::start(false).await;
    let mut worker = WorkerProcess::start(&server).await;
    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .timeout(Duration::from_secs(15))
        .build()
        .expect("HTTP/2 client");
    let request = bootstrap(&server);
    let response = client.post(format!("{}/bootstrap", worker.address))
        .basic_auth("ganja", Some("synthetic-worker-password"))
        .json(&serde_json::json!({
            "repository": request.repository, "reference": request.reference, "branch": request.branch,
            "author": {"name": request.author.name, "email": request.author.email}, "deadline_secs": 15,
        })).send().await.expect("HTTP/2 bootstrap");
    assert_eq!(response.version(), Version::HTTP_2);
    assert!(!response.headers().contains_key(CONNECTION));
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.expect("complete HTTP/2 bootstrap body");
    replacement_announced(&worker).await;
    let response = client
        .get(format!("{}/global/health", worker.address))
        .basic_auth("ganja", Some("synthetic-worker-password"))
        .send()
        .await
        .expect("first HTTP/2 health after GOAWAY");
    assert_eq!(response.version(), Version::HTTP_2);
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("complete HTTP/2 health body");
    assert_eq!(body["healthy"], true);
    drop(client);
    worker.terminate().await;
    drop(worker);
    server.close().await;
}
