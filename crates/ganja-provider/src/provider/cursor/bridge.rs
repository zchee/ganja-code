//! The paused duplex: a Run held open while ganja's engine runs a tool.
//!
//! Spec: the `opencode-cursor` plugin's proxy at
//! `a37a6ba9a6d6d8d176bb68248f59240271f46767` (MIT, see
//! `THIRD_PARTY_NOTICES.md`) — `proxy.ts:1319`'s `deriveBridgeKey` and the
//! pause/resume around `:161-163`. **Behaviour only**, and the two ends differ:
//! there the bridge lives inside one long-lived proxy process, here it lives
//! in a `Provider` the engine calls once per step.
//!
//! # Why a Run is held rather than restarted
//!
//! Cursor's Run RPC is a duplex whose request body stays open, and the server
//! generates by *asking*: the model calls a tool, the exec arrives on the
//! response stream, and generation stops until an answer lands on the request
//! body. Ganja's engine, though, runs tools **between** provider calls — a
//! step ends, the calls run, and the next step opens with their results. The
//! two shapes do not meet on their own: the exec needs an answer on a body the
//! stream has already finished with.
//!
//! So the stream **pauses** instead of ending. When a bridgeable exec arrives,
//! the wire surfaces it as an ordinary tool call, finishes the step, and moves
//! the entire fold — the response body, the splitter, the mapping, the request
//! body's sender, the blob store — into [`HeldRuns`] under the key the *next*
//! request will also compute. A keeper task sends the run-level heartbeat
//! every [`HEARTBEAT`] so the server keeps the exchange alive; a live 25-second
//! hold on that ping alone was measured before any of this was built
//! (`tests/fixtures/cursor-mcp-tools-probe.txt`, (b)).
//!
//! When the engine calls back with the tool's result, [`HeldRuns::resolve`]
//! finds the held run, the answers go out on the body that was never closed,
//! and the same fold reads on. From the engine's side nothing is unusual: one
//! step ended with tool calls and the next one continues.
//!
//! # The key, and what must never be dropped
//!
//! The key is **`(model, messages[turn_start].id)`** — the reference's own
//! shape. It has to be computable from a `ChatRequest` alone, because that is
//! the whole of what a provider is handed, and it has to be stable across the
//! turn's resumes: `turn_start` names the message that opened the turn, and a
//! `MessageId` is a UUIDv7 that never changes.
//!
//! Matching the pending execs' call ids against the resumed request's tool
//! results is then a **confirmation** of that hit, never the lookup. It
//! matters because one engine drives far more than the root turn through one
//! `Arc<dyn Provider>`: a title one-shot, a compaction summary, and up to
//! `agents.concurrency` subagent turns all call [`super::CursorWire::stream`]
//! on the same wire while a root turn is held. So:
//!
//! - **a `stream()` never drops a held run it does not key to**; a keyed miss
//!   opens a fresh Run and leaves every held run alone;
//! - a keyed hit whose confirmation *fails* — the request carries no result
//!   for some pending exec — also opens a fresh Run and leaves the held run
//!   intact, rather than dropping it or failing;
//! - a keyed miss that carries tool results for a *different* held run evicts
//!   that run, which is [`Reason::Evicted`]: whatever it was waiting for has
//!   been answered somewhere else, so nobody will ever key to it again.
//!
//! The three drops are the pausing turn's cancellation, that eviction, and the
//! [`IDLE_BOUND`]. The last is the backstop for the case nothing else covers:
//! a turn that pauses and then ends without resuming — a provider error on a
//! sibling path, a subscriber going away — leaves a Run nobody will key to,
//! and without a bound it would hold a task and a socket for the life of the
//! process.
//!
//! A resume against a bridge that is **gone** is answered with
//! [`ProviderEvent::Failed`](crate::provider::ProviderEvent::Failed) naming it,
//! and never with a silent fresh Run: this wire carries only the newest user
//! turn, so a fresh Run there would quietly drop everything the tool answered
//! and let the model continue as though it had never asked (bead
//! `ganja-code-lnlq` is the history-composition work that closes that hole).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use buffa::Message as _;
use tokio_util::sync::CancellationToken;

use super::{ID, connect, native, request};
use crate::protocol::{MessageId, PartBody, ToolState};
use crate::provider::ChatRequest;

/// How often the run-level `client_heartbeat = 7` goes out while a Run is
/// held.
///
/// The cadence the live hold was measured at (`(b)`, five-second ticks across
/// 25 seconds). The shipped client's own *exec*-level ping is three seconds
/// (`index.js@4272747`); this is the run-level one, and this build sends no
/// other — measurement (b′) showed the exec ping changes nothing observable.
pub(super) const HEARTBEAT: Duration = Duration::from_secs(5);

/// How long a held Run waits for a resume that never comes.
///
/// **600 seconds, and it is a backstop rather than a timeout guess.** Nothing
/// measured says where the server's own ceiling is — the recording bounds only
/// 25 seconds from below — so this number is not an attempt to stay inside
/// one. It is the answer to "how long may a stranded Run hold a task and a
/// socket", and it is generous on purpose: a permission dialog somebody is
/// reading and a two-minute shell command both have to fit inside it
/// comfortably, and the cost of being wrong on the long side is one socket for
/// ten minutes in a case that should not happen at all.
pub(super) const IDLE_BOUND: Duration = Duration::from_secs(600);

/// How many dropped keys the ring remembers, so a resume can be told *why* its
/// bridge is gone rather than only that it is.
const DROPPED: usize = 16;

/// What a held Run is filed under: the model, and the id of the message that
/// opened the turn.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Key {
    model: String,
    opening: MessageId,
}

impl Key {
    /// The key `request` would resume under, or [`None`] for a request with no
    /// messages at all — which nothing can key to and nothing can resume.
    ///
    /// `turn_start` is a caller's value on a `pub` field, so it is clamped
    /// into range rather than trusted: a marker past the end names no message,
    /// and slicing on it would panic the wire.
    pub(super) fn of(request: &ChatRequest) -> Option<Self> {
        let opening = request.messages.get(turn_start(request))?;

        Some(Self { model: request.model.clone(), opening: opening.id.clone() })
    }
}

/// `request.turn_start`, clamped to an index its message list actually has.
fn turn_start(request: &ChatRequest) -> usize {
    request.turn_start.min(request.messages.len().saturating_sub(1))
}

/// One exec waiting on ganja's engine.
#[derive(Debug)]
pub(super) struct Pending {
    /// The exec's numeric id, which every answer and the close echo.
    pub(super) id: Option<u32>,
    /// The exec's string id, which a result echoes and a throw has no room
    /// for.
    pub(super) exec_id: Option<String>,
    /// The id the engine's tool part carries, and the id this exec is matched
    /// back by. For an `mcp_args` it is the server's own `tool_call_id`; for a
    /// native kind this wire mints one, because a native exec carries no id
    /// the engine could use.
    pub(super) call_id: String,
    /// The registry name the engine runs.
    pub(super) tool: String,
    /// The arguments it runs with.
    pub(super) input: serde_json::Value,
    /// How its outcome is rendered back onto the wire.
    pub(super) answer: native::Answer,
}

/// Why a held Run is no longer there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reason {
    /// The turn that paused it was cancelled.
    Cancelled,
    /// Nothing resumed it within [`IDLE_BOUND`].
    Idle,
    /// The request body closed under it, so no answer could reach the server.
    Closed,
    /// Its results turned up on a request that keys somewhere else, so nothing
    /// will ever resume it.
    Evicted,
}

impl Reason {
    /// The clause a dead-bridge failure ends with.
    fn spelled(self) -> &'static str {
        match self {
            Self::Cancelled => "the turn that opened it was cancelled",
            Self::Idle => "it was not resumed within the idle bound",
            Self::Closed => "cursor closed the request body under it",
            Self::Evicted => "its results arrived on a differently-keyed request",
        }
    }
}

/// One held Run: everything the fold needs to read on, and the execs it is
/// waiting for.
pub(super) struct Entry {
    pub(super) fold: super::Fold,
    pub(super) pending: Vec<Pending>,
    /// Fired the moment the entry leaves the table — taken for a resume, or
    /// dropped. It is what stops the keeper, and stopping the keeper is what
    /// closes the request body: the keeper holds a clone of the body's sender,
    /// so a keeper left running would hold a socket open for a Run nobody is
    /// waiting on any more.
    done: CancellationToken,
}

impl Entry {
    /// Sends every pending exec's answer on the body the pause left open, then
    /// hands the fold back to be read on.
    ///
    /// # Errors
    ///
    /// Returns the fold anyway when the body has closed — an answer that
    /// cannot be delivered is a turn that will hang, and the caller reports it
    /// rather than reading a stream nobody is generating into.
    pub(super) fn settle(mut self, request: &ChatRequest) -> Result<super::Fold, ()> {
        let results = results(request);
        let mut delivered = true;
        for exec in &self.pending {
            let Some(outcome) = results.get(&exec.call_id).and_then(native::Outcome::of) else {
                // `resolve` confirmed every call id before handing the entry
                // over, so this is unreachable; answering nothing at all would
                // hang the turn, where the refusal is something the loop reads.
                continue;
            };

            tracing::debug!(
                provider = ID,
                exec = exec.id,
                tool = exec.tool,
                outcome = match &outcome {
                    native::Outcome::Ran { .. } => "ran",
                    native::Outcome::Failed(_) => "failed",
                    native::Outcome::Refused(_) => "refused",
                },
                "answering a bridged exec from the engine's result"
            );

            for message in native::answer(&exec.answer, exec.id, exec.exec_id.as_deref(), &outcome)
            {
                let framed = connect::envelope(&message.encode_to_vec());
                delivered &= self.fold.duplex.answers.unbounded_send(Ok(framed)).is_ok();
            }
        }

        self.pending.clear();
        if delivered { Ok(self.fold) } else { Err(()) }
    }
}

/// What a `stream()` should do about the held runs.
pub(super) enum Resolution {
    /// Open a fresh Run. Every held run is untouched.
    Fresh,
    /// Continue this one, after settling the execs it was waiting for.
    Resume(Box<Entry>),
    /// This request is resuming a bridge that is gone; the sentence says which
    /// and, when the ring still knows, why.
    Dead(String),
}

/// The Runs one provider is holding open, shared by every wire it builds.
///
/// Shared deliberately: `Turn::child` clones the `Arc<dyn Provider>`, so a
/// subagent's turns run against the same table as its parent's — which is what
/// makes "a `stream()` never drops a held run it does not key to" a rule about
/// one table rather than a hope about several.
#[derive(Default)]
pub(super) struct HeldRuns {
    runs: Mutex<HashMap<Key, Entry>>,
    /// The last few keys to go, and why. Bounded, because its only reader is a
    /// resume that wants one sentence.
    dropped: Mutex<VecDeque<(Key, Reason)>>,
}

impl HeldRuns {
    /// What `request` should do: resume a held Run, open a fresh one, or fail
    /// because the bridge it is resuming is gone.
    ///
    /// **No guard is held across an await here, and none can be**: this
    /// function does not await at all. It takes the lock, decides, and gives
    /// it back.
    pub(super) fn resolve(&self, request: &ChatRequest) -> Resolution {
        let results = results(request);
        let Some(key) = Key::of(request) else {
            return Resolution::Fresh;
        };

        let mut keyed = false;
        let taken = {
            let mut runs = self.runs.lock().expect("the held-run table is never poisoned");
            match runs.get(&key) {
                // Every pending exec has an answer waiting: this is the resume
                // the pause was for.
                Some(entry) if confirmed(entry, &results) => runs.remove(&key),
                // Keyed, but the results are not here. A fresh Run, and the
                // held one is left exactly as it was.
                Some(_) => {
                    tracing::debug!(
                        provider = ID,
                        "a request keyed a held run whose results have not arrived; opening a \
                         fresh run and leaving it held"
                    );
                    keyed = true;
                    None
                }
                None => None,
            }
        };

        if let Some(entry) = taken {
            entry.done.cancel();
            return Resolution::Resume(Box::new(entry));
        }

        // Somebody *else's* results turned up on this request: whatever those
        // runs were waiting for has been answered on a request that keys
        // elsewhere, so nothing will ever resume them. The run this request
        // does key to is exempt — a partly-answered batch is still a bridge
        // worth keeping.
        self.evict_answered(&results, &key);

        if keyed || results.is_empty() {
            return Resolution::Fresh;
        }

        Resolution::Dead(self.dead(&key))
    }

    /// Holds `fold` open under `key`, beating on the request body until the
    /// turn resumes, is cancelled, or the idle bound passes.
    ///
    /// `turn` is the *pausing* `stream()`'s cancellation token — a cancel
    /// there is a turn the user left, and a Run held for it has nobody left to
    /// answer.
    pub(super) fn hold(
        self: &Arc<Self>,
        key: Key,
        fold: super::Fold,
        pending: Vec<Pending>,
        turn: &CancellationToken,
    ) {
        let done = CancellationToken::new();
        let answers = fold.duplex.answers.clone();
        tracing::debug!(provider = ID, execs = pending.len(), "holding a run open for the engine");

        self.runs
            .lock()
            .expect("the held-run table is never poisoned")
            .insert(key.clone(), Entry { fold, pending, done: done.clone() });

        // A child of nothing a turn owns except the pausing turn's own cancel,
        // which is one of the three drops.
        let table = Arc::downgrade(self);
        let turn = turn.clone();
        tokio::spawn(async move {
            let reason = keep(&answers, &done, &turn).await;
            let Some(dropped) = reason else { return };
            if let Some(table) = table.upgrade() {
                table.drop_run(&key, dropped);
            }
        });
    }

    /// Removes a held Run and remembers why, so a later resume can say.
    fn drop_run(&self, key: &Key, reason: Reason) {
        let held = self.runs.lock().expect("the held-run table is never poisoned").remove(key);
        let Some(entry) = held else { return };
        // Stops the keeper, which drops the last clone of the request body's
        // sender and closes the body — a Run nobody will resume must not keep
        // a socket.
        entry.done.cancel();

        tracing::debug!(provider = ID, reason = ?reason, "dropping a held run");
        let mut dropped = self.dropped.lock().expect("the dropped ring is never poisoned");
        if dropped.len() == DROPPED {
            dropped.pop_front();
        }
        dropped.push_back((key.clone(), reason));
    }

    /// Evicts every held Run whose execs have been answered on a request that
    /// keys somewhere else — `keep` being the one this request *does* key to,
    /// which is never evicted by its own resume attempt.
    fn evict_answered(&self, results: &HashMap<String, ToolState>, keep: &Key) {
        if results.is_empty() {
            return;
        }

        let answered: Vec<Key> = {
            let runs = self.runs.lock().expect("the held-run table is never poisoned");
            runs.iter()
                .filter(|(key, entry)| {
                    *key != keep
                        && entry.pending.iter().any(|exec| results.contains_key(&exec.call_id))
                })
                .map(|(key, _)| key.clone())
                .collect()
        };

        for key in answered {
            self.drop_run(&key, Reason::Evicted);
        }
    }

    /// Whether a Run is still held under `key`, so a test can watch a drop
    /// actually happen rather than infer it from a resolution.
    #[cfg(test)]
    pub(super) fn holds(&self, key: &Key) -> bool {
        self.runs.lock().expect("the held-run table is never poisoned").contains_key(key)
    }

    /// The sentence a resume against a gone bridge fails with.
    fn dead(&self, key: &Key) -> String {
        let dropped = self.dropped.lock().expect("the dropped ring is never poisoned");
        let reason = dropped
            .iter()
            .rev()
            .find(|(dropped, _)| dropped == key)
            .map(|(_, reason)| reason.spelled());

        match reason {
            Some(reason) => format!(
                "this turn is answering a cursor tool call, but the run that asked for it is \
                 gone: {reason}. Cursor's wire carries only the newest message, so continuing \
                 would drop what the tool answered"
            ),
            None => "this turn is answering a cursor tool call, but the run that asked for it is \
                     gone. Cursor's wire carries only the newest message, so continuing would \
                     drop what the tool answered"
                .to_owned(),
        }
    }
}

/// Whether every exec the entry is waiting for has a finished result in
/// `results`.
fn confirmed(entry: &Entry, results: &HashMap<String, ToolState>) -> bool {
    entry.pending.iter().all(|exec| {
        results.get(&exec.call_id).is_some_and(|state| {
            matches!(state, ToolState::Completed { .. } | ToolState::Error { .. })
        })
    })
}

/// The finished tool results this request carries **at or after** its turn's
/// own opening.
///
/// The bound is what keeps an earlier turn's results from reading as this
/// one's: a session's message list keeps growing, and every previous turn's
/// tool parts are still in it.
fn results(request: &ChatRequest) -> HashMap<String, ToolState> {
    let start = turn_start(request);

    request.messages[start.min(request.messages.len())..]
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter_map(|part| match &part.body {
            PartBody::Tool { call_id, state, .. } => Some((call_id.clone(), state.clone())),
            _ => None,
        })
        .collect()
}

/// Beats on the request body until the hold ends, reporting how — or [`None`]
/// when the entry was taken for a resume, which is not a drop.
async fn keep(
    answers: &futures::channel::mpsc::UnboundedSender<Result<Vec<u8>, std::convert::Infallible>>,
    done: &CancellationToken,
    turn: &CancellationToken,
) -> Option<Reason> {
    let beat = request::run_heartbeat().encode_to_vec();
    let idle = tokio::time::sleep(IDLE_BOUND);
    tokio::pin!(idle);

    loop {
        tokio::select! {
            biased;
            () = done.cancelled() => return None,
            () = turn.cancelled() => return Some(Reason::Cancelled),
            () = &mut idle => return Some(Reason::Idle),
            () = tokio::time::sleep(HEARTBEAT) => {
                if answers.unbounded_send(Ok(connect::envelope(&beat))).is_err() {
                    return Some(Reason::Closed);
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "bridge_tests.rs"]
mod tests;
