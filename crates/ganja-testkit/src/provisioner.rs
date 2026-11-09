//! Scriptable cloud control plane. Delayed router wakes are independent tasks:
//! a later response, including an owner-triggered suspend, never joins them.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
/// Configuration of the real served engine used by the cloud fixture.
pub use ganja_serve::ServeConfig;
/// Bootstrap stage used by scripted initializer failures.
pub use ganja_serve::bootstrap::Stage as BootstrapStage;
use ganja_teammate_cloud::provisioner::{
    Endpoint, ProvisionError, ProvisionErrorKind, Provisioner, Resumed, Worker, WorkerHandle,
    WorkerSpec, WorkerState,
};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// One control-plane operation, recorded independently from worker HTTP requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProvisionOperation {
    /// Allocate a suspended worker.
    Spawn,
    /// Resume and wait for readiness.
    Resume,
    /// Request suspension.
    Suspend,
    /// Read authoritative state.
    State,
    /// Delete in any state.
    Kill,
}

#[derive(Debug)]
enum Step {
    Fail(ProvisionErrorKind),
    Wait(Arc<Notify>),
}

#[derive(Debug, Default)]
struct Control {
    workers: BTreeMap<String, WorkerState>,
    used: BTreeSet<String>,
    calls: Vec<(ProvisionOperation, String)>,
    scripts: BTreeMap<ProvisionOperation, VecDeque<Step>>,
}

/// A deterministic control plane over a test-owned worker endpoint.
///
/// The endpoint can be an init router or a served engine. Control-plane calls do
/// not make HTTP requests; tests can therefore distinguish a resume from an
/// accidental routed request to a suspended worker.
#[derive(Clone, Debug)]
pub struct FakeProvisioner {
    endpoint: Endpoint,
    control: Arc<Mutex<Control>>,
}

impl FakeProvisioner {
    /// Uses a test-owned endpoint for workers created by this control plane.
    pub fn from_endpoint(endpoint: Endpoint) -> Self {
        Self { endpoint, control: Arc::new(Mutex::new(Control::default())) }
    }

    /// Queues a typed failure before the next operation can mutate state.
    pub fn fail_next(&self, operation: ProvisionOperation, kind: ProvisionErrorKind) {
        self.control
            .lock()
            .expect("fake control lock")
            .scripts
            .entry(operation)
            .or_default()
            .push_back(Step::Fail(kind));
    }

    /// Pauses the next operation until the returned signal is notified.
    pub fn hold_next(&self, operation: ProvisionOperation) -> Arc<Notify> {
        let notify = Arc::new(Notify::new());
        self.control
            .lock()
            .expect("fake control lock")
            .scripts
            .entry(operation)
            .or_default()
            .push_back(Step::Wait(Arc::clone(&notify)));
        notify
    }

    /// Returns each attempted control-plane call, including failed calls.
    pub fn calls(&self) -> Vec<(ProvisionOperation, String)> {
        self.control.lock().expect("fake control lock").calls.clone()
    }

    /// The current state without recording a control-plane lookup.
    pub fn current(&self, worker: &Worker) -> WorkerState {
        self.control
            .lock()
            .expect("fake control lock")
            .workers
            .get(worker.handle.as_str())
            .cloned()
            .unwrap_or(WorkerState::Gone)
    }

    /// Models an owner's out-of-band suspend without cancelling any delayed wake.
    pub fn owner_recover(&self, worker: &Worker) {
        if let Some(state) =
            self.control.lock().expect("fake control lock").workers.get_mut(worker.handle.as_str())
        {
            *state = WorkerState::Suspended;
        }
    }

    /// Starts a router wake independent of every other router's registry.
    ///
    /// The returned task is held by the test and must be joined. Its late dispatch
    /// cannot recreate a deleted worker, even when a replacement exists.
    pub fn delayed_wake(&self, worker: &Worker, release: Arc<Notify>) -> JoinHandle<()> {
        let control = Arc::clone(&self.control);
        let handle = worker.handle.as_str().to_owned();
        tokio::spawn(async move {
            release.notified().await;
            if let Some(state) = control.lock().expect("fake control lock").workers.get_mut(&handle)
            {
                *state = WorkerState::Running;
            }
        })
    }

    async fn before(
        &self,
        operation: ProvisionOperation,
        worker: &str,
    ) -> Result<(), ProvisionError> {
        let step = {
            let mut control = self.control.lock().expect("fake control lock");
            control.calls.push((operation, worker.to_owned()));
            control.scripts.get_mut(&operation).and_then(VecDeque::pop_front)
        };
        match step {
            Some(Step::Fail(kind)) => return Err(failure(operation, kind)),
            Some(Step::Wait(release)) => release.notified().await,
            None => {}
        }
        Ok(())
    }
}

fn failure(operation: ProvisionOperation, kind: ProvisionErrorKind) -> ProvisionError {
    let operation = match operation {
        ProvisionOperation::Spawn => "spawn",
        ProvisionOperation::Resume => "resume",
        ProvisionOperation::Suspend => "suspend",
        ProvisionOperation::State => "state",
        ProvisionOperation::Kill => "kill",
    };
    ProvisionError { operation, kind }
}

#[async_trait]
impl Provisioner for FakeProvisioner {
    async fn spawn(&self, spec: &WorkerSpec) -> Result<Worker, ProvisionError> {
        self.before(ProvisionOperation::Spawn, spec.name.as_str()).await?;
        let mut control = self.control.lock().expect("fake control lock");
        if !control.used.insert(spec.name.as_str().to_owned()) {
            return Err(failure(ProvisionOperation::Spawn, ProvisionErrorKind::Refused));
        }
        control.workers.insert(spec.name.as_str().to_owned(), WorkerState::Suspended);
        let mut endpoint = self.endpoint.clone();
        endpoint.headers.insert(
            "x-ganja-fake-worker",
            spec.name.as_str().parse().expect("a validated member name is a header value"),
        );
        Ok(Worker { handle: WorkerHandle::new(spec.name.as_str()), endpoint })
    }

    async fn resume(&self, worker: &Worker) -> Result<Resumed, ProvisionError> {
        self.before(ProvisionOperation::Resume, worker.handle.as_str()).await?;
        let mut control = self.control.lock().expect("fake control lock");
        let state = control
            .workers
            .get_mut(worker.handle.as_str())
            .ok_or_else(|| failure(ProvisionOperation::Resume, ProvisionErrorKind::Gone))?;
        if *state == WorkerState::Crashed {
            return Err(failure(ProvisionOperation::Resume, ProvisionErrorKind::Crashed));
        }
        let was_suspended = *state == WorkerState::Suspended;
        *state = WorkerState::Running;
        Ok(Resumed { was_suspended })
    }

    async fn suspend(&self, worker: &Worker) -> Result<(), ProvisionError> {
        self.before(ProvisionOperation::Suspend, worker.handle.as_str()).await?;
        let mut control = self.control.lock().expect("fake control lock");
        let state = control
            .workers
            .get_mut(worker.handle.as_str())
            .ok_or_else(|| failure(ProvisionOperation::Suspend, ProvisionErrorKind::Gone))?;
        *state = WorkerState::Suspended;
        Ok(())
    }

    async fn state(&self, worker: &Worker) -> Result<WorkerState, ProvisionError> {
        self.before(ProvisionOperation::State, worker.handle.as_str()).await?;
        Ok(self.current(worker))
    }

    async fn kill(&self, worker: &Worker) -> Result<(), ProvisionError> {
        self.before(ProvisionOperation::Kill, worker.handle.as_str()).await?;
        self.control.lock().expect("fake control lock").workers.remove(worker.handle.as_str());
        Ok(())
    }

    fn describe(&self, worker: &Worker) -> String {
        format!("test worker {}", worker.handle.as_str())
    }
}

/// One worker request observed by the fake router. Header values are never formatted.
#[derive(Clone)]
pub struct WorkerRequest {
    /// Worker named by the request's routing header.
    pub worker: String,
    /// HTTP method, without any request body.
    pub method: String,
    /// Path without its potentially credential-bearing query.
    pub path: String,
    /// Control-plane state when the request was admitted.
    pub state: WorkerState,
    headers: axum::http::HeaderMap,
}

impl std::fmt::Debug for WorkerRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerRequest")
            .field("worker", &self.worker)
            .field("method", &self.method)
            .field("path", &self.path)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl WorkerRequest {
    /// Compares a recorded header without rendering it in an assertion message.
    pub fn header_matches(&self, name: &str, expected: &str) -> bool {
        self.headers.get(name).is_some_and(|value| value.as_bytes() == expected.as_bytes())
    }
}

enum RouteStep {
    Reply(u16),
    Hold(Arc<Notify>),
}
enum InitStep {
    Fail(ganja_serve::bootstrap::Stage),
    Hold(Arc<Notify>),
}

#[derive(Default)]
struct HttpScript {
    requests: Vec<WorkerRequest>,
    routes: VecDeque<(String, RouteStep)>,
    bootstrap: VecDeque<InitStep>,
    bootstrap_calls: usize,
}

struct Initializer {
    script: Arc<Mutex<HttpScript>>,
    ready: Arc<Notify>,
    cancel: tokio_util::sync::CancellationToken,
}

impl ganja_serve::bootstrap::Executor for Initializer {
    fn execute(
        &self,
        request: ganja_serve::bootstrap::Bootstrap,
        deadline: tokio::time::Instant,
    ) -> futures::future::BoxFuture<
        'static,
        Result<ganja_serve::bootstrap::Cloned, ganja_serve::bootstrap::Failure>,
    > {
        let step = {
            let mut script = self.script.lock().expect("HTTP script lock");
            script.bootstrap_calls += 1;
            script.bootstrap.pop_front()
        };
        let ready = Arc::clone(&self.ready);
        let cancel = self.cancel.clone();
        Box::pin(async move {
            match step {
                Some(InitStep::Fail(stage)) => {
                    return Err(ganja_serve::bootstrap::Failure {
                        stage,
                        detail: "scripted initialization failure".to_owned(),
                    });
                }
                Some(InitStep::Hold(release)) => tokio::select! {
                    () = release.notified() => {},
                    () = cancel.cancelled() => return Err(ganja_serve::bootstrap::Failure { stage: ganja_serve::bootstrap::Stage::Cleanup, detail: "fake initialization stopped".to_owned() }),
                },
                None => {}
            }
            if tokio::time::Instant::now() < deadline {
                ready.notify_one();
            }
            Ok(ganja_serve::bootstrap::Cloned {
                head: "1111111111111111111111111111111111111111".to_owned(),
                branch: request.branch,
            })
        })
    }
}

#[derive(Clone)]
struct RouterState {
    control: Arc<Mutex<Control>>,
    script: Arc<Mutex<HttpScript>>,
    target: Arc<Mutex<String>>,
    client: reqwest::Client,
    cancel: tokio_util::sync::CancellationToken,
}

/// A two-phase init/serve fixture behind a router that records every worker request.
/// The caller must close it so every listener and stream task is joined.
pub struct FakeServer {
    script: Arc<Mutex<HttpScript>>,
    cancel: tokio_util::sync::CancellationToken,
    tasks: Vec<JoinHandle<std::io::Result<()>>>,
}

impl FakeServer {
    /// Every routed request in arrival order, with redacting Debug output.
    pub fn requests(&self) -> Vec<WorkerRequest> {
        self.script.lock().expect("HTTP script lock").requests.clone()
    }

    /// Counts admitted clone operations, not POST attempts.
    pub fn bootstrap_calls(&self) -> usize {
        self.script.lock().expect("HTTP script lock").bootstrap_calls
    }

    /// Replies without a serve-provenance header on the next matching route.
    pub fn reply_next(&self, path: &str, status: u16) {
        self.script
            .lock()
            .expect("HTTP script lock")
            .routes
            .push_back((path.to_owned(), RouteStep::Reply(status)));
    }

    /// Holds the next matching request without cancelling its server-side work.
    pub fn hold_next(&self, path: &str) -> Arc<Notify> {
        let release = Arc::new(Notify::new());
        self.script
            .lock()
            .expect("HTTP script lock")
            .routes
            .push_back((path.to_owned(), RouteStep::Hold(Arc::clone(&release))));
        release
    }

    /// Makes the next admitted initialization return a typed failure.
    pub fn fail_bootstrap(&self, stage: ganja_serve::bootstrap::Stage) {
        self.script.lock().expect("HTTP script lock").bootstrap.push_back(InitStep::Fail(stage));
    }

    /// Keeps one admitted initialization alive until the test releases it.
    pub fn hold_bootstrap(&self) -> Arc<Notify> {
        let release = Arc::new(Notify::new());
        self.script
            .lock()
            .expect("HTTP script lock")
            .bootstrap
            .push_back(InitStep::Hold(Arc::clone(&release)));
        release
    }

    /// Stops and joins the router and whichever worker phase is serving.
    ///
    /// # Errors
    /// Returns a listener or task failure, never a remote response body.
    pub async fn close(mut self) -> std::io::Result<()> {
        self.cancel.cancel();
        for task in self.tasks.drain(..) {
            task.await.map_err(std::io::Error::other)??;
        }
        Ok(())
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl FakeProvisioner {
    /// Starts the real init protocol, then switches to the real served engine after
    /// bootstrap's response is drained. No worker request is made by a lifecycle call.
    ///
    /// # Errors
    /// Returns a local listener or client-construction failure.
    pub async fn serving(
        engine: Arc<ganja_core::Engine>,
        mut config: ganja_serve::ServeConfig,
        username: &str,
        password: &str,
    ) -> std::io::Result<(Self, FakeServer)> {
        let cancel = tokio_util::sync::CancellationToken::new();
        let script = Arc::new(Mutex::new(HttpScript::default()));
        let ready = Arc::new(Notify::new());
        let credentials = ganja_serve::Credentials {
            username: username.to_owned(),
            password: secrecy::SecretString::from(password.to_owned()),
        };
        config.credentials = Some(credentials.clone());
        config.listen = ganja_serve::Listen::loopback();
        let initializer = Arc::new(Initializer {
            script: Arc::clone(&script),
            ready: Arc::clone(&ready),
            cancel: cancel.clone(),
        });
        let init = ganja_serve::bootstrap::Service::new(
            credentials,
            config.directory.clone(),
            initializer,
        )
        .serve(tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?)?;
        let target = Arc::new(Mutex::new(format!("http://{}", init.address())));
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let endpoint = Endpoint {
            base_url: format!("http://{}", listener.local_addr()?),
            headers: axum::http::HeaderMap::new(),
            username: username.to_owned(),
            password: secrecy::SecretString::from(password.to_owned()),
        };
        let provisioner = Self::from_endpoint(endpoint);
        let state = RouterState {
            control: Arc::clone(&provisioner.control),
            script: Arc::clone(&script),
            target: Arc::clone(&target),
            client: reqwest::Client::builder().no_proxy().build().map_err(std::io::Error::other)?,
            cancel: cancel.clone(),
        };
        let app = axum::Router::new().fallback(routed).with_state(state);
        let stopped = cancel.clone();
        let router = tokio::spawn(async move {
            axum::serve(listener, app).with_graceful_shutdown(stopped.cancelled_owned()).await
        });
        let stopped = cancel.clone();
        let worker = tokio::spawn(async move {
            tokio::select! {
                () = stopped.cancelled() => return init.shutdown().await,
                () = ready.notified() => {},
            }
            init.handoff().await?;
            let serving =
                ganja_serve::serve(engine, config).await.map_err(std::io::Error::other)?;
            *target.lock().expect("worker target lock") = format!("http://{}", serving.address());
            stopped.cancelled().await;
            serving.shutdown().await
        });
        Ok((provisioner, FakeServer { script, cancel, tasks: vec![router, worker] }))
    }

    /// Retrieves a created worker for out-of-band lifecycle schedules in tests.
    pub fn worker(&self, name: &str) -> Option<Worker> {
        self.control.lock().expect("fake control lock").workers.get(name)?;
        let mut endpoint = self.endpoint.clone();
        endpoint.headers.insert("x-ganja-fake-worker", name.parse().ok()?);
        Some(Worker { handle: WorkerHandle::new(name), endpoint })
    }
}

async fn routed(
    axum::extract::State(state): axum::extract::State<RouterState>,
    request: axum::extract::Request,
) -> axum::response::Response {
    let (parts, body) = request.into_parts();
    let worker = parts
        .headers
        .get("x-ganja-fake-worker")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let path = parts.uri.path().to_owned();
    let observed = {
        let mut control = state.control.lock().expect("fake control lock");
        let observed = control.workers.get(&worker).cloned().unwrap_or(WorkerState::Gone);
        if observed == WorkerState::Suspended {
            control.workers.insert(worker.clone(), WorkerState::Running);
        }
        observed
    };
    let step = {
        let mut script = state.script.lock().expect("HTTP script lock");
        script.requests.push(WorkerRequest {
            worker: worker.clone(),
            method: parts.method.to_string(),
            path: path.clone(),
            state: observed.clone(),
            headers: parts.headers.clone(),
        });
        script
            .routes
            .iter()
            .position(|(match_path, _)| *match_path == path)
            .and_then(|position| script.routes.remove(position))
            .map(|(_, step)| step)
    };
    if observed == WorkerState::Gone {
        return plain_response(404);
    }
    match step {
        Some(RouteStep::Reply(status)) => return plain_response(status),
        Some(RouteStep::Hold(release)) => {
            tokio::select! { () = release.notified() => {}, () = state.cancel.cancelled() => return plain_response(503) }
        }
        None => {}
    }
    if !state.control.lock().expect("fake control lock").workers.contains_key(&worker) {
        return plain_response(404);
    }
    let target = state.target.lock().expect("worker target lock").clone();
    let route = parts.uri.path_and_query().map_or("/", |value| value.as_str());
    let Ok(body) = axum::body::to_bytes(body, 4 * 1024 * 1024).await else {
        return plain_response(400);
    };
    let response = state
        .client
        .request(parts.method, format!("{target}{route}"))
        .headers(parts.headers)
        .body(body)
        .send()
        .await;
    let Ok(response) = response else { return plain_response(502) };
    let status = response.status();
    let mut headers = response.headers().clone();
    headers.remove(axum::http::header::TRANSFER_ENCODING);
    headers.remove(axum::http::header::CONTENT_LENGTH);
    let mut result =
        axum::response::Response::new(axum::body::Body::from_stream(response.bytes_stream()));
    *result.status_mut() = status;
    *result.headers_mut() = headers;
    result
}

fn plain_response(status: u16) -> axum::response::Response {
    axum::response::Response::builder()
        .status(status)
        .body(axum::body::Body::empty())
        .expect("scripted HTTP status")
}
