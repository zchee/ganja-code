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
