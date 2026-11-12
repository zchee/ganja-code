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

#[cfg(unix)]
async fn blocked_runner_read(
    hosted: &Hosted,
) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let name = MemberName::parse(&hosted.name).expect("name");
    let inbox = hosted.root.inbox_path(&hosted.team, &name);
    std::fs::remove_file(&inbox).expect("remove drained inbox");
    assert!(
        std::process::Command::new("/usr/bin/mkfifo")
            .args(["-m", "600"])
            .arg(&inbox)
            .status()
            .expect("mkfifo")
            .success()
    );
    let (paired, paired_rx) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let _writer = std::fs::OpenOptions::new().write(true).open(inbox).expect("FIFO paired");
        paired.send(()).expect("paired signal");
        released.recv().expect("release writer");
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), paired_rx)
        .await
        .expect("runner opens FIFO read-only")
        .expect("paired");
    (release, writer)
}

#[cfg(unix)]
#[tokio::test]
async fn a_cloud_runner_exits_without_retaining_its_registry() {
    let hosted = Hosted::new().await;
    hosted.start().await;
    hosted.wait_finished().await;
    let weak = Arc::downgrade(&hosted.registry);
    let (release, writer) = blocked_runner_read(&hosted).await;
    let worker = hosted.provisioner.worker(&hosted.name).expect("worker before deletion");
    tokio::time::timeout(std::time::Duration::from_secs(3), hosted.registry.shutdown())
        .await
        .expect("registry shutdown while native read remains blocked");
    assert_eq!(hosted.provisioner.current(&worker), WorkerState::Gone);
    let Hosted { home, registry, door, server, .. } = hosted;
    drop((door, registry));
    assert!(weak.upgrade().is_none(), "blocked payload must not retain registry");
    release.send(()).expect("release FIFO");
    writer.join().expect("writer");
    server.close().await.expect("server close");
    drop(home);
}

#[cfg(unix)]
#[test]
fn the_process_exits_after_bounded_cloud_cleanup() {
    const CHILD: &str = "GANJA_TEST_CLOUD_EXIT_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let hosted = Hosted::new().await;
            hosted.start().await;
            hosted.wait_finished().await;
            let (release, writer) = blocked_runner_read(&hosted).await;
            std::mem::forget(release);
            drop(writer);
            let worker = hosted.provisioner.worker(&hosted.name).expect("worker before deletion");
            tokio::time::timeout(std::time::Duration::from_secs(3), hosted.registry.shutdown()).await.expect("registry and runners return");
            assert_eq!(hosted.provisioner.current(&worker), WorkerState::Gone);
            hosted.server.close().await.expect("server close");
            println!("kill attempted; Gone confirmed; registry and runner returned; FIFO delivery pending");
        });
        drop(runtime);
        println!("ordinary runtime destruction returned");
        return;
    }
    let home = tempfile::tempdir().expect("child isolation");
    let mut child = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "the_process_exits_after_bounded_cloud_cleanup", "--nocapture"])
        .env(CHILD, "1")
        .env("XDG_DATA_HOME", home.path())
        .env("GANJA_CONFIG_HOME", home.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("child");
    // The launch allowance is separate from the fixture's one-second cleanup budget.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if child.try_wait().expect("child status").is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().expect("kill timed-out child");
            let output = child.wait_with_output().expect("reap timed-out child");
            panic!(
                "normal process exit exceeded the bound: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let output = child.wait_with_output().expect("actual process wait");
    assert!(output.status.success(), "child failed: {}", String::from_utf8_lossy(&output.stderr));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ordinary runtime destruction returned")
    );
}

#[tokio::test]
async fn creation_returns_suspended_and_delete_accepts_running_workers() {
    let (provisioner, spec) = fixture("worker");
    let worker = provisioner.spawn(&spec).await.expect("spawn");
    assert_eq!(provisioner.current(&worker), WorkerState::Suspended);
    assert!(provisioner.resume(&worker).await.expect("resume").was_suspended);
    assert!(!provisioner.resume(&worker).await.expect("resume again").was_suspended);
    assert_eq!(
        provisioner.suspend(&worker).await.expect("suspend"),
        ganja_teammate_cloud::provisioner::SuspendOutcome::Suspended
    );
    assert_eq!(
        provisioner.suspend(&worker).await.expect("suspend again"),
        ganja_teammate_cloud::provisioner::SuspendOutcome::AlreadySuspended
    );
    provisioner.resume(&worker).await.expect("running before delete");
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

#[derive(Debug)]
struct InitHealthProxy {
    provisioner: Arc<FakeProvisioner>,
    address: std::net::SocketAddr,
    target: Arc<std::sync::Mutex<Option<std::net::SocketAddr>>>,
}

#[async_trait::async_trait]
impl Provisioner for InitHealthProxy {
    async fn spawn(
        &self,
        spec: &WorkerSpec,
    ) -> Result<
        ganja_teammate_cloud::provisioner::Worker,
        ganja_teammate_cloud::provisioner::ProvisionError,
    > {
        let mut worker = self.provisioner.spawn(spec).await?;
        let target = url::Url::parse(&worker.endpoint.base_url).expect("fake router URL");
        *self.target.lock().expect("proxy target") = Some(std::net::SocketAddr::new(
            target.host_str().expect("fake router host").parse().expect("loopback IP"),
            target.port().expect("fake router port"),
        ));
        worker.endpoint.base_url = format!("http://{}", self.address);
        Ok(worker)
    }

    async fn resume(
        &self,
        worker: &ganja_teammate_cloud::provisioner::Worker,
    ) -> Result<
        ganja_teammate_cloud::provisioner::Resumed,
        ganja_teammate_cloud::provisioner::ProvisionError,
    > {
        self.provisioner.resume(worker).await
    }

    async fn suspend(
        &self,
        worker: &ganja_teammate_cloud::provisioner::Worker,
    ) -> Result<
        ganja_teammate_cloud::provisioner::SuspendOutcome,
        ganja_teammate_cloud::provisioner::ProvisionError,
    > {
        self.provisioner.suspend(worker).await
    }

    async fn state(
        &self,
        worker: &ganja_teammate_cloud::provisioner::Worker,
    ) -> Result<WorkerState, ganja_teammate_cloud::provisioner::ProvisionError> {
        self.provisioner.state(worker).await
    }

    async fn kill(
        &self,
        worker: &ganja_teammate_cloud::provisioner::Worker,
    ) -> Result<(), ganja_teammate_cloud::provisioner::ProvisionError> {
        self.provisioner.kill(worker).await
    }

    fn describe(&self, worker: &ganja_teammate_cloud::provisioner::Worker) -> String {
        self.provisioner.describe(worker)
    }
}

#[derive(Clone, Copy)]
enum LaunchHealthFault {
    InitRefusal,
    ConnectionFailure,
}

struct InitHealthWire {
    cancel: tokio_util::sync::CancellationToken,
    task: tokio::task::JoinHandle<()>,
    health_requests: Arc<std::sync::atomic::AtomicUsize>,
    dropped_health_requests: Arc<std::sync::atomic::AtomicUsize>,
}

impl InitHealthWire {
    async fn new(
        provisioner: Arc<FakeProvisioner>,
        fault: LaunchHealthFault,
    ) -> (Arc<InitHealthProxy>, Self) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("proxy listener");
        let address = listener.local_addr().expect("proxy address");
        let target = Arc::new(std::sync::Mutex::new(None));
        let cancel = tokio_util::sync::CancellationToken::new();
        let health_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dropped_health_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task = tokio::spawn({
            let target = Arc::clone(&target);
            let cancel = cancel.clone();
            let health_requests = Arc::clone(&health_requests);
            let dropped_health_requests = Arc::clone(&dropped_health_requests);
            async move {
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => break,
                        Some(result) = connections.join_next() => result.expect("proxy connection"),
                        accepted = listener.accept() => {
                            let (socket, _) = accepted.expect("proxy accept");
                            let target = *target.lock().expect("proxy target");
                            let health_requests = Arc::clone(&health_requests);
                            let dropped_health_requests = Arc::clone(&dropped_health_requests);
                            connections.spawn(async move {
                                let mut reader = tokio::io::BufReader::new(socket);
                                let mut head = String::new();
                                let received = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                                    loop {
                                        let mut line = String::new();
                                        if reader.read_line(&mut line).await.expect("request head") == 0 {
                                            return false;
                                        }
                                        assert!(head.len() + line.len() <= 16_384, "bounded fixture head");
                                        if line == "\r\n" { break; }
                                        if !line.to_ascii_lowercase().starts_with("connection:") {
                                            head.push_str(&line);
                                        }
                                    }
                                    true
                                }).await.expect("proxy receives request head");
                                if !received { return; }
                                if head.starts_with("GET /global/health ") {
                                    let attempt = health_requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                    if matches!(fault, LaunchHealthFault::ConnectionFailure) && attempt == 0 {
                                        dropped_health_requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                        return;
                                    }
                                    let reply = match (fault, attempt) {
                                        (LaunchHealthFault::InitRefusal, 0) => Some("HTTP/1.1 404 Not Found\r\nganja-serve-epoch: 01900000-0000-7000-8000-000000000001\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                                        (LaunchHealthFault::InitRefusal, 1) => Some("HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                                        _ => None,
                                    };
                                    if let Some(reply) = reply {
                                        reader.get_mut().write_all(reply.as_bytes()).await.expect("health refusal");
                                        return;
                                    }
                                }
                                // One request per connection keeps the scripted health boundary observable;
                                // Tokio supplies streaming forwarding, including the event response.
                                head.push_str("Connection: close\r\n\r\n");
                                let mut upstream = tokio::net::TcpStream::connect(target.expect("spawn set target")).await.expect("fake router connection");
                                upstream.write_all(head.as_bytes()).await.expect("forward head");
                                upstream.write_all(reader.buffer()).await.expect("forward buffered body");
                                let mut socket = reader.into_inner();
                                let _closed = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
                            });
                        }
                    }
                }
                connections.abort_all();
                while let Some(result) = connections.join_next().await {
                    if let Err(error) = result {
                        assert!(error.is_cancelled(), "proxy task: {error}");
                    }
                }
            }
        });
        (
            Arc::new(InitHealthProxy { provisioner, address, target }),
            Self { cancel, task, health_requests, dropped_health_requests },
        )
    }

    async fn close(self) {
        self.cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(3), self.task)
            .await
            .expect("proxy joins")
            .expect("proxy task");
    }
}

struct Hosted {
    home: tempfile::TempDir,
    storage: ganja_core::Storage,
    session: ganja_protocol::SessionId,
    root: ganja_team::TeamsRoot,
    team: ganja_team::TeamName,
    name: String,
    registry: Arc<ganja_core::teammate::TeammateRegistry>,
    door: ganja_core::Teammates,
    provisioner: Arc<FakeProvisioner>,
    server: ganja_testkit::provisioner::FakeServer,
    init_health_wire: Option<InitHealthWire>,
}

impl Hosted {
    async fn new() -> Self {
        Self::with_deadline(std::time::Duration::from_secs(5)).await
    }

    async fn with_deadline(launch: std::time::Duration) -> Self {
        Self::configured(launch, None).await
    }

    async fn configured(
        launch: std::time::Duration,
        health_fault: Option<LaunchHealthFault>,
    ) -> Self {
        Self::delivery_log();
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
        config.storage = Some(storage.clone());
        let (provisioner, server) =
            FakeProvisioner::serving(engine, config, "ganja", "test-password")
                .await
                .expect("two phase fake");
        let provisioner = Arc::new(provisioner);
        let (backend_provisioner, init_health_wire): (Arc<dyn Provisioner>, _) =
            if let Some(fault) = health_fault {
                let (proxy, wire) = InitHealthWire::new(Arc::clone(&provisioner), fault).await;
                (proxy, Some(wire))
            } else {
                (provisioner.clone(), None)
            };
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
            backend_provisioner,
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
        Self {
            home,
            storage,
            session,
            root,
            team,
            name,
            registry,
            door,
            provisioner,
            server,
            init_health_wire,
        }
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

    fn delivery_log() -> &'static ganja_testkit::LogCapture {
        static LOG: std::sync::OnceLock<ganja_testkit::LogCapture> = std::sync::OnceLock::new();
        LOG.get_or_init(|| {
            let capture = ganja_testkit::LogCapture::default();
            tracing::subscriber::set_global_default(
                tracing_subscriber::fmt()
                    .with_writer(capture.clone())
                    .with_ansi(false)
                    .with_env_filter(
                        "ganja_teammate_cloud::delivery=debug,ganja_serve::bootstrap=debug",
                    )
                    .finish(),
            )
            .expect("delivery diagnostics");
            capture
        })
    }

    fn boundary_diagnostics(&self) -> String {
        let worker = self.provisioner.worker(&self.name);
        let state = worker.as_ref().map(|worker| self.provisioner.current(worker));
        let control = ganja_teammate_cloud::control(&self.registry, &self.name);
        let phase = control.as_ref().map(|control| control.phase());
        let mail = ganja_team::mailbox::read(&self.registry.lead_inbox())
            .map(|mail| mail.valid.into_iter().map(|message| message.text).collect::<Vec<_>>());
        let delivery = Self::delivery_log().logged();
        let outcomes = delivery
            .lines()
            .filter(|line| !line.contains("kind=\"read\""))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "worker={state:?}; phase={phase:?}; control={control:?}; members={:?}; lead inbox={mail:?}; calls={:?}; requests={:?}; delivery={outcomes}",
            self.registry.view(),
            self.provisioner.calls(),
            self.server.requests(),
        )
    }

    async fn wait_finished(&self) {
        // Completion is durable; neither automatic suspension nor best-effort idle mail
        // is required to prove that the runner can leave a blocked inbox read.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if self.storage.load_transcript(&self.session).expect("transcript").iter().any(
                    |message| {
                        message.role == ganja_protocol::Role::Assistant
                            && message.time.completed.is_some()
                    },
                ) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|error| {
            panic!("first turn finishes: {error:?}; {}", self.boundary_diagnostics())
        });
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
        .unwrap_or_else(|error| {
            panic!("member reaches suspended boundary: {error:?}; {}", self.boundary_diagnostics())
        });
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
        let control =
            ganja_teammate_cloud::control(&self.registry, &self.name).expect("cloud control");
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

    async fn assert_launch_health_recovers(self, expected_probes: usize, expected_drops: usize) {
        let started = tokio::time::timeout(
            std::time::Duration::from_secs(6),
            self.door.start(
                ganja_testkit::spawn_with_prompt(&self.name, Some("cloud"), "first request"),
                &ganja_testkit::caller(self.home.path()),
                &ganja_testkit::AllowSpawn,
            ),
        )
        .await;
        let suspended = if matches!(started, Ok(Ok(_))) {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if let Some(worker) = self.provisioner.worker(&self.name)
                        && self.provisioner.current(&worker) == WorkerState::Suspended
                        && self.idle_count() == 1
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_ok()
        } else {
            false
        };
        let diagnostics = self.boundary_diagnostics();
        let suspends = self.suspends();
        let probes = self.init_health_wire.as_ref().map_or_else(
            || {
                self.server
                    .requests()
                    .iter()
                    .filter(|request| request.path == "/global/health")
                    .count()
            },
            |wire| wire.health_requests.load(std::sync::atomic::Ordering::SeqCst),
        );
        let drops = self.init_health_wire.as_ref().map_or(0, |wire| {
            wire.dropped_health_requests.load(std::sync::atomic::Ordering::SeqCst)
        });
        let inbox = self.registry.lead_inbox();
        let _home = self.close().await;
        let mail = ganja_team::mailbox::read(&inbox).expect("joined member mail");
        assert_eq!(drops, expected_drops, "scripted health connection drops; {diagnostics}");
        assert!(matches!(started, Ok(Ok(_))), "launch completes: {started:?}; {diagnostics}");
        assert!(suspended, "first suspend follows the launch health retry; {diagnostics}");
        assert_eq!(suspends, 1, "exactly one first suspension; {diagnostics}");
        assert!(probes >= expected_probes, "health retries observed: {probes}; {diagnostics}");
        assert!(!diagnostics.contains("uncertain; automatic suspend"), "{diagnostics}");
        assert!(
            !mail.valid.iter().any(|message| message.text.contains("earlier request")),
            "startup probes must not report uncertainty: {mail:?}"
        );
    }

    async fn close(self) -> tempfile::TempDir {
        self.registry.shutdown().await;
        let server_closed = self.server.close().await;
        if let Some(wire) = self.init_health_wire {
            wire.close().await;
        }
        server_closed.expect("all fake servers stop");
        self.home
    }
}

#[tokio::test]
async fn a_launch_health_gap_does_not_disable_the_first_suspend() {
    let hosted = Hosted::new().await;
    hosted.server.reply_next("/global/health", 502);
    hosted.assert_launch_health_recovers(2, 0).await;
}

#[tokio::test]
async fn a_launch_health_connection_failure_does_not_disable_the_first_suspend() {
    let hosted = Hosted::configured(
        std::time::Duration::from_secs(5),
        Some(LaunchHealthFault::ConnectionFailure),
    )
    .await;
    hosted.assert_launch_health_recovers(2, 1).await;
}

#[tokio::test]
async fn an_init_health_refusal_does_not_end_the_launch_probe_exception() {
    let hosted =
        Hosted::configured(std::time::Duration::from_secs(5), Some(LaunchHealthFault::InitRefusal))
            .await;
    hosted.assert_launch_health_recovers(3, 0).await;
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
        let control =
            ganja_teammate_cloud::control(&hosted.registry, &hosted.name).expect("control");
        hosted.send("next request");
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while control.phase() != phase {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("terminal lifecycle state");
        assert_eq!(hosted.idle_count(), 1, "an interrupted resume is not another idle turn");
        let inbox = hosted.registry.lead_inbox();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let mail = ganja_team::mailbox::read(&inbox).expect("inbox");
                if mail.valid.iter().any(|message| message.text.contains(sentence)) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("terminal notice arrives");
        let mail = ganja_team::mailbox::read(&inbox).expect("inbox");
        assert_eq!(mail.valid.iter().filter(|message| message.text.contains(sentence)).count(), 1);
        let _home = hosted.close().await;
        let mail = ganja_team::mailbox::read(&inbox).expect("inbox after shutdown");
        assert_eq!(mail.valid.iter().filter(|message| message.text.contains(sentence)).count(), 1);
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
async fn abandoning_a_pending_launch_deletes_and_reconciles_the_worker() {
    use ganja_testkit::provisioner::ProvisionOperation;

    let hosted = Hosted::new().await;
    let _release = hosted.provisioner.hold_next(ProvisionOperation::Resume);
    let mut starting = Box::pin(hosted.start());
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            tokio::select! {
                () = &mut starting => panic!("launch should be held"),
                () = tokio::task::yield_now() => {
                    if hosted.provisioner.calls().iter().any(|(operation, _)| *operation == ProvisionOperation::Resume) {
                        break;
                    }
                }
            }
        }
    }).await.expect("allocated worker reaches resume");
    let worker = hosted.provisioner.worker(&hosted.name).expect("allocated worker");
    drop(starting);
    tokio::time::timeout(std::time::Duration::from_secs(3), hosted.registry.shutdown())
        .await
        .expect("pending launch cleanup is joined");
    assert_eq!(hosted.provisioner.current(&worker), WorkerState::Gone);
    let calls = hosted.provisioner.calls();
    assert_eq!(
        calls.iter().filter(|(operation, _)| *operation == ProvisionOperation::Kill).count(),
        1
    );
    assert!(calls.iter().any(|(operation, _)| *operation == ProvisionOperation::State));
    assert!(ganja_testkit::teammates_recorded(&hosted.root, &hosted.team).is_empty());
    let name = MemberName::parse(&hosted.name).expect("name");
    assert!(
        ganja_team::mailbox::read(&hosted.root.inbox_path(&hosted.team, &name))
            .expect("inbox")
            .valid
            .is_empty()
    );
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
    let control = ganja_teammate_cloud::control(&hosted.registry, &hosted.name).expect("control");
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
