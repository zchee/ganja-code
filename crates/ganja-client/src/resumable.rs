//! Pull-driven reconnects with an acknowledged control path.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt as _;
use tokio::sync::{Mutex, Notify};

use crate::sse::Replay;
use crate::{Client, ClientError, Event, EventId, Events};

/// A caller-owned admission guard. Dropping it releases the admission.
pub type ConnectionPermit = Box<dyn Send + Sync>;

/// One classified connection attempt, separate from conversation events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// The worker supplied a typed registration acknowledgement.
    Connected(crate::sse::Connected),
    /// An HTTP refusal, whose provenance the caller must assess.
    Http {
        /// HTTP response status.
        status: u16,
        /// Whether the response carried a well-formed server epoch.
        from_serve: bool,
    },
    /// A 503 shed the request before a registration acknowledgement.
    Shed,
    /// The connect or first-read budget expired.
    TimedOut,
    /// The transport failed before a registration acknowledgement.
    Transport,
    /// The caller stopped an admitted attempt before it completed.
    Cancelled,
    /// A successful response did not carry the expected wire shape.
    InvalidResponse,
}

/// Admission and observation for every attempt, including automatic reconnects.
///
/// Observers must not block; the callback runs before hold acknowledgement.
pub trait ConnectPolicy: std::fmt::Debug + Send + Sync {
    /// Waits for admission. `None` refuses this attempt without sending a request.
    /// The guard is released on failure, or when the successful connection drops.
    fn admit(&self) -> futures::future::BoxFuture<'_, Option<ConnectionPermit>>;

    /// Receives exactly one outcome for each admitted attempt.
    fn observe(&self, outcome: AttemptOutcome);
}

/// One event or an explicit boundary where the caller must reload state.
#[derive(Clone, Debug, PartialEq)]
pub enum Resumed {
    /// A complete event, delivered exactly once within retained history.
    Event(Box<Event>),
    /// History is unavailable. Register fresh before reading REST projections.
    Gap {
        /// Journal head at the failed replay registration.
        head: EventId,
    },
}

/// A pull-driven consumer that reconnects from its last delivered event.
///
/// No task runs in the background. Call [`Self::next`] to receive events and
/// use [`Self::hold`] from a separate control path to stop an in-flight read
/// or connection attempt. A hold wakes the reader without waiting for network
/// admission; it returns only after the response or request future is dropped.
#[derive(Debug)]
pub struct Resumable {
    client: Client,
    state: Mutex<State>,
    held: AtomicBool,
    changed: Notify,
    policy: Option<Arc<dyn ConnectPolicy>>,
}

#[derive(Default)]
struct State {
    events: Option<Events>,
    permit: Option<ConnectionPermit>,
    cursor: Option<EventId>,
    retry: Duration,
    terminal: bool,
    gap: Option<EventId>,
}

impl std::fmt::Debug for State {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("State")
            .field("cursor", &self.cursor)
            .field("held_connection", &self.events.is_some())
            .finish_non_exhaustive()
    }
}

impl Resumable {
    /// Starts a fresh consumer. Connections are made only while it is polled.
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client,
            state: Mutex::new(State::default()),
            held: AtomicBool::new(false),
            changed: Notify::new(),
            policy: None,
        }
    }

    /// Installs admission and observation for all connection attempts.
    #[must_use]
    pub fn with_policy(mut self, policy: Arc<dyn ConnectPolicy>) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Stops the current request/read and prevents another attempt until release.
    ///
    /// This acknowledgement does not prove a remote router cancelled its work;
    /// callers must classify a cancelled connection attempt accordingly.
    pub async fn hold(&self) {
        self.held.store(true, Ordering::SeqCst);
        self.changed.notify_waiters();
        let mut state = self.state.lock().await;
        state.events = None;
        state.permit = None;
    }

    /// Allows subsequent reads to reconnect from the committed cursor.
    pub fn release(&self) {
        self.held.store(false, Ordering::SeqCst);
    }

    /// Discards the cursor and any buffered stream before the next connection.
    ///
    /// Call while held when invalidating a checkpointed server incarnation.
    pub async fn invalidate(&self) {
        self.hold().await;
        let mut state = self.state.lock().await;
        state.cursor = None;
        state.retry = Duration::ZERO;
        state.terminal = false;
        state.gap = None;
    }

    /// The last yielded position or fresh registration head.
    pub async fn cursor(&self) -> Option<EventId> {
        self.state.lock().await.cursor.clone()
    }

    /// Registers a connection without waiting for its first conversation event.
    ///
    /// Returns `None` if held. A gap remains available as the next yielded item;
    /// call again after consuming that gap to register fresh before REST reads.
    ///
    /// # Errors
    ///
    /// The same transport, authentication and protocol errors as `events_from`.
    pub async fn connect(&self) -> Result<Option<crate::sse::Connected>, ClientError> {
        let mut state = self.state.lock().await;
        let changed = self.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if self.held.load(Ordering::SeqCst) || state.terminal {
            return Ok(None);
        }
        if let Some(events) = &state.events {
            return Ok(Some(events.connected().clone()));
        }
        let admitted = tokio::select! {
            biased;
            () = &mut changed => return Ok(None),
            result = self.attempt(state.cursor.clone()) => result?,
        };
        let Some((events, permit)) = admitted else { return Ok(None) };
        let connected = events.connected().clone();
        if connected.replay == Replay::Gap {
            state.cursor = None;
            state.gap = Some(connected.head.clone());
        } else {
            state.cursor = events.cursor().cloned();
            state.events = Some(events);
            state.permit = permit;
        }
        Ok(Some(connected))
    }

    async fn attempt(
        &self,
        cursor: Option<EventId>,
    ) -> Result<Option<(Events, Option<ConnectionPermit>)>, ClientError> {
        let permit = if let Some(policy) = &self.policy {
            let Some(permit) = policy.admit().await else { return Ok(None) };
            Some(permit)
        } else {
            None
        };
        if self.held.load(Ordering::SeqCst) {
            return Ok(None);
        }
        let mut attempt = Attempt { policy: self.policy.as_deref(), finished: false };
        let result = self.client.events_from(cursor).await;
        let outcome = match &result {
            Ok(events) => AttemptOutcome::Connected(events.connected().clone()),
            Err(ClientError::Silent { .. }) => AttemptOutcome::TimedOut,
            Err(ClientError::Transport { source, .. }) if source.is_timeout() => {
                AttemptOutcome::TimedOut
            }
            Err(ClientError::Transport { .. } | ClientError::Disconnected { .. }) => {
                AttemptOutcome::Transport
            }
            Err(ClientError::Refused { status: 503, from_serve: false, .. }) => {
                AttemptOutcome::Shed
            }
            Err(ClientError::Refused { status, from_serve, .. }) => {
                AttemptOutcome::Http { status: *status, from_serve: *from_serve }
            }
            Err(ClientError::Unauthorized { from_serve, .. }) => {
                AttemptOutcome::Http { status: 401, from_serve: *from_serve }
            }
            Err(_) => AttemptOutcome::InvalidResponse,
        };
        attempt.finish(outcome);
        result.map(|events| Some((events, permit)))
    }

    /// Receives an event, reconnecting on transport loss with bounded backoff.
    ///
    /// A hold returns `None` without making the consumer terminal; release and
    /// poll again to continue. A gap drops the old stream and cursor; the next
    /// poll registers fresh, never splicing the discarded tail into new state.
    /// Protocol skew, eviction, and explicit HTTP refusal are terminal errors.
    pub async fn next(&self) -> Option<Result<Resumed, ClientError>> {
        let mut state = self.state.lock().await;
        loop {
            // Register the notification before inspecting the flag: otherwise a
            // hold between those operations could leave an in-flight read asleep.
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.held.load(Ordering::SeqCst) || state.terminal {
                return None;
            }
            if let Some(head) = state.gap.take() {
                return Some(Ok(Resumed::Gap { head }));
            }
            if state.events.is_none() {
                if !state.retry.is_zero() {
                    tokio::select! {
                        biased;
                        () = &mut changed => return None,
                        () = tokio::time::sleep(state.retry) => {}
                    }
                }
                let connected = tokio::select! {
                    biased;
                    () = &mut changed => return None,
                    connected = self.attempt(state.cursor.clone()) => connected,
                };
                match connected {
                    Ok(Some((events, permit))) => {
                        if events.connected().replay == Replay::Gap {
                            let head = events.connected().head.clone();
                            state.cursor = None;
                            state.retry = Duration::ZERO;
                            return Some(Ok(Resumed::Gap { head }));
                        }
                        state.cursor = events.cursor().cloned();
                        state.events = Some(events);
                        state.permit = permit;
                    }
                    Ok(None) => return None,
                    Err(
                        ClientError::Transport { .. }
                        | ClientError::Silent { .. }
                        | ClientError::Disconnected { .. }
                        | ClientError::Refused { status: 503, .. },
                    ) => {
                        retry(&mut state);
                        continue;
                    }
                    Err(error) => {
                        state.terminal = true;
                        return Some(Err(error));
                    }
                }
            }
            let events = state.events.as_mut().expect("connected immediately above");
            let item = tokio::select! {
                    biased;
                    () = &mut changed => {
                        state.events = None;
            state.permit = None;
                        return None;
                    }
                    item = events.next() => item,
                };
            match item {
                Some(Ok(event)) => {
                    state.cursor =
                        state.events.as_ref().and_then(|events| events.cursor().cloned());
                    state.retry = Duration::ZERO;
                    return Some(Ok(Resumed::Event(Box::new(event))));
                }
                Some(Err(ClientError::Transport { .. } | ClientError::Silent { .. })) | None => {
                    state.events = None;
                    state.permit = None;
                    retry(&mut state);
                }
                Some(Err(error)) => {
                    state.events = None;
                    state.permit = None;
                    state.terminal = true;
                    return Some(Err(error));
                }
            }
        }
    }
}

struct Attempt<'a> {
    policy: Option<&'a dyn ConnectPolicy>,
    finished: bool,
}

impl Attempt<'_> {
    fn finish(&mut self, outcome: AttemptOutcome) {
        self.finished = true;
        if let Some(policy) = self.policy {
            policy.observe(outcome);
        }
    }
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.finish(AttemptOutcome::Cancelled);
        }
    }
}

fn retry(state: &mut State) {
    // Two bounded durations need no retry-policy dependency or hidden task.
    state.retry = (state.retry * 2).max(Duration::from_millis(100)).min(Duration::from_secs(5));
}
