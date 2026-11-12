//! The pre-engine listener. A successful response drains before the caller may exec serve.
//! HTTP/1 responses close their connection explicitly; HTTP/2 remains accepted and
//! retires through GOAWAY during the handoff's graceful shutdown.
//!
//! Git execution belongs to the worker process. This module owns admission, strict wire
//! bodies and status reconciliation, so a disconnected POST cannot admit a second clone.

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, Version, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use base64::Engine as _;
use futures::future::BoxFuture;
use secrecy::ExposeSecret as _;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};
use tokio::time::Instant;

use crate::Credentials;
use crate::auth::{self, WWW_AUTHENTICATE};

/// A clone request. The relative budget starts when this request is admitted.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Bootstrap {
    /// Repository to clone; the executor validates its transport and credentials.
    pub repository: String,
    /// Revision to check out before creating the work branch.
    pub reference: String,
    /// Branch the worker will publish.
    pub branch: String,
    /// Identity written to the checkout's local Git config.
    pub author: Author,
    /// Server-side budget, in seconds; values outside 5 through 600 are refused.
    pub deadline_secs: u64,
}

impl fmt::Debug for Bootstrap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bootstrap")
            .field("deadline_secs", &self.deadline_secs)
            .finish_non_exhaustive()
    }
}

/// The Git author of the worker's commits.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Author {
    /// Git author name.
    pub name: String,
    /// Git author email.
    pub email: String,
}

/// The stage at which bootstrap failed.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Refused before starting Git.
    Dispatch,
    /// Clone failed.
    Clone,
    /// Revision or branch creation failed.
    Switch,
    /// Local Git configuration failed.
    Config,
    /// The admission-based budget expired.
    Deadline,
    /// The partial checkout could not be removed; no retry is allowed.
    Cleanup,
}

/// A successful checkout, returned only after its local configuration is ready.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Cloned {
    /// Checked-out commit id.
    pub head: String,
    /// Created work branch.
    pub branch: String,
}

/// An executor failure. Raw diagnostics are never included in Debug or Display.
#[derive(Clone, thiserror::Error)]
#[error("bootstrap failed at {stage:?}")]
pub struct Failure {
    /// Failed operation.
    pub stage: Stage,
    /// Diagnostic text; the router redacts request values and the serve credential.
    pub detail: String,
}

impl fmt::Debug for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Failure").field("stage", &self.stage).finish_non_exhaustive()
    }
}

/// Current init-phase status. Serve itself answers bootstrap routes with 404.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Status {
    /// No operation has been admitted.
    Idle {},
    /// Exactly one clone is running.
    InProgress {},
    /// Client interpretation of the engine's 404; the init router never emits it.
    AlreadyServing {},
    /// The response can drain and the worker may enter serve.
    Cloned {
        /// Checked-out commit id.
        head: String,
        /// Created work branch.
        branch: String,
    },
    /// A fixed failure message; executor diagnostics remain in the server log.
    Failed {
        /// Failed operation.
        stage: Stage,
        /// Fixed text with no executor output.
        detail: String,
        /// True while the timed-out executor is still finishing cleanup.
        #[serde(default)]
        cleanup_in_progress: bool,
    },
}

/// Executes the worker's clone before an engine is assembled.
///
/// The future must honor `deadline`, kill and reap its process group on expiry,
/// and clean the partial checkout before returning a retryable failure. It must
/// return `Stage::Cleanup` if removal fails. The router keeps the future alive
/// when the requesting client disconnects. No credential may enter diagnostics.
pub trait Executor: Send + Sync + 'static {
    /// Clone and configure one checkout within the admission-based deadline.
    fn execute(
        &self,
        request: Bootstrap,
        deadline: Instant,
    ) -> BoxFuture<'static, Result<Cloned, Failure>>;
}

/// Shared admission and reconciliation state for one worker initialization.
#[derive(Clone)]
pub struct Service {
    inner: Arc<Inner>,
}

struct Inner {
    epoch: String,
    credentials: Credentials,
    directory: crate::state::ServedDirectory,
    executor: Arc<dyn Executor>,
    operation: Mutex<Operation>,
    ready: watch::Sender<Option<Cloned>>,
}

struct Operation {
    status: Status,
    running: bool,
}

impl Service {
    /// Creates the init listener state, initially idle.
    #[must_use]
    pub fn new(credentials: Credentials, directory: PathBuf, executor: Arc<dyn Executor>) -> Self {
        let (ready, _) = watch::channel(None);
        Self {
            inner: Arc::new(Inner {
                epoch: ganja_protocol::uuidv7(),
                credentials,
                directory: crate::state::ServedDirectory::new(directory),
                executor,
                operation: Mutex::new(Operation { status: Status::Idle {}, running: false }),
                ready,
            }),
        }
    }

    /// Routes available before assembly; no engine/session route exists here.
    pub fn router(&self) -> axum::Router {
        axum::Router::new()
            .route("/readyz", get(crate::routes::ready))
            .route("/bootstrap", get(status).post(bootstrap))
            .layer(middleware::from_fn_with_state(self.clone(), guard))
            .with_state(self.clone())
    }

    /// Starts serving on an already-bound TCP listener.
    ///
    /// # Errors
    /// Returns the listener's local-address error before spawning a server.
    pub fn serve(&self, listener: TcpListener) -> std::io::Result<Handle> {
        let address = listener.local_addr()?;
        let (shutdown, watch) = watch::channel(false);
        let task = crate::spawn_server(listener, self.router(), watch);
        Ok(Handle { address, shutdown, task, ready: self.inner.ready.subscribe() })
    }
}

/// A running init listener and its flush-before-exec boundary.
pub struct Handle {
    address: SocketAddr,
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    ready: watch::Receiver<Option<Cloned>>,
}

impl Handle {
    /// The bound address.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Waits for clone success, stops admission and drains responses before returning.
    ///
    /// Only after this method returns may the caller exec the engine. Draining
    /// is transport completion, not proof that the remote application read it.
    ///
    /// # Errors
    /// Returns a server or task failure, or a closed completion channel.
    pub async fn handoff(mut self) -> std::io::Result<Cloned> {
        let cloned = loop {
            if let Some(cloned) = self.ready.borrow_and_update().clone() {
                break cloned;
            }
            self.ready.changed().await.map_err(std::io::Error::other)?;
        };
        self.shutdown().await?;
        Ok(cloned)
    }

    /// Stops admission and drains every in-flight response.
    ///
    /// # Errors
    /// Returns an accept-loop or task failure.
    pub async fn shutdown(self) -> std::io::Result<()> {
        let _ = self.shutdown.send(true);
        self.task.await.map_err(std::io::Error::other)?
    }
}

async fn guard(State(service): State<Service>, request: Request, next: Next) -> Response {
    let version = request.version();
    let inner = service.inner.clone();
    let mut response =
        crate::routes::stamp_epoch(guarded(service, request, next).await, &inner.epoch);
    // A delayed TCP FIN must not let a peer pool an initializer connection
    // across exec. HTTP/2 uses GOAWAY during the existing graceful shutdown.
    if matches!(version, Version::HTTP_10 | Version::HTTP_11) {
        // Preserve HTTP/1.0 before Hyper applies its keep-alive compatibility fix.
        *response.version_mut() = version;
        response
            .headers_mut()
            .insert(header::CONNECTION, header::HeaderValue::from_static("close"));
    }
    response
}

async fn guarded(service: Service, request: Request, next: Next) -> Response {
    tracing::debug!(method = %request.method(), path = request.uri().path(), "serving init");
    let readiness = request.method() == Method::GET && request.uri().path() == "/readyz";
    if !readiness
        && !auth::authorized(request.headers(), request.uri().query(), &service.inner.credentials)
    {
        return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, WWW_AUTHENTICATE)])
            .into_response();
    }
    let asked =
        request.uri().query().and_then(|query| auth::query_param(query, "directory")).or_else(
            || {
                request
                    .headers()
                    .get(crate::DIRECTORY_HEADER)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
            },
        );
    if asked.is_some_and(|asked| !service.inner.directory.matches(&asked)) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    next.run(request).await
}

async fn status(State(service): State<Service>, body: Bytes) -> Response {
    if !body.is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    Json(service.inner.operation.lock().expect("bootstrap state is never poisoned").status.clone())
        .into_response()
}

async fn bootstrap(State(service): State<Service>, body: Bytes) -> Response {
    // Parse errors deliberately omit serde's input-derived diagnostic text.
    let Ok(request) = serde_json::from_slice::<Bootstrap>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if !(5..=600).contains(&request.deadline_secs) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let deadline = {
        let mut operation =
            service.inner.operation.lock().expect("bootstrap state is never poisoned");
        if operation.running
            || matches!(
                operation.status,
                Status::Cloned { .. } | Status::Failed { stage: Stage::Cleanup, .. }
            )
        {
            return (StatusCode::CONFLICT, Json(operation.status.clone())).into_response();
        }
        operation.status = Status::InProgress {};
        operation.running = true;
        Instant::now() + Duration::from_secs(request.deadline_secs)
    };
    let (sent, received) = oneshot::channel();
    tokio::spawn(async move {
        let mut sent = Some(sent);
        let work = service.inner.executor.execute(request.clone(), deadline);
        tokio::pin!(work);
        let (result, expired) = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => {
                let status = Status::Failed { stage: Stage::Deadline, detail: "bootstrap budget expired".into(), cleanup_in_progress: true };
                service.inner.operation.lock().expect("bootstrap state is never poisoned").status = status.clone();
                if let Some(sent) = sent.take() { let _ = sent.send(status); }
                // Keep polling cleanup. The admission latch stays set until it
                // finishes, so timing out HTTP can never start a second clone.
                (work.await, true)
            }
            result = &mut work => (result, false),
        };
        let status = match (result, expired) {
            (Ok(_), true) => Status::Failed {
                stage: Stage::Cleanup,
                detail: "executor completed after its deadline without confirming cleanup".into(),
                cleanup_in_progress: false,
            },
            (Ok(cloned), false) => Status::Cloned { head: cloned.head, branch: cloned.branch },
            (Err(failure), expired) => {
                tracing::warn!(
                    stage = ?failure.stage,
                    detail = %redact(failure.detail, &request, &service.inner.credentials),
                    "bootstrap executor failed"
                );
                Status::Failed {
                    stage: if expired && failure.stage != Stage::Cleanup {
                        Stage::Deadline
                    } else {
                        failure.stage
                    },
                    detail: "bootstrap failed; consult the server log".into(),
                    cleanup_in_progress: false,
                }
            }
        };
        {
            let mut operation =
                service.inner.operation.lock().expect("bootstrap state is never poisoned");
            operation.status = status.clone();
            operation.running = false;
        }
        let success = match &status {
            Status::Cloned { head, branch } => {
                Some(Cloned { head: head.clone(), branch: branch.clone() })
            }
            _ => None,
        };
        if let Some(sent) = sent {
            let _ = sent.send(status);
        }
        if let Some(success) = success {
            service.inner.ready.send_replace(Some(success));
        }
    });
    match received.await {
        Ok(status @ Status::Cloned { .. }) => (StatusCode::OK, Json(status)).into_response(),
        Ok(status) => (StatusCode::CONFLICT, Json(status)).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(Status::Failed {
                stage: Stage::Dispatch,
                detail: "bootstrap executor stopped".into(),
                cleanup_in_progress: false,
            }),
        )
            .into_response(),
    }
}

fn redact(mut detail: String, request: &Bootstrap, credentials: &Credentials) -> String {
    let basic = base64::engine::general_purpose::STANDARD.encode(format!(
        "{}:{}",
        credentials.username,
        credentials.password.expose_secret(),
    ));
    let encoded_password =
        base64::engine::general_purpose::STANDARD.encode(credentials.password.expose_secret());
    for value in [
        basic.as_str(),
        encoded_password.as_str(),
        credentials.password.expose_secret(),
        &request.repository,
        &request.reference,
        &request.branch,
        &request.author.name,
        &request.author.email,
    ] {
        if !value.is_empty() {
            detail = detail.replace(value, "[redacted]");
        }
    }
    if let Ok(repository) = url::Url::parse(&request.repository) {
        for value in std::iter::once(repository.username()).chain(repository.password()) {
            if !value.is_empty() {
                detail = detail.replace(value, "[redacted]");
                let decoded = percent_encoding::percent_decode_str(value).decode_utf8_lossy();
                detail = detail.replace(decoded.as_ref(), "[redacted]");
            }
        }
    }
    detail
}

#[cfg(test)]
#[path = "bootstrap_tests.rs"]
mod tests;
