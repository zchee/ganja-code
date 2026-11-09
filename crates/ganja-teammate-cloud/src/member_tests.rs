use ganja_client::EventId;
use ganja_client::sse::{Connected, Replay};
use ganja_team::{MemberName, TeamName, TeamsRoot};
use tokio::sync::Notify;

use super::*;
use crate::gate::Permit;
use crate::provisioner::{Author, Endpoint, Provisioner, WorkerSpec};
use crate::state::Deadlines;

#[path = "member_recovery_tests.rs"]
mod recovery;

// A unit-test target is a different crate instance from the library linked by
// testkit's dev-only cycle. This local double must implement this instance's trait.
#[derive(Debug)]
struct TestProvisioner {
    endpoint: Endpoint,
    state: Mutex<WorkerState>,
    suspended: std::sync::atomic::AtomicUsize,
}

impl TestProvisioner {
    fn from_endpoint(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            state: Mutex::new(WorkerState::Suspended),
            suspended: std::sync::atomic::AtomicUsize::new(0),
        }
    }
    fn current(&self, _worker: &Worker) -> WorkerState {
        self.state.lock().expect("state").clone()
    }
    fn owner_recover(&self, _worker: &Worker) {
        *self.state.lock().expect("state") = WorkerState::Suspended;
    }
}

#[async_trait]
impl Provisioner for TestProvisioner {
    async fn spawn(&self, spec: &WorkerSpec) -> Result<Worker, crate::provisioner::ProvisionError> {
        Ok(Worker {
            handle: crate::provisioner::WorkerHandle::new(spec.name.as_str()),
            endpoint: self.endpoint.clone(),
        })
    }
    async fn resume(
        &self,
        _worker: &Worker,
    ) -> Result<crate::provisioner::Resumed, crate::provisioner::ProvisionError> {
        let mut state = self.state.lock().expect("state");
        let was_suspended = *state == WorkerState::Suspended;
        *state = WorkerState::Running;
        Ok(crate::provisioner::Resumed { was_suspended })
    }
    async fn suspend(
        &self,
        _worker: &Worker,
    ) -> Result<crate::provisioner::SuspendOutcome, crate::provisioner::ProvisionError> {
        self.suspended.fetch_add(1, Ordering::SeqCst);
        let mut state = self.state.lock().expect("state");
        let outcome = if *state == WorkerState::Suspended {
            crate::provisioner::SuspendOutcome::AlreadySuspended
        } else {
            crate::provisioner::SuspendOutcome::Suspended
        };
        *state = WorkerState::Suspended;
        Ok(outcome)
    }
    async fn state(
        &self,
        worker: &Worker,
    ) -> Result<WorkerState, crate::provisioner::ProvisionError> {
        Ok(self.current(worker))
    }
    async fn kill(&self, _worker: &Worker) -> Result<(), crate::provisioner::ProvisionError> {
        *self.state.lock().expect("state") = WorkerState::Gone;
        Ok(())
    }
    fn describe(&self, _worker: &Worker) -> String {
        "unit worker".to_owned()
    }
}

#[derive(Debug)]
struct QuietSource {
    gate: Arc<WorkerGate>,
    permit: Mutex<Option<Permit>>,
    held: AtomicBool,
    changed: Notify,
    hold_started: Notify,
    hold_release: Mutex<Option<Arc<Notify>>>,
    invalidations: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl EventSource for QuietSource {
    async fn connect(&self) -> Result<Option<Connected>, ClientError> {
        let Some(mut permit) = self.gate.admit(Door::Stream) else { return Ok(None) };
        permit.classify(AttemptOutcome::Serve);
        *self.permit.lock().expect("stream permit") = Some(permit);
        Ok(Some(Connected { head: cursor(), replay: Replay::Fresh }))
    }

    async fn next(&self) -> Option<Result<Resumed, ClientError>> {
        let notified = self.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.held.load(Ordering::SeqCst) {
            notified.await;
        }
        None
    }

    async fn hold(&self) {
        self.held.store(true, Ordering::SeqCst);
        self.hold_started.notify_one();
        let release = self.hold_release.lock().expect("hold release").take();
        if let Some(release) = release {
            release.notified().await;
        }
        self.permit.lock().expect("stream permit").take();
        self.changed.notify_waiters();
    }

    fn release(&self) {
        self.held.store(false, Ordering::SeqCst);
    }
    async fn invalidate(&self) {
        self.invalidations.fetch_add(1, Ordering::SeqCst);
        self.hold().await;
    }
    async fn cursor(&self) -> Option<EventId> {
        Some(cursor())
    }
}

fn cursor() -> EventId {
    "01998ad0-0000-7000-8000-000000000000-0".parse().expect("cursor")
}

struct Fixture {
    _home: tempfile::TempDir,
    member: Arc<CloudMember>,
    source: Arc<QuietSource>,
    provisioner: Arc<TestProvisioner>,
    worker: Worker,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_endpoint(Endpoint {
            base_url: "http://127.0.0.1:1".to_owned(),
            headers: Default::default(),
            username: "ganja".to_owned(),
            password: secrecy::SecretString::from("test-password"),
        })
        .await
    }

    async fn with_endpoint(endpoint: Endpoint) -> Self {
        let home = tempfile::tempdir().expect("fixture home");
        let name = MemberName::parse("gate-worker").expect("name");
        let lead = MemberName::lead();
        let bootstrap = Bootstrap {
            repository: "https://example.invalid/repo.git".to_owned(),
            reference: "main".to_owned(),
            branch: "workers/gate".to_owned(),
            author: Author {
                name: "worker".to_owned(),
                email: "worker@example.invalid".to_owned(),
            },
            deadline_secs: 5,
        };
        let provisioner = Arc::new(TestProvisioner::from_endpoint(endpoint));
        let worker = provisioner
            .spawn(&WorkerSpec {
                name: name.clone(),
                lead: lead.clone(),
                bootstrap: bootstrap.clone(),
                git_host: "example.invalid".to_owned(),
            })
            .await
            .expect("spawn");
        provisioner.resume(&worker).await.expect("running worker");
        let runtime = Arc::new(Runtime {
            config: ganja_core::config::CloudConfig {
                provisioner: "fake".to_owned(),
                username: "ganja".to_owned(),
                no_reply_email: "worker@example.invalid".to_owned(),
                git_host: "example.invalid".to_owned(),
                bootstrap: ganja_core::config::CloudBootstrap {
                    repository: bootstrap.repository.clone(),
                    reference: bootstrap.reference.clone(),
                    branch_prefix: "workers".to_owned(),
                },
            },
            provisioner: provisioner.clone(),
            password: None,
            deadlines: Deadlines {
                quiesce: Duration::from_millis(30),
                terminal: Duration::from_millis(30),
                ..Deadlines::default()
            },
        });
        let spec = SpawnSpec {
            name,
            team: TeamName::parse("gate-test").expect("team"),
            lead,
            root: TeamsRoot::new(home.path().join("teams")),
            backend: MemberBackend::Cloud,
            agent_type: "general".to_owned(),
            model: "fake".to_owned(),
            color: "blue".to_owned(),
            prompt: "work".to_owned(),
            cwd: home.path().to_owned(),
            plan_mode_required: false,
            parent_session_id: "lead".to_owned(),
        };
        let (exits, _) = tokio::sync::mpsc::unbounded_channel();
        let lent = Lent {
            lead_inbox: home.path().join("lead.json"),
            cancel: tokio_util::sync::CancellationToken::new(),
            dialogs: None,
            exits,
            registry: Weak::new(),
        };
        let mut member = CloudMember::new(runtime, worker.clone(), spec, lent, bootstrap);
        let source = Arc::new(QuietSource {
            gate: Arc::clone(&member.gate),
            permit: Mutex::new(None),
            held: AtomicBool::new(false),
            changed: Notify::new(),
            hold_started: Notify::new(),
            hold_release: Mutex::new(None),
            invalidations: std::sync::atomic::AtomicUsize::new(0),
        });
        Arc::get_mut(&mut member).expect("unique member").events = Some(source.clone());
        member.set_phase(Phase::Idle);
        member.gate.open_stream();
        member.gate.open_rest();
        source.connect().await.expect("connect");
        Self { _home: home, member, source, provisioner, worker }
    }

    fn suspends(&self) -> usize {
        self.provisioner.suspended.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn identical_disk_paths_do_not_alias_live_registry_controls() {
    let mut first = Fixture::new().await;
    let mut second = Fixture::new().await;
    Arc::get_mut(&mut second.member).expect("unique member").spec = first.member.spec.clone();
    let registry = || {
        Arc::new(teammate::TeammateRegistry::new(
            first.member.spec.root.clone(),
            first.member.spec.team.clone(),
            "lead",
            first._home.path(),
        ))
    };
    let first_registry = registry();
    let second_registry = registry();
    Arc::get_mut(&mut first.member).expect("unique member").lent.registry =
        Arc::downgrade(&first_registry);
    Arc::get_mut(&mut second.member).expect("unique member").lent.registry =
        Arc::downgrade(&second_registry);
    register(&first.member);
    register(&second.member);
    assert!(Arc::ptr_eq(
        &control(&first_registry, "gate-worker").expect("first control").0,
        &first.member,
    ));
    assert!(Arc::ptr_eq(
        &control(&second_registry, "gate-worker").expect("second control").0,
        &second.member,
    ));
    drop(second);
    assert!(control(&second_registry, "gate-worker").is_none());
    assert!(Arc::ptr_eq(
        &control(&first_registry, "gate-worker").expect("first remains").0,
        &first.member,
    ));
}

#[tokio::test]
async fn expired_controls_are_pruned_and_dead_registries_are_not_registered() {
    let mut owners = Vec::new();
    let mut expired = Vec::new();
    for _ in 0..32 {
        let mut fixture = Fixture::new().await;
        let registry = Arc::new(teammate::TeammateRegistry::new(
            fixture.member.spec.root.clone(),
            fixture.member.spec.team.clone(),
            "lead",
            fixture._home.path(),
        ));
        Arc::get_mut(&mut fixture.member).expect("unique member").lent.registry =
            Arc::downgrade(&registry);
        register(&fixture.member);
        expired.push(Arc::downgrade(&fixture.member));
        owners.push(registry);
    }
    let mut fixture = Fixture::new().await;
    register(&fixture.member);
    let dead_member = Arc::downgrade(&fixture.member);
    assert!(CONTROLS.lock().expect("control map").values().all(|entry| {
        !Weak::ptr_eq(entry, &dead_member) && expired.iter().all(|old| !Weak::ptr_eq(entry, old))
    }));
    drop(dead_member);
    let dead_registry = Arc::new(teammate::TeammateRegistry::new(
        fixture.member.spec.root.clone(),
        fixture.member.spec.team.clone(),
        "lead",
        fixture._home.path(),
    ));
    Arc::get_mut(&mut fixture.member).expect("unique member").lent.registry =
        Arc::downgrade(&dead_registry);
    drop(dead_registry);
    register(&fixture.member);
    let dead_member = Arc::downgrade(&fixture.member);
    assert!(
        CONTROLS
            .lock()
            .expect("control map")
            .values()
            .all(|entry| !Weak::ptr_eq(entry, &dead_member))
    );
    assert!(control(&owners[0], "gate-worker").is_none());
    let mut lookup_fixture = Fixture::new().await;
    Arc::get_mut(&mut lookup_fixture.member).expect("unique member").lent.registry =
        Arc::downgrade(&owners[0]);
    register(&lookup_fixture.member);
    let expired_lookup = Arc::downgrade(&lookup_fixture.member);
    drop(lookup_fixture);
    assert!(control(&owners[0], "invalid name").is_none());
    assert!(
        CONTROLS
            .lock()
            .expect("control map")
            .values()
            .all(|entry| !Weak::ptr_eq(entry, &expired_lookup))
    );
}

#[tokio::test]
async fn same_named_members_in_different_teams_keep_independent_controls() {
    let mut first = Fixture::new().await;
    let mut second = Fixture::new().await;
    Arc::get_mut(&mut second.member).expect("unique member").spec.team =
        TeamName::parse("other-team").expect("team");
    let registry = |fixture: &Fixture| {
        Arc::new(teammate::TeammateRegistry::new(
            fixture.member.spec.root.clone(),
            fixture.member.spec.team.clone(),
            "lead",
            fixture._home.path(),
        ))
    };
    let first_registry = registry(&first);
    let second_registry = registry(&second);
    Arc::get_mut(&mut first.member).expect("unique member").lent.registry =
        Arc::downgrade(&first_registry);
    Arc::get_mut(&mut second.member).expect("unique member").lent.registry =
        Arc::downgrade(&second_registry);
    register(&first.member);
    register(&second.member);
    let first_control = control(&first_registry, "gate-worker").expect("first control");
    assert!(Arc::ptr_eq(&first_control.0, &first.member));
    first_control.report_external_revert().await;
    assert_eq!(first.member.phase(), Phase::Suspended);
    assert_eq!(second.member.phase(), Phase::Idle);
    let second_control = control(&second_registry, "gate-worker").expect("second control");
    assert!(Arc::ptr_eq(&second_control.0, &second.member));
    second_control.retire().await.expect("second worker retired");
    assert_eq!(second.member.phase(), Phase::Gone);
    assert_ne!(first.member.phase(), Phase::Gone);
    drop(second_control);
    drop(second);
    assert!(
        control(&second_registry, "gate-worker").is_none(),
        "second registry has no live member"
    );
    assert!(Arc::ptr_eq(
        &control(&first_registry, "gate-worker").expect("first remains").0,
        &first.member
    ));
}

#[tokio::test]
async fn worker_clients_require_trusted_endpoints_and_separate_auth_headers() {
    let cases = [
        ("https://worker.example", None, true),
        ("http://127.0.0.1:12345", Some("ate-target-actor"), true),
        ("http://127.12.34.56:80", None, true),
        ("http://[::1]:12345", None, true),
        ("http://localhost:12345", None, true),
        ("http://worker.example", None, false),
        ("http://192.0.2.1", None, false),
        ("http://[::2]", None, false),
        ("https://user@worker.example", None, false),
        ("https://@worker.example", None, false),
        ("https://worker.example?route=other", None, false),
        ("https://worker.example#route", None, false),
        ("http://127.0.0.1:12345", Some("Authorization"), false),
        ("http://127.0.0.1:12345", Some("aUtHoRiZaTiOn"), false),
        ("http://127.0.0.1:12345", Some("Proxy-Authorization"), false),
    ];
    for (base_url, header, accepted) in cases {
        let mut headers = http::HeaderMap::new();
        if let Some(header) = header {
            headers.insert(
                http::HeaderName::from_bytes(header.as_bytes()).expect("header name"),
                http::HeaderValue::from_static("space/actor"),
            );
        }
        let fixture = Fixture::with_endpoint(Endpoint {
            base_url: base_url.to_owned(),
            headers,
            username: "ganja".to_owned(),
            password: secrecy::SecretString::from("test-password"),
        })
        .await;
        assert_eq!(fixture.member.client.is_some(), accepted, "{base_url}, {header:?}");
    }
}

#[tokio::test]
async fn an_already_suspended_result_invalidates_history_without_reconnecting() {
    let fixture = Fixture::new().await;
    fixture.provisioner.owner_recover(&fixture.worker);
    fixture.member.quiesce().await;
    assert_eq!(fixture.member.phase(), Phase::Suspended);
    assert_eq!(fixture.source.invalidations.load(Ordering::SeqCst), 1);
    assert!(fixture.member.needs_resync.load(Ordering::SeqCst));
    assert!(!fixture.member.gate.state().stream_open);
    assert!(!fixture.member.gate.state().rest_open);
    assert_eq!(fixture.provisioner.current(&fixture.worker), WorkerState::Suspended);
}

#[tokio::test]
async fn inspection_of_an_external_suspend_invalidates_without_waking_the_worker() {
    let fixture = Fixture::new().await;
    fixture.provisioner.owner_recover(&fixture.worker);
    let control = CloudControl(fixture.member.clone());
    assert!(control.resume_for_inspection().await.is_err());
    assert_eq!(fixture.provisioner.current(&fixture.worker), WorkerState::Suspended);
    assert_eq!(fixture.source.invalidations.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.member.phase(), Phase::Suspended);
    assert!(fixture.member.needs_resync.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_crashed_state_followed_by_external_recovery_invalidates_before_reconnect() {
    let fixture = Fixture::new().await;
    let control = CloudControl(fixture.member.clone());
    *fixture.provisioner.state.lock().expect("state") = WorkerState::Crashed;
    assert!(control.resume_for_inspection().await.is_err());
    assert!(fixture.member.observed_crash.load(Ordering::SeqCst));
    assert!(!fixture.member.gate.state().stream_open);
    *fixture.provisioner.state.lock().expect("state") = WorkerState::Running;
    assert!(control.resume_for_inspection().await.is_err());
    assert_eq!(fixture.source.invalidations.load(Ordering::SeqCst), 1);
    assert!(fixture.member.needs_resync.load(Ordering::SeqCst));
    assert!(!fixture.member.observed_crash.load(Ordering::SeqCst));
    assert_eq!(fixture.member.phase(), Phase::RecoveryRequired);
}

#[tokio::test]
async fn an_unexpected_resumed_transition_keeps_uncertainty_and_requires_new_inspection() {
    let fixture = Fixture::new().await;
    fixture.member.gate.admit(Door::Rest).expect("open").finish(AttemptOutcome::Uncertain);
    fixture.member.set_phase(Phase::RecoveryRequired);
    fixture.provisioner.owner_recover(&fixture.worker);
    assert!(fixture.member.resume().await.is_err());
    assert!(fixture.member.needs_resync.load(Ordering::SeqCst));
    assert!(fixture.member.gate.state().uncertain);
    assert!(!fixture.member.gate.state().stream_open);
    assert_eq!(fixture.member.phase(), Phase::RecoveryRequired);
    assert_eq!(
        fixture
            .member
            .recent
            .lock()
            .expect("recent")
            .iter()
            .filter(|row| row.contains("unexpected suspended"))
            .count(),
        1
    );
}

#[tokio::test]
async fn an_absent_initial_envelope_does_not_wait_forever_or_claim_idle() {
    let fixture = Fixture::new().await;
    *fixture.member.session.lock().expect("session") = Some(SessionId::ascending());
    let result =
        timeout(Duration::from_secs(1), fixture.member.wait_for_terminal(None, None, false))
            .await
            .expect("terminal reconciliation is bounded");
    assert_eq!(result, Err("terminal state unavailable"));
    assert!(fixture.member.terminal.lock().expect("terminal decisions").is_empty());
    assert!(!fixture.member.lent.lead_inbox.exists(), "no idle notification was written");
}

#[tokio::test]
async fn a_healthy_stream_is_actively_ended_before_suspend() {
    let fixture = Fixture::new().await;
    let source = fixture.source.clone();
    let reader = tokio::spawn(async move { source.next().await });
    fixture.member.quiesce().await;
    assert!(reader.await.expect("reader joined").is_none());
    assert_eq!(fixture.member.gate.state().streams, 0);
    assert_eq!(fixture.provisioner.current(&fixture.worker), WorkerState::Suspended);
    assert_eq!(fixture.suspends(), 1);
}

#[tokio::test]
async fn suspend_waits_for_stream_hold_acknowledgement_with_both_doors_closed() {
    let fixture = Fixture::new().await;
    let release = Arc::new(Notify::new());
    *fixture.source.hold_release.lock().expect("hold release") = Some(release.clone());
    let member = fixture.member.clone();
    let quiesce = tokio::spawn(async move { member.quiesce().await });
    timeout(Duration::from_secs(1), fixture.source.hold_started.notified())
        .await
        .expect("hold requested");
    assert_eq!(fixture.suspends(), 0, "suspend cannot precede hold acknowledgement");
    let state = fixture.member.gate.state();
    assert!(!state.stream_open && !state.rest_open);
    assert_eq!(state.streams, 1, "the established stream still holds its permit");
    assert!(fixture.source.connect().await.expect("closed admission").is_none());
    release.notify_one();
    quiesce.await.expect("quiesce joined");
    assert_eq!(fixture.member.gate.state().streams, 0);
    assert_eq!(fixture.suspends(), 1);
}

#[tokio::test]
async fn an_inflight_rest_request_completes_before_suspend() {
    let fixture = Fixture::new().await;
    let permit = fixture.member.gate.admit(Door::Rest).expect("REST open");
    let member = fixture.member.clone();
    let quiesce = tokio::spawn(async move { member.quiesce().await });
    tokio::time::timeout(Duration::from_secs(1), async {
        while !fixture.source.held.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("hold acknowledged");
    assert_eq!(fixture.suspends(), 0);
    assert!(fixture.member.gate.admit(Door::Stream).is_none());
    assert!(fixture.member.gate.admit(Door::Rest).is_none());
    permit.finish(AttemptOutcome::Serve);
    quiesce.await.expect("quiesce joined");
    assert_eq!(fixture.suspends(), 1);
}

#[tokio::test]
async fn a_rest_drain_deadline_defers_without_latching_uncertainty() {
    let fixture = Fixture::new().await;
    let permit = fixture.member.gate.admit(Door::Rest).expect("REST open");
    fixture.member.quiesce().await;
    assert_eq!(fixture.suspends(), 0);
    assert_eq!(fixture.member.phase(), Phase::Idle);
    assert!(!fixture.member.gate.state().uncertain);
    assert!(fixture.member.gate.state().rest_open);
    permit.finish(AttemptOutcome::Serve);
    fixture.member.quiesce().await;
    assert_eq!(fixture.suspends(), 1);
}

#[tokio::test]
async fn an_unproven_rest_attempt_stays_uncertain_after_owner_recovery() {
    let fixture = Fixture::new().await;
    fixture.member.gate.admit(Door::Rest).expect("REST open").finish(AttemptOutcome::Uncertain);
    fixture.member.quiesce().await;
    fixture.provisioner.owner_recover(&fixture.worker);
    fixture.provisioner.resume(&fixture.worker).await.expect("owner resumes");
    fixture.member.quiesce().await;
    assert_eq!(fixture.suspends(), 0);
    assert!(fixture.member.gate.state().uncertain);
    let mail = mailbox::read(&fixture.member.lent.lead_inbox).expect("owner mail");
    assert_eq!(
        mail.valid.iter().filter(|message| message.text.contains("earlier request")).count(),
        1
    );
}
