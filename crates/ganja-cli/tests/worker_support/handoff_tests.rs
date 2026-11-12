//! Real-process listener inheritance and failure cleanup, without /proc.

use std::io::Write as _;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::os::fd::AsRawFd as _;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::time::Instant;

use super::worker_support::{GitServer, listener};

const PROBE: &str = "GANJA_TEST_LISTENER_PROBE";

#[test]
fn inherited_socket_probe() {
    if let Some(spec) = std::env::var_os(PROBE) {
        let spec: serde_json::Value =
            serde_json::from_str(spec.to_str().expect("probe specification")).expect("probe JSON");
        let address = if let Some(address) = spec.get("address") {
            address.as_str().expect("socket address").to_owned()
        } else {
            std::fs::read_to_string(spec["address_file"].as_str().expect("address file"))
                .expect("announced listener address")
        };
        let address: SocketAddr = address.trim().parse().expect("announced socket address");
        let result = if let Some(number) = spec.get("number").and_then(serde_json::Value::as_i64) {
            listener::matches(i32::try_from(number).expect("descriptor"), address)
        } else {
            listener::scan(address)
        };
        let outcome = match result {
            Ok(true) => "present".to_owned(),
            Ok(false) => "absent".to_owned(),
            Err(error) => format!("error {error}"),
        };
        let output = spec
            .get("output_environment")
            .map(|key| {
                std::env::var_os(key.as_str().expect("output environment name"))
                    .expect("probe output environment")
            })
            .unwrap_or_else(|| spec["output"].as_str().expect("probe output path").into());
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(output)
            .expect("probe output");
        writeln!(file, "{outcome}").expect("record probe outcome");
        return;
    }
    // Positive and negative controls run when this binary is not acting as a child probe.
    let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("control socket");
    let address = socket.local_addr().expect("control address");
    assert!(listener::matches(socket.as_raw_fd(), address).expect("listening control"));
    let file = std::fs::File::open("/dev/null").expect("non-socket control");
    assert!(!listener::matches(file.as_raw_fd(), address).expect("file control"));
    let connected = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(3))
        .expect("connected stream control");
    assert!(
        !listener::matches(connected.as_raw_fd(), connected.local_addr().expect("client address"))
            .expect("connected does not mean listening")
    );
}

#[cfg(target_vendor = "apple")]
#[test]
fn a_socket_name_failure_reports_read_only_descriptor_facts() {
    use std::os::fd::{FromRawFd as _, OwnedFd};

    // SAFETY: socket returns a fresh unconnected kernel-control socket owned by this test.
    let number = unsafe { libc::socket(libc::PF_SYSTEM, libc::SOCK_DGRAM, libc::SYSPROTO_CONTROL) };
    assert!(number >= 0, "kernel-control socket: {}", std::io::Error::last_os_error());
    // SAFETY: this successful socket call created a new descriptor with no other owner.
    let socket = unsafe { OwnedFd::from_raw_fd(number) };
    // SAFETY: F_GETFD is a read-only query of the live test-owned socket.
    let flags = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_GETFD) };
    let error = listener::matches(socket.as_raw_fd(), (Ipv4Addr::LOCALHOST, 0).into())
        .expect_err("kernel control does not implement getsockname");
    let message = error.to_string();
    for field in [
        format!("getsockname fd={number} errno={}", libc::EOPNOTSUPP),
        "fstat_type=0o140000".to_owned(),
        "cloexec=false".to_owned(),
        format!("so_type={}", libc::SOCK_DGRAM),
        format!("getpeername_errno={}", libc::ENOTCONN),
    ] {
        assert!(message.contains(&field), "missing {field}: {message}");
    }
    // SAFETY: the probe must leave this descriptor open and its flags unchanged.
    assert_eq!(unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_GETFD) }, flags);
}

fn probe_command(spec: serde_json::Value) -> String {
    let spec = serde_json::to_string(&spec).expect("probe JSON");
    let binary = std::env::current_exe().expect("test binary");
    format!(
        "{PROBE}={} {} --exact handoff::inherited_socket_probe --nocapture",
        shlex::try_quote(&spec).expect("quote probe"),
        shlex::try_quote(binary.to_str().expect("binary path")).expect("quote binary")
    )
}

fn record_probe_command(mut spec: serde_json::Value) -> String {
    let output = spec["output"].as_str().expect("probe output").to_owned();
    spec["output_environment"] = serde_json::json!("GANJA_TEST_LISTENER_PROBE_OUTPUT");
    let probe = probe_command(spec);
    let output = shlex::try_quote(&output).expect("quote output");
    format!(
        "probe_file={output}.probe.$$\nif GANJA_TEST_LISTENER_PROBE_OUTPUT=\"$probe_file\" {probe} >/dev/null 2>&1; then\n  IFS= read -r result < \"$probe_file\" || result='probe-failed status=0'\nelse\n  status=$?; result=\"probe-failed status=$status\"\nfi\nrm -f \"$probe_file\"\nprintf '%s\\n' \"$result\" >> {output}",
    )
}

#[tokio::test]
async fn a_probe_failure_is_recorded_without_stopping_git() {
    let home = tempfile::tempdir().expect("failed probe home");
    let output = home.path().join("probe-result");
    let probe = record_probe_command(serde_json::json!({
        "address_file": home.path().join("missing-address"), "output": output,
    }));
    let mut child = tokio::process::Command::new("sh")
        .args(["-c", &format!("{probe}\nexec git --version")])
        .stdout(std::fs::File::create(home.path().join("stdout")).expect("git stdout"))
        .stderr(std::fs::File::create(home.path().join("stderr")).expect("git stderr"))
        .kill_on_drop(true)
        .spawn()
        .expect("git wrapper");
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    let cleanup = reap(&mut child).await;
    cleanup.expect("git wrapper reaped");
    assert!(status.expect("bounded wrapper").expect("git status").success());
    assert_eq!(
        std::fs::read_to_string(output).expect("failure record"),
        "probe-failed status=101\n"
    );
    assert!(
        std::fs::read_to_string(home.path().join("stdout"))
            .expect("git output")
            .starts_with("git version ")
    );
}

async fn reap(child: &mut tokio::process::Child) -> std::io::Result<std::process::ExitStatus> {
    child.start_kill()?;
    tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .map_err(std::io::Error::other)?
}

async fn refused_start(number: i32, inherit: bool, expected: SocketAddr, reason: &str) {
    let home = tempfile::tempdir().expect("invalid-start home");
    let mut command = tokio::process::Command::new(assert_cmd::cargo::cargo_bin!("ganja"));
    command
        .args([
            "serve",
            "--hostname",
            &expected.ip().to_string(),
            "--port",
            &expected.port().to_string(),
            "--listen-fd",
            &number.to_string(),
        ])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").expect("PATH"))
        .env("HOME", home.path())
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("GANJA_CONFIG_HOME", home.path().join("config"))
        .env("GANJA_PROVIDER", "fake")
        .env("GANJA_DISABLE_MODELS_FETCH", "1")
        .current_dir(home.path())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(home.path().join("stderr")).expect("stderr"))
        .kill_on_drop(true);
    if inherit {
        listener::inherit(&mut command, &number);
    }
    let mut child = command.spawn().expect("real serve startup");
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    reap(&mut child).await.expect("invalid-start child reaped");
    assert_eq!(
        status.expect("bounded refusal").expect("child status").code(),
        Some(1),
        "invalid descriptors cause an ordinary startup error, not an abort"
    );
    let diagnostics = std::fs::read_to_string(home.path().join("stderr")).expect("diagnostics");
    assert!(diagnostics.contains(reason), "expected {reason}: {diagnostics}");
}

#[tokio::test]
async fn standard_inherited_descriptor_numbers_exit_with_an_ordinary_error() {
    for number in -1..3 {
        refused_start(number, false, (Ipv4Addr::LOCALHOST, 4096).into(), "at least 3").await;
    }
}

#[tokio::test]
async fn a_closed_inherited_descriptor_number_exits_with_an_ordinary_error() {
    refused_start(i32::MAX, false, (Ipv4Addr::LOCALHOST, 4096).into(), "not open").await;
}

#[tokio::test]
async fn a_regular_file_inherited_as_the_listener_exits_with_an_ordinary_error() {
    let file = tempfile::tempfile().expect("regular file");
    refused_start(file.as_raw_fd(), true, (Ipv4Addr::LOCALHOST, 4096).into(), "usable socket")
        .await;
    file.metadata().expect("parent still owns its file");
}

#[tokio::test]
async fn a_bound_non_listening_inherited_stream_exits_with_an_ordinary_error() {
    let socket = tokio::net::TcpSocket::new_v4().expect("stream socket");
    socket.bind((Ipv4Addr::LOCALHOST, 0).into()).expect("bind without listening");
    refused_start(socket.as_raw_fd(), true, socket.local_addr().expect("address"), "not listening")
        .await;
}

#[tokio::test]
async fn an_inherited_datagram_socket_exits_with_an_ordinary_error() {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("datagram socket");
    refused_start(
        socket.as_raw_fd(),
        true,
        socket.local_addr().expect("address"),
        "not a stream socket",
    )
    .await;
}

#[tokio::test]
async fn an_inherited_connected_stream_exits_with_an_ordinary_error() {
    let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("listener");
    let client = std::net::TcpStream::connect_timeout(
        &socket.local_addr().expect("address"),
        Duration::from_secs(3),
    )
    .expect("connected stream");
    refused_start(client.as_raw_fd(), true, client.local_addr().expect("address"), "not listening")
        .await;
}

#[tokio::test]
async fn an_inherited_listening_unix_stream_exits_with_an_ordinary_error() {
    let home = tempfile::tempdir().expect("Unix listener directory");
    let socket = std::os::unix::net::UnixListener::bind(home.path().join("listener.sock"))
        .expect("listening Unix stream");
    refused_start(
        socket.as_raw_fd(),
        true,
        (Ipv4Addr::LOCALHOST, 4096).into(),
        "not an IPv4 or IPv6 socket",
    )
    .await;
    socket.local_addr().expect("parent still owns its Unix listener");
}

#[tokio::test]
async fn an_inherited_address_mismatch_exits_with_an_ordinary_error() {
    let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("listener");
    let port = socket.local_addr().expect("address").port();
    refused_start(socket.as_raw_fd(), true, (Ipv4Addr::UNSPECIFIED, port).into(), "does not match")
        .await;
}

#[tokio::test]
async fn an_inherited_port_mismatch_exits_with_an_ordinary_error() {
    let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("listener");
    let port = socket.local_addr().expect("address").port().wrapping_add(1);
    refused_start(socket.as_raw_fd(), true, (Ipv4Addr::LOCALHOST, port).into(), "does not match")
        .await;
}

#[tokio::test]
async fn the_bounded_child_probe_detects_an_inheritable_listener_duplicate() {
    use std::os::fd::AsFd as _;

    let home = tempfile::tempdir().expect("probe home");
    let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("control listener");
    let address = socket.local_addr().expect("control address");
    let duplicate = socket.as_fd().try_clone_to_owned().expect("inheritable control duplicate");
    assert!(duplicate.as_raw_fd() < listener::scan_bound().expect("scan bound"));
    let output = home.path().join("probe-result");
    let spec = serde_json::json!({"address": address, "output": output});
    let mut command = tokio::process::Command::new(std::env::current_exe().expect("test binary"));
    command
        .args(["--exact", "handoff::inherited_socket_probe", "--nocapture"])
        .env(PROBE, spec.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    listener::inherit(&mut command, &duplicate);
    let mut child = command.spawn().expect("probe child with selected inheritable duplicate");
    drop(duplicate);
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    let cleanup = reap(&mut child).await;
    cleanup.expect("probe child reaped");
    assert!(status.expect("bounded positive control").expect("child status").success());
    assert_eq!(std::fs::read_to_string(output).expect("scan output"), "present\n");
}

#[tokio::test]
async fn the_child_scan_distinguishes_unrelated_inherited_descriptor_kinds() {
    use std::os::fd::AsFd as _;

    let home = tempfile::tempdir().expect("descriptor matrix home");
    let (pipe_read, pipe_write) = std::io::pipe().expect("pipe");
    let file = tempfile::tempfile().expect("regular file");
    let null = std::fs::File::open("/dev/null").expect("null device");
    let unix_listener = std::os::unix::net::UnixListener::bind(home.path().join("listener.sock"))
        .expect("Unix listener");
    let unix_datagram = std::os::unix::net::UnixDatagram::bind(home.path().join("datagram.sock"))
        .expect("Unix datagram");
    let udp = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bound UDP");
    let tcp_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("different TCP listener");
    let address = tcp_listener.local_addr().expect("TCP address");
    let connected = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(3))
        .expect("connected TCP");
    let (_peer, _) = tcp_listener.accept().expect("connected peer");
    let shutdown = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(3))
        .expect("TCP before shutdown");
    let (peer, _) = tcp_listener.accept().expect("shutdown peer");
    shutdown.shutdown(std::net::Shutdown::Both).expect("shutdown TCP");
    drop(peer);
    let (socketpair, peer) = std::os::unix::net::UnixStream::pair().expect("socketpair");
    drop(peer);
    let cases = [
        ("pipe read end", pipe_read.as_fd()),
        ("pipe write end", pipe_write.as_fd()),
        ("regular file", file.as_fd()),
        ("/dev/null", null.as_fd()),
        ("Unix stream listener", unix_listener.as_fd()),
        ("Unix datagram", unix_datagram.as_fd()),
        ("bound UDP", udp.as_fd()),
        ("different TCP listener", tcp_listener.as_fd()),
        ("connected TCP", connected.as_fd()),
        ("shutdown TCP with closed peer", shutdown.as_fd()),
        ("socketpair with closed peer", socketpair.as_fd()),
    ];
    // A kqueue cannot reach a forked child, so it is not an inheritance case.
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: initialized writable storage receives the process limit.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) }, 0);
    let limit = i32::try_from(limit.rlim_cur).expect("descriptor limit fits raw fd");
    for (kind, descriptor) in cases {
        let number = descriptor.as_raw_fd();
        assert!(number < listener::scan_bound().expect("scan bound"), "{kind} is scanned");
        let output = home.path().join("probe-result");
        std::fs::write(&output, "").expect("reset matrix output");
        let mut command =
            tokio::process::Command::new(std::env::current_exe().expect("test binary"));
        command
            .args(["--exact", "handoff::inherited_socket_probe", "--nocapture"])
            .env_clear()
            .env(PROBE, serde_json::json!({"address": "127.0.0.1:0", "output": output}).to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(home.path().join("stderr")).expect("matrix stderr"))
            .kill_on_drop(true);
        listener::inherit(&mut command, &descriptor);
        // SAFETY: only async-signal-safe flag queries/updates run after fork.
        // Other descriptors, including spawn's error pipe, remain usable until exec;
        // only the selected descriptor survives it alongside stdio.
        unsafe {
            command.pre_exec(move || {
                for candidate in 3..limit {
                    if candidate == number {
                        continue;
                    }
                    let flags = libc::fcntl(candidate, libc::F_GETFD);
                    if flags == -1 {
                        let error = std::io::Error::last_os_error();
                        if error.raw_os_error() != Some(libc::EBADF) {
                            return Err(error);
                        }
                    } else if libc::fcntl(candidate, libc::F_SETFD, flags | libc::FD_CLOEXEC) == -1
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let mut child = command.spawn().expect("matrix child");
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
        let cleanup = reap(&mut child).await;
        cleanup.expect("matrix child reaped");
        assert!(status.expect("bounded matrix child").expect("matrix exit").success(), "{kind}");
        let outcome = std::fs::read_to_string(&output).expect("matrix result");
        assert_eq!(outcome, "absent\n", "{kind}: {outcome}");
    }
}

#[tokio::test]
async fn the_bootstrap_git_child_does_not_inherit_the_retained_listener() {
    use std::os::unix::fs::PermissionsExt as _;

    let server = GitServer::start(false).await;
    let mut barrier = super::HandoffBarrier::new();
    let home = tempfile::tempdir().expect("git probe home");
    let bin = home.path().join("bin");
    std::fs::create_dir(&bin).expect("wrapper directory");
    let address_file = home.path().join("announced-address");
    let output = home.path().join("probe-result");
    let path = std::env::var_os("PATH").expect("PATH");
    let git = std::env::split_paths(&path)
        .map(|directory| directory.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("real git");
    let probe =
        record_probe_command(serde_json::json!({"address_file": address_file, "output": output}));
    let wrapper = bin.join("git");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n{probe}\nexec {} \"$@\"\n",
            shlex::try_quote(git.to_str().expect("git path")).expect("quote git")
        ),
    )
    .expect("test-owned git wrapper");
    std::fs::set_permissions(wrapper, std::fs::Permissions::from_mode(0o700))
        .expect("executable wrapper");
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&path)))
        .expect("wrapper PATH");
    let deadline = Instant::now() + Duration::from_secs(30);
    let (mut worker, worker_home) =
        super::handoff_worker(&server, &barrier, false, None, &[("PATH", path.as_os_str())]);
    let result: anyhow::Result<()> = async {
        super::bootstrap_worker(&mut worker, &server, deadline, Some(&address_file)).await?;
        barrier.arrived(deadline).await?;
        let probes = std::fs::read_to_string(&output)?;
        anyhow::ensure!(
            !probes.is_empty() && probes.lines().all(|line| line == "absent"),
            "bootstrap Git must not inherit a listening stream socket: {probes}"
        );
        Ok(())
    }
    .await;
    let cleanup = barrier.cleanup(&mut worker).await;
    server.close().await;
    super::assert_handoff(
        result,
        cleanup,
        super::HookCleanup::Gone,
        &mut worker,
        worker_home.path(),
    );
}

#[tokio::test]
async fn the_inherited_listener_is_not_passed_to_engine_children() {
    use std::os::unix::fs::PermissionsExt as _;

    let server = GitServer::start(false).await;
    let home = tempfile::tempdir().expect("serve home");
    let config = home.path().join("config");
    let bin = home.path().join("bin");
    std::fs::create_dir(&config).expect("trusted config");
    std::fs::create_dir(&bin).expect("wrapper directory");
    let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("known listener");
    let address = socket.local_addr().expect("listener address");
    let number = socket.as_raw_fd();
    let snapshot_probe = home.path().join("snapshot-result");
    let hook_probe = home.path().join("hook-result");
    let hook = probe_command(
        serde_json::json!({"number": number, "address": address, "output": hook_probe}),
    );
    std::fs::write(config.join("ganja.toml"), format!(
        "model = \"fake/handoff\"\n[[hooks.SessionStart]]\nhooks = [{{ type = \"command\", command = {hook:?}, timeout = 120 }}]\n"
    )).expect("trusted hook config");
    let path = std::env::var_os("PATH").expect("PATH");
    let git = std::env::split_paths(&path)
        .map(|directory| directory.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("real git");
    let wrapper = bin.join("git");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then {}\nfi\nexec {} \"$@\"\n",
            record_probe_command(
                serde_json::json!({"number": number, "address": address, "output": snapshot_probe})
            ),
            shlex::try_quote(git.to_str().expect("git path")).expect("quote git")
        ),
    )
    .expect("git wrapper");
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700))
        .expect("executable wrapper");
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&path)))
        .expect("wrapper PATH");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut command = listener::command(&socket, home.path(), &server.root.path().join("source"));
    command.env("PATH", path).stdout(Stdio::piped());
    let mut child = command.spawn().expect("inherited serve");
    drop(socket);
    let result: anyhow::Result<()> = async {
        let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
        let line = tokio::time::timeout_at(deadline, lines.next_line())
            .await??
            .ok_or_else(|| anyhow::anyhow!("serve exited before its address line"))?;
        anyhow::ensure!(
            line.starts_with("ganja server listening on http://"),
            "address line: {line}"
        );
        anyhow::ensure!(
            std::fs::read_to_string(&snapshot_probe)? == "absent\n",
            "snapshot Git inherited the listener"
        );
        anyhow::ensure!(
            std::fs::read_to_string(&hook_probe)? == "absent\n",
            "SessionStart inherited the listener"
        );
        Ok(())
    }
    .await;
    let cleanup = reap(&mut child).await;
    server.close().await;
    cleanup.expect("child reaped");
    result.expect("known listener is resealed before fake-provider snapshot assembly and hooks");
}

#[tokio::test]
async fn an_assembly_failure_closes_the_inherited_port_and_queued_connection() {
    let home = tempfile::tempdir().expect("serve home");
    std::fs::create_dir(home.path().join("config")).expect("config directory");
    std::fs::write(home.path().join("config/ganja.toml"), "unknown_configuration_key = true\n")
        .expect("invalid config");
    let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("listener");
    let address = socket.local_addr().expect("address");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut queued = tokio::time::timeout_at(deadline, tokio::net::TcpStream::connect(address))
        .await
        .expect("single connect deadline")
        .expect("connection queued before assembly");
    tokio::time::timeout_at(
        deadline,
        queued.write_all(
            b"GET /global/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        ),
    )
    .await
    .expect("write deadline")
    .expect("complete queued request");
    let mut child = listener::command(&socket, home.path(), home.path()).spawn().expect("serve");
    drop(socket);
    let result: anyhow::Result<()> = async {
        let status = tokio::time::timeout_at(deadline, child.wait()).await??;
        anyhow::ensure!(status.code() == Some(1), "ordinary assembly failure: {status}");
        let mut response = Vec::new();
        let read = tokio::time::timeout_at(deadline, queued.read_to_end(&mut response)).await?;
        anyhow::ensure!(response.is_empty(), "failed assembly must not fabricate an HTTP response");
        if let Err(error) = read {
            anyhow::ensure!(
                matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ),
                "unexpected queued read error: {error}"
            );
        }
        let connection =
            tokio::time::timeout_at(deadline, tokio::net::TcpStream::connect(address)).await?;
        anyhow::ensure!(
            connection.is_err(),
            "no inherited or replacement listener may remain after exit"
        );
        let diagnostics = std::fs::read_to_string(home.path().join("stderr"))?;
        anyhow::ensure!(
            diagnostics.contains("failed to read the configuration"),
            "assembly was reached: {diagnostics}"
        );
        Ok(())
    }
    .await;
    reap(&mut child).await.expect("failure child reaped");
    result.expect("assembly failure releases the socket without rebinding");
}

#[tokio::test]
async fn an_exec_failure_closes_the_worker_port_without_a_replacement_listener() {
    use std::os::unix::fs::PermissionsExt as _;

    let server = GitServer::start(false).await;
    let mut barrier = super::HandoffBarrier::new();
    let binary_home = tempfile::tempdir().expect("private binary directory");
    let binary = binary_home.path().join("ganja");
    std::fs::copy(assert_cmd::cargo::cargo_bin!("ganja"), &binary)
        .expect("private executable copy");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700))
        .expect("executable copy");
    let deadline = Instant::now() + Duration::from_secs(30);
    let (mut worker, home) = super::handoff_worker(&server, &barrier, false, Some(&binary), &[]);
    // The already-running initializer keeps executing, but its later exec is refused.
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o600))
        .expect("refuse replacement exec");
    let result: anyhow::Result<()> = async {
        let (port, _) = super::bootstrap_worker(&mut worker, &server, deadline, None).await?;
        let status = tokio::time::timeout_at(deadline, worker.wait()).await??;
        anyhow::ensure!(status.code() == Some(1), "ordinary exec failure: {status}");
        let connection = tokio::time::timeout_at(
            deadline,
            tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)),
        )
        .await?;
        anyhow::ensure!(connection.is_err(), "failed exec leaves no second or retained listener");
        let diagnostics = std::fs::read_to_string(home.path().join("stderr"))?;
        anyhow::ensure!(
            diagnostics.contains("failed to replace worker init with ganja serve"),
            "the exec itself failed: {diagnostics}"
        );
        Ok(())
    }
    .await;
    let cleanup = barrier.cleanup(&mut worker).await;
    server.close().await;
    super::assert_handoff(
        result,
        cleanup,
        super::HookCleanup::NoHookObserved,
        &mut worker,
        home.path(),
    );
}
