//! The real git process-group executor is exercised here through its sibling
//! unit tests. This binary also owns the worker-init HTTP/exec integration:
//! git credentials, the projected CA, checkout cleanup, author identity and
//! project semantics belong to the CLI that runs git, not to the cloud adapter.

#![cfg(unix)]

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

#[tokio::test]
async fn the_initializer_and_replacement_server_announce_the_same_ephemeral_port() {
    let server = GitServer::start(false).await;
    let worker = worker_support::WorkerProcess::start(&server).await;
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
    worker.client.health().await.expect("same client reaches replacement server");
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
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if requested != 0 {
            drop(reservation.take());
        }
        let mut child = command.spawn().expect("worker init");
        let stdout = child.stdout.take().expect("stdout");
        let mut lines = BufReader::new(stdout).lines();
        let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .expect("initializer announces its bound address")
            .expect("address line")
            .expect("stdout stays open");
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
        client.health().await.expect("same port reaches the served engine");
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
