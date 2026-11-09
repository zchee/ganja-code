//! Scriptable cloud control plane. Delayed router wakes are independent tasks:
//! a later response, including an owner-triggered suspend, never joins them.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
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
        Ok(Worker {
            handle: WorkerHandle::new(spec.name.as_str()),
            endpoint: self.endpoint.clone(),
        })
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
