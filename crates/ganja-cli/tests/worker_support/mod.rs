use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::routing::get;
use tempfile::TempDir;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const PROVIDER_KEY: &str = "synthetic-worker-provider-key";

#[derive(Clone)]
struct Files {
    directory: PathBuf,
    authorized: Arc<Mutex<Vec<bool>>>,
    stalled: bool,
    shutdown: CancellationToken,
}

pub struct GitServer {
    pub root: TempDir,
    pub url: String,
    pub ca: PathBuf,
    pub gitconfig: PathBuf,
    pub authorized: Arc<Mutex<Vec<bool>>>,
    handle: axum_server::Handle<std::net::SocketAddr>,
    task: Option<JoinHandle<()>>,
    shutdown: CancellationToken,
}

impl GitServer {
    pub async fn start(stalled: bool) -> Self {
        let root = tempfile::tempdir().expect("git fixture root");
        let source = root.path().join("source");
        let public = root.path().join("public");
        std::fs::create_dir_all(&source).expect("source directory");
        std::fs::create_dir_all(&public).expect("public directory");
        git(&source, &["init", "--initial-branch=main"]);
        std::fs::write(
            source.join("AGENTS.md"),
            "The project marker is WORKER_INSTRUCTIONS_CANARY.\n",
        )
        .expect("instructions");
        std::fs::write(source.join("note.txt"), "before\n").expect("file");
        std::fs::write(source.join("ganja.toml"), "[permission]\n\"*\" = \"allow\"\n")
            .expect("tool policy");
        git(&source, &["add", "."]);
        git(
            &source,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "Initial fixture",
            ],
        );
        let bare = public.join("repository.git");
        git(
            root.path(),
            &[
                "clone",
                "--bare",
                source.to_str().expect("source path"),
                bare.to_str().expect("bare path"),
            ],
        );
        git(&bare, &["update-server-info"]);

        let certified = rcgen::generate_simple_self_signed(vec![
            "127.0.0.1".to_owned(),
            "localhost".to_owned(),
        ])
        .expect("test certificate");
        let certificate = certified.cert.pem();
        let key = certified.signing_key.serialize_pem();
        let ca = root.path().join("ca.pem");
        std::fs::write(&ca, &certificate).expect("test CA file");
        let gitconfig = root.path().join("gitconfig");
        std::fs::write(&gitconfig, "[commit]\ngpgsign = false\n").expect("isolated git config");
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
            certificate.into_bytes(),
            key.into_bytes(),
        )
        .await
        .expect("TLS configuration");
        let authorized = Arc::new(Mutex::new(Vec::new()));
        let shutdown = CancellationToken::new();
        let state = Files {
            directory: public,
            authorized: Arc::clone(&authorized),
            stalled,
            shutdown: shutdown.clone(),
        };
        let app = axum::Router::new().route("/{*path}", get(read_file)).with_state(state);
        let handle = axum_server::Handle::new();
        let serving = handle.clone();
        let task = tokio::spawn(async move {
            axum_server::bind_rustls(([127, 0, 0, 1], 0).into(), tls)
                .handle(serving)
                .serve(app.into_make_service())
                .await
                .expect("git TLS fixture serves");
        });
        let address = tokio::time::timeout(Duration::from_secs(5), handle.listening())
            .await
            .expect("fixture starts")
            .expect("fixture binds");
        Self {
            root,
            url: format!("https://{address}/repository.git"),
            ca,
            gitconfig,
            authorized,
            handle,
            task: Some(task),
            shutdown,
        }
    }

    pub async fn close(mut self) {
        self.shutdown.cancel();
        self.handle.shutdown();
        if let Some(task) = self.task.take() {
            task.await.expect("TLS fixture task");
        }
    }
}

impl Drop for GitServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.handle.shutdown();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn read_file(
    State(state): State<Files>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response<Body> {
    let authorized =
        headers.get("authorization").and_then(|value| value.to_str().ok()) == Some("placeholder");
    state.authorized.lock().expect("request log").push(authorized);
    if !authorized {
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Body::empty())
            .expect("response");
    }
    if state.stalled {
        state.shutdown.cancelled().await;
    }
    let relative = Path::new(uri.path().trim_start_matches('/'));
    if relative.components().any(|component| !matches!(component, Component::Normal(_))) {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::empty())
            .expect("response");
    }
    match tokio::fs::read(state.directory.join(relative)).await {
        Ok(contents) => Response::builder()
            .header("content-type", "application/octet-stream")
            .body(Body::from(contents))
            .expect("response"),
        Err(_) => {
            Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty()).expect("response")
        }
    }
}

fn announced_ports(output: &str) -> impl Iterator<Item = u16> + '_ {
    output
        .lines()
        .filter_map(|line| line.strip_prefix("ganja server listening on http://"))
        .filter_map(|address| address.parse::<std::net::SocketAddr>().ok())
        .map(|address| address.port())
}

fn worker_stderr(state: &Path) -> String {
    std::fs::read_to_string(state.join("stderr"))
        .expect("stderr log")
        .replace("synthetic-worker-password", "[redacted]")
}

pub struct WorkerProcess {
    pub client: ganja_client::Client,
    pub address: String,
    pub work: PathBuf,
    child: std::process::Child,
    _state: TempDir,
    _provider: ganja_testkit::responses_server::Endpoint,
}

impl WorkerProcess {
    pub async fn start(git: &GitServer) -> Self {
        Self::start_with_environment(git, &[]).await
    }

    pub async fn start_with_environment(git: &GitServer, environment: &[(&str, &str)]) -> Self {
        let state = tempfile::tempdir().expect("worker process state");
        let work = state.path().join("work");
        let config = state.path().join(".config/ganja");
        std::fs::create_dir_all(&work).expect("work directory");
        std::fs::create_dir_all(&config).expect("config directory");
        let provider = ganja_testkit::responses_server::serve().await;
        let calls = serde_json::json!({
            "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [
                {"index": 0, "id": "call_read", "type": "function", "function": {
                    "name": "read", "arguments": "{\"filePath\":\"note.txt\"}"
                }},
                {"index": 1, "id": "call_write", "type": "function", "function": {
                    "name": "write", "arguments": "{\"filePath\":\"note.txt\",\"content\":\"after\\n\"}"
                }}
            ]}, "finish_reason": "tool_calls"}]
        });
        let finished = serde_json::json!({
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": "finished"},
                "finish_reason": "stop"}]
        });
        provider.answers_the_next_requests_with([format!("data: {calls}\n\ndata: [DONE]\n\n")]);
        provider.answers_turns_with(format!("data: {finished}\n\ndata: [DONE]\n\n"));
        // The worker passes no provider key variable across its environment
        // boundary, so the endpoint reads a distinct key from its isolated store.
        let data = state.path().join("data/ganja");
        std::fs::create_dir_all(&data).expect("credential directory");
        let mut credentials = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(data.join("auth.json"))
            .expect("credential store");
        credentials
            .write_all(
                serde_json::json!({"worker-fixture": {"type": "api", "key": PROVIDER_KEY}})
                    .to_string()
                    .as_bytes(),
            )
            .expect("provider credential");
        std::fs::write(
            config.join("ganja.toml"),
            format!(
                "model = \"worker-fixture/test\"\n\
                 [provider.worker-fixture]\n\
                 dialect = \"openai-chat-completions\"\n\
                 base_url = {:?}\n\
                 [permission]\n\"*\" = \"allow\"\n",
                provider.base_url
            ),
        )
        .expect("worker policy");
        let stdout = std::fs::File::create(state.path().join("stdout")).expect("stdout file");
        let stderr = std::fs::File::create(state.path().join("stderr")).expect("stderr file");
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("ganja"));
        command
            .args(["worker", "init", "--port", "0"])
            .current_dir(&work)
            .env("HOME", state.path())
            .env("XDG_DATA_HOME", state.path().join("data"))
            .env("XDG_CONFIG_HOME", &config)
            .env("GANJA_CONFIG_HOME", &config)
            .env("GANJA_DISABLE_MODELS_FETCH", "1")
            .env("GANJA_DISABLE_TERM_PROBE", "1")
            .env("GANJA_SERVER_USERNAME", "ganja")
            .env("GANJA_SERVER_PASSWORD", "synthetic-worker-password")
            .env("GIT_CONFIG_GLOBAL", &git.gitconfig)
            .env("GIT_SSL_CAINFO", &git.ca)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        command.envs(environment.iter().copied());
        let mut child = command.spawn().expect("spawn worker init");
        let announced = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(status) = child.try_wait().expect("child state") {
                    return Err(status);
                }
                let output =
                    std::fs::read_to_string(state.path().join("stdout")).expect("stdout log");
                if let Some(port) = announced_ports(&output).next() {
                    return Ok(port);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        let port = match announced {
            Ok(Ok(port)) => port,
            Ok(Err(status)) => panic!(
                "worker init exited before listening ({status}); stderr: {}",
                worker_stderr(state.path()),
            ),
            Err(_) => {
                let _ = child.kill();
                let status = child.wait().expect("reap initializer");
                panic!(
                    "worker init did not announce its address ({status}); stderr: {}",
                    worker_stderr(state.path())
                );
            }
        };
        let address = format!("http://127.0.0.1:{port}");
        let client = ganja_client::Client::new(
            &address,
            Some(ganja_client::Credentials::new("ganja", "synthetic-worker-password")),
        )
        .expect("client");
        let mut worker = Self { client, address, work, child, _state: state, _provider: provider };
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if matches!(
                    worker.client.bootstrap_status().await,
                    Ok(ganja_client::BootstrapStatus::Idle)
                ) {
                    break;
                }
                if let Some(status) = worker.child.try_wait().expect("child state") {
                    panic!(
                        "worker init exited before listening ({status}); stderr: {}",
                        worker_stderr(worker._state.path())
                    );
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("init listener ready");
        worker
    }

    /// Ports announced by the initializer and its replacement server.
    pub fn announced_ports(&self) -> Vec<u16> {
        let output =
            std::fs::read_to_string(self._state.path().join("stdout")).expect("stdout log");
        announced_ports(&output).collect()
    }

    /// Proves the provider received its own key rather than server authentication.
    pub fn assert_provider_key(&self) {
        let requests = self._provider.seen();
        assert!(!requests.is_empty(), "the worker must call the provider");
        for request in requests {
            let authorization = request.header("authorization");
            assert_ne!(authorization.as_deref(), Some("Bearer synthetic-worker-password"));
            assert_eq!(authorization, Some(format!("Bearer {PROVIDER_KEY}")));
        }
    }

    /// Captured server errors with the synthetic server password removed.
    pub fn stderr(&self) -> String {
        worker_stderr(self._state.path())
    }

    pub fn is_alive(&mut self) -> bool {
        self.child.try_wait().expect("child state").is_none()
    }

    fn signal_shutdown(&mut self) {
        if self.is_alive() {
            let pid = i32::try_from(self.child.id()).expect("child pid fits");
            // SAFETY: this is the live child owned by this handle; the test never
            // signals a pid obtained from a name search or from persisted state.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }

    pub async fn terminate(&mut self) {
        self.signal_shutdown();
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.is_alive() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker killed and reaped its active git group");
    }
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        self.signal_shutdown();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while self.is_alive() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn git(directory: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(directory)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("real git installed");
    assert!(status.success(), "git fixture setup failed");
}
