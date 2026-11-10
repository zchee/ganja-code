use std::os::unix::fs::PermissionsExt as _;

use super::*;

fn executable(directory: &std::path::Path, body: &str) -> PathBuf {
    let path = directory.join("proxy-fixture");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write proxy fixture");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
        .expect("make fixture executable");
    path
}

#[tokio::test]
async fn proxy_readiness_needs_no_actor_connection_and_cancellation_reaps_the_child() {
    let directory = tempfile::tempdir().expect("temporary fixture directory");
    let binary = executable(
        directory.path(),
        "printf 'Proxying 127.0.0.1:12345 -> ganja/worker:80\\n' >&2\nexec sleep 60",
    );
    let actor = ObjectRef { atespace: "ganja".to_owned(), name: "worker".to_owned() };
    let mut proxy = start_proxy(
        &binary,
        &directory.path().join("operator-config"),
        None,
        &actor,
        Duration::from_secs(2),
    )
    .await
    .expect("read readiness without connecting");
    assert_eq!(proxy.address, "127.0.0.1:12345".parse::<SocketAddr>().expect("loopback"));
    proxy.cancel.cancel();
    tokio::time::timeout(Duration::from_secs(2), &mut proxy.task)
        .await
        .expect("bounded reaping")
        .expect("join child supervisor");
}

#[tokio::test]
async fn a_held_startup_does_not_block_other_actors_and_same_actor_callers_share_a_child() {
    let directory = tempfile::tempdir().expect("temporary fixture directory");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("listener address");
    let started = directory.path().join("started");
    let release = directory.path().join("release");
    let binary = executable(
        directory.path(),
        &format!(
            "case \"$*\" in\n*held*)\nprintf 'started\\n' >> '{}'\nwhile [ ! -f '{}' ]; do sleep 0.01; done\nname=held;;\n*) name=ready;;\nesac\nprintf 'Proxying {address} -> ganja/%s:80\\n' \"$name\" >&2\nexec sleep 60",
            started.display(),
            release.display(),
        ),
    );
    let tunnel = std::sync::Arc::new(KubectlTunnel {
        channel: tonic::transport::Endpoint::from_static("http://127.0.0.1:1").connect_lazy(),
        executable: binary,
        kubeconfig: directory.path().join("operator-config"),
        context: None,
        proxies: Mutex::new(BTreeMap::new()),
    });
    let ready = ObjectRef { atespace: "ganja".to_owned(), name: "ready".to_owned() };
    tunnel.actor(&ready).await.expect("establish ready proxy");
    let held = ObjectRef { atespace: "ganja".to_owned(), name: "held".to_owned() };
    let first = {
        let tunnel = tunnel.clone();
        let held = held.clone();
        tokio::spawn(async move { tunnel.actor(&held).await })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("held child started");
    let second = {
        let tunnel = tunnel.clone();
        tokio::spawn(async move { tunnel.actor(&held).await })
    };
    let ready_result = tokio::time::timeout(Duration::from_millis(200), tunnel.actor(&ready)).await;
    // Release even on regression so the fixture leaves no waiting child behind.
    std::fs::write(&release, b"release").expect("release held startup");
    ready_result.expect("other actors must not wait for held startup").expect("ready proxy");
    first.await.expect("first caller").expect("first proxy");
    second.await.expect("second caller").expect("shared proxy");
    assert_eq!(std::fs::read_to_string(started).expect("startup count"), "started\n");
}

#[tokio::test]
async fn retiring_a_proxy_reaps_its_child_removes_the_entry_and_allows_a_fresh_start() {
    const CHILD_ENV: &str = "GANJA_TEST_KUBECTL_LISTENER";
    if std::env::var_os(CHILD_ENV).is_some() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("child listener");
        eprintln!("Proxying {} -> ganja/worker:80", listener.local_addr().expect("address"));
        loop {
            let _ = listener.accept().await.expect("proxy connection");
        }
    }
    let directory = tempfile::tempdir().expect("temporary fixture directory");
    let pid_file = directory.path().join("pid");
    let starts = directory.path().join("starts");
    let binary = executable(
        directory.path(),
        &format!(
            "printf '%s' \"$$\" > '{}'\nprintf 'start\\n' >> '{}'\n{CHILD_ENV}=1 exec '{}' --exact provisioner::substrate::kubectl::tests::retiring_a_proxy_reaps_its_child_removes_the_entry_and_allows_a_fresh_start --nocapture",
            pid_file.display(),
            starts.display(),
            std::env::current_exe().expect("test executable").display(),
        ),
    );
    let tunnel = KubectlTunnel {
        channel: tonic::transport::Endpoint::from_static("http://127.0.0.1:1").connect_lazy(),
        executable: binary,
        kubeconfig: directory.path().join("operator-config"),
        context: None,
        proxies: Mutex::new(BTreeMap::new()),
    };
    let actor = ObjectRef { atespace: "ganja".to_owned(), name: "worker".to_owned() };
    tunnel.actor(&actor).await.expect("first proxy");
    let entry = Arc::clone(tunnel.proxies.lock().await.get("ganja/worker").expect("proxy entry"));
    let address = entry.child.lock().await.as_ref().expect("child").address;
    let pid = std::fs::read_to_string(&pid_file).expect("child PID");
    tunnel.retire(&actor).await;
    assert!(TcpStream::connect(address).await.is_err(), "child listener closed");
    assert!(tunnel.proxies.lock().await.is_empty());
    assert!(
        !std::process::Command::new("kill")
            .args(["-0", &pid])
            .stderr(Stdio::null())
            .status()
            .expect("check retired PID")
            .success()
    );
    tunnel.retire(&actor).await;
    tunnel.actor(&actor).await.expect("fresh proxy");
    assert_eq!(std::fs::read_to_string(&starts).expect("starts"), "start\nstart\n");
    tunnel.retire(&actor).await;
}

#[tokio::test]
async fn proxy_child_receives_only_operator_paths_and_plugin_flags() {
    const MODE: &str = "GANJA_TEST_PROXY_ENV";
    let Ok(mode) = std::env::var(MODE) else {
        for mode in ["record", "PATH", "HOME", "relative PATH"] {
            let mut child =
                std::process::Command::new(std::env::current_exe().expect("test executable"));
            child.env_clear()
                .env(MODE, mode)
                .env("PATH", ".::relative:/trusted/plugin/bin:")
                .env("HOME", "/trusted/home")
                .env("GANJA_CLOUD_PASSWORD", "synthetic-password-canary")
                .env("ANTHROPIC_API_KEY", "synthetic-provider-canary")
                .env("OPENAI_API_KEY", "synthetic-provider-canary")
                .env("GOOGLE_API_KEY", "synthetic-provider-canary")
                .env("HTTP_PROXY", "synthetic-proxy-canary")
                .env("KUBECONFIG", "/ambient/config")
                .args(["--exact", "provisioner::substrate::kubectl::tests::proxy_child_receives_only_operator_paths_and_plugin_flags", "--nocapture"]);
            if mode == "relative PATH" {
                child.env("PATH", ".::relative");
            } else if mode != "record" {
                child.env_remove(mode);
            }
            let output = child.output().expect("isolated environment fixture");
            assert!(output.status.success(), "{mode}: {}", String::from_utf8_lossy(&output.stderr));
        }
        return;
    };
    let directory = tempfile::tempdir().expect("fixture directory");
    let args = directory.path().join("args");
    let env = directory.path().join("env");
    let cwd = directory.path().join("cwd");
    let binary = executable(
        directory.path(),
        &format!(
            "printf '%s\\n' \"$@\" > '{}'\npwd > '{}'\nunset PWD SHLVL _\n/usr/bin/env > '{}'\nprintf 'Proxying 127.0.0.1:12345 -> ganja/worker:80\\n' >&2\nexec /bin/sleep 60",
            args.display(),
            cwd.display(),
            env.display(),
        ),
    );
    let kubeconfig = directory.path().join("operator-config");
    let actor = ObjectRef { atespace: "ganja".to_owned(), name: "worker".to_owned() };
    if mode == "record" {
        let command =
            proxy_command(&binary, &kubeconfig, Some("chosen-context"), &actor).expect("command");
        let names = command
            .as_std()
            .get_envs()
            .map(|(name, _)| name.to_str().expect("environment name"))
            .collect::<Vec<_>>();
        assert_eq!(names, ["HOME", "KUBECONFIG", "PATH"]);
    }
    let result =
        start_proxy(&binary, &kubeconfig, Some("chosen-context"), &actor, Duration::from_secs(2))
            .await;
    if mode != "record" {
        let Err(error) = result else { panic!("missing {mode} must fail") };
        assert_eq!(error.operation, if mode == "relative PATH" { "PATH" } else { &mode });
        assert_eq!(error.kind, ProvisionErrorKind::Refused);
        return;
    }
    let mut proxy = result.expect("record command");
    proxy.cancel.cancel();
    (&mut proxy.task).await.expect("reap fixture");
    let environment = std::fs::read_to_string(env).expect("recorded environment");
    assert_eq!(std::fs::read_to_string(cwd).expect("recorded working directory").trim(), "/");
    // The shell sets `_`, and macOS injects its encoding variable at exec.
    // Neither comes from the command's explicit environment checked above.
    let mut names = environment
        .lines()
        .map(|line| line.split_once('=').expect("env entry").0)
        .filter(|name| !matches!(*name, "_" | "__CF_USER_TEXT_ENCODING"))
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(names, ["HOME", "KUBECONFIG", "PATH"]);
    assert!(environment.contains("HOME=/trusted/home\n"));
    assert!(environment.contains("PATH=/trusted/plugin/bin\n"));
    assert!(environment.contains(&format!("KUBECONFIG={}\n", kubeconfig.display())));
    for canary in [
        "synthetic-password-canary",
        "synthetic-provider-canary",
        "synthetic-proxy-canary",
        "/ambient/config",
    ] {
        assert!(!environment.contains(canary), "ambient value escaped");
    }
    let actual = std::fs::read_to_string(args).expect("recorded arguments");
    assert_eq!(
        actual.lines().collect::<Vec<_>>(),
        [
            "ate",
            "proxy",
            "actor",
            "worker",
            "0:80",
            "--atespace=ganja",
            &format!("--kubeconfig={}", kubeconfig.display()),
            "--context=chosen-context",
            "--address=127.0.0.1"
        ]
    );
}

#[tokio::test]
async fn proxy_startup_errors_discard_stderr_and_return_within_the_budget() {
    let directory = tempfile::tempdir().expect("temporary fixture directory");
    let actor = ObjectRef { atespace: "ganja".to_owned(), name: "worker".to_owned() };
    for body in ["printf 'synthetic-private-key-canary\\n' >&2\nexit 3", "exec sleep 60"] {
        let binary = executable(directory.path(), body);
        let result = start_proxy(
            &binary,
            &directory.path().join("operator-config"),
            None,
            &actor,
            Duration::from_millis(100),
        )
        .await;
        let Err(error) = result else { panic!("unready proxy must fail") };
        assert!(!format!("{error:?} {error}").contains("synthetic-private-key-canary"));
    }
}
