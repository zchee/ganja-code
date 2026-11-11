//! Bounded, ordered mailbox work outside Tokio's blocking pool.
//!
//! A fixed native worker service keeps uncancellable mailbox calls outside Tokio
//! shutdown; per-lane FIFO admission and independently supervised expiry bound
//! retained work using standard channels. Existing thread pools do not supply
//! this combination of receipt credit, lane serialization and queued expiry.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use ganja_team::{MailboxMessage, mailbox};
use tokio::sync::Notify;

pub(crate) const OBSERVATION: Duration = Duration::from_millis(250);
pub(crate) const FINAL_OBSERVATION: Duration = Duration::from_millis(250);
#[cfg(test)]
pub(crate) const TEST_SCHEDULER_ALLOWANCE: Duration = Duration::from_secs(2);
const EXPIRY: Duration = Duration::from_secs(1);
const WORKERS: usize = 4;
const CAPACITY: usize = 64;
const LANE_CAPACITY: usize = 8;
const PAYLOAD_CAPACITY: usize = 1024 * 1024;

static SERVICE: LazyLock<Arc<Service>> = LazyLock::new(Service::start);

#[derive(Debug)]
pub(crate) enum Payload {
    Read { path: PathBuf },
    Prune { path: PathBuf, identities: Vec<mailbox::Identity>, bytes: usize },
    Write { path: PathBuf, envelope: Box<MailboxMessage> },
}

impl Payload {
    pub(crate) fn prune(path: PathBuf, messages: &[MailboxMessage]) -> Self {
        Self::Prune {
            path,
            identities: messages.iter().map(mailbox::identity).collect(),
            bytes: messages
                .iter()
                .map(|m| m.from.len() + m.timestamp.len() + m.text.len() + 2)
                .sum(),
        }
    }

    fn bytes(&self) -> usize {
        match self {
            Self::Read { path } => path.as_os_str().len() + PAYLOAD_CAPACITY,
            Self::Prune { path, bytes, .. } => path.as_os_str().len() + bytes,
            Self::Write { path, envelope } => {
                path.as_os_str().len()
                    + serde_json::to_vec(envelope).map_or(usize::MAX / 2, |bytes| bytes.len())
            }
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Read { .. } => "read",
            Self::Prune { .. } => "prune",
            Self::Write { .. } => "write",
        }
    }

    fn execute(self) -> Value {
        match self {
            Self::Read { path } => Value::Read(mailbox::read(&path).map_err(|_| IoFailure)),
            Self::Prune { path, identities, .. } => {
                Value::Prune(mailbox::prune_delivered(&path, &identities).map_err(|_| IoFailure))
            }
            Self::Write { path, envelope } => Value::Write(
                mailbox::write_bounded(
                    &path,
                    *envelope,
                    Some(ganja_core::teammate::postbox::INBOX_CEILING),
                )
                .map_err(|_| IoFailure),
            ),
        }
    }
}

#[derive(Debug)]
pub(crate) struct IoFailure;

#[derive(Debug)]
pub(crate) enum Value {
    Read(Result<mailbox::Contents, IoFailure>),
    Prune(Result<mailbox::Pruned, IoFailure>),
    Write(Result<String, IoFailure>),
    Dropped(DropReason),
}

impl Value {
    fn successful(&self) -> bool {
        matches!(self, Self::Read(Ok(_)) | Self::Prune(Ok(_)) | Self::Write(Ok(_)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DropReason {
    Full,
    Oversize,
    Closed,
    Unavailable,
    Expired,
    Cancelled,
}

#[derive(Debug)]
pub(crate) enum Observation {
    Final(Value),
    PendingQueued,
    Late,
}

#[derive(Debug)]
enum Status {
    Queued,
    Running,
    Final(Option<Value>),
}

#[cfg(test)]
static TEST_EVENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TestTrace {
    pub(crate) admitted: u64,
    pub(crate) started: Option<u64>,
    pub(crate) completed: Option<u64>,
    pub(crate) outcome: Option<&'static str>,
    pub(crate) observation: Option<&'static str>,
    pub(crate) observation_budget: Option<Duration>,
}

#[derive(Debug)]
struct Cell {
    lane: u64,
    sequence: u64,
    kind: &'static str,
    status: Mutex<Status>,
    changed: Notify,
    credited: AtomicBool,
    late: AtomicBool,
    #[cfg(test)]
    trace: Mutex<TestTrace>,
}

impl Cell {
    fn finish(&self, value: Value) {
        let mut status = self.status.lock().expect("delivery status");
        if matches!(*status, Status::Final(_)) {
            return;
        }
        let dropped = match &value {
            Value::Dropped(reason) => Some(*reason),
            _ => None,
        };
        let outcome = match &value {
            Value::Dropped(_) => "Dropped",
            value if value.successful() => {
                if self.late.load(Ordering::SeqCst) {
                    "LateSucceeded"
                } else {
                    "Delivered"
                }
            }
            _ => {
                if self.late.load(Ordering::SeqCst) {
                    "LateFailed"
                } else {
                    "Failed"
                }
            }
        };
        #[cfg(test)]
        {
            let mut trace = self.trace.lock().expect("test delivery trace");
            trace.completed = Some(TEST_EVENT.fetch_add(1, Ordering::SeqCst));
            trace.outcome = Some(outcome);
        }
        *status = Status::Final(Some(value));
        drop(status);
        if let Some(reason) = dropped {
            tracing::warn!(
                lane = self.lane,
                sequence = self.sequence,
                kind = self.kind,
                ?reason,
                "cloud delivery dropped"
            );
        }
        tracing::debug!(
            lane = self.lane,
            sequence = self.sequence,
            kind = self.kind,
            outcome,
            "cloud delivery outcome"
        );
        self.changed.notify_waiters();
    }
}

#[derive(Debug)]
pub(crate) struct Receipt {
    cell: Arc<Cell>,
    wake: mpsc::SyncSender<()>,
    detached: bool,
}

impl Receipt {
    pub(crate) fn take_final(&mut self) -> Option<Value> {
        let mut status = self.cell.status.lock().expect("delivery status");
        if let Status::Final(value) = &mut *status {
            let value = value.take();
            if value.is_some() {
                self.cell.credited.store(false, Ordering::SeqCst);
            }
            value
        } else {
            None
        }
    }

    pub(crate) async fn observe(&mut self, budget: Duration) -> Observation {
        #[cfg(test)]
        {
            self.cell.trace.lock().expect("test delivery trace").observation_budget = Some(budget);
        }
        let _ = tokio::time::timeout(budget, async {
            loop {
                let notified = self.cell.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if matches!(*self.cell.status.lock().expect("delivery status"), Status::Final(_)) {
                    return;
                }
                notified.await;
            }
        })
        .await;
        let mut status = self.cell.status.lock().expect("delivery status");
        let observation = match &mut *status {
            Status::Final(value) => {
                self.cell.credited.store(false, Ordering::SeqCst);
                Observation::Final(value.take().expect("receipt consumed once"))
            }
            Status::Queued => {
                self.cell.late.store(true, Ordering::SeqCst);
                Observation::PendingQueued
            }
            Status::Running => {
                self.cell.late.store(true, Ordering::SeqCst);
                Observation::Late
            }
        };
        drop(status);
        let outcome = match observation {
            Observation::Final(_) => return observation,
            Observation::PendingQueued => "PendingQueued",
            Observation::Late => "Late",
        };
        #[cfg(test)]
        {
            self.cell.trace.lock().expect("test delivery trace").observation = Some(outcome);
            self.cell.changed.notify_waiters();
        }
        tracing::debug!(
            lane = self.cell.lane,
            sequence = self.cell.sequence,
            kind = self.cell.kind,
            outcome,
            "cloud delivery observation"
        );
        observation
    }

    pub(crate) fn detach(mut self) {
        self.detached = true;
    }
}

impl Drop for Receipt {
    fn drop(&mut self) {
        if !self.detached {
            let mut status = self.cell.status.lock().expect("delivery status");
            let cancelled = matches!(*status, Status::Queued);
            if cancelled {
                *status = Status::Final(Some(Value::Dropped(DropReason::Cancelled)));
                #[cfg(test)]
                {
                    let mut trace = self.cell.trace.lock().expect("test delivery trace");
                    trace.completed = Some(TEST_EVENT.fetch_add(1, Ordering::SeqCst));
                    trace.outcome = Some("Dropped");
                }
            } else if matches!(*status, Status::Running) {
                self.cell.late.store(true, Ordering::SeqCst);
            }
            drop(status);
            if cancelled {
                tracing::debug!(
                    lane = self.cell.lane,
                    sequence = self.cell.sequence,
                    kind = self.cell.kind,
                    outcome = "Dropped",
                    reason = "Cancelled",
                    "cloud delivery outcome"
                );
                self.cell.changed.notify_waiters();
            }
            let _ = self.wake.try_send(());
        }
    }
}

#[derive(Debug)]
struct Job {
    payload: Payload,
    cell: Arc<Cell>,
    expiry: Instant,
}

#[derive(Debug, Default)]
struct Queue {
    #[cfg(test)]
    last_read_sequence: u64,
    open: bool,
    sequence: u64,
    active: bool,
    jobs: VecDeque<Job>,
}

#[derive(Debug, Default)]
struct State {
    next_lane: u64,
    lanes: BTreeMap<u64, Queue>,
    credits: Vec<Weak<Cell>>,
}

#[derive(Debug)]
struct Service {
    state: Mutex<State>,
    wake: mpsc::SyncSender<()>,
    unavailable: AtomicBool,
}

impl Service {
    fn start() -> Arc<Self> {
        Self::start_with_failure(None)
    }

    fn start_with_failure(fail_at: Option<usize>) -> Arc<Self> {
        let (wake, receiver) = mpsc::sync_channel(1);
        let service = Arc::new(Self {
            state: Mutex::new(State::default()),
            wake,
            unavailable: AtomicBool::new(false),
        });
        let (completed, completions) = mpsc::sync_channel(WORKERS);
        let mut workers = Vec::with_capacity(WORKERS);
        for index in 0..WORKERS {
            let (sender, jobs) = mpsc::sync_channel::<Job>(1);
            let completed = completed.clone();
            let wake = service.wake.clone();
            if fail_at == Some(index)
                || thread::Builder::new()
                    .name(format!("cloud-mailbox-{index}"))
                    .spawn(move || {
                        while let Ok(job) = jobs.recv() {
                            let start = {
                                let mut status = job.cell.status.lock().expect("delivery status");
                                if !matches!(*status, Status::Queued)
                                    || Instant::now() >= job.expiry
                                {
                                    false
                                } else {
                                    *status = Status::Running;
                                    #[cfg(test)]
                                    {
                                        job.cell
                                            .trace
                                            .lock()
                                            .expect("test delivery trace")
                                            .started =
                                            Some(TEST_EVENT.fetch_add(1, Ordering::SeqCst));
                                    }
                                    true
                                }
                            };
                            let value = if start {
                                tracing::debug!(
                                    lane = job.cell.lane,
                                    sequence = job.cell.sequence,
                                    kind = job.cell.kind,
                                    "cloud delivery started"
                                );
                                job.payload.execute()
                            } else {
                                let reason =
                                    match &*job.cell.status.lock().expect("delivery status") {
                                        Status::Final(Some(Value::Dropped(reason))) => *reason,
                                        _ => DropReason::Expired,
                                    };
                                Value::Dropped(reason)
                            };
                            if completed.send((index, job.cell, value)).is_err() {
                                break;
                            }
                            let _ = wake.try_send(());
                        }
                    })
                    .is_err()
            {
                service.unavailable.store(true, Ordering::SeqCst);
                return service;
            }
            workers.push(sender);
        }
        let supervisor = Arc::clone(&service);
        if fail_at == Some(WORKERS)
            || thread::Builder::new()
                .name("cloud-mailbox-deadlines".to_owned())
                .spawn(move || {
                    supervisor.supervise(receiver, workers, completions);
                })
                .is_err()
        {
            service.unavailable.store(true, Ordering::SeqCst);
        }
        service
    }

    fn supervise(
        &self,
        wake: mpsc::Receiver<()>,
        workers: Vec<mpsc::SyncSender<Job>>,
        completions: mpsc::Receiver<(usize, Arc<Cell>, Value)>,
    ) {
        let mut busy = [false; WORKERS];
        loop {
            while let Ok((index, cell, value)) = completions.try_recv() {
                cell.finish(value);
                let mut state = self.state.lock().expect("delivery service");
                if let Some(queue) = state.lanes.get_mut(&cell.lane) {
                    queue.active = false;
                }
                busy[index] = false;
            }
            let mut discarded = Vec::new();
            let mut dispatch = Vec::new();
            let mut wait = EXPIRY;
            {
                let mut state = self.state.lock().expect("delivery service");
                for queue in state.lanes.values_mut() {
                    queue.jobs.retain(|job| {
                        let queued = matches!(
                            *job.cell.status.lock().expect("delivery status"),
                            Status::Queued
                        );
                        if !queued || Instant::now() >= job.expiry {
                            if queued {
                                discarded.push(Arc::clone(&job.cell));
                            }
                            false
                        } else {
                            wait = wait.min(job.expiry.saturating_duration_since(Instant::now()));
                            true
                        }
                    });
                    if !queue.active
                        && let Some(index) = busy.iter().position(|busy| !busy)
                        && let Some(job) = queue.jobs.pop_front()
                    {
                        queue.active = true;
                        busy[index] = true;
                        dispatch.push((index, job));
                    }
                }
                state.credits.retain(|cell| {
                    cell.upgrade().is_some_and(|cell| cell.credited.load(Ordering::SeqCst))
                });
                state.lanes.retain(|_, queue| queue.open || queue.active || !queue.jobs.is_empty());
            }
            for cell in discarded {
                cell.finish(Value::Dropped(DropReason::Expired));
            }
            for (index, job) in dispatch {
                if let Err(error) = workers[index].send(job) {
                    self.unavailable.store(true, Ordering::SeqCst);
                    error.0.cell.finish(Value::Dropped(DropReason::Unavailable));
                }
            }
            let _ = wake.recv_timeout(wait);
        }
    }
}

#[derive(Debug)]
pub(crate) struct Lane {
    service: Arc<Service>,
    id: u64,
}

impl Lane {
    pub(crate) fn new() -> Self {
        Self::on(Arc::clone(&SERVICE))
    }

    fn on(service: Arc<Service>) -> Self {
        let id = {
            let mut state = service.state.lock().expect("delivery service");
            state.next_lane += 1;
            let id = state.next_lane;
            state.lanes.insert(id, Queue { open: true, ..Queue::default() });
            id
        };
        Self { service, id }
    }

    pub(crate) fn submit(&self, payload: Payload) -> Result<Receipt, DropReason> {
        let kind = payload.kind();
        let result = self.admit(payload);
        if let Err(reason) = &result {
            tracing::warn!(
                lane = self.id,
                kind,
                ?reason,
                "cloud delivery dropped before admission"
            );
        }
        result
    }

    fn admit(&self, payload: Payload) -> Result<Receipt, DropReason> {
        let bytes = payload.bytes();
        if bytes > PAYLOAD_CAPACITY && !matches!(payload, Payload::Read { .. }) {
            return Err(DropReason::Oversize);
        }
        if self.service.unavailable.load(Ordering::SeqCst) {
            return Err(DropReason::Unavailable);
        }
        let mut state = self.service.state.lock().expect("delivery service");
        state
            .credits
            .retain(|cell| cell.upgrade().is_some_and(|cell| cell.credited.load(Ordering::SeqCst)));
        if state.credits.len() >= CAPACITY
            || state
                .credits
                .iter()
                .filter(|cell| cell.upgrade().is_some_and(|cell| cell.lane == self.id))
                .count()
                >= LANE_CAPACITY
        {
            return Err(DropReason::Full);
        }
        let queue = state.lanes.get_mut(&self.id).ok_or(DropReason::Closed)?;
        if !queue.open {
            return Err(DropReason::Closed);
        }
        queue.sequence += 1;
        #[cfg(test)]
        if matches!(&payload, Payload::Read { .. }) {
            queue.last_read_sequence = queue.sequence;
        }
        let cell = Arc::new(Cell {
            lane: self.id,
            sequence: queue.sequence,
            kind: payload.kind(),
            status: Mutex::new(Status::Queued),
            changed: Notify::new(),
            credited: AtomicBool::new(true),
            late: AtomicBool::new(false),
            #[cfg(test)]
            trace: Mutex::new(TestTrace {
                admitted: TEST_EVENT.fetch_add(1, Ordering::SeqCst),
                ..TestTrace::default()
            }),
        });
        queue.jobs.push_back(Job {
            payload,
            cell: Arc::clone(&cell),
            expiry: Instant::now() + EXPIRY,
        });
        state.credits.push(Arc::downgrade(&cell));
        drop(state);
        tracing::debug!(
            lane = cell.lane,
            sequence = cell.sequence,
            kind = cell.kind,
            "cloud delivery admitted"
        );
        let _ = self.service.wake.try_send(());
        Ok(Receipt { cell, wake: self.service.wake.clone(), detached: false })
    }

    #[cfg(test)]
    pub(crate) fn global_counts(&self) -> (usize, usize, usize) {
        let state = self.service.state.lock().expect("delivery service");
        let jobs = state
            .credits
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|cell| cell.credited.load(Ordering::SeqCst))
            .count();
        let active = state.lanes.values().filter(|queue| queue.active).count();
        (state.lanes.len(), jobs, active)
    }

    #[cfg(test)]
    pub(crate) fn last_read_sequence(&self) -> u64 {
        self.service
            .state
            .lock()
            .expect("delivery service")
            .lanes
            .get(&self.id)
            .expect("live lane")
            .last_read_sequence
    }

    #[cfg(test)]
    pub(crate) fn last_sequence(&self) -> u64 {
        self.service
            .state
            .lock()
            .expect("delivery service")
            .lanes
            .get(&self.id)
            .expect("live lane")
            .sequence
    }

    #[cfg(test)]
    pub(crate) fn watch(&self, kind: &str) -> JobWatch {
        let cell = self
            .service
            .state
            .lock()
            .expect("delivery service")
            .credits
            .iter()
            .filter_map(Weak::upgrade)
            .find(|cell| cell.lane == self.id && cell.kind == kind)
            .expect("an admitted operation");
        JobWatch(cell)
    }

    #[cfg(test)]
    pub(crate) fn watch_sequence(&self, sequence: u64) -> JobWatch {
        let cell = self
            .service
            .state
            .lock()
            .expect("delivery service")
            .credits
            .iter()
            .filter_map(Weak::upgrade)
            .find(|cell| cell.lane == self.id && cell.sequence == sequence)
            .expect("an admitted sequence");
        JobWatch(cell)
    }

    #[cfg(test)]
    pub(crate) fn outstanding(&self) -> usize {
        self.service
            .state
            .lock()
            .expect("delivery service")
            .credits
            .iter()
            .filter(|cell| {
                cell.upgrade().is_some_and(|cell| {
                    cell.lane == self.id && cell.credited.load(Ordering::SeqCst)
                })
            })
            .count()
    }

    #[cfg(test)]
    pub(crate) fn running(&self, kind: &str) -> bool {
        self.service.state.lock().expect("delivery service").credits.iter().any(|cell| {
            cell.upgrade().is_some_and(|cell| {
                cell.lane == self.id
                    && cell.kind == kind
                    && matches!(*cell.status.lock().expect("delivery status"), Status::Running)
            })
        })
    }

    pub(crate) fn seal(&self) {
        let mut state = self.service.state.lock().expect("delivery service");
        let remove = state.lanes.get_mut(&self.id).is_some_and(|queue| {
            queue.open = false;
            queue.jobs.is_empty() && !queue.active
        });
        if remove {
            state.lanes.remove(&self.id);
        }
        drop(state);
        let _ = self.service.wake.try_send(());
    }
}

impl Drop for Lane {
    fn drop(&mut self) {
        self.seal();
    }
}

#[cfg(test)]
pub(crate) struct JobWatch(Arc<Cell>);

#[cfg(test)]
impl JobWatch {
    pub(crate) fn sequence(&self) -> u64 {
        self.0.sequence
    }

    pub(crate) async fn observed(&self, expected: &str) {
        loop {
            let notified = self.0.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.snapshot().observation == Some(expected) {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn snapshot(&self) -> TestTrace {
        *self.0.trace.lock().expect("test delivery trace")
    }

    pub(crate) async fn settled(&self) {
        loop {
            let notified = self.0.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if matches!(*self.0.status.lock().expect("delivery status"), Status::Final(_)) {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
#[path = "delivery_tests.rs"]
mod tests;
