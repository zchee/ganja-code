//! Bounded, process-local replay with permissions projected before the head advances.
//!
//! A deque and a watch notification suffice: replay needs byte and frame limits and an
//! atomic projection barrier, which a general broadcast queue does not provide.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, Weak};

use futures::StreamExt as _;
use ganja_core::Engine;
use ganja_protocol::Event;
use tokio::sync::watch;

use crate::state::{Pending, PendingPermission};

/// Maximum retained engine events in the default journal.
pub const JOURNAL_FRAMES: usize = 8192;
/// Maximum retained serialized event bytes in the default journal.
pub const JOURNAL_BYTES: usize = 16 * 1024 * 1024;

/// A caller-owned event history. Reuse it when serving the same engine again.
///
/// One lossless consumer remains attached while the engine lives, even between
/// listeners. A journal cannot be reused for a different engine.
#[derive(Clone)]
pub struct EventJournal(Arc<Shared>);

struct Shared {
    epoch: String,
    state: Mutex<History>,
    attachment: tokio::sync::Mutex<Option<Weak<Engine>>>,
    changed: watch::Sender<()>,
    pending: Pending,
    max_frames: usize,
    max_bytes: usize,
    #[cfg(test)]
    before_advance: Mutex<Option<Arc<std::sync::Barrier>>>,
}

struct History {
    head: u64,
    bytes: usize,
    frames: VecDeque<JournalFrame>,
}

#[derive(Clone)]
pub(crate) struct JournalFrame {
    pub(crate) seq: u64,
    pub(crate) id: String,
    pub(crate) data: Arc<str>,
}

pub(crate) struct Registration {
    pub(crate) head: String,
    pub(crate) cursor: String,
    pub(crate) seq: u64,
    pub(crate) replay: &'static str,
    pub(crate) changed: watch::Receiver<()>,
}

impl Default for EventJournal {
    fn default() -> Self {
        Self::new()
    }
}

impl EventJournal {
    /// Creates an empty journal with the default frame and byte bounds.
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(
            NonZeroUsize::new(JOURNAL_FRAMES).expect("the default frame bound is nonzero"),
            NonZeroUsize::new(JOURNAL_BYTES).expect("the default byte bound is nonzero"),
        )
    }

    /// Creates an empty journal bounded by both retained frame count and JSON bytes.
    #[must_use]
    pub fn with_limits(max_frames: NonZeroUsize, max_bytes: NonZeroUsize) -> Self {
        let (changed, _) = watch::channel(());
        Self(Arc::new(Shared {
            epoch: ganja_protocol::uuidv7(),
            state: Mutex::new(History { head: 0, bytes: 0, frames: VecDeque::new() }),
            attachment: tokio::sync::Mutex::new(None),
            changed,
            pending: Pending::default(),
            max_frames: max_frames.get(),
            max_bytes: max_bytes.get(),
            #[cfg(test)]
            before_advance: Mutex::new(None),
        }))
    }

    /// The immutable process incarnation carried on HTTP response headers.
    #[must_use]
    pub fn epoch(&self) -> &str {
        &self.0.epoch
    }

    /// The current cursor, after all permissions at or before it have been projected.
    #[must_use]
    pub fn head(&self) -> String {
        let history = self.0.state.lock().expect("the journal is never poisoned");
        format!("{}-{}", self.0.epoch, history.head)
    }

    pub(crate) fn pending(&self) -> Pending {
        self.0.pending.clone()
    }

    pub(crate) async fn attach(&self, engine: &Arc<Engine>) -> Result<(), crate::ServeError> {
        let mut attachment = self.0.attachment.lock().await;
        if let Some(existing) = attachment.as_ref() {
            return if existing.ptr_eq(&Arc::downgrade(engine)) {
                Ok(())
            } else {
                Err(crate::ServeError::JournalEngineMismatch)
            };
        }
        let mut events = engine.subscribe().await?;
        *attachment = Some(Arc::downgrade(engine));
        let journal = self.clone();
        tokio::spawn(async move {
            while let Some(event) = events.next().await {
                journal.append(event);
            }
        });
        Ok(())
    }

    fn append(&self, event: Event) {
        let data: Arc<str> =
            serde_json::to_string(&event).expect("engine events serialize to JSON").into();
        let mut history = self.0.state.lock().expect("the journal is never poisoned");
        match event {
            Event::PermissionRequested {
                session_id,
                id,
                call_id,
                tool,
                title,
                args,
                directories,
            } => {
                self.0.pending.insert(PendingPermission {
                    session_id,
                    id,
                    call_id,
                    tool,
                    title,
                    args,
                    directories,
                });
            }
            Event::PermissionReplied { id, .. } => self.0.pending.remove(&id),
            _ => {}
        }
        #[cfg(test)]
        if let Some(barrier) =
            self.0.before_advance.lock().expect("test barrier is never poisoned").clone()
        {
            barrier.wait();
            barrier.wait();
        }
        history.head = history.head.checked_add(1).expect("journal sequence exhausted");
        let seq = history.head;
        let id = format!("{}-{seq}", self.0.epoch);
        history.bytes += data.len();
        history.frames.push_back(JournalFrame { seq, id, data });
        while history.frames.len() > self.0.max_frames || history.bytes > self.0.max_bytes {
            let removed = history.frames.pop_front().expect("an over-limit journal is not empty");
            history.bytes -= removed.data.len();
        }
        self.0.changed.send_replace(());
    }

    pub(crate) fn register(&self, cursor: Option<&str>) -> Registration {
        let history = self.0.state.lock().expect("the journal is never poisoned");
        let head = format!("{}-{}", self.0.epoch, history.head);
        let parsed = cursor.and_then(|id| {
            let (epoch, seq) = id.split_at_checked(36)?;
            let seq = seq.strip_prefix('-')?;
            if !seq.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let seq = seq.parse::<u64>().ok()?;
            (epoch == self.0.epoch && retained(&history, seq)).then_some(seq)
        });
        let (replay, seq, cursor) = match (cursor, parsed) {
            (None, _) => ("fresh", history.head, head.clone()),
            (Some(cursor), Some(seq)) => ("from", seq, cursor.to_owned()),
            (Some(cursor), None) => ("gap", history.head, cursor.to_owned()),
        };
        Registration { head, cursor, seq, replay, changed: self.0.changed.subscribe() }
    }

    pub(crate) fn after(&self, seq: u64) -> Option<Vec<JournalFrame>> {
        let history = self.0.state.lock().expect("the journal is never poisoned");
        retained(&history, seq).then(|| {
            // Retained sequences are contiguous, so skip directly to the tail.
            let start = history.frames.len() - (history.head - seq) as usize;
            history.frames.range(start..).cloned().collect()
        })
    }
}

fn retained(history: &History, seq: u64) -> bool {
    seq <= history.head
        && history.frames.front().map_or(seq == history.head, |first| seq >= first.seq - 1)
}

#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;
