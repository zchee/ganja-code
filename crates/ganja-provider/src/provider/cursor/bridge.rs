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
//! # The recovery, and its cap
//!
//! A resume against a bridge that is **gone** — dropped for one of the three
//! reasons above, or taken for a resume and found with its request body
//! closed — is **recovered** rather than failed (**D553**, amending D552's
//! "fails by name"): [`HeldRuns::resolve`] answers [`Resolution::Recover`]
//! with a sentence naming the drop for the log line, and `stream()` falls
//! through to a fresh Run over the composed conversation (`history`). That
//! Run goes out under `resume_action` by the request's own shape — its newest
//! message is the assistant's, carrying the tool's result — so what the tool
//! answered rides the blob channel into the reopened Run instead of being
//! quietly forgotten, which is the one thing that made failing the honest
//! answer before history was composed.
//!
//! The recovery is **capped at one per key**. Every key recovered is pushed
//! onto a bounded list of its own ([`REOPENED`]) that `drop_run` never
//! touches — deliberately, because the shipped shape is recover → the reopened
//! Run is held again under the *same* key → it is dropped again, and a count
//! kept in the drop ring would be shadowed by that second drop's fresh entry.
//! A key already on the list is answered [`Resolution::Failed`] naming the
//! cap. The check replaces a `Recover` exactly where one would be produced
//! and **never precedes `Resume` or `Fresh`**: a recovered Run held again by
//! a later exec and resumed with its results present resolves `Resume` as
//! any held Run does, so a capped turn loses its second *reopening*, not its
//! ability to continue. Why one: the shipped shapes produce one drop per turn
//! (a dialog past the bound, a body the server closed), a second drop of the
//! same turn is a condition a person should see, and the shipped client stops
//! its own automatic resumes after two attempts without progress. The list is
//! scoped to the provider and reset by nothing — not a clean finish, not a
//! new turn — which is safe because a [`Key`] is the opening message's id and
//! message ids ascend, so no later turn can collide with a recovered one; the
//! FIFO bound is on recoveries, which are rare, so its escape hatch —
//! sixty-four other recoveries between two resumes of one turn — is not a
//! shape a session produces.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use buffa::Message as _;
use tokio_util::sync::CancellationToken;

use super::{Answers, ID, connect, native, request};
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

/// How many recovered keys the cap remembers.
///
/// A bound on *recoveries*, which are rare, rather than on drops, which are
/// not: a turn's key leaves this list only when sixty-four other turns have
/// been recovered after it, which is not a shape a session produces between
/// two resumes of one turn.
const REOPENED: usize = 64;

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
#[derive(Debug)]
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
    /// The clause a recovery's sentence — and a capped one's — names the
    /// drop by.
    fn spelled(&self) -> &'static str {
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
struct Entry {
    fold: super::Fold,
    pending: Vec<Pending>,
    /// Fired the moment the entry leaves the table — taken for a resume, or
    /// dropped. It is what stops the keeper, and stopping the keeper is what
    /// closes the request body: the keeper holds a clone of the body's sender,
    /// so a keeper left running would hold a socket open for a Run nobody is
    /// waiting on any more.
    done: CancellationToken,
}

/// What a `stream()` should do about the held runs.
pub(super) enum Resolution {
    /// Open a fresh Run. Every held run is untouched.
    Fresh,
    /// Read on from this fold: the execs it was waiting for have been answered
    /// on the body the pause left open. Boxed because the fold is the large
    /// variant.
    Resume(Box<super::Fold>),
    /// This request is resuming a bridge that is gone — dropped, or taken and
    /// found with its body closed — and the turn is recovered on a fresh Run
    /// over the composed conversation. The sentence says why, for the log
    /// line; nothing fails.
    Recover(String),
    /// This request is resuming a bridge that is gone and its turn was already
    /// reopened once: the cap, and the sentence the turn fails with.
    Failed(String),
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
    /// Every key recovered so far, oldest first, bounded at [`REOPENED`] —
    /// the cap's memory, kept apart from `dropped` because a second drop of a
    /// recovered key must not erase the fact that it was recovered.
    reopened: Mutex<VecDeque<Key>>,
}

impl HeldRuns {
    /// What `request` should do: resume a held Run, open a fresh one, recover
    /// on a fresh one because the bridge it is resuming is gone, or fail
    /// because that recovery already happened once.
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
            match runs.get(&key).map(|entry| outcomes(entry, &results)) {
                // Every pending exec has an answer waiting: this is the resume
                // the pause was for.
                Some(Some(outcomes)) => runs.remove(&key).map(|entry| (entry, outcomes)),
                // Keyed, but the results are not here. A fresh Run, and the
                // held one is left exactly as it was. Defensive: the engine
                // resumes a step only once every call it started has
                // finished, so this arm is not a shape a turn produces. The
                // fresh Run it opens carries the composed history — the
                // finished results as `[Tool Result]` entries, the call it
                // still lacks as `NO_RESULT` — and, the request's newest
                // message being the assistant's, goes out under
                // `resume_action`.
                Some(None) => {
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

        if let Some((entry, outcomes)) = taken {
            entry.done.cancel();
            return match settle(entry, &outcomes) {
                // The body had closed under the entry this request took: a
                // recovery, capped exactly where the ring's is. The entry left
                // the table for a resume rather than through `drop_run`, so
                // the ring learns of this drop here — a capped second attempt
                // reads the ring for its reason, and without this entry it
                // would name whatever drop the ring last held for the key,
                // which is the *previous* one.
                Resolution::Recover(why) => {
                    self.remember(&key, Reason::Closed);
                    self.reopen(&key, why)
                }
                resumed => resumed,
            };
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

        self.reopen(&key, self.recovery(&key))
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
        self.remember(key, reason);
    }

    /// Files one drop on the ring, evicting the oldest past [`DROPPED`] —
    /// the ring's only writer, so the bound is kept in one place whether the
    /// drop was the keeper's or one a resume found for itself.
    fn remember(&self, key: &Key, reason: Reason) {
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
    fn holds(&self, key: &Key) -> bool {
        self.runs.lock().expect("the held-run table is never poisoned").contains_key(key)
    }

    /// Answers a recovery for `key` — or, for a key already recovered once,
    /// the refusal that caps it.
    ///
    /// Called exactly where a [`Resolution::Recover`] would otherwise be
    /// returned, and nowhere earlier: the cap must never stand in front of a
    /// `Resume` or a `Fresh`, or a recovered Run held again by its next exec
    /// would fail at the very resume it was reopened for.
    fn reopen(&self, key: &Key, recovery: String) -> Resolution {
        let first = {
            let mut reopened = self.reopened.lock().expect("the reopened list is never poisoned");
            if reopened.contains(key) {
                false
            } else {
                if reopened.len() == REOPENED {
                    reopened.pop_front();
                }
                reopened.push_back(key.clone());
                true
            }
        };

        if first { Resolution::Recover(recovery) } else { Resolution::Failed(self.capped(key)) }
    }

    /// The sentence a resume against a gone bridge is recovered under — the
    /// log line's, since nothing fails.
    fn recovery(&self, key: &Key) -> String {
        let because = self.because(key);

        format!(
            "this turn is answering a cursor tool call, but the run that asked for it is \
             gone{because}; reopening a run over the composed conversation"
        )
    }

    /// The sentence a second recovery of one key fails with.
    fn capped(&self, key: &Key) -> String {
        let because = self.because(key);

        format!(
            "this turn is answering a cursor tool call, but the run that asked for it is \
             gone{because}; it was already reopened once for this turn and is not reopened again"
        )
    }

    /// The reason clause the ring still holds for `key`, newest drop first —
    /// or nothing, for a key the ring has turned over or never held.
    fn because(&self, key: &Key) -> String {
        let dropped = self.dropped.lock().expect("the dropped ring is never poisoned");

        dropped
            .iter()
            .rev()
            .find(|(dropped, _)| dropped == key)
            .map(|(_, reason)| format!(": {}", reason.spelled()))
            .unwrap_or_default()
    }
}

/// The finished outcome of every exec the entry is waiting for, in the
/// entry's own order — or [`None`] while any one of them is missing from
/// `results` or still running, which is what makes a keyed hit a fresh Run
/// rather than a resume.
fn outcomes(entry: &Entry, results: &HashMap<String, ToolState>) -> Option<Vec<native::Outcome>> {
    entry
        .pending
        .iter()
        .map(|exec| results.get(&exec.call_id).and_then(native::Outcome::of))
        .collect()
}

/// Sends every pending exec's answer on the body the pause left open, then
/// hands the fold back to be read on — or, when the body had closed under it,
/// answers a recovery: an answer that cannot be delivered is a Run that will
/// never generate again, so the caller reopens one over the composed
/// conversation rather than reading a stream nobody is generating into. The
/// cap on that recovery is the caller's, applied at this function's one call
/// site in [`HeldRuns::resolve`].
///
/// `outcomes` is what [`outcomes`] confirmed for this entry, pairing with
/// `pending` by position; no lock is held here, and nothing awaits.
fn settle(entry: Entry, outcomes: &[native::Outcome]) -> Resolution {
    let mut delivered = true;
    for (exec, outcome) in entry.pending.iter().zip(outcomes) {
        tracing::debug!(
            provider = ID,
            exec = exec.id,
            tool = exec.tool,
            outcome = match outcome {
                native::Outcome::Ran { .. } => "ran",
                native::Outcome::Failed(_) => "failed",
                native::Outcome::Refused(_) => "refused",
            },
            "answering a bridged exec from the engine's result"
        );

        for message in native::answer(&exec.answer, exec.id, exec.exec_id.as_deref(), outcome) {
            let framed = connect::envelope(&message.encode_to_vec());
            delivered &= entry.fold.duplex.answers.unbounded_send(Ok(framed)).is_ok();
        }
    }

    if delivered {
        Resolution::Resume(Box::new(entry.fold))
    } else {
        Resolution::Recover(
            "this turn is answering a cursor tool call, but the run that asked for it closed \
             its request body before the answers could reach it; reopening a run over the \
             composed conversation"
                .to_owned(),
        )
    }
}

/// The finished tool results this request carries **at or after** its turn's
/// own opening.
///
/// The bound is what keeps an earlier turn's results from reading as this
/// one's: a session's message list keeps growing, and every previous turn's
/// tool parts are still in it.
fn results(request: &ChatRequest) -> HashMap<String, ToolState> {
    let start = turn_start(request);

    request.messages[start..]
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
    answers: &Answers,
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
