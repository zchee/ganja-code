//! One hosted member owns its request gate, cursor, terminal decisions and
//! cleanup. The local registry never needs to know which provisioner hosts it.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use ganja_client::{BootstrapAuthor, BootstrapOutcome, Client, ClientError, Prompt, Resumed};
use ganja_core::teammate::{self, Exited, Lent, PaneFate, SpawnSpec, Spawned, Unsupported, runner};
use ganja_protocol::team::{Frame, IdleNotification, IdleReason, MemberBackend, Tagged};
use ganja_protocol::{Event, FinishReason, Message, MessageId, PartBody, Role, SessionId};
use ganja_team::{MailboxMessage, Surface, record};
use secrecy::ExposeSecret as _;
use tokio::sync::{Mutex as AsyncMutex, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout, timeout_at};

use crate::backend::Runtime;
use crate::delivery::{self, Lane, Observation, Payload, Receipt, Value};
use crate::gate::{AttemptOutcome, Door, Drain, WorkerGate};
use crate::provisioner::{Bootstrap, ProvisionErrorKind, SuspendOutcome, Worker, WorkerState};
use crate::state::{LaunchFailure, Phase};
use crate::trace::TraceLog;
use crate::transport::{ClientSource, EventSource};

type ControlKey = (usize, String);

static CONTROLS: LazyLock<Mutex<BTreeMap<ControlKey, Weak<CloudMember>>>> =
    LazyLock::new(Mutex::default);

pub(crate) fn register(member: &Arc<CloudMember>) {
    let mut controls = CONTROLS.lock().expect("cloud control map lock");
    controls.retain(|_, member| {
        member.upgrade().is_some_and(|member| member.lent.registry.strong_count() != 0)
    });
    if member.lent.registry.strong_count() == 0 {
        return;
    }
    // A live member retains the registry's Weak allocation; prune dead owners
    // as well as expired members so no control survives its registry lifetime.
    let key = (member.lent.registry.as_ptr() as usize, member.spec.name.as_str().to_owned());
    controls.insert(key, Arc::downgrade(member));
}

/// Looks up a hosted member in its owning registry without retaining it globally.
pub fn control(registry: &teammate::TeammateRegistry, name: &str) -> Option<CloudControl> {
    let mut controls = CONTROLS.lock().expect("cloud control map lock");
    controls.retain(|_, member| {
        member.upgrade().is_some_and(|member| member.lent.registry.strong_count() != 0)
    });
    let name = ganja_team::MemberName::parse(name).ok()?;
    let key = (std::ptr::from_ref(registry) as usize, name.as_str().to_owned());
    controls
        .get(&key)?
        .upgrade()
        .filter(|member| member.lent.registry.strong_count() != 0)
        .map(CloudControl)
}

/// An explicit owner action on a hosted member.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlAction {
    /// Inspect the worker state and resume through the connected acknowledgement.
    ResumeForInspection,
    /// Delete the worker and confirm its absence.
    Retire,
    /// Invalidate local history without contacting the provisioner.
    ReportExternalRevert,
}

/// Why an owner action could not be admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ControlAdmissionError {
    /// Another owner action is unfinished.
    #[error("another owner action is in progress")]
    Busy,
    /// Member shutdown has started.
    #[error("the member is shutting down")]
    Cancelled,
    /// The current runtime cannot own an action.
    #[error("owner actions require an active asynchronous runtime")]
    Unsupported,
}

/// A sanitized result of an admitted action; never contains remote response text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlOutcome {
    /// The requested action completed.
    Completed,
    /// Deletion was attempted, but absence was not confirmed.
    UnconfirmedDeletion,
    /// Shutdown prevented the action from starting its transition.
    Cancelled,
    /// The action could not complete in the current member state.
    Unsupported,
}

/// An observation handle, not ownership of the member's action.
///
/// Dropping this handle or cancelling a wait does not cancel the action. The member
/// retains its task and joins it during cleanup. A wait can be resumed across frames.
#[derive(Debug)]
pub struct ControlOperation {
    receiver: oneshot::Receiver<ControlOutcome>,
    outcome: Option<ControlOutcome>,
}

impl ControlOperation {
    /// Observes completion without consuming the handle.
    ///
    /// Cancelling this wait preserves the result for a later wait. Repeated waits
    /// return the same outcome.
    pub async fn wait(&mut self) -> ControlOutcome {
        if let Some(outcome) = self.outcome {
            return outcome;
        }
        let outcome = (&mut self.receiver).await.unwrap_or(ControlOutcome::Unsupported);
        self.outcome = Some(outcome);
        outcome
    }
}

/// Owner controls bypass neither the request gate nor the permanent uncertain latch.
#[derive(Clone)]
pub struct CloudControl(Arc<CloudMember>);

impl fmt::Debug for CloudControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl CloudControl {
    /// Current activity, without making a request to the worker.
    pub fn phase(&self) -> Phase {
        self.0.phase()
    }

    /// Admits one member-owned action without waiting for lifecycle I/O.
    ///
    /// Remote work is bounded by the configured budgets: launch and resume each
    /// use `launch`, inspection uses `launch`, quiescence uses two `quiesce`
    /// budgets, and deletion uses `cleanup`. An action can wait behind launch,
    /// the runner's resume/quiescence, and cleanup, then spends at most two
    /// `launch` budgets for inspection/resume, one `cleanup` for retirement,
    /// or no remote work for a revert report.
    ///
    /// Let `H = 2 * launch + 2 * quiesce + cleanup + 4 * B`, where
    /// `B` is the 250 ms event-barrier budget. Including preceding holders,
    /// inspection takes at most `H + 2 * launch + 4 * B`, retirement takes
    /// `H + cleanup + F`, and a revert report takes `H + B`. `F` is a shared
    /// 250 ms final-delivery observation outside the transition lock.
    /// Mailbox work uses a bounded native service and never extends a holder.
    /// Its queued, late, failed and dropped outcomes are not delivery success.
    ///
    /// Event barriers are interruption-bounded: hold/invalidate notify before
    /// waiting for the consumer's state lock. Every connection, retry sleep and
    /// read holding that lock gives priority to that notification, dropping its
    /// I/O without awaiting the remote peer. Adding an await under that lock
    /// without racing the notification would break this hand-off invariant.
    ///
    /// Shutdown cancels an action waiting for the transition lock without side
    /// effects. Once the lock is acquired the action finishes its own budgets;
    /// the member's cleanup joins the task before deleting the worker.
    ///
    /// # Errors
    /// Returns a typed refusal when another action is unfinished, shutdown has
    /// started, or there is no asynchronous runtime to own the task.
    pub fn admit(&self, action: ControlAction) -> Result<ControlOperation, ControlAdmissionError> {
        let mut task = self.0.control_task.try_lock().map_err(|_| ControlAdmissionError::Busy)?;
        if task.as_ref().is_some_and(|task| !task.is_finished()) {
            return Err(ControlAdmissionError::Busy);
        }
        if self.0.lent.cancel.is_cancelled() {
            return Err(ControlAdmissionError::Cancelled);
        }
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| ControlAdmissionError::Unsupported)?;
        let (sender, receiver) = oneshot::channel();
        let member = Arc::clone(&self.0);
        *task = Some(runtime.spawn(async move {
            let outcome = member.control_action(action).await;
            let _ = sender.send(outcome);
        }));
        Ok(ControlOperation { receiver, outcome: None })
    }

    /// Resumes through the ordinary connected-ack barrier for explicit inspection.
    ///
    /// # Errors
    /// Returns a sanitized lifecycle failure, never an endpoint or credential.
    pub async fn resume_for_inspection(&self) -> Result<(), Unsupported> {
        self.observe(ControlAction::ResumeForInspection).await
    }

    /// Ends the worker in any state, then confirms absence once.
    ///
    /// # Errors
    /// Unconfirmed deletion leaves an owner-action notice and is not called gone.
    pub async fn retire(&self) -> Result<(), Unsupported> {
        self.observe(ControlAction::Retire).await
    }

    /// Invalidates a checkpointed cursor before any request follows an owner-reported revert.
    ///
    /// A busy or shutting-down member does not admit another report.
    pub async fn report_external_revert(&self) {
        let _ = self.observe(ControlAction::ReportExternalRevert).await;
    }

    async fn observe(&self, action: ControlAction) -> Result<(), Unsupported> {
        let mut operation =
            self.admit(action).map_err(|error| self.0.unsupported(error.to_string()))?;
        match operation.wait().await {
            ControlOutcome::Completed => Ok(()),
            ControlOutcome::UnconfirmedDeletion => {
                Err(self.0.unsupported("worker deletion is unconfirmed"))
            }
            ControlOutcome::Cancelled => Err(self.0.unsupported("owner action cancelled")),
            ControlOutcome::Unsupported => {
                Err(self.0.unsupported("owner action could not complete"))
            }
        }
    }
}

/// One hosted teammate with deduplicated terminal decisions and best-effort ordered mail.
pub struct CloudMember {
    runtime: Arc<Runtime>,
    worker: Worker,
    spec: SpawnSpec,
    lent: Lent,
    bootstrap: Bootstrap,
    client: Option<Client>,
    events: Option<Arc<dyn EventSource>>,
    gate: Arc<WorkerGate>,
    transition: AsyncMutex<()>,
    control_task: AsyncMutex<Option<JoinHandle<()>>>,
    phase: Mutex<Phase>,
    session: Mutex<Option<SessionId>>,
    recent: Mutex<VecDeque<String>>,
    alive: AtomicBool,
    cleaned: AtomicBool,
    exited: AtomicBool,
    terminal: Mutex<BTreeMap<MessageId, Option<FinishReason>>>,
    observed_text: Mutex<BTreeMap<MessageId, usize>>,
    launch_timeout: Mutex<LaunchFailure>,
    uncertainty_reported: AtomicBool,
    trace: Mutex<Option<TraceLog>>,
    epoch: Mutex<Option<String>>,
    needs_resync: AtomicBool,
    observed_crash: AtomicBool,
    delivery: Lane,
    notifications: Mutex<VecDeque<Receipt>>,
    final_observed: AtomicBool,
    barrier_failed: AtomicBool,
}

impl fmt::Debug for CloudMember {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CloudMember")
            .field("name", &self.spec.name)
            .field("phase", &self.phase())
            .field("gate", &self.gate.state())
            .finish_non_exhaustive()
    }
}

impl CloudMember {
    pub(crate) fn new(
        runtime: Arc<Runtime>,
        worker: Worker,
        spec: SpawnSpec,
        lent: Lent,
        bootstrap: Bootstrap,
    ) -> Arc<Self> {
        let gate = WorkerGate::new();
        let endpoint = &worker.endpoint;
        let trusted = url::Url::parse(&endpoint.base_url).is_ok_and(|url| {
            let loopback = match url.host() {
                Some(url::Host::Ipv4(address)) => address.is_loopback(),
                Some(url::Host::Ipv6(address)) => address.is_loopback(),
                Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
                None => false,
            };
            (url.scheme() == "https" || (url.scheme() == "http" && loopback))
                && url.host().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == "/"
                // Inspect the original spelling because URL parsing normalizes
                // empty userinfo and dot-segments before the origin check.
                && endpoint.base_url.split_once("://").is_some_and(|(_, rest)| {
                    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
                    !authority.contains('@') && !rest.contains('\\') && path.is_empty()
                })
        }) && !endpoint.headers.contains_key(http::header::AUTHORIZATION)
            && !endpoint.headers.contains_key(http::header::PROXY_AUTHORIZATION);
        let client = if trusted {
            Client::new(
                &endpoint.base_url,
                Some(ganja_client::Credentials::new(
                    &endpoint.username,
                    endpoint.password.expose_secret(),
                )),
            )
            .map(|client| client.with_headers(endpoint.headers.clone()))
            .ok()
        } else {
            None
        };
        let events = client.as_ref().map(|client| {
            Arc::new(ClientSource::new(client.clone(), Arc::clone(&gate))) as Arc<dyn EventSource>
        });
        let trace = TraceLog::from_env(spec.name.as_str()).unwrap_or_else(|_| {
            tracing::warn!("cloud trace could not be opened");
            None
        });
        Arc::new(Self {
            runtime,
            worker,
            spec,
            lent,
            bootstrap,
            client,
            events,
            gate,
            transition: AsyncMutex::new(()),
            control_task: AsyncMutex::new(None),
            phase: Mutex::new(Phase::Creating),
            session: Mutex::new(None),
            recent: Mutex::new(VecDeque::new()),
            alive: AtomicBool::new(true),
            cleaned: AtomicBool::new(false),
            exited: AtomicBool::new(false),
            terminal: Mutex::new(BTreeMap::new()),
            observed_text: Mutex::new(BTreeMap::new()),
            launch_timeout: Mutex::new(LaunchFailure::ResumeTimedOut),
            uncertainty_reported: AtomicBool::new(false),
            trace: Mutex::new(trace),
            epoch: Mutex::new(None),
            needs_resync: AtomicBool::new(false),
            observed_crash: AtomicBool::new(false),
            delivery: Lane::new(),
            notifications: Mutex::new(VecDeque::new()),
            final_observed: AtomicBool::new(false),
            barrier_failed: AtomicBool::new(false),
        })
    }

    fn traced(&self, write: impl FnOnce(&mut TraceLog) -> std::io::Result<()>) {
        let mut trace = self.trace.lock().expect("trace writer lock");
        if trace.as_mut().is_some_and(|trace| write(trace).is_err()) {
            *trace = None;
            tracing::warn!("cloud trace write failed; tracing stopped");
        }
    }

    fn connected(&self, connected: &ganja_client::sse::Connected) {
        self.epoch
            .lock()
            .expect("epoch lock")
            .get_or_insert_with(|| connected.head.epoch().to_owned());
        self.traced(|trace| trace.frame(&connected.head, "connected", None));
    }

    /// Current activity. A closed local record is not proof that the worker is gone.
    pub fn phase(&self) -> Phase {
        *self.phase.lock().expect("member phase lock")
    }

    fn set_phase(&self, phase: Phase) {
        *self.phase.lock().expect("member phase lock") = phase;
    }

    fn remember(&self, line: String) {
        teammate::push_recent(&self.recent, line);
    }

    fn unsupported(&self, reason: impl Into<String>) -> Unsupported {
        Unsupported { backend: MemberBackend::Cloud, reason: reason.into() }
    }

    fn client(&self) -> Result<&Client, &'static str> {
        self.client.as_ref().ok_or("the worker endpoint is invalid")
    }
    fn events(&self) -> Result<&Arc<dyn EventSource>, &'static str> {
        self.events.as_ref().ok_or("the worker endpoint is invalid")
    }
    fn session(&self) -> Result<SessionId, &'static str> {
        self.session.lock().expect("member session lock").clone().ok_or("the worker has no session")
    }

    async fn rest<T>(
        &self,
        call: impl Future<Output = Result<T, ClientError>>,
    ) -> Result<T, &'static str> {
        let permit = self.gate.admit(Door::Rest).ok_or("worker request admission is closed")?;
        let result = call.await;
        let outcome = match &result {
            Ok(_) => AttemptOutcome::Serve,
            Err(ClientError::Refused { from_serve: true, .. }) => AttemptOutcome::Serve,
            Err(_) => AttemptOutcome::Uncertain,
        };
        permit.finish(outcome);
        result.map_err(|_| "the worker did not provide the expected response")
    }

    async fn launch_inner(&self) -> Result<(), LaunchFailure> {
        let client = self.client().map_err(LaunchFailure::Refused)?;
        self.set_phase(Phase::Resuming);
        self.runtime
            .provisioner
            .resume(&self.worker)
            .await
            .map_err(|_| LaunchFailure::Refused("resume"))?;
        self.set_phase(Phase::Bootstrapping);
        *self.launch_timeout.lock().expect("launch phase lock") = LaunchFailure::BootstrapTimedOut;
        self.gate.open_rest();
        let request = ganja_client::Bootstrap {
            repository: self.bootstrap.repository.clone(),
            reference: self.bootstrap.reference.clone(),
            branch: self.bootstrap.branch.clone(),
            author: BootstrapAuthor {
                name: self.bootstrap.author.name.clone(),
                email: self.bootstrap.author.email.clone(),
            },
        };
        let bootstrap = self
            .rest(client.bootstrap(&request, self.runtime.deadlines.bootstrap))
            .await
            .map_err(LaunchFailure::Refused)?;
        match bootstrap {
            BootstrapOutcome::Cloned { .. } | BootstrapOutcome::AlreadyServing => {}
            BootstrapOutcome::AmbiguousStartup => return Err(LaunchFailure::AmbiguousStartup),
            BootstrapOutcome::TimedOut | BootstrapOutcome::InProgress => {
                return Err(LaunchFailure::BootstrapTimedOut);
            }
            BootstrapOutcome::Failed { .. } => {
                return Err(LaunchFailure::BootstrapFailed("initialization"));
            }
        }
        *self.launch_timeout.lock().expect("launch phase lock") = LaunchFailure::HealthTimedOut;
        let health = timeout(self.runtime.deadlines.health, async {
            loop {
                if let Ok(health) = self.rest(client.health()).await {
                    return health;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| LaunchFailure::HealthTimedOut)?;
        self.set_phase(Phase::Idle);
        *self.launch_timeout.lock().expect("launch phase lock") = LaunchFailure::SessionTimedOut;
        let sessions = self.rest(client.sessions()).await.map_err(LaunchFailure::Refused)?;
        let session = if sessions.iter().any(|row| row.id == health.session_id) {
            health.session_id
        } else {
            self.rest(client.create_session()).await.map_err(LaunchFailure::Refused)?
        };
        *self.session.lock().expect("member session lock") = Some(session);
        self.gate.close();
        self.gate.open_stream();
        let events = self.events().map_err(LaunchFailure::Refused)?;
        events.release();
        *self.launch_timeout.lock().expect("launch phase lock") = LaunchFailure::ConnectTimedOut;
        let connected = events
            .connect()
            .await
            .map_err(|_| LaunchFailure::Refused("initial event connection"))?
            .ok_or(LaunchFailure::ConnectTimedOut)?;
        self.connected(&connected);
        self.gate.open_rest();
        self.remember("worker ready".to_owned());
        Ok(())
    }

    async fn invalidate_history(&self, phase: Phase, reason: &str) {
        self.gate.close();
        if let Some(events) = &self.events
            && timeout(delivery::OBSERVATION, events.invalidate()).await.is_err()
        {
            self.barrier_failed.store(true, Ordering::SeqCst);
        }
        self.terminal.lock().expect("terminal decision lock").clear();
        self.observed_text.lock().expect("observed text lock").clear();
        self.needs_resync.store(true, Ordering::SeqCst);
        self.set_phase(phase);
        self.remember(reason.to_owned());
    }

    async fn control_action(&self, action: ControlAction) -> ControlOutcome {
        let _transition = tokio::select! {
            biased;
            () = self.lent.cancel.cancelled() => return ControlOutcome::Cancelled,
            transition = self.transition.lock() => transition,
        };
        if self.lent.cancel.is_cancelled() {
            return ControlOutcome::Cancelled;
        }
        let outcome = match action {
            ControlAction::ResumeForInspection => {
                if self.inspect_state().await.is_err() || self.resume_locked().await.is_err() {
                    ControlOutcome::Unsupported
                } else {
                    ControlOutcome::Completed
                }
            }
            ControlAction::Retire => {
                self.lent.cancel.cancel();
                self.cleanup_locked().await;
                if self.phase() == Phase::Gone {
                    ControlOutcome::Completed
                } else {
                    ControlOutcome::UnconfirmedDeletion
                }
            }
            ControlAction::ReportExternalRevert => {
                self.invalidate_history(
                    Phase::Suspended,
                    "external revert reported; cursor invalidated",
                )
                .await;
                ControlOutcome::Completed
            }
        };
        drop(_transition);
        if action == ControlAction::Retire {
            self.observe_final().await;
        }
        outcome
    }

    async fn inspect_state(&self) -> Result<(), &'static str> {
        let state =
            timeout(self.runtime.deadlines.launch, self.runtime.provisioner.state(&self.worker))
                .await
                .map_err(|_| "worker state lookup timed out")?
                .map_err(|_| "worker state lookup failed")?;
        match state {
            WorkerState::Crashed => {
                self.observed_crash.store(true, Ordering::SeqCst);
                self.gate.close();
                timeout(delivery::OBSERVATION, self.events()?.hold())
                    .await
                    .map_err(|_| "worker event barrier timed out")?;
                self.set_phase(Phase::RecoveryRequired);
                Err("worker is crashed; inspect after owner recovery")
            }
            WorkerState::Gone => {
                self.terminal_failure(Phase::Gone, "worker is gone").await;
                Err("worker is gone")
            }
            WorkerState::Transitioning => Err("worker state is still transitioning"),
            WorkerState::Running | WorkerState::Suspended => {
                let recovered = self.observed_crash.swap(false, Ordering::SeqCst);
                let changed = (state == WorkerState::Suspended && self.phase() != Phase::Suspended)
                    || (state == WorkerState::Running && self.phase() == Phase::Suspended);
                if recovered || changed {
                    let phase = if state == WorkerState::Suspended {
                        Phase::Suspended
                    } else {
                        Phase::RecoveryRequired
                    };
                    self.invalidate_history(
                        phase,
                        "external worker transition observed; cursor invalidated",
                    )
                    .await;
                    return Err("worker history changed; resume explicitly after inspection");
                }
                Ok(())
            }
        }
    }

    async fn resume(&self) -> Result<(), &'static str> {
        let _transition = tokio::select! {
            biased;
            () = self.lent.cancel.cancelled() => return Err("member shutdown started"),
            transition = self.transition.lock() => transition,
        };
        self.resume_locked().await
    }

    async fn resume_locked(&self) -> Result<(), &'static str> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err("worker is retired");
        }
        let recovering = self.phase() == Phase::RecoveryRequired;
        if !matches!(self.phase(), Phase::Suspended | Phase::RecoveryRequired) {
            return Ok(());
        }
        if recovering {
            self.gate.close();
            timeout(delivery::OBSERVATION, self.events()?.invalidate())
                .await
                .map_err(|_| "worker event invalidation timed out")?;
            self.needs_resync.store(true, Ordering::SeqCst);
        }
        self.set_phase(Phase::Resuming);
        let result = timeout(self.runtime.deadlines.launch, async {
            let resumed = match self.runtime.provisioner.resume(&self.worker).await {
                Ok(resumed) => resumed,
                Err(error) => match error.kind {
                    ProvisionErrorKind::Crashed => {
                        self.terminal_failure(
                            Phase::Crashed,
                            "worker crashed; owner recovery is required",
                        )
                        .await;
                        return Err("worker crashed");
                    }
                    ProvisionErrorKind::Gone => {
                        self.terminal_failure(Phase::Gone, "worker is gone").await;
                        return Err("worker is gone");
                    }
                    _ => return Err("worker resume failed"),
                },
            };
            if resumed.was_suspended && recovering {
                self.invalidate_history(
                    Phase::RecoveryRequired,
                    "unexpected suspended worker resumed; cursor invalidated",
                )
                .await;
                return Err("worker history changed; resume explicitly after inspection");
            }
            let events = self.events()?;
            self.gate.open_stream();
            events.release();
            let connected = events
                .connect()
                .await
                .map_err(|_| "worker reconnect failed")?
                .ok_or("worker reconnect was held")?;
            self.connected(&connected);
            if connected.replay == ganja_client::sse::Replay::Gap
                || self.needs_resync.load(Ordering::SeqCst)
            {
                let head = if connected.replay == ganja_client::sse::Replay::Gap {
                    let Some(Ok(Resumed::Gap { head })) = events.next().await else {
                        return Err("gap acknowledgement missing");
                    };
                    events
                        .connect()
                        .await
                        .map_err(|_| "fresh registration failed")?
                        .ok_or("fresh registration held")?;
                    head
                } else {
                    connected.head.clone()
                };
                self.gate.open_rest();
                if let Some(pending) = self.recover_gap(&head, None, &BTreeMap::new()).await? {
                    self.wait_for_terminal(None, Some(pending), true).await?;
                }
                self.needs_resync.store(false, Ordering::SeqCst);
            }
            self.gate.open_rest();
            Ok::<_, &'static str>(())
        })
        .await
        .unwrap_or(Err("worker resume timed out"));
        if result.is_ok() {
            self.set_phase(Phase::Idle);
        } else {
            self.gate.close();
            if self.alive.load(Ordering::SeqCst) {
                self.set_phase(Phase::RecoveryRequired);
            }
        }
        result
    }

    async fn terminal_failure(&self, phase: Phase, message: &str) {
        self.gate.close();
        if let Some(events) = &self.events
            && timeout(delivery::OBSERVATION, events.invalidate()).await.is_err()
        {
            self.barrier_failed.store(true, Ordering::SeqCst);
        }
        if self.alive.swap(false, Ordering::SeqCst) {
            self.mail(message.to_owned()).await;
        }
        self.set_phase(phase);
        self.announce_exit();
    }

    async fn quiesce(&self) {
        let _transition = tokio::select! {
            biased;
            () = self.lent.cancel.cancelled() => return,
            transition = self.transition.lock() => transition,
        };
        if !self.alive.load(Ordering::SeqCst) {
            return;
        }
        self.set_phase(Phase::Quiescing);
        self.gate.close();
        if let Some(events) = &self.events
            && timeout(delivery::OBSERVATION, events.hold()).await.is_err()
        {
            self.barrier_failed.store(true, Ordering::SeqCst);
        }
        if self.barrier_failed.load(Ordering::SeqCst) {
            self.reopen();
            self.mail(
                "worker event barrier is unconfirmed; automatic suspend is disabled".to_owned(),
            )
            .await;
            return;
        }
        if self.gate.drain_rest(self.runtime.deadlines.quiesce).await == Drain::Deferred {
            self.set_phase(Phase::SuspendDeferred);
            self.reopen();
            self.mail("a request to the worker is still outstanding; suspend deferred".to_owned())
                .await;
            return;
        }
        if self.gate.state().uncertain {
            self.reopen();
            if !self.uncertainty_reported.swap(true, Ordering::SeqCst) {
                self.remember(
                    "uncertain; automatic suspend off for this worker identity".to_owned(),
                );
                self.mail("an earlier request may still wake this worker; automatic suspend is off until it is retired and replaced".to_owned()).await;
            }
            return;
        }
        self.set_phase(Phase::Suspending);
        match timeout(
            self.runtime.deadlines.quiesce,
            self.runtime.provisioner.suspend(&self.worker),
        )
        .await
        {
            Ok(Ok(SuspendOutcome::Suspended)) => {
                self.set_phase(Phase::Suspended);
                self.remember("suspended".to_owned());
            }
            Ok(Ok(SuspendOutcome::AlreadySuspended)) => {
                self.invalidate_history(
                    Phase::Suspended,
                    "worker was already suspended; cursor invalidated",
                )
                .await;
            }
            Ok(Err(_)) | Err(_) => {
                self.reopen();
                self.mail("worker suspension was not confirmed; admission reopened".to_owned())
                    .await;
            }
        }
    }

    fn reopen(&self) {
        self.gate.open_stream();
        self.gate.open_rest();
        if let Some(events) = &self.events {
            events.release();
        }
        self.set_phase(Phase::Idle);
    }

    async fn cleanup(&self) {
        self.cleanup_work().await;
        self.observe_final().await;
    }

    async fn cleanup_work(&self) {
        self.lent.cancel.cancel();
        {
            let mut task = self.control_task.lock().await;
            // Keep ownership in the member if this cleanup wait is cancelled.
            if let Some(task) = task.as_mut() {
                let _ = task.await;
            }
            *task = None;
        }
        let _transition = self.transition.lock().await;
        self.cleanup_locked().await;
    }

    async fn cleanup_locked(&self) {
        if self.cleaned.swap(true, Ordering::SeqCst) {
            return;
        }
        self.alive.store(false, Ordering::SeqCst);
        self.set_phase(Phase::Unwinding);
        self.gate.close();
        let deadline = Instant::now() + self.runtime.deadlines.cleanup;
        if let Some(events) = &self.events {
            let _ = timeout(self.runtime.deadlines.cleanup / 4, events.hold()).await;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let _ = timeout(remaining / 2, self.runtime.provisioner.kill(&self.worker)).await;
        let gone = matches!(
            timeout_at(deadline, self.runtime.provisioner.state(&self.worker)).await,
            Ok(Ok(WorkerState::Gone))
                | Ok(Err(crate::provisioner::ProvisionError {
                    kind: ProvisionErrorKind::Gone,
                    ..
                }))
        );
        self.set_phase(if gone { Phase::Gone } else { Phase::CleanupUnconfirmed });
        if !gone {
            self.mail(format!(
                "worker deletion is unconfirmed; owner action needed for {}",
                self.runtime.provisioner.describe(&self.worker)
            ))
            .await;
        }
        self.announce_exit();
    }

    fn announce_exit(&self) {
        if !self.exited.swap(true, Ordering::SeqCst) {
            let _ = self.lent.exits.send(Exited {
                name: self.spec.name.as_str().to_owned(),
                cli: None,
                backend: MemberBackend::Cloud,
                pane_id: String::new(),
                pane: PaneFate::Closed,
                last_words: None,
            });
        }
    }

    async fn mail(&self, text: String) {
        self.enqueue(Payload::Write {
            path: self.lent.lead_inbox.clone(),
            envelope: Box::new(MailboxMessage::new(
                self.spec.name.as_str(),
                text,
                record::now_iso8601(),
            )),
        });
    }

    fn frame(&self, frame: &Frame) {
        match MailboxMessage::from_frame(self.spec.name.as_str(), frame, record::now_iso8601()) {
            Ok(envelope) => self.enqueue(Payload::Write {
                path: self.lent.lead_inbox.clone(),
                envelope: Box::new(envelope),
            }),
            Err(_) => tracing::error!("cloud notification encoding failed"),
        }
    }

    fn enqueue(&self, payload: Payload) {
        let mut notifications = self.notifications.lock().expect("notification receipts");
        notifications.retain_mut(|receipt| receipt.take_final().is_none());
        match self.delivery.submit(payload) {
            Ok(receipt) => notifications.push_back(receipt),
            Err(reason) => tracing::warn!(?reason, "cloud notification dropped before admission"),
        }
    }

    async fn observe_final(&self) {
        self.delivery.seal();
        if self.final_observed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.observe_notifications().await;
    }

    async fn observe_notifications(&self) {
        let deadline = Instant::now() + delivery::FINAL_OBSERVATION;
        let notifications =
            std::mem::take(&mut *self.notifications.lock().expect("notification receipts"));
        for mut receipt in notifications {
            let _ = receipt.observe(deadline.saturating_duration_since(Instant::now())).await;
            receipt.detach();
        }
    }

    async fn observe_io(&self, mut receipt: Receipt) -> Option<Value> {
        loop {
            let observation = tokio::select! {
                biased;
                () = self.lent.cancel.cancelled() => return None,
                observation = receipt.observe(delivery::OBSERVATION) => observation,
            };
            match observation {
                Observation::Final(value) => return Some(value),
                Observation::PendingQueued | Observation::Late => {}
            }
        }
    }

    async fn finish(
        &self,
        message: &Message,
        reason: Option<FinishReason>,
        gap: bool,
        observed_bytes: usize,
    ) {
        let first = {
            let mut terminal = self.terminal.lock().expect("terminal decision lock");
            match terminal.entry(message.id.clone()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(reason);
                    true
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    if reason.is_some() {
                        entry.insert(reason);
                    }
                    false
                }
            }
        };
        if !first {
            if let Some(reason) = reason {
                self.remember(format!("finish retained: {reason:?}"));
            }
            return;
        }
        let observed_bytes = observed_bytes.max(
            self.observed_text
                .lock()
                .expect("observed text lock")
                .remove(&message.id)
                .unwrap_or_default(),
        );
        let mut text = message
            .parts
            .iter()
            .filter_map(|part| match &part.body {
                PartBody::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if gap {
            let recovered_bytes: usize =
                message.parts.iter().filter_map(|part| part.as_text()).map(str::len).sum();
            if recovered_bytes < observed_bytes {
                text.push_str("\n\ntext lost");
            }
            text.push_str("\n\nfinal text may be incomplete");
            if reason.is_none() {
                text.push_str("; turn outcome unknown");
            }
        }
        self.mail(text).await;
        let idle_reason = match reason {
            Some(FinishReason::Completed) => IdleReason::Available,
            Some(FinishReason::Failed) => IdleReason::Failed,
            Some(FinishReason::Cancelled) | None => IdleReason::Interrupted,
        };
        self.frame(&Frame::IdleNotification(IdleNotification {
            from: self.spec.name.as_str().to_owned(),
            timestamp: record::now_iso8601(),
            idle_reason: Some(idle_reason),
            summary: None,
            completed_task_id: None,
            completed_status: None,
            failure_reason: None,
        }));
    }

    async fn recover_gap(
        &self,
        head: &ganja_client::EventId,
        after: Option<&MessageId>,
        observed: &BTreeMap<MessageId, usize>,
    ) -> Result<Option<MessageId>, &'static str> {
        let cold = self
            .epoch
            .lock()
            .expect("epoch lock")
            .as_ref()
            .is_some_and(|epoch| epoch != head.epoch());
        if cold {
            self.terminal.lock().expect("terminal decision lock").clear();
        }
        self.traced(|trace| trace.frame(head, "gap", None));
        let events = self.events()?;
        events
            .connect()
            .await
            .map_err(|_| "fresh event registration failed")?
            .ok_or("fresh event registration was held")?;
        let client = self.client()?;
        if !self.rest(client.permissions()).await?.is_empty() {
            self.rest(client.abort(&self.session()?)).await?;
        }
        let messages = self.rest(client.messages(&self.session()?)).await?;
        let mut pending = None;
        for message in messages.iter().filter(|message| {
            message.role == Role::Assistant && after.is_none_or(|after| message.id > *after)
        }) {
            if message.time.completed.is_some() || cold || self.needs_resync.load(Ordering::SeqCst)
            {
                self.finish(
                    message,
                    None,
                    true,
                    observed.get(&message.id).copied().unwrap_or_default(),
                )
                .await;
            } else if !self
                .terminal
                .lock()
                .expect("terminal decision lock")
                .contains_key(&message.id)
            {
                pending = Some(message.id.clone());
            }
        }
        *self.epoch.lock().expect("epoch lock") = Some(head.epoch().to_owned());
        Ok(pending)
    }

    async fn turn(&self, from: &str, text: &str) -> Result<(), &'static str> {
        self.resume().await?;
        let client = self.client()?;
        let session = self.session()?;
        let before = self.rest(client.messages(&session)).await?;
        let last = before.last().map(|message| message.id.clone());
        let prompt_text = runner::envelope(from, text);
        self.set_phase(Phase::Prompting);
        if self.rest(client.prompt(&session, &Prompt::new(&prompt_text))).await.is_err() {
            let mut accepted = false;
            for attempt in 0..2 {
                if attempt != 0 {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                let messages = self.rest(client.messages(&session)).await?;
                accepted = messages.iter().any(|message| message.role == Role::User
                    && last.as_ref().is_none_or(|last| message.id > *last)
                    && message.parts.iter().any(|part| matches!(&part.body, PartBody::Text { text } if text == &prompt_text)));
                if accepted {
                    break;
                }
            }
            if !accepted {
                return Err("prompt acceptance is ambiguous; no prompt was resent");
            }
        }
        self.wait_for_terminal(last, None, false).await
    }

    async fn wait_for_terminal(
        &self,
        last: Option<MessageId>,
        mut current: Option<MessageId>,
        mut gap: bool,
    ) -> Result<(), &'static str> {
        self.set_phase(Phase::Turning);
        let client = self.client()?;
        let session = self.session()?;
        let mut streamed = BTreeMap::<MessageId, Message>::new();
        let mut observed = self.observed_text.lock().expect("observed text lock").clone();
        let mut deadline = Instant::now() + self.runtime.deadlines.terminal;
        loop {
            let item = timeout_at(deadline, self.events()?.next())
                .await
                .map_err(|_| "terminal state unavailable")?;
            let item = item
                .ok_or("event stream held before a terminal response")?
                .map_err(|_| "event stream failed")?;
            if let Resumed::Event(event) = &item
                && let Some(id) = self.events()?.cursor().await
            {
                self.traced(|trace| trace.event(&id, event));
            }
            match item {
                Resumed::Gap { head } => {
                    gap = true;
                    for (id, message) in &streamed {
                        let bytes = message
                            .parts
                            .iter()
                            .filter_map(|part| part.as_text())
                            .map(str::len)
                            .sum();
                        observed
                            .entry(id.clone())
                            .and_modify(|previous| *previous = (*previous).max(bytes))
                            .or_insert(bytes);
                    }
                    *self.observed_text.lock().expect("observed text lock") = observed.clone();
                    streamed.clear();
                    current = self.recover_gap(&head, last.as_ref(), &observed).await?;
                    if current.is_none()
                        && self
                            .terminal
                            .lock()
                            .expect("terminal decision lock")
                            .keys()
                            .any(|id| last.as_ref().is_none_or(|last| id > last))
                    {
                        return Ok(());
                    }
                }
                Resumed::Event(event) => match *event {
                    Event::MessageStarted { message, .. }
                        if message.role == Role::Assistant
                            && last.as_ref().is_none_or(|last| message.id > *last) =>
                    {
                        current = Some(message.id.clone());
                        streamed.insert(message.id.clone(), message);
                        deadline = Instant::now() + self.runtime.deadlines.terminal;
                    }
                    Event::PartStarted { message_id, part, .. }
                    | Event::PartUpdated { message_id, part, .. } => {
                        if let Some(message) = streamed.get_mut(&message_id) {
                            if let Some(existing) =
                                message.parts.iter_mut().find(|existing| existing.id == part.id)
                            {
                                *existing = part;
                            } else {
                                message.parts.push(part);
                            }
                        }
                        if current.as_ref() == Some(&message_id) {
                            deadline = Instant::now() + self.runtime.deadlines.terminal;
                        }
                    }
                    Event::PartDelta { message_id, part_id, delta, .. } => {
                        if let Some(message) = streamed.get_mut(&message_id)
                            && let Some(part) =
                                message.parts.iter_mut().find(|part| part.id == part_id)
                            && let PartBody::Text { text } = &mut part.body
                        {
                            text.push_str(&delta);
                        }
                        if current.as_ref() == Some(&message_id) {
                            deadline = Instant::now() + self.runtime.deadlines.terminal;
                        }
                    }
                    Event::PermissionRequested { .. } => {
                        self.rest(client.abort(&session)).await?;
                    }
                    Event::MessageFinished { message_id, reason, .. }
                        if self
                            .terminal
                            .lock()
                            .expect("terminal decision lock")
                            .contains_key(&message_id) =>
                    {
                        self.terminal
                            .lock()
                            .expect("terminal decision lock")
                            .insert(message_id, Some(reason));
                        self.remember(format!("finish retained: {reason:?}"));
                    }
                    Event::MessageFinished { message_id, reason, .. }
                        if last.as_ref().is_none_or(|last| message_id > *last) =>
                    {
                        let message = if !gap { streamed.remove(&message_id) } else { None };
                        let message = match message {
                            Some(message) => Some(message),
                            None => self
                                .rest(client.messages(&session))
                                .await?
                                .into_iter()
                                .find(|message| message.id == message_id),
                        }
                        .ok_or("terminal message was not available in storage or stream")?;
                        self.finish(
                            &message,
                            Some(reason),
                            gap,
                            observed.get(&message_id).copied().unwrap_or_default(),
                        )
                        .await;
                        return Ok(());
                    }
                    _ => {}
                },
            }
        }
    }

    async fn run(self: Arc<Self>) {
        self.run_inbox().await;
        if !self.lent.cancel.is_cancelled() {
            self.observe_notifications().await;
        }
    }

    async fn run_inbox(&self) {
        while !self.lent.cancel.is_cancelled() && self.alive.load(Ordering::SeqCst) {
            let receipt = self.delivery.submit(Payload::Read { path: self.spec.inbox() });
            if let Ok(receipt) = receipt
                && let Some(Value::Read(Ok(contents))) = self.observe_io(receipt).await
            {
                if let Some((_, message, request)) = runner::shutdown_ahead(&contents.valid) {
                    let request_id = request.request_id.clone();
                    self.cleanup_work().await;
                    self.enqueue(Payload::prune(self.spec.inbox(), std::slice::from_ref(message)));
                    self.frame(&Frame::ShutdownApproved(ganja_protocol::team::ShutdownApproved {
                        request_id,
                        from: self.spec.name.as_str().to_owned(),
                        timestamp: record::now_iso8601(),
                        pane_id: None,
                        backend_type: Some("cloud".to_owned()),
                    }));
                    self.observe_final().await;
                    return;
                }
                for message in contents.valid {
                    let receipt = self
                        .delivery
                        .submit(Payload::prune(self.spec.inbox(), std::slice::from_ref(&message)));
                    let Ok(receipt) = receipt else {
                        break;
                    };
                    if !matches!(self.observe_io(receipt).await, Some(Value::Prune(Ok(_)))) {
                        break;
                    }
                    if self.lent.cancel.is_cancelled() {
                        return;
                    }
                    if !matches!(
                        Frame::classify(&message.text),
                        Tagged::NotAnObject | Tagged::Untagged
                    ) {
                        continue;
                    }
                    let result = tokio::select! {
                        biased;
                        () = self.lent.cancel.cancelled() => return,
                        result = self.turn(&message.from, &message.text) => result,
                    };
                    match result {
                        Ok(()) => self.quiesce().await,
                        Err(reason) => {
                            if !self.alive.load(Ordering::SeqCst) {
                                return;
                            }
                            self.set_phase(Phase::RecoveryRequired);
                            self.mail(format!("recovery required: {reason}")).await;
                        }
                    }
                }
            }
            tokio::select! { () = self.lent.cancel.cancelled() => break, () = tokio::time::sleep(runner::POLL) => {} }
        }
    }
}

#[async_trait]
impl Spawned for CloudMember {
    fn surface(&self) -> Surface {
        Surface::Cloud
    }

    async fn launch(&self) -> Result<(), Unsupported> {
        let _transition = tokio::select! {
            biased;
            () = self.lent.cancel.cancelled() => return Err(self.unsupported("launch cancelled")),
            transition = self.transition.lock() => transition,
        };
        let result = tokio::select! {
            () = self.lent.cancel.cancelled() => Err(LaunchFailure::Refused("launch cancelled")),
            result = timeout(self.runtime.deadlines.launch, self.launch_inner()) => match result {
                Ok(result) => result,
                Err(_) => Err(self.launch_timeout.lock().expect("launch phase lock").clone()),
            },
        };
        result.map_err(|error| self.unsupported(error.to_string()))
    }

    fn start(self: Arc<Self>) -> Vec<JoinHandle<()>> {
        vec![tokio::spawn(self.run())]
    }
    fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
    fn recent(&self) -> Vec<String> {
        self.recent.lock().expect("member recent lock").iter().cloned().collect()
    }
    async fn kill(&self) {
        self.cleanup().await;
    }
}

#[cfg(test)]
#[path = "member_tests.rs"]
mod tests;
