//! Resource lifecycle independent of any hosting vendor. Creation returns a
//! suspended worker with its policy installed; only the caller starts it.

use std::fmt;

use async_trait::async_trait;
use ganja_team::MemberName;
use http::HeaderMap;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

/// Agent Substrate's lifecycle and tunnel implementation.
pub mod substrate;

/// A worker identity opaque to the member and interpreted only by its provisioner.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkerHandle(String);

impl WorkerHandle {
    /// Wraps a provider-assigned identity, which must contain no credential.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The identity passed back to the issuing provisioner.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for WorkerHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorkerHandle(<opaque>)")
    }
}

/// Authenticated ingress to a worker. Headers may contain routing credentials.
#[derive(Clone)]
pub struct Endpoint {
    /// Validated by the client before any request is made.
    pub base_url: String,
    /// Provider-owned routing and local-ingress credentials, separate from worker Basic auth.
    pub headers: HeaderMap,
    /// Basic-auth username configured in the worker template.
    pub username: String,
    /// Basic-auth password configured in the worker template.
    pub password: SecretString,
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Endpoint(<redacted>)")
    }
}

/// A created, suspended resource with its routing policy already installed.
#[derive(Clone, Debug)]
pub struct Worker {
    /// Opaque identity returned to lifecycle calls.
    pub handle: WorkerHandle,
    /// Ingress address and credentials returned by the provisioner.
    pub endpoint: Endpoint,
}

/// Git identity recorded in the worker's local checkout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Author {
    /// Display name of the worker making a commit.
    pub name: String,
    /// No-reply address, never an account credential.
    pub email: String,
}

/// Checkout requested before the worker assembles its engine.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bootstrap {
    /// HTTPS repository URL without userinfo, query, or fragment.
    pub repository: String,
    /// Starting commit or reference.
    pub reference: String,
    /// New branch to create inside the checkout.
    pub branch: String,
    /// Git author and committer identity.
    pub author: Author,
    /// Relative process-group budget, counted from request admission.
    pub deadline_secs: u64,
}

impl fmt::Debug for Bootstrap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bootstrap")
            .field("deadline_secs", &self.deadline_secs)
            .finish_non_exhaustive()
    }
}

/// Vendor-neutral information used while creating a worker and its policy.
#[derive(Clone, Debug)]
pub struct WorkerSpec {
    /// Mailbox identity of the teammate.
    pub name: MemberName,
    /// Mailbox identity of its lead.
    pub lead: MemberName,
    /// Checkout to prepare after the worker is resumed.
    pub bootstrap: Bootstrap,
    /// Host permitted to receive the injected git authorization header.
    pub git_host: String,
}

/// Authoritative resource states returned by a provisioner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerState {
    /// The process may accept work.
    Running,
    /// The control plane confirms no process is running.
    Suspended,
    /// A lifecycle operation has not settled.
    Transitioning,
    /// The process cannot continue without recovery.
    Crashed,
    /// The identity no longer exists.
    Gone,
}

/// A completed resume and whether it changed a suspended worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resumed {
    /// False if the worker was already running.
    pub was_suspended: bool,
}

/// A confirmed suspension and whether the caller caused the transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuspendOutcome {
    /// This call changed the worker to suspended.
    Suspended,
    /// The worker was suspended before this call.
    AlreadySuspended,
}

/// What a provider failure means to the lifecycle caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProvisionErrorKind {
    /// A read may succeed later; mutations still require reconciliation.
    Transient,
    /// The worker crashed.
    Crashed,
    /// The named worker does not exist.
    Gone,
    /// Configuration or policy prevents the operation.
    Refused,
    /// The provider cannot prove whether a mutation happened.
    Ambiguous,
}

/// A sanitized provider failure. Remote error bodies are deliberately not stored.
#[derive(Clone, Debug, thiserror::Error)]
#[error("worker {operation} failed ({kind:?})")]
pub struct ProvisionError {
    /// Static operation name; never a remote error body or credential.
    pub operation: &'static str,
    /// Machine-readable classification of the failure.
    pub kind: ProvisionErrorKind,
}

/// The single lifecycle contract every cloud provider implements.
#[async_trait]
pub trait Provisioner: fmt::Debug + Send + Sync {
    /// Creates a suspended worker with policy installed and endpoint known.
    ///
    /// # Errors
    /// A failure must not leave an unreported resource; implementations reconcile
    /// ambiguous creates and clean up a failed policy installation.
    async fn spawn(&self, spec: &WorkerSpec) -> Result<Worker, ProvisionError>;

    /// Resumes the worker and waits for its local readiness probe.
    ///
    /// # Errors
    /// Reports a refused, crashed, gone, or ambiguous resume without retrying it.
    async fn resume(&self, worker: &Worker) -> Result<Resumed, ProvisionError>;

    /// Suspends the worker and waits for control-plane confirmation.
    ///
    /// # Errors
    /// An unconfirmed operation is an error, never a suspended claim.
    async fn suspend(&self, worker: &Worker) -> Result<SuspendOutcome, ProvisionError>;

    /// Reads the authoritative state without waking the worker.
    ///
    /// # Errors
    /// Reports an unavailable or refused state lookup.
    async fn state(&self, worker: &Worker) -> Result<WorkerState, ProvisionError>;

    /// Deletes the worker in any state, without first resuming or suspending it.
    ///
    /// # Errors
    /// Reports an unconfirmed delete; callers perform one state reconciliation.
    async fn kill(&self, worker: &Worker) -> Result<(), ProvisionError>;

    /// Formats a credential-free resource description for operator notices.
    fn describe(&self, worker: &Worker) -> String;
}
