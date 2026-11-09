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
use ganja_team::{MailboxMessage, Surface, mailbox, record};
use secrecy::ExposeSecret as _;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout, timeout_at};

use crate::backend::Runtime;
use crate::gate::{AttemptOutcome, Door, Drain, WorkerGate};
use crate::provisioner::{Bootstrap, ProvisionErrorKind, Worker, WorkerState};
use crate::state::{LaunchFailure, Phase};
use crate::trace::TraceLog;
use crate::transport::{ClientSource, EventSource};

static CONTROLS: LazyLock<Mutex<BTreeMap<String, Weak<CloudMember>>>> =
    LazyLock::new(Mutex::default);

pub(crate) fn register(member: &Arc<CloudMember>) {
    CONTROLS
        .lock()
        .expect("cloud control map lock")
        .insert(member.spec.name.as_str().to_owned(), Arc::downgrade(member));
}

/// Looks up a live hosted member without retaining it in a global registry.
pub fn control(name: &str) -> Option<CloudControl> {
    CONTROLS.lock().expect("cloud control map lock").get(name)?.upgrade().map(CloudControl)
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

    /// Resumes through the ordinary connected-ack barrier for explicit inspection.
    ///
    /// # Errors
    /// Returns a sanitized lifecycle failure, never an endpoint or credential.
    pub async fn resume_for_inspection(&self) -> Result<(), Unsupported> {
        self.0.resume().await.map_err(|reason| self.0.unsupported(reason))
    }

    /// Ends the worker in any state, then confirms absence once.
    ///
    /// # Errors
    /// Unconfirmed deletion leaves an owner-action notice and is not called gone.
    pub async fn retire(&self) -> Result<(), Unsupported> {
        self.0.cleanup().await;
        if self.0.phase() == Phase::Gone {
            Ok(())
        } else {
            Err(self.0.unsupported("worker deletion is unconfirmed"))
        }
    }

    /// Invalidates a checkpointed cursor before any request follows an owner-reported revert.
    pub async fn report_external_revert(&self) {
        let _transition = self.0.transition.lock().await;
        self.0.gate.close();
        if let Some(events) = &self.0.events {
            events.invalidate().await;
        }
        self.0.needs_resync.store(true, Ordering::SeqCst);
        self.0.set_phase(Phase::Suspended);
        self.0.remember("external revert reported; cursor invalidated".to_owned());
    }
}

/// The runtime of one hosted teammate, including exactly-once local notifications.
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
    phase: Mutex<Phase>,
    session: Mutex<Option<SessionId>>,
    recent: Mutex<VecDeque<String>>,
    alive: AtomicBool,
    cleaned: AtomicBool,
    exited: AtomicBool,
    terminal: Mutex<BTreeMap<MessageId, Option<FinishReason>>>,
    launch_timeout: Mutex<LaunchFailure>,
    uncertainty_reported: AtomicBool,
    trace: Mutex<Option<TraceLog>>,
    epoch: Mutex<Option<String>>,
    needs_resync: AtomicBool,
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
        let client = Client::new(
            &endpoint.base_url,
            Some(ganja_client::Credentials::new(
                &endpoint.username,
                endpoint.password.expose_secret(),
            )),
        )
        .map(|client| client.with_headers(endpoint.headers.clone()))
        .ok();
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
            phase: Mutex::new(Phase::Creating),
            session: Mutex::new(None),
            recent: Mutex::new(VecDeque::new()),
            alive: AtomicBool::new(true),
            cleaned: AtomicBool::new(false),
            exited: AtomicBool::new(false),
            terminal: Mutex::new(BTreeMap::new()),
            launch_timeout: Mutex::new(LaunchFailure::ResumeTimedOut),
            uncertainty_reported: AtomicBool::new(false),
            trace: Mutex::new(trace),
            epoch: Mutex::new(None),
            needs_resync: AtomicBool::new(false),
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

    async fn resume(&self) -> Result<(), &'static str> {
        let _transition = self.transition.lock().await;
        if !self.alive.load(Ordering::SeqCst) {
            return Err("worker is retired");
        }
        let recovering = self.phase() == Phase::RecoveryRequired;
        if !matches!(self.phase(), Phase::Suspended | Phase::RecoveryRequired) {
            return Ok(());
        }
        if recovering {
            self.gate.close();
            self.events()?.invalidate().await;
            self.needs_resync.store(true, Ordering::SeqCst);
        }
        self.set_phase(Phase::Resuming);
        let result = timeout(self.runtime.deadlines.launch, async {
            if let Err(error) = self.runtime.provisioner.resume(&self.worker).await {
                match error.kind {
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
                }
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
                if self.recover_gap(&head, None).await?.is_some() {
                    return Err("terminal state requires recovery");
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
        if let Some(events) = &self.events {
            events.invalidate().await;
        }
        if self.alive.swap(false, Ordering::SeqCst) {
            self.mail(message.to_owned()).await;
        }
        self.set_phase(phase);
        self.announce_exit();
    }

    async fn quiesce(&self) {
        let _transition = self.transition.lock().await;
        if !self.alive.load(Ordering::SeqCst) {
            return;
        }
        self.set_phase(Phase::Quiescing);
        self.gate.close();
        if let Some(events) = &self.events {
            events.hold().await;
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
            Ok(Ok(())) => {
                self.set_phase(Phase::Suspended);
                self.remember("suspended".to_owned());
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
        self.lent.cancel.cancel();
        let _transition = self.transition.lock().await;
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
        let message = MailboxMessage::new(self.spec.name.as_str(), text, record::now_iso8601());
        let inbox = self.lent.lead_inbox.clone();
        if teammate::blocking_io(move || {
            mailbox::write_bounded(&inbox, message, Some(teammate::postbox::INBOX_CEILING))
        })
        .await
        .is_err()
        {
            tracing::error!(
                member = self.spec.name.as_str(),
                "cloud notification could not be delivered"
            );
        }
    }

    async fn finish(&self, message: &Message, reason: Option<FinishReason>, gap: bool) {
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
            text.push_str("\n\nfinal text may be incomplete; turn outcome unknown");
        }
        self.mail(text).await;
        let idle_reason = match reason {
            Some(FinishReason::Completed) => IdleReason::Available,
            Some(FinishReason::Failed) => IdleReason::Failed,
            Some(FinishReason::Cancelled) | None => IdleReason::Interrupted,
        };
        runner::write_frame(
            self.lent.lead_inbox.clone(),
            self.spec.name.as_str(),
            &Frame::IdleNotification(IdleNotification {
                from: self.spec.name.as_str().to_owned(),
                timestamp: record::now_iso8601(),
                idle_reason: Some(idle_reason),
                summary: None,
                completed_task_id: None,
                completed_status: None,
                failure_reason: None,
            }),
            "cloud turn completion",
        )
        .await;
    }

    async fn recover_gap(
        &self,
        head: &ganja_client::EventId,
        after: Option<&MessageId>,
    ) -> Result<Option<MessageId>, &'static str> {
        let cold = self
            .epoch
            .lock()
            .expect("epoch lock")
            .as_ref()
            .is_some_and(|epoch| epoch != head.epoch());
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
                self.finish(message, None, true).await;
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
        self.set_phase(Phase::Turning);
        let mut current = None;
        let mut streamed = BTreeMap::<MessageId, Message>::new();
        let mut gap = false;
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
                    streamed.clear();
                    current = self.recover_gap(&head, last.as_ref()).await?;
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
                        self.finish(&message, Some(reason), gap).await;
                        return Ok(());
                    }
                    _ => {}
                },
            }
        }
    }

    async fn run(self: Arc<Self>) {
        while !self.lent.cancel.is_cancelled() && self.alive.load(Ordering::SeqCst) {
            let contents = runner::read_inbox(self.spec.inbox(), self.spec.name.as_str()).await;
            if let Some(contents) = contents {
                if let Some((_, message, request)) = runner::shutdown_ahead(&contents.valid) {
                    let request_id = request.request_id.clone();
                    runner::prune_inbox(
                        self.spec.inbox(),
                        vec![mailbox::identity(message)],
                        self.spec.name.as_str(),
                    )
                    .await;
                    self.cleanup().await;
                    runner::write_frame(
                        self.lent.lead_inbox.clone(),
                        self.spec.name.as_str(),
                        &Frame::ShutdownApproved(ganja_protocol::team::ShutdownApproved {
                            request_id,
                            from: self.spec.name.as_str().to_owned(),
                            timestamp: record::now_iso8601(),
                            pane_id: None,
                            backend_type: Some("cloud".to_owned()),
                        }),
                        "cloud shutdown acknowledgement",
                    )
                    .await;
                    return;
                }
                for message in contents.valid {
                    let identity = mailbox::identity(&message);
                    if !matches!(
                        Frame::classify(&message.text),
                        Tagged::NotAnObject | Tagged::Untagged
                    ) {
                        runner::prune_inbox(
                            self.spec.inbox(),
                            vec![identity],
                            self.spec.name.as_str(),
                        )
                        .await;
                        continue;
                    }
                    runner::prune_inbox(self.spec.inbox(), vec![identity], self.spec.name.as_str())
                        .await;
                    let result = tokio::select! {
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
        let _transition = self.transition.lock().await;
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
