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

struct Hosted {
    home: tempfile::TempDir,
    root: ganja_team::TeamsRoot,
    team: ganja_team::TeamName,
    name: String,
    registry: Arc<ganja_core::teammate::TeammateRegistry>,
    door: ganja_core::Teammates,
    provisioner: Arc<FakeProvisioner>,
    server: ganja_testkit::provisioner::FakeServer,
}

impl Hosted {
    async fn new() -> Self {
        Self::with_deadline(std::time::Duration::from_secs(5)).await
    }

    async fn with_deadline(launch: std::time::Duration) -> Self {
        let home = tempfile::tempdir().expect("fixture home");
        let storage = ganja_core::Storage::open(home.path().join("storage"));
        let session = ganja_testkit::seed_session(&storage, 0);
        let (provider, _) = ganja_testkit::ScriptedProvider::new(vec![
            ganja_testkit::says("first answer"),
            ganja_testkit::says("second answer"),
            ganja_testkit::says("third answer"),
        ]);
        let engine = Arc::new(ganja_core::Engine::persistent(
            provider,
            "fake",
            Arc::new(ganja_core::tool::Registry::new(Vec::new())),
            ganja_core::permission::Permissions::default(),
            storage.clone(),
        ));
        engine.resume(&session).await.expect("seeded session resumes");
        let mut config =
            ganja_testkit::provisioner::ServeConfig::in_directory(home.path().to_owned());
        config.storage = Some(storage);
        let (provisioner, server) =
            FakeProvisioner::serving(engine, config, "ganja", "test-password")
                .await
                .expect("two phase fake");
        let provisioner = Arc::new(provisioner);
        let root = ganja_team::TeamsRoot::new(home.path().join("teams"));
        let team = ganja_team::TeamName::parse("cloud-test").expect("team");
        let name = format!("worker-{}", ganja_protocol::SessionId::ascending().as_str());
        let registry = Arc::new(ganja_core::teammate::TeammateRegistry::new(
            root.clone(),
            team.clone(),
            "lead-session",
            home.path(),
        ));
        let cloud = ganja_teammate_cloud::CloudBackend::configured(
            ganja_core::config::CloudConfig {
                provisioner: "fake".to_owned(),
                username: "ganja".to_owned(),
                no_reply_email: "test@example.invalid".to_owned(),
                git_host: "example.invalid".to_owned(),
                bootstrap: ganja_core::config::CloudBootstrap {
                    repository: "https://example.invalid/repo.git".to_owned(),
                    reference: "main".to_owned(),
                    branch_prefix: "workers".to_owned(),
                },
            },
            provisioner.clone(),
            Some(SecretString::from("test-password")),
            ganja_teammate_cloud::state::Deadlines {
                launch,
                bootstrap: std::time::Duration::from_secs(5),
                health: std::time::Duration::from_secs(3),
                quiesce: std::time::Duration::from_millis(200),
                cleanup: std::time::Duration::from_secs(1),
                terminal: std::time::Duration::from_secs(2),
            },
        );
        let door = ganja_core::Teammates::new(
            Arc::clone(&registry),
            ganja_core::Backends::new().with(Arc::new(cloud)),
        );
        Self { home, root, team, name, registry, door, provisioner, server }
    }

    async fn start(&self) {
        self.door
            .start(
                ganja_testkit::spawn_with_prompt(&self.name, Some("cloud"), "first request"),
                &ganja_testkit::caller(self.home.path()),
                &ganja_testkit::AllowSpawn,
            )
            .await
            .expect("cloud spawn");
    }

    fn idle_count(&self) -> usize {
        ganja_team::mailbox::read(&self.registry.lead_inbox())
            .expect("lead inbox")
            .valid
            .iter()
            .filter(|message| {
                matches!(
                    serde_json::from_str::<ganja_protocol::team::Frame>(&message.text),
                    Ok(ganja_protocol::team::Frame::IdleNotification(_))
                )
            })
            .count()
    }

    async fn wait_suspended(&self, idle: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(worker) = self.provisioner.worker(&self.name)
                    && self.provisioner.current(&worker) == WorkerState::Suspended
                    && self.idle_count() == idle
                {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("member reaches suspended boundary");
    }

    fn send(&self, text: &str) {
        let name = MemberName::parse(&self.name).expect("member");
        ganja_team::mailbox::write(
            &self.root.inbox_path(&self.team, &name),
            ganja_team::MailboxMessage::new("team-lead", text, ganja_team::record::now_iso8601()),
        )
        .expect("inbox delivery");
    }

    async fn wait_idle(&self, idle: usize) {
        let control = ganja_teammate_cloud::control(&self.name).expect("cloud control");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while self.idle_count() != idle
                || control.phase() != ganja_teammate_cloud::state::Phase::Idle
            {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("member reaches idle boundary");
    }

    fn suspends(&self) -> usize {
        self.provisioner
            .calls()
            .iter()
            .filter(|(operation, _)| {
                *operation == ganja_testkit::provisioner::ProvisionOperation::Suspend
            })
            .count()
    }

    async fn close(self) {
        self.registry.shutdown().await;
        self.server.close().await.expect("all fake servers stop");
    }
}

#[tokio::test]
async fn a_crashed_or_gone_worker_stops_once_without_an_idle_claim() {
    use ganja_teammate_cloud::provisioner::ProvisionErrorKind;
    use ganja_teammate_cloud::state::Phase;
    for (kind, phase, sentence) in [
        (ProvisionErrorKind::Crashed, Phase::Crashed, "worker crashed"),
        (ProvisionErrorKind::Gone, Phase::Gone, "worker is gone"),
    ] {
        let hosted = Hosted::new().await;
        hosted.start().await;
        hosted.wait_suspended(1).await;
        hosted.provisioner.fail_next(ganja_testkit::provisioner::ProvisionOperation::Resume, kind);
        let control = ganja_teammate_cloud::control(&hosted.name).expect("control");
        hosted.send("next request");
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while control.phase() != phase {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("terminal lifecycle state");
        assert_eq!(hosted.idle_count(), 1, "an interrupted resume is not another idle turn");
        let mail = ganja_team::mailbox::read(&hosted.registry.lead_inbox()).expect("inbox");
        assert_eq!(mail.valid.iter().filter(|message| message.text.contains(sentence)).count(), 1);
        hosted.close().await;
    }
}

#[tokio::test]
async fn a_shutdown_request_is_acknowledged_without_resuming_the_worker() {
    let hosted = Hosted::new().await;
    hosted.start().await;
    hosted.wait_suspended(1).await;
    let frame =
        ganja_protocol::team::Frame::ShutdownRequest(ganja_protocol::team::ShutdownRequest {
            request_id: "shutdown-test".to_owned(),
            from: "team-lead".to_owned(),
            reason: None,
            timestamp: ganja_team::record::now_iso8601(),
        });
    hosted.send(&serde_json::to_string(&frame).expect("shutdown frame"));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let mail = ganja_team::mailbox::read(&hosted.registry.lead_inbox()).expect("inbox");
            if mail.valid.iter().any(|message| matches!(serde_json::from_str::<ganja_protocol::team::Frame>(&message.text), Ok(ganja_protocol::team::Frame::ShutdownApproved(ref approved)) if approved.request_id == "shutdown-test")) { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("shutdown acknowledgement");
    assert_eq!(
        hosted
            .provisioner
            .calls()
            .iter()
            .filter(|(operation, _)| *operation
                == ganja_testkit::provisioner::ProvisionOperation::Resume)
            .count(),
        1
    );
    assert!(hosted.provisioner.worker(&hosted.name).is_none());
    hosted.close().await;
}

#[tokio::test]
async fn a_stalled_resume_releases_the_registry_name_and_record() {
    let hosted = Hosted::with_deadline(std::time::Duration::from_millis(150)).await;
    let _release =
        hosted.provisioner.hold_next(ganja_testkit::provisioner::ProvisionOperation::Resume);
    let error = hosted
        .door
        .start(
            ganja_testkit::spawn_with_prompt(&hosted.name, Some("cloud"), "first request"),
            &ganja_testkit::caller(hosted.home.path()),
            &ganja_testkit::AllowSpawn,
        )
        .await
        .expect_err("resume times out");
    assert!(error.reason.contains("resuming"), "{error:?}");
    assert!(ganja_testkit::teammates_recorded(&hosted.root, &hosted.team).is_empty());
    assert!(hosted.provisioner.worker(&hosted.name).is_none());
    assert_eq!(
        hosted
            .provisioner
            .calls()
            .iter()
            .filter(
                |(operation, _)| *operation == ganja_testkit::provisioner::ProvisionOperation::Kill
            )
            .count(),
        1
    );
    hosted.close().await;
}

#[tokio::test]
async fn a_missing_first_connected_ack_times_out_and_unwinds() {
    let hosted = Hosted::with_deadline(std::time::Duration::from_secs(2)).await;
    let release = hosted.server.hold_next("/event");
    let error = hosted
        .door
        .start(
            ganja_testkit::spawn_with_prompt(&hosted.name, Some("cloud"), "first request"),
            &ganja_testkit::caller(hosted.home.path()),
            &ganja_testkit::AllowSpawn,
        )
        .await
        .expect_err("connect times out");
    assert!(error.reason.contains("event stream"), "{error:?}");
    assert!(ganja_testkit::teammates_recorded(&hosted.root, &hosted.team).is_empty());
    assert!(hosted.provisioner.worker(&hosted.name).is_none());
    release.notify_one();
    hosted.close().await;
}

#[tokio::test]
async fn a_stalled_delete_is_unconfirmed_and_mails_the_owner_exactly_once() {
    let hosted = Hosted::with_deadline(std::time::Duration::from_secs(2)).await;
    hosted.server.fail_bootstrap(ganja_testkit::provisioner::BootstrapStage::Dispatch);
    let _release =
        hosted.provisioner.hold_next(ganja_testkit::provisioner::ProvisionOperation::Kill);
    let _error = hosted
        .door
        .start(
            ganja_testkit::spawn_with_prompt(&hosted.name, Some("cloud"), "first request"),
            &ganja_testkit::caller(hosted.home.path()),
            &ganja_testkit::AllowSpawn,
        )
        .await
        .expect_err("bootstrap fails");
    assert!(ganja_testkit::teammates_recorded(&hosted.root, &hosted.team).is_empty());
    let mail = ganja_team::mailbox::read(&hosted.registry.lead_inbox()).expect("owner inbox");
    assert_eq!(
        mail.valid.iter().filter(|message| message.text.contains("owner action needed")).count(),
        1
    );
    let worker = hosted.provisioner.worker(&hosted.name).expect("unconfirmed worker remains");
    assert_eq!(hosted.provisioner.current(&worker), WorkerState::Running);
    hosted.provisioner.kill(&worker).await.expect("explicit cleanup");
    hosted.close().await;
}

#[tokio::test]
async fn a_failed_suspend_reopens_admission_for_the_next_turn() {
    let hosted = Hosted::new().await;
    hosted.provisioner.fail_next(
        ganja_testkit::provisioner::ProvisionOperation::Suspend,
        ganja_teammate_cloud::provisioner::ProvisionErrorKind::Transient,
    );
    hosted.start().await;
    hosted.wait_idle(1).await;
    hosted.send("second request");
    hosted.wait_suspended(2).await;
    assert_eq!(hosted.suspends(), 2);
    hosted.close().await;
}

#[tokio::test]
async fn a_shed_attempt_and_an_owners_suspend_never_clear_the_uncertain_latch() {
    let hosted = Hosted::new().await;
    hosted.start().await;
    hosted.wait_suspended(1).await;
    let worker = hosted.provisioner.worker(&hosted.name).expect("worker");
    let control = ganja_teammate_cloud::control(&hosted.name).expect("control");
    hosted.server.reply_next("/event", 503);
    assert!(control.resume_for_inspection().await.is_err(), "shed reconnect is not success");
    let first_release = Arc::new(Notify::new());
    let second_release = Arc::new(Notify::new());
    let first_wake = hosted.provisioner.delayed_wake(&worker, Arc::clone(&first_release));
    let second_wake = hosted.provisioner.delayed_wake(&worker, Arc::clone(&second_release));
    hosted.send("second request");
    hosted.wait_idle(2).await;
    assert_eq!(hosted.suspends(), 1, "a later typed connection did not clear uncertainty");
    hosted.provisioner.owner_recover(&worker);
    first_release.notify_one();
    first_wake.await.expect("first independent wake");
    hosted.send("third request");
    hosted.wait_idle(3).await;
    assert_eq!(hosted.suspends(), 1, "owner recovery of the same identity is not a fence");
    control.retire().await.expect("retire confirms absence");
    second_release.notify_one();
    second_wake.await.expect("second independent wake");
    assert_eq!(hosted.provisioner.current(&worker), WorkerState::Gone);
    hosted.close().await;
}

#[tokio::test]
async fn a_cloud_member_suspends_and_resumes_without_worker_requests_while_asleep() {
    let hosted = Hosted::new().await;
    hosted.start().await;
    hosted.wait_suspended(1).await;
    let count = hosted.server.requests().len();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        hosted.server.requests().len(),
        count,
        "no idle reconnect or health request wakes a suspended worker"
    );
    let name = MemberName::parse(&hosted.name).expect("member");
    ganja_team::mailbox::write(
        &hosted.root.inbox_path(&hosted.team, &name),
        ganja_team::MailboxMessage::new(
            "team-lead",
            "second request",
            ganja_team::record::now_iso8601(),
        ),
    )
    .expect("second message");
    hosted.wait_suspended(2).await;
    assert_eq!(hosted.server.bootstrap_calls(), 1, "resume never sends a second bootstrap POST");
    assert!(
        hosted.server.requests().iter().all(|request| request.state == WorkerState::Running),
        "every worker request was admitted only while running"
    );
    hosted.close().await;
}
