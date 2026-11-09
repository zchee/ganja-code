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
