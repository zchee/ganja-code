//! The real git process-group executor is exercised here through its sibling
//! unit tests. This binary also owns the worker-init HTTP/exec integration:
//! git credentials, the projected CA, checkout cleanup, author identity and
//! project semantics belong to the CLI that runs git, not to the cloud adapter.

#![cfg(unix)]

#[path = "../src/worker_git.rs"]
mod worker_git;
mod worker_support;

use std::time::Duration;

use futures::StreamExt as _;
use ganja_client::{Bootstrap, BootstrapAuthor, BootstrapOutcome, Prompt};
use ganja_protocol::{Event, FinishReason};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use worker_git::{Checkout, GitOptions, Stage, prepare};
use worker_support::GitServer;

#[tokio::test]
async fn a_flushed_bootstrap_becomes_a_real_project_with_working_undo() {
    let server = GitServer::start(false).await;
    let mut worker = worker_support::WorkerProcess::start(&server).await;
    let bootstrap = Bootstrap {
        repository: server.url.clone(),
        reference: "main".to_owned(),
        branch: "workers/http".to_owned(),
        author: BootstrapAuthor {
            name: "ganja-cloud-http".to_owned(),
            email: "http@example.invalid".to_owned(),
        },
    };
    let result = worker
        .client
        .bootstrap(&bootstrap, Duration::from_secs(15))
        .await
        .expect("bootstrap response");
    assert!(matches!(result, BootstrapOutcome::Cloned { .. }), "{result:?}");
    let health = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(health) = worker.client.health().await {
                break health;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("exec reaches authenticated health");
    assert!(worker.is_alive(), "the original worker process remains alive after exec");
    let mut events = worker.client.events().await.expect("event stream");
    worker
        .client
        .prompt(&health.session_id, &Prompt::new("inspect and update note.txt"))
        .await
        .expect("prompt accepted");
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(event) = events.next().await {
            if let Event::MessageFinished { reason, .. } = event.expect("event") {
                assert_eq!(reason, FinishReason::Completed);
                break;
            }
        }
    })
    .await
    .expect("tool turn completes");
    assert_eq!(
        std::fs::read_to_string(worker.work.join("note.txt")).expect("updated file"),
        "after\n"
    );
    let stored = worker.client.messages(&health.session_id).await.expect("stored transcript");
    let text = serde_json::to_string(&stored).expect("transcript serializes");
    assert!(
        text.contains("before"),
        "the read tool resolved its relative path inside the checkout"
    );
    let response = reqwest::Client::new()
        .post(format!("{}/session/{}/revert", worker.address, health.session_id.as_str()))
        .basic_auth("ganja", Some("synthetic-worker-password"))
        .send()
        .await
        .expect("undo request");
    assert!(response.status().is_success());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if std::fs::read_to_string(worker.work.join("note.txt"))
                .is_ok_and(|text| text == "before\n")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("undo restored the real checkout");
    let author = std::process::Command::new("git")
        .args(["config", "--local", "user.email"])
        .current_dir(&worker.work)
        .output()
        .expect("author config");
    assert_eq!(String::from_utf8(author.stdout).expect("author").trim(), "http@example.invalid");
    drop(worker);
    server.close().await;
}

#[tokio::test]
async fn the_served_worker_does_not_inherit_host_config_overrides() {
    let server = GitServer::start(false).await;
    let mut worker = worker_support::WorkerProcess::start_with_environment(
        &server,
        &[("GANJA_CONFIG", "/missing-synthetic-host-config.toml")],
    )
    .await;
    let bootstrap = Bootstrap {
        repository: server.url.clone(),
        reference: "main".to_owned(),
        branch: "workers/environment".to_owned(),
        author: BootstrapAuthor {
            name: "fixture".to_owned(),
            email: "fixture@example.invalid".to_owned(),
        },
    };
    assert!(matches!(
        worker.client.bootstrap(&bootstrap, Duration::from_secs(15)).await.expect("bootstrap"),
        BootstrapOutcome::Cloned { .. }
    ));
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if worker.client.health().await.is_ok() {
                break;
            }
            assert!(worker.is_alive(), "host config override must not reach serve");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("isolated serve starts");
    drop(worker);
    server.close().await;
}

#[tokio::test]
async fn terminating_init_reaps_a_running_clone_before_exit() {
    let server = GitServer::start(true).await;
    let mut worker = worker_support::WorkerProcess::start(&server).await;
    let client = worker.client.clone();
    let bootstrap = Bootstrap {
        repository: server.url.clone(),
        reference: "main".to_owned(),
        branch: "workers/cancel".to_owned(),
        author: BootstrapAuthor {
            name: "fixture".to_owned(),
            email: "fixture@example.invalid".to_owned(),
        },
    };
    let request =
        tokio::spawn(async move { client.bootstrap(&bootstrap, Duration::from_secs(5)).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while server.authorized.lock().expect("requests").is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("clone reached the peer");
    worker.terminate().await;
    assert_eq!(std::fs::read_dir(&worker.work).expect("work directory remains").count(), 0);
    let outcome = request.await.expect("bootstrap request task");
    assert!(
        !matches!(outcome, Ok(BootstrapOutcome::Cloned { .. } | BootstrapOutcome::AlreadyServing)),
        "a terminated initializer cannot report success: {outcome:?}"
    );
    drop(worker);
    server.close().await;
}

fn request(server: &GitServer, reference: &str) -> Checkout {
    Checkout {
        repository: server.url.clone(),
        reference: reference.to_owned(),
        branch: "workers/fixture".to_owned(),
        author_name: "ganja-cloud-fixture".to_owned(),
        author_email: "fixture@example.invalid".to_owned(),
        git: GitOptions {
            cancel: CancellationToken::new(),
            global_config: Some(server.gitconfig.clone()),
            ssl_ca: Some(server.ca.clone()),
        },
    }
}

#[tokio::test]
async fn a_real_https_checkout_sends_the_placeholder_and_writes_the_author() {
    let server = GitServer::start(false).await;
    let root = tempfile::tempdir().expect("worker root");
    let head = prepare(
        root.path().to_owned(),
        request(&server, "main"),
        Instant::now() + Duration::from_secs(15),
    )
    .await
    .expect("real HTTPS clone");
    assert_eq!(head.len(), 40);
    assert_eq!(
        std::fs::read_to_string(root.path().join("note.txt")).expect("checkout file"),
        "before\n"
    );
    assert!(root.path().join(".git").is_dir());
    let email = std::process::Command::new("git")
        .args(["config", "--local", "user.email"])
        .current_dir(root.path())
        .output()
        .expect("read author");
    assert!(email.status.success());
    assert_eq!(
        String::from_utf8(email.stdout).expect("author text").trim(),
        "fixture@example.invalid"
    );
    let requests = server.authorized.lock().expect("requests").clone();
    assert!(!requests.is_empty(), "the clone reached the actual TLS peer");
    assert!(
        requests.into_iter().all(|authorized| authorized),
        "every git request carried the placeholder"
    );
    assert!(server.root.path().exists());
    server.close().await;
}

#[tokio::test]
async fn a_failed_switch_removes_the_partial_checkout() {
    let server = GitServer::start(false).await;
    let root = tempfile::tempdir().expect("worker root");
    let result = prepare(
        root.path().to_owned(),
        request(&server, "missing-reference"),
        Instant::now() + Duration::from_secs(15),
    )
    .await;
    assert_eq!(result.expect_err("missing ref").stage, Stage::Switch);
    assert_eq!(std::fs::read_dir(root.path()).expect("root survives").count(), 0);
    prepare(
        root.path().to_owned(),
        request(&server, "main"),
        Instant::now() + Duration::from_secs(15),
    )
    .await
    .expect("a later different checkout starts cleanly");
    server.close().await;
}

#[tokio::test]
async fn a_silent_git_peer_is_killed_and_its_checkout_is_removed() {
    let server = GitServer::start(true).await;
    let root = tempfile::tempdir().expect("worker root");
    let result = prepare(
        root.path().to_owned(),
        request(&server, "main"),
        Instant::now() + Duration::from_secs(2),
    )
    .await;
    assert_eq!(result.expect_err("silent peer").stage, Stage::Deadline);
    assert!(
        !server.authorized.lock().expect("requests").is_empty(),
        "git reached the peer before the deadline"
    );
    assert_eq!(std::fs::read_dir(root.path()).expect("root survives").count(), 0);
    server.close().await;
}
