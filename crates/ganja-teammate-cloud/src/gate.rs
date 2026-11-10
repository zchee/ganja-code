//! Request admission is separate from lifecycle control: closing a door must
//! never block the stream's hold acknowledgement or a control-plane delete.
//!
//! The state is two counters and two admission flags under one standard mutex;
//! a general state-machine crate adds no synchronization guarantee to this seam.
//! Tokio's notification primitive supplies the cancellable drain wait.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

/// Which independent admission door a request uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Door {
    /// Held for the entire lifetime of one event-stream connection.
    Stream,
    /// Held until one REST request and its response have completed.
    Rest,
}

/// Whether an attempt was answered by the worker's own authenticated server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// A typed answer from the worker proves this attempt reached serve.
    Serve,
    /// A router error, transport failure, timeout, or cancellation proves no such thing.
    Uncertain,
}

/// The result of waiting for outstanding REST requests to finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drain {
    /// Every REST permit has been released.
    Complete,
    /// A permit is still held. This alone does not imply an uncertain wake.
    Deferred,
}

/// Observable admission state, without request contents or credentials.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GateState {
    /// Whether a new stream attempt can begin.
    pub stream_open: bool,
    /// Whether a new REST call can begin.
    pub rest_open: bool,
    /// Number of live stream connections or attempts.
    pub streams: usize,
    /// Number of unfinished REST calls.
    pub rest: usize,
    /// Permanent for this gate's worker identity, even after a later health answer.
    pub uncertain: bool,
}

/// Admission and completion accounting for exactly one worker identity.
#[derive(Debug, Default)]
pub struct WorkerGate {
    state: Mutex<GateState>,
    changed: Notify,
}

impl WorkerGate {
    /// Creates a closed gate: a newly allocated worker is still suspended.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Returns the current admission and completion counts.
    pub fn state(&self) -> GateState {
        *self.state.lock().expect("gate state is never held across user code")
    }

    /// Opens only stream admission; REST remains closed until the connected ack.
    pub fn open_stream(&self) {
        self.state.lock().expect("gate state is never held across user code").stream_open = true;
    }

    /// Opens REST after the connected acknowledgement has been received.
    pub fn open_rest(&self) {
        self.state.lock().expect("gate state is never held across user code").rest_open = true;
    }

    /// Atomically stops both kinds of new request.
    pub fn close(&self) {
        let mut state = self.state.lock().expect("gate state is never held across user code");
        state.stream_open = false;
        state.rest_open = false;
    }

    /// Admits a request only while its door is open.
    pub fn admit(self: &Arc<Self>, door: Door) -> Option<Permit> {
        let mut state = self.state.lock().expect("gate state is never held across user code");
        match door {
            Door::Stream if state.stream_open => state.streams += 1,
            Door::Rest if state.rest_open => state.rest += 1,
            Door::Stream | Door::Rest => return None,
        }
        Some(Permit { gate: Arc::clone(self), door, classified: false })
    }

    /// Waits for REST completions without cancelling them or changing the latch.
    ///
    /// A timeout is only a deferred suspend. When a caller later drops an
    /// unfinished request, that permit's own drop records the uncertain attempt.
    pub async fn drain_rest(&self, deadline: Duration) -> Drain {
        let drained = async {
            loop {
                let notified = self.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.state().rest == 0 {
                    return;
                }
                notified.await;
            }
        };
        match tokio::time::timeout(deadline, drained).await {
            Ok(()) => Drain::Complete,
            Err(_) => Drain::Deferred,
        }
    }
}

/// One admitted request. Dropping it without a classified serve answer latches uncertainty.
#[derive(Debug)]
pub struct Permit {
    gate: Arc<WorkerGate>,
    door: Door,
    classified: bool,
}

impl Permit {
    /// Records a typed serve answer while retaining the stream's lifetime permit.
    /// An earlier uncertain attempt cannot be undone by this answer.
    pub fn classify(&mut self, outcome: AttemptOutcome) {
        if outcome == AttemptOutcome::Uncertain {
            self.gate.state.lock().expect("gate state is never held across user code").uncertain =
                true;
        }
        self.classified = true;
    }

    /// Records the attempt's outcome and releases its permit.
    pub fn finish(mut self, outcome: AttemptOutcome) {
        self.classify(outcome);
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock().expect("gate state is never held across user code");
        if !self.classified {
            state.uncertain = true;
        }
        match self.door {
            Door::Stream => state.streams -= 1,
            Door::Rest => state.rest -= 1,
        }
        drop(state);
        self.gate.changed.notify_waiters();
    }
}

#[cfg(test)]
#[path = "gate_tests.rs"]
mod tests;
