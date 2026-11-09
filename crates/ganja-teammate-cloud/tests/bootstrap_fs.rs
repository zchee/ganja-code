//! Bootstrap protocol coverage uses the real init router in the testkit. The
//! actual git process group, TLS/CA handling, author identity and post-exec
//! project semantics are tested by ganja-cli/tests/worker_init.rs; git execution
//! is deliberately not copied into this fixture.

use std::sync::Arc;
use std::time::Duration;

use ganja_client::{
    Bootstrap, BootstrapAuthor, BootstrapOutcome, BootstrapStatus, Client, Credentials,
};
use ganja_core::permission::Permissions;
use ganja_core::provider::FakeProvider;
use ganja_core::tool::Registry;
use ganja_team::MemberName;
use ganja_teammate_cloud::provisioner::{
    Author, Bootstrap as WorkerBootstrap, Provisioner, Worker, WorkerSpec,
};
use ganja_testkit::provisioner::{FakeProvisioner, FakeServer, ServeConfig};

struct BootstrapFixture {
    _home: tempfile::TempDir,
    provisioner: FakeProvisioner,
    server: FakeServer,
    worker: Worker,
    client: Client,
}

impl BootstrapFixture {
    async fn new() -> Self {
        let home = tempfile::tempdir().expect("bootstrap root");
        let engine = Arc::new(ganja_core::Engine::new(
            Arc::new(FakeProvider::new("ok", Duration::ZERO)),
            "fake",
            Arc::new(Registry::new(Vec::new())),
            Permissions::default(),
        ));
        let (provisioner, server) = FakeProvisioner::serving(
            engine,
            ServeConfig::in_directory(home.path().to_owned()),
            "ganja",
            "test-password",
        )
        .await
        .expect("init fixture");
        let worker = provisioner
            .spawn(&WorkerSpec {
                name: MemberName::parse("bootstrap-worker").expect("name"),
                lead: MemberName::lead(),
                git_host: "example.invalid".to_owned(),
                bootstrap: WorkerBootstrap {
                    repository: "https://example.invalid/repo.git".to_owned(),
                    reference: "main".to_owned(),
                    branch: "workers/test".to_owned(),
                    author: Author {
                        name: "test".to_owned(),
                        email: "test@example.invalid".to_owned(),
                    },
                    deadline_secs: 5,
                },
            })
            .await
            .expect("suspended worker");
        provisioner.resume(&worker).await.expect("init listener ready");
        let client = Client::new(
            &worker.endpoint.base_url,
            Some(Credentials::new("ganja", "test-password")),
        )
        .expect("client")
        .with_headers(worker.endpoint.headers.clone());
        Self { _home: home, provisioner, server, worker, client }
    }

    fn request() -> Bootstrap {
        Bootstrap {
            repository: "https://example.invalid/repo.git".to_owned(),
            reference: "main".to_owned(),
            branch: "workers/test".to_owned(),
            author: BootstrapAuthor {
                name: "test".to_owned(),
                email: "test@example.invalid".to_owned(),
            },
        }
    }

    async fn close(self) {
        self.provisioner.kill(&self.worker).await.expect("delete worker");
        self.server.close().await.expect("stop fixture");
    }
}

#[tokio::test]
async fn a_successful_bootstrap_hands_off_once_to_the_real_engine() {
    let fixture = BootstrapFixture::new().await;
    let result = fixture
        .client
        .bootstrap(&BootstrapFixture::request(), Duration::from_secs(5))
        .await
        .expect("bootstrap");
    assert!(matches!(result, BootstrapOutcome::Cloned { .. }));
    tokio::time::timeout(Duration::from_secs(3), async {
        while fixture.client.health().await.is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("serve health barrier");
    assert_eq!(
        fixture.client.bootstrap_status().await.expect("post-handoff status"),
        BootstrapStatus::AlreadyServing
    );
    assert_eq!(fixture.server.bootstrap_calls(), 1);
    fixture.close().await;
}

#[tokio::test]
async fn a_lost_post_that_was_not_admitted_is_never_resent() {
    let fixture = BootstrapFixture::new().await;
    let release = fixture.server.hold_next("/bootstrap");
    let client = fixture.client.clone();
    let post = tokio::spawn(async move {
        client.bootstrap(&BootstrapFixture::request(), Duration::from_secs(5)).await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !fixture.server.requests().iter().any(|request| request.method == "POST") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("POST reaches router before admission");
    assert_eq!(
        fixture.client.bootstrap_status().await.expect("idle status"),
        BootstrapStatus::Idle
    );
    let outcome = post.await.expect("client task").expect("bounded result");
    assert_eq!(outcome, BootstrapOutcome::AmbiguousStartup);
    assert_eq!(
        fixture.server.requests().iter().filter(|request| request.method == "POST").count(),
        1
    );
    assert_eq!(fixture.server.bootstrap_calls(), 0);
    fixture.provisioner.kill(&fixture.worker).await.expect("delete before late dispatch");
    release.notify_one();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(
        fixture.server.bootstrap_calls(),
        0,
        "a late dispatch cannot enter a deleted worker"
    );
    fixture.close().await;
}

#[tokio::test]
async fn deadline_failure_keeps_cleanup_visible_and_never_hands_off_late_success() {
    let fixture = BootstrapFixture::new().await;
    let release = fixture.server.hold_bootstrap();
    let outcome = fixture
        .client
        .bootstrap(&BootstrapFixture::request(), Duration::from_secs(5))
        .await
        .expect("bounded result");
    assert!(!matches!(outcome, BootstrapOutcome::Cloned { .. } | BootstrapOutcome::AlreadyServing));
    let status = fixture.client.bootstrap_status().await.expect("cleanup status");
    assert!(
        matches!(status, BootstrapStatus::Failed { cleanup_in_progress: true, .. }),
        "{status:?}"
    );
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(
                fixture.client.bootstrap_status().await,
                Ok(BootstrapStatus::Failed { cleanup_in_progress: false, .. })
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("late result becomes terminal cleanup failure");
    assert!(fixture.client.health().await.is_err(), "late success must not exec the engine");
    assert_eq!(fixture.server.bootstrap_calls(), 1);
    fixture.close().await;
}
