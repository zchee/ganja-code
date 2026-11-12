//! Init-phase admission, strict bodies, reconciliation and drained handoff over TCP.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::future::BoxFuture;
use ganja_serve::bootstrap::{Bootstrap, Cloned, Executor, Failure, Service, Stage};
use support::{DEADLINE, basic, credentials};
use tokio::sync::Semaphore;

struct Controlled {
    calls: Arc<AtomicUsize>,
    release: Arc<Semaphore>,
    fail: bool,
}

impl Executor for Controlled {
    fn execute(
        &self,
        request: Bootstrap,
        deadline: tokio::time::Instant,
    ) -> BoxFuture<'static, Result<Cloned, Failure>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let release = self.release.clone();
        let fail = self.fail;
        Box::pin(async move {
            tokio::time::timeout_at(deadline, release.acquire())
                .await
                .map_err(|_| Failure { stage: Stage::Deadline, detail: "budget expired".into() })?
                .expect("release stays open")
                .forget();
            if fail {
                return Err(Failure { stage: Stage::Clone, detail: "clone failed".into() });
            }
            Ok(Cloned { head: "abc123".into(), branch: request.branch })
        })
    }
}

struct Cleaning {
    entered: Arc<AtomicUsize>,
    release: Arc<Semaphore>,
    success: bool,
}

impl Executor for Cleaning {
    fn execute(
        &self,
        _: Bootstrap,
        _: tokio::time::Instant,
    ) -> BoxFuture<'static, Result<Cloned, Failure>> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let release = self.release.clone();
        let success = self.success;
        Box::pin(async move {
            release.acquire().await.expect("cleanup release").forget();
            if success {
                return Ok(Cloned { head: "abc123".into(), branch: "work".into() });
            }
            Err(Failure { stage: Stage::Deadline, detail: "cleanup finished".into() })
        })
    }
}

#[tokio::test]
async fn a_server_deadline_returns_without_cancelling_cleanup_or_admitting_another_clone() {
    let entered = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Semaphore::new(0));
    let service = Service::new(
        credentials(),
        std::env::current_dir().expect("cwd"),
        Arc::new(Cleaning { entered: entered.clone(), release: release.clone(), success: false }),
    );
    let handle = service
        .serve(tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind"))
        .expect("serve");
    let base = format!("http://{}", handle.address());
    let client = reqwest::Client::new();
    let response = tokio::time::timeout(
        DEADLINE,
        client
            .post(format!("{base}/bootstrap"))
            .header("authorization", basic())
            .json(&body())
            .send(),
    )
    .await
    .expect("bounded HTTP response")
    .expect("POST");
    assert_eq!(response.status(), 409);
    let failure: serde_json::Value = response.json().await.expect("JSON");
    assert_eq!(failure["stage"], "deadline");
    assert_eq!(failure["cleanup_in_progress"], true);
    assert_eq!(status(&client, &base).await["stage"], "deadline");
    let duplicate = client
        .post(format!("{base}/bootstrap"))
        .header("authorization", basic())
        .json(&body())
        .send()
        .await
        .expect("duplicate");
    assert_eq!(duplicate.status(), 409);
    assert_eq!(entered.load(Ordering::SeqCst), 1, "cleanup retains admission");
    release.add_permits(1);
    tokio::time::timeout(DEADLINE, async {
        while status(&client, &base).await["cleanup_in_progress"] != false {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cleanup future remained live");
    assert_eq!(status(&client, &base).await["cleanup_in_progress"], false);
    handle.shutdown().await.expect("stop");
}

#[tokio::test]
async fn a_stalled_executor_stays_closed_and_late_success_is_terminal() {
    for late_success in [false, true] {
        let entered = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Semaphore::new(0));
        let service = Service::new(
            credentials(),
            std::env::current_dir().expect("cwd"),
            Arc::new(Cleaning {
                entered: entered.clone(),
                release: release.clone(),
                success: late_success,
            }),
        );
        let handle = service
            .serve(tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind"))
            .expect("serve");
        let base = format!("http://{}", handle.address());
        let client = reqwest::Client::new();
        let response = tokio::time::timeout(
            DEADLINE,
            client
                .post(format!("{base}/bootstrap"))
                .header("authorization", basic())
                .json(&body())
                .send(),
        )
        .await
        .expect("deadline result")
        .expect("POST");
        let failure: serde_json::Value = response.json().await.expect("JSON");
        assert_eq!(failure["stage"], "deadline");
        assert_eq!(failure["cleanup_in_progress"], true);
        if late_success {
            release.add_permits(1);
            tokio::time::timeout(DEADLINE, async {
                while status(&client, &base).await["stage"] != "cleanup" {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("late success rejected");
            assert_eq!(status(&client, &base).await["cleanup_in_progress"], false);
        }
        let duplicate = client
            .post(format!("{base}/bootstrap"))
            .header("authorization", basic())
            .json(&body())
            .send()
            .await
            .expect("duplicate");
        assert_eq!(duplicate.status(), 409);
        assert_eq!(entered.load(Ordering::SeqCst), 1);
        handle.shutdown().await.expect("stop");
    }
}

fn body() -> serde_json::Value {
    serde_json::json!({"repository":"https://example.test/repo", "reference":"main", "branch":"work",
        "author":{"name":"worker", "email":"worker@example.test"}, "deadline_secs":5})
}

async fn fixture(
    fail: bool,
) -> (ganja_serve::bootstrap::Handle, String, Arc<AtomicUsize>, Arc<Semaphore>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Semaphore::new(0));
    let service = Service::new(
        credentials(),
        std::env::current_dir().expect("cwd"),
        Arc::new(Controlled { calls: calls.clone(), release: release.clone(), fail }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let handle = service.serve(listener).expect("serve init");
    let base = format!("http://{}", handle.address());
    (handle, base, calls, release)
}

async fn status(client: &reqwest::Client, base: &str) -> serde_json::Value {
    client
        .get(format!("{base}/bootstrap"))
        .header("authorization", basic())
        .send()
        .await
        .expect("status")
        .json()
        .await
        .expect("JSON")
}

#[tokio::test]
async fn strict_admission_refuses_unknown_fields_and_out_of_range_budgets() {
    let (handle, base, calls, _) = fixture(false).await;
    let client = reqwest::Client::new();
    assert_eq!(status(&client, &base).await["status"], "idle");
    let ready = client.get(format!("{base}/readyz")).send().await.expect("ready");
    assert_eq!(ready.status(), 200);
    let epoch = ready.headers()[ganja_serve::EPOCH_HEADER].to_str().expect("epoch").to_owned();
    assert!(ganja_protocol::is_uuidv7(&epoch));
    let refused = client.get(format!("{base}/bootstrap")).send().await.expect("auth");
    assert_eq!(refused.status(), 401);
    assert_eq!(refused.headers()[ganja_serve::EPOCH_HEADER], epoch);
    for invalid in [
        serde_json::json!({"unexpected":"canary"}),
        {
            let mut value = body();
            value["deadline_secs"] = 4.into();
            value
        },
        {
            let mut value = body();
            value["deadline_secs"] = 601.into();
            value
        },
        {
            let mut value = body();
            value["author"]["unexpected"] = true.into();
            value
        },
    ] {
        let response = client
            .post(format!("{base}/bootstrap"))
            .header("authorization", basic())
            .json(&invalid)
            .send()
            .await
            .expect("refusal");
        assert_eq!(response.status(), 400);
        assert_eq!(response.headers()[ganja_serve::EPOCH_HEADER], epoch);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    handle.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn concurrent_posts_admit_one_clone_and_success_drains_before_handoff() {
    let (handle, base, calls, release) = fixture(false).await;
    let client = reqwest::Client::new();
    let post =
        client.post(format!("{base}/bootstrap")).header("authorization", basic()).json(&body());
    let first = tokio::spawn(async move { post.send().await.expect("bootstrap") });
    tokio::time::timeout(DEADLINE, async {
        while status(&client, &base).await["status"] != "in_progress" {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("admitted");
    let duplicate = client
        .post(format!("{base}/bootstrap"))
        .header("authorization", basic())
        .json(&body())
        .send()
        .await
        .expect("duplicate");
    assert_eq!(duplicate.status(), 409);
    assert_eq!(duplicate.json::<serde_json::Value>().await.expect("JSON")["status"], "in_progress");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let handoff = tokio::spawn(handle.handoff());
    release.add_permits(1);
    let response = first.await.expect("post joined");
    assert_eq!(response.status(), 200);
    let result: serde_json::Value = response.json().await.expect("the success body was flushed");
    assert_eq!(result, serde_json::json!({"status":"cloned","head":"abc123","branch":"work"}));
    let cloned = tokio::time::timeout(DEADLINE, handoff)
        .await
        .expect("drains")
        .expect("join")
        .expect("handoff");
    assert_eq!(cloned.branch, "work");
}

#[tokio::test]
async fn a_retained_init_listener_accepts_connections_after_handoff() {
    use std::os::fd::AsFd as _;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let deadline = tokio::time::Instant::now() + DEADLINE;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind init");
    let retained = std::net::TcpListener::from(
        listener.as_fd().try_clone_to_owned().expect("retain the same socket"),
    );
    let address = listener.local_addr().expect("bound address");
    let release = Arc::new(Semaphore::new(0));
    let init = Service::new(
        credentials(),
        std::env::current_dir().expect("cwd"),
        Arc::new(Controlled {
            calls: Arc::new(AtomicUsize::new(0)),
            release: release.clone(),
            fail: false,
        }),
    )
    .serve(listener)
    .expect("serve init");
    let base = format!("http://{address}");
    let client = reqwest::Client::new();
    let initial = client.get(format!("{base}/readyz")).send().await.expect("init readiness");
    let init_epoch = initial.headers()[ganja_serve::EPOCH_HEADER].clone();
    let post =
        client.post(format!("{base}/bootstrap")).header("authorization", basic()).json(&body());
    let posting = tokio::spawn(async move { post.send().await });
    tokio::time::timeout_at(deadline, async {
        while status(&client, &base).await["status"] != "in_progress" {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("executor is held");
    let handoff = tokio::spawn(init.handoff());
    release.add_permits(1);
    let response = tokio::time::timeout_at(deadline, posting)
        .await
        .expect("bootstrap responds")
        .expect("post joins")
        .expect("POST");
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<serde_json::Value>().await.expect("complete success body")["status"],
        "cloned"
    );
    tokio::time::timeout_at(deadline, handoff)
        .await
        .expect("handoff drains")
        .expect("handoff joins")
        .expect("checkout");

    // No engine exists yet: the retained socket alone holds this request.
    let mut stream = tokio::time::timeout_at(deadline, tokio::net::TcpStream::connect(address))
        .await
        .expect("connect deadline")
        .expect("queued connection");
    let request = format!(
        "GET /global/health HTTP/1.1\r\nHost: localhost\r\nAuthorization: {}\r\nConnection: close\r\n\r\n",
        basic()
    );
    tokio::time::timeout_at(deadline, stream.write_all(request.as_bytes()))
        .await
        .expect("write deadline")
        .expect("queue complete request");
    retained.set_nonblocking(true).expect("nonblocking retained socket");
    let listener = tokio::net::TcpListener::from_std(retained).expect("register retained socket");
    let mut config = support::loopback_config();
    config.credentials = Some(credentials());
    let epoch = config.journal.epoch().to_owned();
    let server = ganja_serve::serve_on_tcp(support::engine(), config, listener)
        .await
        .expect("serve the retained socket without rebinding");
    assert_eq!(server.address().tcp(), Some(address));
    let mut response = String::new();
    let read = tokio::time::timeout_at(deadline, stream.read_to_string(&mut response)).await;
    server.shutdown().await.expect("engine shutdown");
    read.expect("response deadline").expect("same connection answers");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains(&format!("ganja-serve-epoch: {epoch}")), "{response}");
    assert!(!response.contains(init_epoch.to_str().expect("init epoch")), "new engine epoch");
    let body = response.split_once("\r\n\r\n").expect("HTTP body").1;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body).expect("engine health")["healthy"],
        true
    );
}

#[tokio::test]
async fn a_lost_response_keeps_one_operation_and_a_cleaned_failure_allows_a_new_body() {
    let (handle, base, calls, release) = fixture(true).await;
    let client = reqwest::Client::new();
    let post =
        client.post(format!("{base}/bootstrap")).header("authorization", basic()).json(&body());
    let first = tokio::spawn(async move { post.send().await });
    tokio::time::timeout(DEADLINE, async {
        while status(&client, &base).await["status"] != "in_progress" {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("admitted");
    first.abort();
    release.add_permits(1);
    tokio::time::timeout(DEADLINE, async {
        while status(&client, &base).await["status"] != "failed" {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("operation survived disconnection");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    release.add_permits(1);
    let mut retry = body();
    retry["branch"] = "another-work".into();
    let response = client
        .post(format!("{base}/bootstrap"))
        .header("authorization", basic())
        .json(&retry)
        .send()
        .await
        .expect("retry");
    assert_eq!(response.status(), 409);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    handle.shutdown().await.expect("shutdown");
}
