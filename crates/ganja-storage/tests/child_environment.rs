//! Snapshot git processes exclude parent-only credentials.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

use ganja_permission::Project;
use ganja_storage::snapshot::Snapshots;

#[tokio::test]
async fn snapshot_git_children_do_not_receive_server_credentials() {
    const TEST: &str = "snapshot_git_children_do_not_receive_server_credentials";
    const CHILD: &str = "GANJA_TEST_SNAPSHOT_CHILD";
    if std::env::var(CHILD).as_deref() != Ok(TEST) {
        let fixture = tempfile::tempdir().expect("snapshot fixture");
        let tools = fixture.path().join("tools");
        let root = fixture.path().join("project");
        std::fs::create_dir_all(&tools).expect("tools directory");
        std::fs::create_dir_all(&root).expect("project directory");
        let path = std::env::var_os("PATH").expect("PATH is set");
        let git = std::env::split_paths(&path)
            .map(|directory| directory.join("git"))
            .find(|candidate| candidate.is_file())
            .expect("git is installed");
        assert!(Command::new(&git).arg("init").arg(&root).status().expect("git init").success());
        std::fs::write(root.join("tracked.txt"), "snapshot contents\n").expect("tracked file");
        let fake = tools.join("git");
        std::fs::write(
            &fake,
            "#!/bin/sh\nprintf '%s|%s|%s|%s|%s\\n' \"$*\" \"${GANJA_SERVER_PASSWORD+present}\" \"${GANJA_SERVER_USERNAME+present}\" \"${GANJA_CLOUD_PASSWORD+present}\" \"$GANJA_TEST_CHILD_VISIBLE\" >> \"$GANJA_TEST_GIT_RECORD\"\nexec \"$GANJA_TEST_REAL_GIT\" \"$@\"\n",
        )
        .expect("git wrapper");
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))
            .expect("executable git wrapper");
        let mut paths = vec![tools];
        paths.extend(std::env::split_paths(&path));
        let output = Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, TEST)
            .env("PATH", std::env::join_paths(paths).expect("fixture PATH"))
            .env("XDG_DATA_HOME", fixture.path().join("data"))
            .env("GANJA_TEST_PROJECT", root)
            .env("GANJA_TEST_REAL_GIT", git)
            .env("GANJA_TEST_GIT_RECORD", fixture.path().join("observed"))
            .env("GANJA_TEST_CHILD_VISIBLE", "retained")
            .env("GANJA_SERVER_PASSWORD", "synthetic-server-password")
            .env("GANJA_SERVER_USERNAME", "synthetic-server-username")
            .env("GANJA_CLOUD_PASSWORD", "synthetic-cloud-password")
            .output()
            .expect("isolated snapshot test");
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let root = std::env::var_os("GANJA_TEST_PROJECT").expect("project path");
    let project = Project::resolve(std::path::Path::new(&root));
    let snapshots = Snapshots::new(&project, true);
    assert!(snapshots.enabled(), "snapshot probe must succeed");
    assert!(snapshots.track().await.is_some(), "snapshot operation must succeed");
    let observed =
        std::fs::read_to_string(std::env::var_os("GANJA_TEST_GIT_RECORD").expect("record path"))
            .expect("git invocations recorded");
    assert!(observed.lines().any(|line| line.starts_with("--version|")), "{observed}");
    assert!(observed.lines().any(|line| line.contains("write-tree|")), "{observed}");
    for line in observed.lines() {
        assert!(line.ends_with("||||retained"), "private environment reached git: {line}");
    }
}
