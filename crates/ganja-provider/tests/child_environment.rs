//! The provider's real process spawner keeps server credentials in the parent.

#![cfg(unix)]

use std::ffi::OsString;
use std::path::Path;

use ganja_provider::provider::claude_code::argv::ChildEnv;
use ganja_provider::provider::claude_code::process::{Real, Spawner as _};
use tokio::io::AsyncReadExt as _;

#[tokio::test]
async fn provider_children_do_not_receive_server_credentials() {
    const TEST: &str = "provider_children_do_not_receive_server_credentials";
    const CHILD: &str = "GANJA_TEST_PROVIDER_CHILD_ENVIRONMENT";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .env("GANJA_SERVER_PASSWORD", "synthetic-server-password")
            .env("GANJA_SERVER_USERNAME", "synthetic-server-username")
            .env("GANJA_CLOUD_PASSWORD", "synthetic-cloud-password")
            .env("GANJA_TEST_CHILD_VISIBLE", "retained")
            .output()
            .expect("isolated test process");
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        return;
    }

    let home = tempfile::tempdir().expect("child working directory");
    let argv: Vec<OsString> = [
        "-c",
        "printf '%s|%s|%s|%s' \"${GANJA_SERVER_PASSWORD+present}\" \"${GANJA_SERVER_USERNAME+present}\" \"${GANJA_CLOUD_PASSWORD+present}\" \"$GANJA_TEST_CHILD_VISIBLE\"",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    let mut child = Real
        .spawn(Path::new("/bin/sh"), &argv, &ChildEnv { cwd: home.path().to_path_buf() })
        .expect("real process spawn");
    drop(child.stdin);
    let mut observed = String::new();
    child.stdout.read_to_string(&mut observed).await.expect("child environment observation");
    assert!(child.exit.await.expect("child exits").success());
    assert_eq!(observed, "|||retained");
}
