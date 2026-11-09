//! The event source is the only place the member knows the client's reconnect
//! implementation. Admission guards span connections, not just registration.

use std::fmt;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use ganja_client::sse::Connected;
use ganja_client::{
    Client, ClientError, ConnectPolicy, ConnectionPermit, EventId, Resumable, Resumed,
};

use crate::gate::{AttemptOutcome, Door, Permit, WorkerGate};

#[async_trait]
pub(crate) trait EventSource: fmt::Debug + Send + Sync {
    async fn connect(&self) -> Result<Option<Connected>, ClientError>;
    async fn next(&self) -> Option<Result<Resumed, ClientError>>;
    async fn hold(&self);
    fn release(&self);
    async fn invalidate(&self);
    async fn cursor(&self) -> Option<EventId>;
}

#[derive(Debug)]
pub(crate) struct ClientSource(Resumable);

impl ClientSource {
    pub(crate) fn new(client: Client, gate: Arc<WorkerGate>) -> Self {
        Self(
            Resumable::new(client)
                .with_policy(Arc::new(StreamPolicy { gate, current: Mutex::new(Weak::new()) })),
        )
    }
}

#[async_trait]
impl EventSource for ClientSource {
    async fn connect(&self) -> Result<Option<Connected>, ClientError> {
        self.0.connect().await
    }

    async fn next(&self) -> Option<Result<Resumed, ClientError>> {
        self.0.next().await
    }

    async fn hold(&self) {
        // The client has no hidden pump task. Its acknowledgement waits for the
        // in-flight read/attempt to end, drops Events and releases the permit.
        self.0.hold().await;
    }

    fn release(&self) {
        self.0.release();
    }

    async fn invalidate(&self) {
        self.0.invalidate().await;
    }

    async fn cursor(&self) -> Option<EventId> {
        self.0.cursor().await
    }
}

#[derive(Debug)]
struct StreamPolicy {
    gate: Arc<WorkerGate>,
    current: Mutex<Weak<Mutex<Admission>>>,
}

#[derive(Debug)]
struct Admission {
    permit: Option<Permit>,
    observed: bool,
}

impl Drop for Admission {
    fn drop(&mut self) {
        if !self.observed {
            // Hold can win after admission but before the client starts a
            // request. Its contract sends no callback for that case: releasing
            // unused admission must not manufacture an uncertain routed wake.
            if let Some(permit) = self.permit.take() {
                permit.finish(AttemptOutcome::Serve);
            }
        }
    }
}

impl ConnectPolicy for StreamPolicy {
    fn admit(&self) -> futures::future::BoxFuture<'_, Option<ConnectionPermit>> {
        Box::pin(async {
            let permit = self.gate.admit(Door::Stream)?;
            let admission =
                Arc::new(Mutex::new(Admission { permit: Some(permit), observed: false }));
            *self.current.lock().expect("stream observation lock") = Arc::downgrade(&admission);
            Some(Box::new(admission) as ConnectionPermit)
        })
    }

    fn observe(&self, outcome: ganja_client::AttemptOutcome) {
        let admission = self.current.lock().expect("stream observation lock").upgrade();
        let Some(admission) = admission else { return };
        let mut admission = admission.lock().expect("stream admission lock");
        admission.observed = true;
        let outcome = match outcome {
            ganja_client::AttemptOutcome::Connected(_) => AttemptOutcome::Serve,
            ganja_client::AttemptOutcome::Http { from_serve: true, .. } => AttemptOutcome::Serve,
            ganja_client::AttemptOutcome::Http { from_serve: false, .. }
            | ganja_client::AttemptOutcome::Shed
            | ganja_client::AttemptOutcome::TimedOut
            | ganja_client::AttemptOutcome::Transport
            | ganja_client::AttemptOutcome::Cancelled
            | ganja_client::AttemptOutcome::InvalidResponse => AttemptOutcome::Uncertain,
        };
        if let Some(permit) = admission.permit.as_mut() {
            permit.classify(outcome);
        }
    }
}
