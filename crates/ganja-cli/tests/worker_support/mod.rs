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
