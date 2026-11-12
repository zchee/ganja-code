//! The real git process-group executor is exercised here through its sibling
//! unit tests. This binary also owns the worker-init HTTP/exec integration:
//! git credentials, the projected CA, checkout cleanup, author identity and
//! project semantics belong to the CLI that runs git, not to the cloud adapter.

#![cfg(unix)]

#[path = "worker_support/handoff_tests.rs"]
mod handoff;
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

#[test]
fn worker_init_defaults_to_port_8080() {
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin!("ganja"))
        .args(["worker", "init", "--help"])
        .output()
        .expect("worker init help");
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).expect("help text");
    assert!(help.contains("[default: 8080]"), "unexpected default: {help}");
}

#[derive(Debug, PartialEq, Eq)]
enum HookCleanup {
    Gone,
    NoHookObserved,
}

// FIFO owners stay read-write and nonblocking on the Linux and macOS hosts
// this suite supports. No helper thread can remain blocked in open or read.
struct HandoffBarrier {
    arrival: tokio::io::unix::AsyncFd<std::fs::File>,
    release: std::fs::File,
    group: Option<i32>,
    token: Vec<u8>,
    home: tempfile::TempDir,
}

impl HandoffBarrier {
    fn new() -> Self {
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::OpenOptionsExt as _;

        let home = tempfile::tempdir().expect("barrier home");
        let open = |name| {
            let path = home.path().join(name);
            let name = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("FIFO path");
            // SAFETY: this NUL-terminated path is under the fixture's private
            // directory; mkfifo creates no child process that could outlive it.
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0, "create test-owned FIFO");
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(path)
                .expect("nonblocking FIFO owner")
        };
        // SAFETY: AsyncFd owns this nonblocking FIFO file until deregistration;
        // the fixture never closes or replaces its descriptor through another handle.
        let arrival = unsafe { tokio::io::unix::AsyncFd::register(open("arrival")) }
            .expect("arrival readiness");
        let release = open("release");
        let command = format!(
            "printf '%s\\n' \"$$\" > {:?}; read -r token < {:?}",
            home.path().join("arrival"),
            home.path().join("release"),
        );
        // The hook cannot time out during a successful scenario (30 seconds).
        std::fs::write(
            home.path().join("ganja.toml"),
            format!(
                "model = \"fake/handoff\"\n[[hooks.SessionStart]]\nhooks = [{{ type = \"command\", command = {command:?}, timeout = 120 }}]\n"
            ),
        )
        .expect("trusted barrier hook");
        Self { arrival, release, group: None, token: Vec::new(), home }
    }

    async fn arrived(&mut self, deadline: Instant) -> std::io::Result<()> {
        use std::io::Read as _;

        tokio::time::timeout_at(deadline, async {
            loop {
                let mut bytes = [0; 32];
                let size = self
                    .arrival
                    .async_io(tokio::io::Interest::READABLE, |file| (&*file).read(&mut bytes))
                    .await?;
                self.token.extend_from_slice(&bytes[..size]);
                if self.token.ends_with(b"\n") {
                    let group = std::str::from_utf8(&self.token)
                        .ok()
                        .and_then(|value| value.trim().parse::<i32>().ok())
                        .filter(|group| *group > 1)
                        .ok_or_else(|| std::io::Error::other("invalid hook group token"))?;
                    self.group = Some(group);
                    return Ok(());
                }
            }
        })
        .await
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::TimedOut, error))?
    }

    fn release(&mut self) -> std::io::Result<()> {
        use std::io::Write as _;
        self.release.write_all(b"release\n")
    }

    async fn cleanup(
        &mut self,
        worker: &mut tokio::process::Child,
    ) -> std::io::Result<HookCleanup> {
        let hook_deadline = Instant::now() + Duration::from_secs(5);
        let deadline = hook_deadline + Duration::from_secs(5);
        // Parent exit does not end an independently grouped hook. Keep recovering
        // its identity for the whole reserve, even after the worker is reaped.
        let recovery = if self.group.is_none() {
            match self.arrived(hook_deadline).await {
                Err(error) if error.kind() == std::io::ErrorKind::TimedOut => Ok(()),
                result => result,
            }
        } else {
            Ok(())
        };
        // Release errors must not skip either process's teardown.
        let release = self.release();
        let hook_cleanup = if let Some(group) = self.group {
            // SAFETY: this group was reported by the test-owned hook, whose
            // isolated process group is created by the hook runner.
            unsafe { libc::kill(-group, libc::SIGKILL) };
            wait_for_hook_exit(group, hook_deadline).await
        } else {
            Ok(())
        };
        // Keep the parent alive long enough to reap its killed hook first.
        let kill = worker.start_kill();
        let reaped = tokio::time::timeout_at(deadline, worker.wait())
            .await
            .map_err(std::io::Error::other)
            .and_then(|status| status.map(|_| ()));
        recovery.and(release).and(hook_cleanup).and(kill).and(reaped)?;
        Ok(if self.group.is_some() { HookCleanup::Gone } else { HookCleanup::NoHookObserved })
    }
}

fn assert_handoff(
    scenario: anyhow::Result<()>,
    cleanup: std::io::Result<HookCleanup>,
    expected: HookCleanup,
    worker: &mut tokio::process::Child,
    home: &std::path::Path,
) {
    let status = worker.try_wait();
    let stderr = std::fs::read_to_string(home.join("stderr"))
        .map(|text| worker_support::redact_worker_text(&text));
    let stderr = match &stderr {
        Ok(text) => text.chars().rev().take(4096).collect::<String>().chars().rev().collect(),
        Err(error) => format!("unavailable: {error}"),
    };
    let failure = scenario.as_ref().err().map(|error| format!("{error:#}"));
    let message = worker_support::redact_worker_text(&format!(
        "scenario: {}; cleanup: {cleanup:?}; expected: {expected:?}; worker status: {status:?}; stderr tail: {stderr}",
        failure.as_deref().unwrap_or("ok"),
    ));
    assert!(
        scenario.is_ok() && cleanup.as_ref().is_ok_and(|outcome| *outcome == expected),
        "{message}",
    );
}

#[tokio::test]
async fn a_handoff_failure_redacts_passwords_already_in_the_scenario() {
    let home = tempfile::tempdir().expect("diagnostic home");
    std::fs::write(home.path().join("stderr"), "synthetic-worker-password\n")
        .expect("diagnostic stderr");
    let mut worker = tokio::process::Command::new("sh")
        .args(["-c", "exit 0"])
        .kill_on_drop(true)
        .spawn()
        .expect("diagnostic child");
    let status = tokio::time::timeout(Duration::from_secs(5), worker.wait()).await;
    let kill = worker.start_kill();
    let reaped = tokio::time::timeout(Duration::from_secs(5), worker.wait()).await;
    kill.expect("diagnostic child killed or already exited");
    reaped.expect("bounded diagnostic reap").expect("diagnostic child reaped");
    assert!(status.expect("bounded diagnostic child").expect("diagnostic status").success());
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_handoff(
            Err(anyhow::anyhow!("scenario stderr: synthetic-worker-password")),
            Ok(HookCleanup::Gone),
            HookCleanup::Gone,
            &mut worker,
            home.path(),
        );
    }))
    .expect_err("the synthetic scenario must fail");
    let message = panic.downcast_ref::<String>().expect("formatted assertion panic");
    assert!(message.contains("scenario: scenario stderr: [redacted]"), "{message}");
    assert!(
        !message.contains("synthetic-worker-password"),
        "the complete handoff failure must redact the synthetic password: {message}"
    );
}

async fn wait_for_hook_exit(group: i32, deadline: Instant) -> std::io::Result<()> {
    tokio::time::timeout_at(deadline, async {
        loop {
            // SAFETY: signal zero only queries the isolated test-owned group.
            if unsafe { libc::kill(-group, 0) } == -1 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    return Ok(());
                }
                // Measured on macOS: EPERM means this fixture's same-uid group
                // is all-zombie, with no member that can still run. Keep its
                // parent alive until reaping makes the group disappear as well.
                #[cfg(target_os = "macos")]
                if error.raw_os_error() == Some(libc::EPERM) {
                    tokio::task::yield_now().await;
                    continue;
                }
                return Err(error);
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "hook group is still live or unreaped at its cleanup deadline",
        )
    })?
}

fn handoff_worker(
    server: &GitServer,
    barrier: &HandoffBarrier,
    invalid_startup: bool,
    binary: Option<(&std::path::Path, Instant)>,
    environment: &[(&str, &std::ffi::OsStr)],
) -> (tokio::process::Child, tempfile::TempDir) {
    let home = tempfile::tempdir().expect("worker home");
    let work = home.path().join("work");
    std::fs::create_dir(&work).expect("checkout directory");
    let mut command = tokio::process::Command::new(
        binary
            .map(|(path, _)| path)
            .unwrap_or(std::path::Path::new(assert_cmd::cargo::cargo_bin!("ganja"))),
    );
    command
        .args(["worker", "init", "--port", "0"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").expect("PATH"))
        .env("HOME", home.path())
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("GANJA_CONFIG_HOME", barrier.home.path())
        .env("GANJA_PROVIDER", "fake")
        .env("GANJA_DISABLE_MODELS_FETCH", "1")
        .env("GANJA_SERVER_PASSWORD", "synthetic-worker-password")
        .env("GIT_CONFIG_GLOBAL", &server.gitconfig)
        .env("GIT_SSL_CAINFO", &server.ca)
        .current_dir(work)
        .stdout(std::process::Stdio::piped())
        .stderr(std::fs::File::create(home.path().join("stderr")).expect("stderr"))
        .kill_on_drop(true);
    command.envs(environment.iter().copied());
    if invalid_startup {
        command.env("GIT_CONFIG_GLOBAL", "/missing-synthetic-gitconfig");
    }
    let worker = if let Some((_, deadline)) = binary {
        worker_support::spawn_written_executable(|| command.spawn(), deadline)
    } else {
        command.spawn()
    };
    (worker.expect("worker init"), home)
}

async fn bootstrap_worker(
    worker: &mut tokio::process::Child,
    server: &GitServer,
    deadline: Instant,
    address_file: Option<&std::path::Path>,
) -> anyhow::Result<(u16, String)> {
    use tokio::io::{AsyncBufReadExt as _, BufReader};

    let mut lines = BufReader::new(worker.stdout.as_mut().expect("worker stdout")).lines();
    let line = tokio::time::timeout_at(deadline, lines.next_line())
        .await??
        .ok_or_else(|| anyhow::anyhow!("worker exited before announcement"))?;
    let address: std::net::SocketAddr = line
        .strip_prefix("ganja server listening on http://")
        .ok_or_else(|| anyhow::anyhow!("unexpected announcement"))?
        .parse()?;
    let base = format!("http://127.0.0.1:{}", address.port());
    if let Some(path) = address_file {
        std::fs::write(path, address.to_string())?;
    }
    let client = reqwest::Client::new();
    let init =
        tokio::time::timeout_at(deadline, client.get(format!("{base}/readyz")).send()).await??;
    let epoch = init
        .headers()
        .get(ganja_serve::EPOCH_HEADER)
        .ok_or_else(|| anyhow::anyhow!("init response omitted its epoch"))?
        .to_str()?
        .to_owned();
    let body = serde_json::json!({
        "repository": server.url, "reference": "main", "branch": "workers/handoff",
        "author": {"name": "fixture", "email": "fixture@example.invalid"}, "deadline_secs": 15
    });
    let response = tokio::time::timeout_at(
        deadline,
        client
            .post(format!("{base}/bootstrap"))
            .basic_auth("ganja", Some("synthetic-worker-password"))
            .json(&body)
            .send(),
    )
    .await??;
    anyhow::ensure!(response.status() == 200, "bootstrap status {}", response.status());
    tokio::time::timeout_at(deadline, response.bytes()).await??;
    Ok((address.port(), epoch))
}

async fn bootstrap_held_worker(
    worker: &mut tokio::process::Child,
    barrier: &mut HandoffBarrier,
    server: &GitServer,
    deadline: Instant,
    diagnostics: &std::path::Path,
) -> anyhow::Result<(u16, String)> {
    let initialized = bootstrap_worker(worker, server, deadline, None).await?;
    tokio::select! {
        arrived = barrier.arrived(deadline) => arrived?,
        exited = worker.wait() => {
            let status = exited?;
            let diagnostic = std::fs::read_to_string(diagnostics)?
                .replace("synthetic-worker-password", "[redacted]");
            anyhow::bail!("worker exited before hook arrival: {status}; startup diagnostic: {diagnostic}");
        }
    }
    Ok(initialized)
}

#[tokio::test]
async fn a_connection_queued_during_worker_handoff_is_answered_by_serve() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let server = GitServer::start(false).await;
    let mut barrier = HandoffBarrier::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    let (mut worker, _home) = handoff_worker(&server, &barrier, false, None, &[]);
    let result: anyhow::Result<()> = async {
        let (port, init_epoch) = bootstrap_held_worker(&mut worker, &mut barrier, &server, deadline, &_home.path().join("stderr")).await?;
        let mut queued = Vec::new();
        for (path, password, status) in [
            ("/global/health", "synthetic-worker-password", "200"),
            ("/global/health", "wrong-password", "401"),
            ("/readyz", "", "200"),
        ] {
            // Exactly one fresh connect while the hook is held: never retry.
            let mut stream = tokio::time::timeout_at(deadline, tokio::net::TcpStream::connect(("127.0.0.1", port))).await?
                .map_err(|error| anyhow::anyhow!("single connect while the hook is held: {error}"))?;
            let built = reqwest::Client::new().get("http://localhost/")
                .basic_auth("ganja", Some(password)).build()?;
            let auth = built.headers()["authorization"].to_str()?;
            let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: {auth}\r\nConnection: close\r\n\r\n");
            tokio::time::timeout_at(deadline, stream.write_all(request.as_bytes())).await??;
            queued.push((stream, status, path));
        }
        barrier.release()?;
        for (mut stream, status, path) in queued {
            let mut response = String::new();
            tokio::time::timeout_at(deadline, stream.read_to_string(&mut response)).await??;
            anyhow::ensure!(response.starts_with(&format!("HTTP/1.1 {status}")), "unexpected queued status: {response}");
            let epoch = response.lines().find_map(|line| line.strip_prefix("ganja-serve-epoch: "))
                .ok_or_else(|| anyhow::anyhow!("missing serve epoch"))?;
            anyhow::ensure!(ganja_protocol::is_uuidv7(epoch) && epoch != init_epoch, "response must carry serve's new epoch");
            if path == "/global/health" && status == "200" {
                let body = response.split_once("\r\n\r\n")
                    .ok_or_else(|| anyhow::anyhow!("missing complete HTTP headers"))?.1;
                let health: ganja_client::Health = serde_json::from_str(body)?;
                anyhow::ensure!(health.healthy && !health.version.is_empty(), "complete healthy engine response");
            }
        }
        anyhow::ensure!(Instant::now() < deadline, "scenario deadline expired");
        Ok(())
    }.await;
    let cleanup = barrier.cleanup(&mut worker).await;
    server.close().await;
    assert_handoff(result, cleanup, HookCleanup::Gone, &mut worker, _home.path());
}

#[tokio::test]
async fn barrier_cleanup_reaps_a_worker_that_fails_before_the_hook_arrives() {
    let server = GitServer::start(false).await;
    let mut barrier = HandoffBarrier::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    let (mut worker, _home) = handoff_worker(&server, &barrier, true, None, &[]);
    let result: anyhow::Result<()> = async {
        let status = tokio::time::timeout_at(deadline, worker.wait()).await??;
        anyhow::ensure!(status.code() == Some(1), "ordinary startup failure: {status}");
        let diagnostics = std::fs::read_to_string(_home.path().join("stderr"))?;
        anyhow::ensure!(
            diagnostics
                .contains("ganja worker init requires GIT_CONFIG_GLOBAL to name a readable file"),
            "failure must precede engine assembly: {diagnostics}"
        );
        Ok(())
    }
    .await;
    let cleanup = barrier.cleanup(&mut worker).await;
    server.close().await;
    assert_handoff(result, cleanup, HookCleanup::NoHookObserved, &mut worker, _home.path());
}

#[tokio::test]
async fn barrier_cleanup_reaps_a_worker_and_hook_after_arrival() {
    let server = GitServer::start(false).await;
    let mut barrier = HandoffBarrier::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    let (mut worker, _home) = handoff_worker(&server, &barrier, false, None, &[]);
    let arrived = bootstrap_held_worker(
        &mut worker,
        &mut barrier,
        &server,
        deadline,
        &_home.path().join("stderr"),
    )
    .await;
    let cleanup = barrier.cleanup(&mut worker).await;
    server.close().await;
    let result = arrived.and_then(|_| {
        anyhow::ensure!(worker.try_wait()?.is_some(), "worker reaped");
        Ok(())
    });
    assert_handoff(result, cleanup, HookCleanup::Gone, &mut worker, _home.path());
}

#[tokio::test]
async fn barrier_cleanup_recovers_a_started_hook_whose_arrival_was_not_consumed() {
    let server = GitServer::start(false).await;
    let mut barrier = HandoffBarrier::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    let (mut worker, _home) = handoff_worker(&server, &barrier, false, None, &[]);
    let started: anyhow::Result<()> = async {
        bootstrap_worker(&mut worker, &server, deadline, None).await?;
        // Readiness proves the hook wrote its token, without consuming its identity.
        drop(tokio::time::timeout_at(deadline, barrier.arrival.readable()).await??);
        anyhow::ensure!(barrier.group.is_none(), "arrival must remain unread");
        Ok(())
    }
    .await;
    let cleanup = barrier.cleanup(&mut worker).await;
    server.close().await;
    let result = started.and_then(|()| {
        let group = barrier.group.ok_or_else(|| {
            anyhow::anyhow!("cleanup must recover and verify the unread hook group")
        })?;
        // SAFETY: this only queries the isolated group identified by the fixture.
        anyhow::ensure!(unsafe { libc::kill(-group, 0) } == -1, "no live hook group after cleanup");
        anyhow::ensure!(
            std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
            "hook group disappeared"
        );
        anyhow::ensure!(worker.try_wait()?.is_some(), "worker reaped");
        Ok(())
    });
    assert_handoff(result, cleanup, HookCleanup::Gone, &mut worker, _home.path());
}

#[tokio::test]
async fn barrier_cleanup_recovers_a_hook_that_reports_arrival_after_its_worker_exits() {
    let server = GitServer::start(false).await;
    let mut barrier = HandoffBarrier::new();
    let mut delayed = HandoffBarrier::new();
    let command = format!(
        "printf '%s\\n' \"$$\" > {:?}; read -r token < {:?}; printf '%s\\n' \"$$\" > {:?}; read -r token < {:?}; read -r token < {:?}",
        delayed.home.path().join("arrival"),
        delayed.home.path().join("release"),
        barrier.home.path().join("arrival"),
        barrier.home.path().join("release"),
        barrier.home.path().join("release"),
    );
    std::fs::write(
        barrier.home.path().join("ganja.toml"),
        format!("model = \"fake/handoff\"\n[[hooks.SessionStart]]\nhooks = [{{ type = \"command\", command = {command:?}, timeout = 120 }}]\n"),
    )
    .expect("delayed arrival hook");
    let deadline = Instant::now() + Duration::from_secs(30);
    let (mut worker, _home) = handoff_worker(&server, &barrier, false, None, &[]);
    let started: anyhow::Result<()> = async {
        bootstrap_worker(&mut worker, &server, deadline, None).await?;
        delayed.arrived(deadline).await?;
        worker.start_kill()?;
        tokio::time::timeout_at(deadline, worker.wait()).await??;
        anyhow::ensure!(barrier.group.is_none(), "hook identity is still unread");
        Ok(())
    }
    .await;
    // The gate stays held until cleanup has been polled with an exited worker.
    let (cleanup, released) = tokio::join!(barrier.cleanup(&mut worker), async {
        tokio::task::yield_now().await;
        delayed.release()
    });
    let recovered = barrier.group;
    // Independent ownership prevents the red control itself leaving a hook alive.
    let emergency = if let Some(group) = delayed.group {
        // SAFETY: the separate startup token identifies this fixture's hook group.
        unsafe { libc::kill(-group, libc::SIGKILL) };
        wait_for_hook_exit(group, Instant::now() + Duration::from_secs(5)).await
    } else {
        Ok(())
    };
    server.close().await;
    let result = started.and_then(|()| {
        emergency?;
        released?;
        anyhow::ensure!(
            recovered.is_some(),
            "cleanup must recover the hook after its worker exits"
        );
        anyhow::ensure!(worker.try_wait()?.is_some(), "worker reaped");
        Ok(())
    });
    assert_handoff(result, cleanup, HookCleanup::Gone, &mut worker, _home.path());
}

#[tokio::test]
async fn a_hook_held_past_its_cleanup_check_deadline_is_reported_as_live() {
    let server = GitServer::start(false).await;
    let mut barrier = HandoffBarrier::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    let (mut worker, home) = handoff_worker(&server, &barrier, false, None, &[]);
    let check: anyhow::Result<std::io::Result<()>> = async {
        bootstrap_held_worker(
            &mut worker,
            &mut barrier,
            &server,
            deadline,
            &home.path().join("stderr"),
        )
        .await?;
        let group = barrier.group.ok_or_else(|| anyhow::anyhow!("held hook group missing"))?;
        // Do not release or signal this hook until its independent check expires.
        Ok(wait_for_hook_exit(group, Instant::now() + Duration::from_millis(50)).await)
    }
    .await;
    let cleanup = barrier.cleanup(&mut worker).await;
    server.close().await;
    let result = check.and_then(|check| {
        let error =
            check.err().ok_or_else(|| anyhow::anyhow!("a live hook cannot pass its exit check"))?;
        anyhow::ensure!(error.kind() == std::io::ErrorKind::TimedOut, "live-hook check: {error}");
        anyhow::ensure!(error.to_string().contains("still live"), "{error}");
        Ok(())
    });
    assert_handoff(result, cleanup, HookCleanup::Gone, &mut worker, home.path());
}

fn health_diagnostic(result: Result<ganja_client::Health, ganja_client::ClientError>) -> String {
    match result {
        Ok(health) => format!("success: {health:?}"),
        Err(error) => {
            let debug = format!("{error:?}");
            let error = anyhow::Error::new(error);
            let io = error.chain().find_map(|source| source.downcast_ref::<std::io::Error>());
            let io = io.map(|error| {
                format!("kind={:?}, raw OS code={:?}", error.kind(), error.raw_os_error())
            });
            format!("{error:#}; debug: {debug}; IO error: {io:?}")
        }
    }
}

#[tokio::test]
async fn the_initializer_and_replacement_server_announce_the_same_ephemeral_port() {
    let server = GitServer::start(false).await;
    let mut worker = worker_support::WorkerProcess::start(&server).await;
    let initial = worker.announced_ports();
    assert_eq!(initial.len(), 1);
    assert_ne!(initial[0], 0);
    let bootstrap = Bootstrap {
        repository: server.url.clone(),
        reference: "main".to_owned(),
        branch: "workers/same-port".to_owned(),
        author: BootstrapAuthor {
            name: "fixture".to_owned(),
            email: "fixture@example.invalid".to_owned(),
        },
    };
    assert!(matches!(
        worker.client.bootstrap(&bootstrap, Duration::from_secs(15)).await.expect("bootstrap"),
        BootstrapOutcome::Cloned { .. }
    ));
    let ports = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let ports = worker.announced_ports();
            if ports.len() >= 2 {
                break ports;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("replacement announces its bound port");
    assert_eq!(
        ports,
        [initial[0], initial[0]],
        "exec preserves the initializer's actual bound port"
    );
    if let Err(error) = worker.client.health().await {
        let error = health_diagnostic(Err(error));
        let status = match worker.status() {
            Ok(None) => "still running".to_owned(),
            Ok(Some(status)) => status.to_string(),
            Err(error) => format!("status unavailable: {error}"),
        };
        let stderr = worker.stderr();
        let tail = &stderr[stderr.floor_char_boundary(stderr.len().saturating_sub(4096))..];
        let announcements = worker.announcements();
        let fresh = async {
            ganja_client::Client::new(
                &worker.address,
                Some(ganja_client::Credentials::new("ganja", "synthetic-worker-password")),
            )?
            .health()
            .await
        }
        .await;
        let fresh = health_diagnostic(fresh);
        let original = health_diagnostic(worker.client.health().await);
        let message = worker_support::redact_worker_text(&format!(
            "same client reaches replacement server: {error}; worker status: {status}; stderr tail: {tail}; announcements: {announcements:?}; fresh client health: {fresh}; original client health after failure: {original}"
        ));
        panic!("{message}");
    }
    drop(worker);
    server.close().await;
}

#[tokio::test]
async fn an_early_initializer_exit_reports_status_and_redacted_stderr() {
    use futures::FutureExt as _;

    let server = GitServer::start(false).await;
    let failure =
        std::panic::AssertUnwindSafe(worker_support::WorkerProcess::start_with_environment(
            &server,
            &[("GIT_CONFIG_GLOBAL", "/missing-synthetic-worker-gitconfig")],
        ))
        .catch_unwind()
        .await
        .err()
        .expect("invalid worker configuration is refused");
    let message = failure
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| failure.downcast_ref::<&str>().copied())
        .expect("panic text");
    assert!(message.contains("exit status: 1"), "child exit status was omitted: {message}");
    assert!(
        message.contains("requires GIT_CONFIG_GLOBAL to name a readable file"),
        "child stderr was omitted: {message}"
    );
    assert!(!message.contains("synthetic-worker-password"), "password must be redacted");
    server.close().await;
}

#[tokio::test]
async fn explicit_initializer_ports_survive_the_server_handoff() {
    use tokio::io::{AsyncBufReadExt as _, BufReader};

    let server = GitServer::start(false).await;
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve an explicit port");
    let explicit = reservation.local_addr().expect("reserved address").port();
    let mut reservation = Some(reservation);
    for requested in [0, explicit] {
        let home = tempfile::tempdir().expect("worker home");
        let work = home.path().join("work");
        std::fs::create_dir(&work).expect("empty checkout directory");
        let mut command = tokio::process::Command::new(assert_cmd::cargo::cargo_bin!("ganja"));
        command
            .args(["worker", "init", "--port", &requested.to_string()])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").expect("PATH"))
            .env("HOME", home.path())
            .env("XDG_DATA_HOME", home.path().join("data"))
            .env("GANJA_CONFIG_HOME", home.path().join("config"))
            .env("GANJA_PROVIDER", "fake")
            .env("GANJA_DISABLE_MODELS_FETCH", "1")
            .env("GANJA_SERVER_PASSWORD", "synthetic-worker-password")
            .env("GIT_CONFIG_GLOBAL", &server.gitconfig)
            .env("GIT_SSL_CAINFO", &server.ca)
            .current_dir(&work)
            .stdout(std::process::Stdio::piped())
            .stderr(std::fs::File::create(home.path().join("stderr")).expect("stderr file"))
            .kill_on_drop(true);
        if requested != 0 {
            drop(reservation.take());
        }
        let mut child = command.spawn().expect("worker init");
        let stdout = child.stdout.take().expect("stdout");
        let mut lines = BufReader::new(stdout).lines();
        let deadline = Instant::now() + Duration::from_secs(5);
        let announced = tokio::time::timeout_at(deadline, lines.next_line())
            .await
            .expect("initializer announces its bound address")
            .expect("address line");
        let Some(line) = announced else {
            let status = tokio::time::timeout_at(deadline, child.wait()).await;
            let stderr = worker_support::worker_stderr(home.path());
            let tail = &stderr[stderr.floor_char_boundary(stderr.len().saturating_sub(4096))..];
            panic!(
                "initializer stdout closed before announcement; requested={requested}; child status: {status:?}; stderr tail: {tail}"
            );
        };
        let address: std::net::SocketAddr = line
            .strip_prefix("ganja server listening on http://")
            .expect("same address format as serve")
            .parse()
            .expect("bound socket address");
        assert!(address.ip().is_unspecified());
        assert_ne!(address.port(), 0);
        if requested != 0 {
            assert_eq!(address.port(), requested);
        }
        let client = ganja_client::Client::new(
            &format!("http://127.0.0.1:{}", address.port()),
            Some(ganja_client::Credentials::new("ganja", "synthetic-worker-password")),
        )
        .expect("client");
        assert!(matches!(
            client.bootstrap_status().await.expect("announced listener responds"),
            ganja_client::BootstrapStatus::Idle
        ));
        let bootstrap = Bootstrap {
            repository: server.url.clone(),
            reference: "main".to_owned(),
            branch: "workers/explicit-port".to_owned(),
            author: BootstrapAuthor {
                name: "fixture".to_owned(),
                email: "fixture@example.invalid".to_owned(),
            },
        };
        let outcome =
            client.bootstrap(&bootstrap, Duration::from_secs(15)).await.expect("bootstrap");
        assert!(matches!(outcome, BootstrapOutcome::Cloned { .. }), "{outcome:?}");
        let replacement = tokio::time::timeout(Duration::from_secs(15), lines.next_line())
            .await
            .expect("replacement announces its port")
            .expect("replacement address")
            .expect("replacement stays open");
        assert_eq!(replacement, line, "explicit port survives checkout and exec");
        if let Err(error) = client.health().await {
            let error = health_diagnostic(Err(error));
            let status = match child.try_wait() {
                Ok(None) => "still running".to_owned(),
                Ok(Some(status)) => status.to_string(),
                Err(error) => format!("status unavailable: {error}"),
            };
            let stderr = worker_support::worker_stderr(home.path());
            let tail = &stderr[stderr.floor_char_boundary(stderr.len().saturating_sub(4096))..];
            let announcements = [&line, &replacement];
            let fresh = async {
                ganja_client::Client::new(
                    &format!("http://127.0.0.1:{}", address.port()),
                    Some(ganja_client::Credentials::new("ganja", "synthetic-worker-password")),
                )?
                .health()
                .await
            }
            .await;
            let fresh = health_diagnostic(fresh);
            let original = health_diagnostic(client.health().await);
            let message = worker_support::redact_worker_text(&format!(
                "same port reaches the served engine: {error}; worker status: {status}; stderr tail: {tail}; announcements: {announcements:?}; fresh client health: {fresh}; original client health after failure: {original}"
            ));
            panic!("{message}");
        }
        child.kill().await.expect("stop served engine");
        child.wait().await.expect("reap served engine");
    }
    server.close().await;
}

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
    let reason = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let event = events
                .next()
                .await
                .expect("event stream ended before MessageFinished")
                .expect("event");
            if let Event::MessageFinished { reason, .. } = event {
                break reason;
            }
        }
    })
    .await
    .expect("tool turn completes");
    assert_eq!(reason, FinishReason::Completed);
    worker.assert_provider_key();
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
async fn a_worker_config_cannot_use_server_authentication_as_a_provider_key() {
    let server = GitServer::start(false).await;
    let config = tempfile::tempdir().expect("worker configuration");
    std::fs::write(
        config.path().join("ganja.toml"),
        "model = \"worker-fixture/test\"\n[provider.worker-fixture]\ndialect = \"openai-chat-completions\"\nbase_url = \"https://provider.invalid\"\nkey_env = \"GANJA_SERVER_PASSWORD\"\n",
    )
    .expect("reserved provider variable config");
    let mut worker = worker_support::WorkerProcess::start_with_environment(
        &server,
        &[("GANJA_CONFIG_HOME", config.path().to_str().expect("configuration path"))],
    )
    .await;
    let bootstrap = Bootstrap {
        repository: server.url.clone(),
        reference: "main".to_owned(),
        branch: "workers/reserved-key".to_owned(),
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
        while worker.is_alive() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("server refuses the reserved provider variable");
    assert!(
        worker.stderr().contains(
            "provider \"worker-fixture\" cannot use reserved server variable `GANJA_SERVER_PASSWORD` as its `key_env`"
        ),
        "{}",
        worker.stderr()
    );
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
async fn the_served_worker_loads_template_config_and_runs_its_scripted_provider() {
    let server = GitServer::start(false).await;
    let template = tempfile::tempdir().expect("worker template");
    let config = template.path().join("engine-config");
    let data = template.path().join("engine-data");
    let script = template.path().join("turns.json");
    std::fs::create_dir(&config).expect("config directory");
    std::fs::write(config.join("ganja.toml"), "model = \"fake/worker-template-model\"\n")
        .expect("template configuration");
    let command = "test -z \"${GIT_SSL_NO_VERIFY+x}${HTTPS_PROXY+x}${ANTHROPIC_API_KEY+x}${GANJA_CONFIG+x}${GANJA_WORKER_ENV_CANARY+x}${GANJA_SERVER_PASSWORD+x}${GANJA_SERVER_USERNAME+x}\" && test \"$GANJA_DISABLE_MODELS_FETCH\" = 1 && test \"$GANJA_DISABLE_TERM_PROBE\" = 1 && test -n \"$SSL_CERT_FILE\" && printf template-environment > environment.txt";
    std::fs::write(&script, serde_json::json!({"turns": [
        {"text": "checking environment", "tool_calls": [{"name": "bash", "args": {"command": command}}]},
        {"text": "worker-template-script-finished"}
    ]}).to_string()).expect("template script");
    let mut worker = worker_support::WorkerProcess::start_with_environment(
        &server,
        &[
            ("GANJA_CONFIG_HOME", config.to_str().expect("config path")),
            ("XDG_DATA_HOME", data.to_str().expect("data path")),
            ("SSL_CERT_FILE", server.ca.to_str().expect("CA path")),
            ("GANJA_PROVIDER", "fake"),
            ("GANJA_FAKE_SCRIPT", script.to_str().expect("script path")),
            ("GIT_SSL_NO_VERIFY", "1"),
            ("HTTPS_PROXY", "http://127.0.0.1:1"),
            ("ANTHROPIC_API_KEY", "synthetic-provider-value"),
            ("GANJA_CONFIG", "/missing-synthetic-host-config.toml"),
            ("GANJA_WORKER_ENV_CANARY", "synthetic-unlisted-value"),
            ("GANJA_SERVER_USERNAME", "ganja"),
        ],
    )
    .await;
    let bootstrap = Bootstrap {
        repository: server.url.clone(),
        reference: "main".to_owned(),
        branch: "workers/template-env".to_owned(),
        author: BootstrapAuthor {
            name: "fixture".to_owned(),
            email: "fixture@example.invalid".to_owned(),
        },
    };
    assert!(matches!(
        worker.client.bootstrap(&bootstrap, Duration::from_secs(15)).await.expect("bootstrap"),
        BootstrapOutcome::Cloned { .. }
    ));
    let health = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(health) = worker.client.health().await {
                break health;
            }
            assert!(worker.is_alive(), "served worker remains alive");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("served worker health");
    let mut events = worker.client.events().await.expect("events");
    worker
        .client
        .prompt(&health.session_id, &Prompt::new("check template environment"))
        .await
        .expect("prompt");
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if matches!(
                events.next().await.expect("stream open").expect("event"),
                Event::MessageFinished { .. }
            ) {
                break;
            }
        }
    })
    .await
    .expect("scripted turn finishes");
    assert_eq!(
        std::fs::read_to_string(worker.work.join("environment.txt"))
            .expect("script ran with clean environment"),
        "template-environment"
    );
    let messages = worker.client.messages(&health.session_id).await.expect("messages");
    assert!(
        messages.iter().any(|message| message.model.as_deref() == Some("worker-template-model")),
        "model from template ganja.toml was used"
    );
    assert!(
        serde_json::to_string(&messages)
            .expect("messages serialize")
            .contains("worker-template-script-finished")
    );
    assert!(data.join("ganja").is_dir(), "template data home was used");
    drop(worker);
    server.close().await;
}

#[tokio::test]
async fn the_served_workers_tools_keep_the_git_policy_and_ca_paths() {
    let server = GitServer::start(false).await;
    let template = tempfile::tempdir().expect("worker template");
    let gitconfig = template.path().join("gitconfig");
    let ca = template.path().join("ca.pem");
    let script = template.path().join("turns.json");
    std::fs::write(
        &gitconfig,
        "[commit]\ngpgsign = false\n[ganja-test]\nmarker = worker-git-policy\n",
    )
    .expect("git policy with marker");
    std::fs::copy(&server.ca, &ca).expect("projected CA bundle");
    let command = "printf '%s\\n%s\\n' \"$GIT_CONFIG_GLOBAL\" \"$GIT_SSL_CAINFO\" > git-environment.txt; git config --get ganja-test.marker >> git-environment.txt";
    std::fs::write(&script, serde_json::json!({"turns": [
        {"text": "checking git policy", "tool_calls": [{"name": "bash", "args": {"command": command}}]},
        {"text": "git-policy-check-finished"}
    ]}).to_string()).expect("template script");
    let mut worker = worker_support::WorkerProcess::start_with_environment(
        &server,
        &[
            ("GIT_CONFIG_GLOBAL", gitconfig.to_str().expect("git config path")),
            ("GIT_SSL_CAINFO", ca.to_str().expect("CA path")),
            ("GANJA_PROVIDER", "fake"),
            ("GANJA_FAKE_SCRIPT", script.to_str().expect("script path")),
        ],
    )
    .await;
    let bootstrap = Bootstrap {
        repository: server.url.clone(),
        reference: "main".to_owned(),
        branch: "workers/git-environment".to_owned(),
        author: BootstrapAuthor {
            name: "fixture".to_owned(),
            email: "fixture@example.invalid".to_owned(),
        },
    };
    assert!(matches!(
        worker.client.bootstrap(&bootstrap, Duration::from_secs(15)).await.expect("bootstrap"),
        BootstrapOutcome::Cloned { .. }
    ));
    let health = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(health) = worker.client.health().await {
                break health;
            }
            assert!(worker.is_alive(), "served worker remains alive");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("served worker health");
    let mut events = worker.client.events().await.expect("events");
    worker
        .client
        .prompt(&health.session_id, &Prompt::new("check git environment"))
        .await
        .expect("prompt");
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if matches!(
                events.next().await.expect("stream open").expect("event"),
                Event::MessageFinished { .. }
            ) {
                break;
            }
        }
    })
    .await
    .expect("scripted turn finishes");
    assert_eq!(
        std::fs::read_to_string(worker.work.join("git-environment.txt")).expect("git tool output"),
        format!("{}\n{}\nworker-git-policy\n", gitconfig.display(), ca.display()),
        "the engine's bash tool keeps both paths and git reads the template policy"
    );
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
