//! The real git process-group executor is exercised here through its sibling
//! unit tests. This binary also owns the worker-init HTTP/exec integration:
//! git credentials, the projected CA, checkout cleanup, author identity and
//! project semantics belong to the CLI that runs git, not to the cloud adapter.

#![cfg(unix)]

#[path = "../src/worker_git.rs"]
mod worker_git;
mod worker_support;

use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use worker_git::{Checkout, GitOptions, Stage, prepare};
use worker_support::GitServer;

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
