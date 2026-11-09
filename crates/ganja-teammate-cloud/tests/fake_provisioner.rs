//! The control-plane contract is independent of the worker's HTTP listener.

use std::sync::Arc;

use ganja_team::MemberName;
use ganja_teammate_cloud::provisioner::{
    Author, Bootstrap, Endpoint, Provisioner, WorkerSpec, WorkerState,
};
use ganja_testkit::provisioner::FakeProvisioner;
use http::HeaderMap;
use secrecy::SecretString;
use tokio::sync::Notify;

fn fixture(name: &str) -> (FakeProvisioner, WorkerSpec) {
    let endpoint = Endpoint {
        base_url: "http://127.0.0.1:1".to_owned(),
        headers: HeaderMap::new(),
        username: "ganja".to_owned(),
        password: SecretString::from("synthetic-password"),
    };
    let spec = WorkerSpec {
        name: MemberName::parse(name).expect("member name"),
        lead: MemberName::parse("team-lead").expect("lead name"),
        bootstrap: Bootstrap {
            repository: "https://example.invalid/repo.git".to_owned(),
            reference: "main".to_owned(),
            branch: format!("workers/{name}"),
            author: Author { name: name.to_owned(), email: "worker@example.invalid".to_owned() },
            deadline_secs: 180,
        },
        git_host: "example.invalid".to_owned(),
    };
    (FakeProvisioner::from_endpoint(endpoint), spec)
}

#[tokio::test]
async fn creation_returns_suspended_and_delete_accepts_running_workers() {
    let (provisioner, spec) = fixture("worker");
    let worker = provisioner.spawn(&spec).await.expect("spawn");
    assert_eq!(provisioner.current(&worker), WorkerState::Suspended);
    assert!(provisioner.resume(&worker).await.expect("resume").was_suspended);
    assert!(!provisioner.resume(&worker).await.expect("resume again").was_suspended);
    provisioner.kill(&worker).await.expect("delete running");
    assert_eq!(provisioner.state(&worker).await.expect("state"), WorkerState::Gone);
    assert!(provisioner.spawn(&spec).await.is_err(), "an old identity must not be reused");
}

#[tokio::test]
async fn owner_recovery_does_not_fence_an_independent_router_wake() {
    let (provisioner, spec) = fixture("worker");
    let worker = provisioner.spawn(&spec).await.expect("spawn");
    provisioner.resume(&worker).await.expect("resume");
    let release = Arc::new(Notify::new());
    let wake = provisioner.delayed_wake(&worker, Arc::clone(&release));
    provisioner.owner_recover(&worker);
    assert_eq!(provisioner.current(&worker), WorkerState::Suspended);
    release.notify_one();
    wake.await.expect("delayed router task");
    assert_eq!(provisioner.current(&worker), WorkerState::Running);
}

#[tokio::test]
async fn a_late_dispatch_cannot_recreate_a_retired_identity_or_wake_its_replacement() {
    let (provisioner, spec) = fixture("worker");
    let worker = provisioner.spawn(&spec).await.expect("spawn");
    let release = Arc::new(Notify::new());
    let wake = provisioner.delayed_wake(&worker, Arc::clone(&release));
    provisioner.kill(&worker).await.expect("delete");
    let mut replacement = spec;
    replacement.name = MemberName::parse("replacement").expect("new member name");
    let replacement = provisioner.spawn(&replacement).await.expect("replacement");
    release.notify_one();
    wake.await.expect("delayed router task");
    assert_eq!(provisioner.current(&worker), WorkerState::Gone);
    assert_eq!(provisioner.current(&replacement), WorkerState::Suspended);
}
