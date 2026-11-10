use super::*;

async fn git(
    directory: &Path,
    arguments: &[&str],
    deadline: Instant,
    stage: Stage,
) -> Result<Vec<u8>, Failure> {
    super::git(directory, arguments, deadline, stage, &GitOptions::default()).await
}

fn checkout(repository: &str) -> Checkout {
    Checkout {
        repository: repository.to_owned(),
        reference: "main".to_owned(),
        branch: "workers/test".to_owned(),
        author_name: "Worker".to_owned(),
        author_email: "worker@example.invalid".to_owned(),
        git: GitOptions::default(),
    }
}

#[tokio::test]
async fn credentials_and_non_https_repositories_are_refused_before_git() {
    for repository in [
        "http://example.invalid/repo.git",
        "https://token@example.invalid/repo.git",
        "https://example.invalid/repo.git?token=secret",
        "https://example.invalid/repo.git#secret",
        "file:///tmp/repo",
    ] {
        let root = tempfile::tempdir().expect("worker root");
        let result = prepare(
            root.path().to_owned(),
            checkout(repository),
            Instant::now() + Duration::from_secs(5),
        )
        .await;
        assert_eq!(result.expect_err("refused").stage, Stage::Dispatch);
        assert_eq!(std::fs::read_dir(root.path()).expect("read root").count(), 0);
    }
}

#[tokio::test]
async fn repositories_with_ambiguous_original_paths_are_refused_before_git() {
    let cases = [
        ("https://example.invalid/repo.git", true),
        ("https://EXAMPLE.invalid/a/repo.git", true),
        ("https://127.0.0.1/repo.git", true),
        ("https://[::1]/repo.git", false),
        ("https://example.invalid/a/./repo.git", false),
        ("https://example.invalid/a/../repo.git", false),
        ("https://example.invalid/a/%2e/repo.git", false),
        ("https://example.invalid/a/%2E%2e/repo.git", false),
        ("https://example.invalid/a/.%2E/repo.git", false),
        ("https://example.invalid/a/%2e./repo.git", false),
        ("https://example.invalid/a\\repo.git", false),
        ("https://example.invalid/a/\nrepo.git", false),
        ("https://example.invalid/a/%2e%2e", false),
        ("https://example.invalid/.git", true),
        ("https://example.invalid/a..b/repo.git", true),
    ];
    for (repository, accepted) in cases {
        let root = tempfile::tempdir().expect("worker root");
        let failure = prepare(root.path().to_owned(), checkout(repository), Instant::now())
            .await
            .expect_err("expired deadline or invalid repository");
        assert_eq!(
            failure.stage,
            if accepted { Stage::Deadline } else { Stage::Dispatch },
            "{repository}"
        );
    }
}

#[tokio::test]
async fn an_existing_worker_file_is_not_deleted_or_overwritten() {
    let root = tempfile::tempdir().expect("worker root");
    std::fs::write(root.path().join("keep"), "owned by another process").expect("file");
    let result = prepare(
        root.path().to_owned(),
        checkout("https://example.invalid/repo.git"),
        Instant::now() + Duration::from_secs(5),
    )
    .await;
    assert_eq!(result.expect_err("nonempty root").stage, Stage::Dispatch);
    assert_eq!(
        std::fs::read_to_string(root.path().join("keep")).expect("kept"),
        "owned by another process"
    );
}

#[tokio::test]
async fn an_expired_budget_never_starts_git() {
    let root = tempfile::tempdir().expect("worker root");
    let result = git(root.path(), &["--version"], Instant::now(), Stage::Clone).await;
    assert_eq!(result.expect_err("expired").stage, Stage::Deadline);
}

#[tokio::test]
async fn a_never_completing_git_child_is_killed_under_the_deadline() {
    let root = tempfile::tempdir().expect("worker root");
    let result = git(
        root.path(),
        &["-c", "alias.hang=!sleep 60", "hang"],
        Instant::now() + Duration::from_millis(100),
        Stage::Clone,
    )
    .await;
    assert_eq!(result.expect_err("deadline").stage, Stage::Deadline);
}

#[tokio::test]
async fn failure_debug_reports_only_the_stage() {
    let root = tempfile::tempdir().expect("git directory");
    let secret = "synthetic-secret-detail";
    let failure =
        git(root.path(), &[secret], Instant::now() + Duration::from_secs(5), Stage::Clone)
            .await
            .expect_err("unknown command fails");
    let debug = format!("{failure:?}");
    assert!(!debug.contains(secret));
    assert_eq!(debug, "Clone");
}

#[tokio::test]
async fn git_children_do_not_inherit_unlisted_environment() {
    const CHILD: &str = "GANJA_WORKER_ENV_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "worker_git::tests::git_children_do_not_inherit_unlisted_environment",
            ])
            .env(CHILD, "1")
            .env("GANJA_WORKER_ENV_CANARY", "synthetic-inherited-secret")
            .output()
            .expect("run isolated environment test");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
        return;
    }
    let root = tempfile::tempdir().expect("git directory");
    let output = git(
        root.path(),
        &["-c", "alias.inspect=!printf '%s' \"${GANJA_WORKER_ENV_CANARY-unset}\"", "inspect"],
        Instant::now() + Duration::from_secs(5),
        Stage::Config,
    )
    .await
    .expect("git alias");
    assert_eq!(output, b"unset");
}

#[tokio::test]
async fn git_children_receive_neither_server_auth_nor_unlisted_settings() {
    const CHILD: &str = "GANJA_GIT_ENV_TEST_CHILD";
    const BLOCKED: [&str; 8] = [
        "GANJA_SERVER_PASSWORD",
        "GANJA_SERVER_USERNAME",
        "GIT_SSL_NO_VERIFY",
        "HTTPS_PROXY",
        "ANTHROPIC_API_KEY",
        "GANJA_CONFIG",
        "GANJA_WORKER_ENV_CANARY",
        "GANJA_PROVIDER",
    ];
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "worker_git::tests::git_children_receive_neither_server_auth_nor_unlisted_settings",
            ])
            .env(CHILD, "1")
            .envs(BLOCKED.map(|name| (name, "synthetic-worker-setting")))
            .output()
            .expect("isolated environment test");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
        return;
    }
    let root = tempfile::tempdir().expect("git directory");
    let output = git(
        root.path(),
        &["-c", "alias.inspect=!env", "inspect"],
        Instant::now() + Duration::from_secs(5),
        Stage::Config,
    )
    .await
    .expect("git environment probe");
    let output = String::from_utf8(output).expect("environment UTF-8");
    for name in BLOCKED {
        assert!(
            !output.lines().any(|line| line.starts_with(&format!("{name}="))),
            "git inherited {name}"
        );
    }
}

#[tokio::test]
async fn git_output_is_bounded_and_command_failures_are_typed() {
    let root = tempfile::tempdir().expect("worker root");
    let version =
        git(root.path(), &["--version"], Instant::now() + Duration::from_secs(5), Stage::Config)
            .await
            .expect("real git");
    assert!(version.starts_with(b"git version "));
    let failure = git(
        root.path(),
        &["not-a-real-git-command"],
        Instant::now() + Duration::from_secs(5),
        Stage::Clone,
    )
    .await
    .expect_err("git refusal");
    assert_eq!(failure.stage, Stage::Clone);
}
