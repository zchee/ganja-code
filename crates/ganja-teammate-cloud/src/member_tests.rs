use ganja_client::EventId;
use ganja_client::sse::{Connected, Replay};
use ganja_team::{MemberName, TeamName, TeamsRoot};
use tokio::sync::Notify;

use super::*;
use crate::gate::Permit;
use crate::provisioner::{Author, Endpoint, Provisioner, WorkerSpec};
use crate::state::Deadlines;

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
    async fn suspend(&self, _worker: &Worker) -> Result<(), crate::provisioner::ProvisionError> {
        self.suspended.fetch_add(1, Ordering::SeqCst);
        *self.state.lock().expect("state") = WorkerState::Suspended;
        Ok(())
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
        let provisioner = Arc::new(TestProvisioner::from_endpoint(Endpoint {
            base_url: "http://127.0.0.1:1".to_owned(),
            headers: Default::default(),
            username: "ganja".to_owned(),
            password: secrecy::SecretString::from("test-password"),
        }));
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
