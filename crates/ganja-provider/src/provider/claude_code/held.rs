//! The processes this wire keeps alive between turns, and the task that owns
//! each one.
//!
//! # Why a process is held at all
//!
//! It is the one continuity the recording served **every time it was tried**.
//! A held process across turns was tried three times and served three times
//! (runs 3, 6 and 8, the last reaching a second turn); `--resume` was served
//! on 2 of 8 turns and refused on 6. So the process outlives the turn, and
//! every divergence that needs a new one opens a **fresh record** rather than
//! resuming anything.
//!
//! What that costs is stated where it is paid: eight held entries are eight
//! **authenticated node runtimes** on the person's machine, not eight pids,
//! and every fresh record loses the words of the model's own earlier replies.
//!
//! # One task per key owns the child
//!
//! The child's pipes never enter the table. A `stream()` that had to await a
//! read while holding the table's lock would be `await_holding_lock` at
//! `-D warnings`, so instead one task holds
//! [`ChildIo`](super::process::ChildIo), runs the reader, the
//! stderr logger and every deadline, and the table holds a channel to it. A
//! turn is [`Input::Turn`]; the events the caller streams are the receiving
//! half of a channel the task writes. Every lock taken here is a short
//! critical section with no `await` inside it, which is what makes the lint
//! hold by construction rather than by review.
//!
//! # The release rules, and what each is for
//!
//! - **(i) idle** — no frame in either direction for `idle_bound`, and
//!   nothing parked. The sweep does not run while an ask is parked, because
//!   a dialog somebody is reading and a twelve-minute `bash` are not
//!   idleness; the deadline is **reset at every frame**, unlike cursor's,
//!   whose deadline is absolute from the hold's start — a turn that streams
//!   for eleven minutes is not idle. The next turn on the key opens a fresh
//!   record and pays a spawn, a full prefix write and the assistant's earlier
//!   words, which is why the bound is the person's to set.
//! - **(i′) stranded** — an ask parked for [`STRANDED_BOUND`], the backstop
//!   rule (i) needs because it excludes exactly that case. One hour is the
//!   `tools/call` timeout this side hands the CLI, past which the CLI has
//!   given up on the call and the entry holds nothing a resolve could answer.
//! - **(ii) cap** — [`HELD_CAP`] live entries. One root conversation plus
//!   `agents.concurrency`'s default four children is five, the most a `/team`
//!   holds busy at once; the other three are headroom, so a busy five never
//!   has to evict to admit a sixth. A spawn that would exceed it closes the
//!   least-recently-used **idle** entry, never a running turn and never one
//!   with a parked ask; with everything busy the spawn proceeds and logs
//!   `held_over_cap`, because refusing a turn to keep a count is the worse
//!   failure.
//! - **(iii) divergence** — a request whose `model` or `effort` differs from
//!   the live entry's. Narrowed to those two: they are what a person chose,
//!   and keeping a chosen model stale bills the opening model under a status
//!   bar that says otherwise. A `model` change alone is first **asked** of
//!   the live process with `set_model` (`eawi`, unmeasured live) and closes
//!   the entry only at the end of a turn whose `system/init` did not confirm
//!   it. A `system` or `tools` difference closes **nothing**: the process
//!   keeps its opening prompt, logged once, and is told a moved roster with
//!   `tools/list_changed` (`i5oi`, unmeasured live).
//! - **(iv) refused** — a `system/model_refusal_no_fallback` arrived. The
//!   entry is closed at that turn's `result` and the binding remembers it, so
//!   the next spawn knows even from a later ganja process.
//! - **`Close`** — the sweep, an explicit close, the provider dropping.
//!
//! What the wire **cannot** see, and says so: a `NewSession` or a compaction
//! is not an event on this side, only a new key on the next request. The old
//! key's process is idle from that moment and rule (i) is what reaps it.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
// **Tokio's clock, not the standard library's.** The idle, stranded and
// silence bounds are all measured against it, and a test that drives the
// runtime's clock (`start_paused`) must move them: a `std::time::Instant`
// would keep advancing in real time under a paused runtime, so an hour-long
// bound would take an hour to prove.
use tokio::time::Instant;

use super::bridge::Pending;
use crate::provider::ProviderEvent;

/// What a held process is filed under: `ids::derived(messages[0].id)`.
///
/// The **conversation**, not the turn — cursor keys a held bridge by the
/// newest user run because what it holds is one turn's socket, where what
/// this holds is a process whose whole benefit is that it outlives turns. The
/// longer the key lives, the more every failure lives with it, which is what
/// the release rules above are for.
pub type Key = String;

/// How many live entries the table holds. See the module doc for the
/// arithmetic; a constant because the provider cannot see
/// `agents.concurrency`.
pub const HELD_CAP: usize = 8;

/// How long an ask may stay parked before the entry holding it is closed.
///
/// One hour, and chosen rather than inherited: it is the `tools/call` timeout
/// this side hands the CLI in its `initialize`, past which the CLI has
/// abandoned the call itself, so holding an authenticated process longer
/// would keep it open for an answer the CLI can no longer take. Shorter would
/// re-seed a dialog somebody was merely slow to answer.
pub const STRANDED_BOUND: Duration = Duration::from_secs(3_600);

/// How long a turn may produce no frame at all before it is failed.
///
/// **Sixty times** the slowest first frame the recording measured after a
/// `user` frame (2.0 s) and forty times the slowest `ttft_ms` in it (2 868).
/// A parked ask suspends it — run 1 held a `tools/call` for 25 s — so this
/// bounds silence, never patience.
pub const SILENCE_BOUND: Duration = Duration::from_secs(120);

/// How long a turn failing because its child went away waits for the rest of
/// that exit to arrive: the exit itself, after the turn's frame was refused,
/// and the end of the CLI's stderr, after an exit before `system/init`.
///
/// Both waits have one reason. An exit before `system/init` is decided by what
/// the CLI said on the way out — its own sentence, or [`NO_LOGIN`] — and the
/// runtime is free to notice the pieces of an exit in any order. Without the
/// first wait, a pipe refusing the frame put `Broken pipe` where that sentence
/// belonged whenever the pipe was noticed before the exit; without the second,
/// an exit handled before the stderr reader had read to the end reported a CLI
/// that said nothing — and on Linux, where tokio reaps a child through a
/// pidfd, the exit and the last stderr bytes can become ready in the same
/// reactor turn (bead `ganja-code-3te9`).
///
/// **Two seconds**: over a hundred times the slowest EOF-to-exit the recording
/// measured (14 ms), and fourteen times run 7's whole life (exit 1 in 141 ms).
/// Wrong on the long side, a child that closed its stdin and kept running, or
/// a descendant still holding its stderr open, delays a turn that fails either
/// way by up to this much per wait.
const EXIT_SETTLE_BOUND: Duration = Duration::from_secs(2);

/// What `idle_bound` is when nobody has said otherwise.
///
/// The number is cursor's; the sentence that sizes it is not. There, 600 s
/// was a backstop for a case that should not happen and being wrong on the
/// long side cost one socket. Here idle eviction is the ordinary reaper of
/// every conversation with a coffee break in it, and being wrong on the
/// **short** side costs the assistant's words for the rest of the
/// conversation. That trade — the person's machine against the person's
/// conversation — is theirs to make, which is why W4 wires it to a config
/// key.
pub const DEFAULT_IDLE_BOUND: Duration = Duration::from_secs(600);

/// How many dropped keys the ring remembers, so a spawn can be told *why* its
/// process is gone rather than only that it is.
const DROPPED: usize = 16;

/// How many recovered turns the memory remembers.
///
/// A bound on **recoveries**, which are rare, rather than on turns: the ring
/// fills by recoveries and not by turns, so posture C's longer-lived entries
/// do not move it, and a turn's key leaves it only when sixty-four other
/// turns have been recovered after it.
const RECOVERED: usize = 64;

/// Why an entry is no longer in the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The child's `exit` resolved while the entry was live and nothing of
    /// ours closed it: a crash, a signal from outside, the binary replaced
    /// under it.
    Exited,
    /// Rule (i).
    IdleEvicted,
    /// Rule (i′).
    Stranded,
    /// Rule (iv). Maps to the Spawn arm's `refused-record` and **never** to
    /// `exited`.
    Refused,
    /// Rule (ii).
    Cap,
    /// Rule (iii). The Spawn arm reads this one only after a model switch the
    /// process did not confirm (`eawi`), which closes the entry at a turn's
    /// end; every other divergence's reason word is decided off the live
    /// entry's own `model`/`effort` before the entry is closed, because the
    /// binding holds neither.
    Divergence,
    /// An explicit close, or the provider going away.
    Close,
}

impl Reason {
    /// The word a Spawn arm logs when the ring is what told it the process is
    /// gone.
    ///
    /// [`None`] for the reasons a spawn cannot learn anything from: a capped,
    /// closed or diverged entry's next request has a reason of its own.
    #[must_use]
    pub fn spawn_word(self) -> Option<&'static str> {
        match self {
            Self::Exited => Some("exited"),
            Self::IdleEvicted => Some("idle-evicted"),
            Self::Stranded => Some("stranded"),
            Self::Refused => Some("refused-record"),
            // Read only after a model switch the process did not confirm
            // (`eawi`): every other divergence spawns its own record at once,
            // under a word of its own, and never asks the ring.
            Self::Divergence => Some("divergence"),
            Self::Cap | Self::Close => None,
        }
    }
}

/// What one live entry knows about itself.
#[derive(Clone, Debug)]
pub struct Meta {
    /// The ids of the user messages written to this process, in order.
    pub sent: Vec<String>,
    /// The asks waiting on ganja's engine.
    pub pending: Vec<Pending>,
    /// The model the CLI said it served, in the vendor's own spelling.
    pub served_model: Option<String>,
    /// The record's id, for logs.
    pub session_id: String,
    /// A hash of the `system` this process opened under.
    pub system_hash: u64,
    /// A hash of the tool roster it opened under.
    pub tools_hash: u64,
    /// The model asked for, which a divergence is decided on.
    pub model: String,
    /// The effort asked for, likewise.
    pub effort: Option<String>,
    /// When a frame last crossed in either direction, or an ask was parked.
    ///
    /// It lives here rather than on the entry because the task writes it on
    /// every frame and already holds this lock: putting it in the table would
    /// make a frame take a second lock to update a value whose owner is
    /// already holding one.
    pub last_frame_at: Instant,
    /// Whether a turn is running right now. The cap never evicts one.
    pub busy: bool,
    /// The `system` hash this entry last logged as stale, so a second request
    /// under the same changed prompt logs nothing and a third under a further
    /// change logs once more.
    pub logged_stale_system: Option<u64>,
}

impl Meta {
    /// A fresh record's own metadata.
    #[must_use]
    pub fn opening(
        session_id: String,
        model: String,
        effort: Option<String>,
        system_hash: u64,
        tools_hash: u64,
    ) -> Self {
        Self {
            sent: Vec::new(),
            pending: Vec::new(),
            served_model: None,
            session_id,
            system_hash,
            tools_hash,
            model,
            effort,
            last_frame_at: Instant::now(),
            busy: false,
            logged_stale_system: None,
        }
    }
}

/// What a caller sends the task that owns a child.
///
/// The two writing arms carry the `sent` list the write produces rather than
/// applying it themselves, because **the frame is written before the
/// binding**: the task writes, then records. A crash between the two leaves
/// the record one message *ahead* of `sent`, so the next request writes that
/// message again — a duplicate the model reads twice. The other order turns
/// the same crash into a message the model never saw and that nothing will
/// send again, which is why nobody should reorder this.
pub enum Input {
    /// Write this frame and stream the turn it opens.
    Turn {
        /// The `user` frame, already rendered.
        frame: String,
        /// What `sent` becomes once the frame is written.
        sent: Vec<String>,
        /// Where the turn's events go.
        events: mpsc::Sender<ProviderEvent>,
    },
    /// Answer the parked asks and stream the rest of the same CLI turn. No
    /// `user` frame is written: the turn never ended.
    Resolve {
        /// One answer per parked ask.
        answers: Vec<super::bridge::Resolution>,
        /// The id of a message carried inside a `deny.message`, which is a
        /// write and so joins `sent` once the answer is out.
        carried_id: Option<String>,
        /// Where the rest of the turn's events go.
        events: mpsc::Sender<ProviderEvent>,
    },
    /// Replace the roster `tools/list` answers with, and tell the CLI it moved
    /// (`i5oi`).
    ///
    /// Sent **ahead of** the `Turn` it belongs to, on the same channel, so the
    /// notification is written before the frame that opens the turn the new
    /// roster is for. Never sent beside a `Resolve`: an answered ask's
    /// `tools/call` may still be on its way, and a roster that moved under it
    /// could refuse by name a call the engine has already run.
    Roster {
        /// The request's roster, in the order the engine advertised it.
        tools: Vec<crate::tool::ToolDefinition>,
    },
    /// Ask the process to answer as `model` from its next turn on, and check
    /// that it did (`eawi`).
    ///
    /// Sent ahead of the `Turn` it is for, like [`Input::Roster`]. The check
    /// is that turn's own `system/init`: a model this side cannot read there
    /// — a different one, or none because no `init` arrived — closes the entry
    /// at that turn's `result` under [`Reason::Divergence`], so the next
    /// request opens a fresh record under `--model` the way every model
    /// change did before. An unhonoured switch costs that one turn, on the
    /// model the served-model slot names, and is never silent.
    SetModel {
        /// The model the request asked for, in ganja's spelling.
        model: String,
    },
    /// End the running turn, keeping the process.
    Cancel,
    /// End the process: stdin EOF, then the two signal bounds.
    Close,
}

/// One live process.
///
/// There is no other variant, and that is the invariant: **every entry in the
/// table is a live process**, `held_entries` is the table's length, and a
/// closed entry is *removed* with its reason pushed onto the ring. A
/// tombstone variant was tried and had two readings, both wrong — replaced by
/// the recovery's own spawn it was dead text, and surviving every spawn it
/// meant "once per conversation for life".
pub struct Held {
    /// The channel to the task that owns the child.
    pub input: mpsc::Sender<Input>,
    /// What the entry knows about itself, shared with that task.
    pub meta: Arc<Mutex<Meta>>,
    /// The task itself, kept so a close can await it.
    pub task: tokio::task::JoinHandle<()>,
}

/// The table, the drop ring and the recovery memory.
///
/// Three fields, which are cursor's own three, for cursor's own stated
/// reason: the once-per-turn memory must not live in a thing the recovery
/// replaces.
pub struct HeldProcesses {
    table: Mutex<HashMap<Key, Held>>,
    /// One `flock` per conversation this ganja owns.
    ///
    /// Beside the table rather than on the provider, and that is the whole
    /// point: a lock is held for as long as the conversation is, and
    /// [`Self::forget`] is the one place a conversation stops being held — so
    /// the release sits where the removal already is and a lock cannot outlive
    /// the entry it guards (CC-8).
    locks: Mutex<HashMap<Key, super::binding::Lock>>,
    dropped: Mutex<VecDeque<(Key, Reason)>>,
    recovered: Mutex<VecDeque<(Key, String)>>,
    idle_bound: Duration,
}

impl HeldProcesses {
    /// An empty table whose entries go idle after `idle_bound`.
    #[must_use]
    pub fn new(idle_bound: Duration) -> Self {
        Self {
            table: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
            dropped: Mutex::new(VecDeque::with_capacity(DROPPED)),
            recovered: Mutex::new(VecDeque::with_capacity(RECOVERED)),
            idle_bound,
        }
    }

    /// Whether this ganja holds `key`'s conversation, claiming the lock at
    /// `path` if nobody does.
    ///
    /// Idempotent: a key already claimed answers `true` without touching the
    /// filesystem. `false` means another ganja holds it, and the caller reads
    /// no binding and writes none.
    pub fn claim_lock(&self, key: &str, path: &std::path::Path) -> bool {
        let mut locks = self.locks.lock().expect("the lock table is never poisoned");
        if locks.contains_key(key) {
            return true;
        }

        match super::binding::Lock::claim(path) {
            Ok(lock) => {
                locks.insert(key.to_owned(), lock);

                true
            }
            Err(error) => {
                tracing::debug!(provider = super::ID, key, %error, "the binding is locked elsewhere");

                false
            }
        }
    }

    /// Lets `key`'s lock go, unless an entry holds the conversation it guards.
    ///
    /// The other half of [`Self::claim_lock`], for a claim whose spawn failed
    /// before any entry was filed: [`Self::forget`] releases a lock only
    /// together with an entry, so such a claim was released by nothing and
    /// every other ganja was told `locked-elsewhere` about a conversation
    /// nobody held (RR-2). A key the table **does** hold keeps its lock —
    /// releasing it would open a live process's conversation to a second
    /// writer — which is why this asks the table rather than trusting the
    /// caller. The table's lock is taken first, the order [`Self::forget`]
    /// takes the two in.
    pub fn release_lock(&self, key: &str) {
        let table = self.table.lock().expect("the held table is never poisoned");
        if table.contains_key(key) {
            return;
        }

        self.locks.lock().expect("the lock table is never poisoned").remove(key);
    }

    /// How long an entry may go without a frame.
    #[must_use]
    pub fn idle_bound(&self) -> Duration {
        self.idle_bound
    }

    /// How many live processes this table holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.table.lock().expect("the held table is never poisoned").len()
    }

    /// Whether it holds none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// What the live entry for `key` knows about itself, or [`None`].
    #[must_use]
    pub fn meta(&self, key: &str) -> Option<Meta> {
        let table = self.table.lock().expect("the held table is never poisoned");
        let held = table.get(key)?;
        let meta = held.meta.lock().expect("an entry's meta is never poisoned").clone();

        Some(meta)
    }

    /// Runs `edit` against the live entry's own metadata.
    ///
    /// Returns [`None`] when nothing is live for `key`. The closure runs
    /// under the entry's lock, so it must not await — which nothing here
    /// does.
    pub fn with_meta<T>(&self, key: &str, edit: impl FnOnce(&mut Meta) -> T) -> Option<T> {
        let table = self.table.lock().expect("the held table is never poisoned");
        let held = table.get(key)?;
        let mut meta = held.meta.lock().expect("an entry's meta is never poisoned");

        Some(edit(&mut meta))
    }

    /// Files a live entry under `key`.
    pub fn insert(&self, key: Key, held: Held) {
        self.table.lock().expect("the held table is never poisoned").insert(key, held);
    }

    /// A channel to the task that owns `key`'s child, or [`None`].
    #[must_use]
    pub fn input(&self, key: &str) -> Option<mpsc::Sender<Input>> {
        let table = self.table.lock().expect("the held table is never poisoned");

        table.get(key).map(|held| held.input.clone())
    }

    /// Removes `key` and records why, without waiting for the child.
    ///
    /// The caller has already asked the task to close, or the task is telling
    /// the table it has ended; either way what leaves the table leaves it at
    /// once, so `held_entries` never counts a process that is gone.
    pub fn forget(&self, key: &str, reason: Reason) -> Option<Held> {
        let held = self.table.lock().expect("the held table is never poisoned").remove(key)?;
        // The lock goes with the entry (CC-8). A conversation this process no
        // longer holds is one another ganja may take, and the next fresh
        // record on this key claims again through
        // [`Self::claim_lock`] — which is why the release is safe here and
        // would not be in a caller that only sometimes runs.
        self.locks.lock().expect("the lock table is never poisoned").remove(key);
        self.push_dropped(key.to_owned(), reason);

        tracing::info!(
            provider = super::ID,
            key,
            reason = ?reason,
            held_entries = self.len(),
            "held process released"
        );

        Some(held)
    }

    /// Closes `key`'s process and removes it: stdin EOF, then the two signal
    /// bounds inside the task.
    pub async fn close(&self, key: &str, reason: Reason) {
        let Some(held) = self.forget(key, reason) else {
            return;
        };

        // A closed channel is EOF to the child by itself — see `process`'s
        // `kill_on_drop(false)` — so a send that fails is the outcome being
        // asked for rather than an error.
        let _ = held.input.send(Input::Close).await;
        let _ = held.task.await;
    }

    /// The ring's word for a key whose process is gone.
    #[must_use]
    pub fn dropped_reason(&self, key: &str) -> Option<Reason> {
        let dropped = self.dropped.lock().expect("the drop ring is never poisoned");

        dropped.iter().rev().find(|(dropped, _)| dropped == key).map(|(_, reason)| *reason)
    }

    /// Records why `key` is gone.
    fn push_dropped(&self, key: Key, reason: Reason) {
        let mut dropped = self.dropped.lock().expect("the drop ring is never poisoned");
        if dropped.len() == DROPPED {
            dropped.pop_front();
        }
        dropped.push_back((key, reason));
    }

    /// Claims the one recovery this `(key, turn)` gets.
    ///
    /// `false` means it has already been recovered, which is
    /// [`super::ID`]'s "already reopened once for this turn": a second
    /// stranding **within one turn** fails by name, and a later turn of the
    /// same conversation may recover again — the memory is per turn, which is
    /// why it is keyed by the opening message's id and not by the key alone.
    pub fn claim_recovery(&self, key: &str, turn: &str) -> bool {
        let mut recovered = self.recovered.lock().expect("the recovery ring is never poisoned");
        if recovered.iter().any(|(had, at)| had == key && at == turn) {
            return false;
        }

        if recovered.len() == RECOVERED {
            recovered.pop_front();
        }
        recovered.push_back((key.to_owned(), turn.to_owned()));

        true
    }

    /// The key of the least-recently-used entry the cap may close, or
    /// [`None`] when every entry is busy or holds a parked ask.
    ///
    /// Never a running turn and never a parked ask: evicting either would
    /// throw away work in flight to keep a count.
    #[must_use]
    pub fn evictable(&self) -> Option<Key> {
        let table = self.table.lock().expect("the held table is never poisoned");

        table
            .iter()
            .filter_map(|(key, held)| {
                let meta = held.meta.lock().expect("an entry's meta is never poisoned");

                (!meta.busy && meta.pending.is_empty()).then(|| (key.clone(), meta.last_frame_at))
            })
            .min_by_key(|(_, last)| *last)
            .map(|(key, _)| key)
    }

    /// Every key whose entry has gone idle past the bound with nothing
    /// parked.
    ///
    /// The task runs its own deadline; this is the table's own view of the
    /// same rule, for a caller sweeping several keys at once.
    #[must_use]
    pub fn idle(&self, now: Instant) -> Vec<Key> {
        let table = self.table.lock().expect("the held table is never poisoned");

        table
            .iter()
            .filter(|(_, held)| {
                let meta = held.meta.lock().expect("an entry's meta is never poisoned");

                !meta.busy
                    && meta.pending.is_empty()
                    && now.duration_since(meta.last_frame_at) >= self.idle_bound
            })
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// Every key whose parked ask has waited past [`STRANDED_BOUND`].
    #[must_use]
    pub fn stranded(&self, now: Instant) -> Vec<Key> {
        let table = self.table.lock().expect("the held table is never poisoned");

        table
            .iter()
            .filter(|(_, held)| {
                let meta = held.meta.lock().expect("an entry's meta is never poisoned");

                !meta.pending.is_empty() && now.duration_since(meta.last_frame_at) >= STRANDED_BOUND
            })
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// Closes every entry: the provider is going away.
    pub async fn close_all(&self) {
        let keys: Vec<Key> = {
            let table = self.table.lock().expect("the held table is never poisoned");

            table.keys().cloned().collect()
        };

        for key in keys {
            self.close(&key, Reason::Close).await;
        }
    }
}

/// What one idle eviction says, for a frontend to render.
///
/// Polled off the provider the way `rate_windows` is (**D484**'s shape), and
/// held only until the fresh record that pays for it spawns — so the sentence
/// a person reads shows exactly between the eviction and the turn that pays
/// for it, and never afterwards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Eviction {
    /// The conversation whose process was closed.
    pub key: Key,
    /// When it was closed.
    pub at: std::time::SystemTime,
}

/// What the CLI actually served, beside what was asked for.
///
/// **Logged and surfaced, never compared.** The served name is the vendor's
/// own spelling of what it chose — `default` comes back
/// `claude-opus-5[1m]`, a fallback comes back as whatever it fell back to —
/// and a request cannot ask for a spelling. Divergence is decided on the
/// request's `model` against the entry's, never on this string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServedModel {
    /// What the request asked for.
    pub requested: String,
    /// What `system/init` or a `model_fallback` said was served.
    pub served: String,
}

/// The provider-wide slots a task writes into, so a frontend can poll what
/// the wire last saw.
///
/// Polled rather than pushed, which is this codebase's own pattern for a wire
/// fact a surface renders (**D484**): a protocol event would be a second copy
/// of something that only moves when a turn does. Provider-wide and
/// newest-wins, so a subagent's turn moves them — said here rather than
/// discovered.
#[derive(Clone, Default)]
pub struct Slots {
    /// The last idle eviction, until the fresh record that pays for it
    /// spawns.
    pub eviction: Arc<Mutex<Option<Eviction>>>,
    /// The newest `{requested, served}` pair.
    pub served_model: Arc<Mutex<Option<ServedModel>>>,
    /// What the vendor last said about the account's windows.
    pub rate: Arc<Mutex<Option<serde_json::Value>>>,
    /// The sentence the last unusual draw earned, until an ordinary one takes
    /// it down (`2z4r`, [`super::draw_notice`]).
    pub rate_notice: Arc<Mutex<Option<String>>>,
}

/// Everything one task needs that is not its own child.
pub struct Wiring {
    /// Which conversation this is.
    pub key: Key,
    /// The entry's own metadata, shared with the table.
    pub meta: Arc<Mutex<Meta>>,
    /// The table, so the task can remove itself when its child ends. Weak,
    /// because the table holds this task's `JoinHandle`.
    pub table: std::sync::Weak<HeldProcesses>,
    /// The provider-wide slots.
    pub slots: Slots,
    /// Where this key's binding is written, or [`None`] on the
    /// `locked-elsewhere` arm, which reads no binding and writes none.
    pub binding: Option<std::path::PathBuf>,
    /// The scratch directory to remove when this entry closes.
    pub cwd: Option<std::path::PathBuf>,
    /// The roster `tools/list` answers with.
    pub tools: Vec<crate::tool::ToolDefinition>,
    /// What the model was asked for, for the served-model pair.
    pub requested_model: String,
    /// This build's version, for `serverInfo`.
    pub version: String,
    /// The `initialize` control request, written before the first frame.
    pub opening: String,
    /// Whether this process answers one turn and ends.
    ///
    /// A one-shot's stdin closes **at the `result`** rather than when the
    /// caller asks: a `Close` racing the turn would end the stream before the
    /// title it was spawned for arrived.
    pub one_shot: bool,
}

#[cfg(test)]
#[path = "held_tests.rs"]
mod tests;

// ------------------------------------------------------- the task itself

/// What one turn is doing right now.
///
/// Everything here is per-turn and reset by the next one, except the parked
/// asks — which live in [`Meta`], because a `stream()` on another key must be
/// able to see them without touching this task.
#[derive(Default)]
struct Turn {
    /// Where this turn's events go, or [`None`] between turns.
    events: Option<mpsc::Sender<ProviderEvent>>,
    /// How many `tool_use` blocks the step's `assistant` frame declared.
    expected_calls: usize,
    /// How many asks have been surfaced so far.
    emitted_calls: usize,
    /// Whether the next `assistant` frame is the CLI's own `API Error:`
    /// banner rather than model speech.
    suppress_assistant: bool,
    /// The refusal this turn saw, held until its `result`.
    refusal: Option<super::frame::Refusal>,
    /// Whether readable thinking is open, so a second block gets a break.
    reasoning_open: bool,
    /// Whether this turn has already parked an unusual-draw notice, so a
    /// second event in the same turn says nothing new (`2z4r`).
    rate_noticed: bool,
    /// The model a `set_model` asked for, until the `system/init` that says
    /// whether it was honoured (`eawi`). Not reset by `open_turn`: it is
    /// written just **before** the turn it is checked on opens.
    model_check: Option<String>,
    /// Whether that `system/init` named a model the switch did not ask for,
    /// so the turn's `result` closes the entry.
    model_unhonoured: bool,
    /// The ids of control requests written **ahead of** a turn — a roster
    /// notice, a model switch — kept apart from `minted` because the turn they
    /// precede is what clears `minted`, so their answers would otherwise read
    /// as a stranger's.
    ahead: std::collections::HashSet<String>,
    /// The `request_id`s this side minted, so an echoed `control_response` is
    /// told from an answer.
    minted: std::collections::HashSet<String>,
    /// What each answered ask's `tools/call` is to be answered with.
    outcomes: HashMap<String, super::rpc::CallToolResult>,
    /// Asks answered `deny`, whose `tools/call` the CLI will never send, each
    /// id beside the registry name it was asked under.
    ///
    /// Read by [`call_arrived`], which is what makes that a rule rather than a
    /// prediction: a call for an id in here is refused from this side and
    /// never surfaced to the engine as a fresh ask — and so is a call carrying
    /// **no** id under one of these names, because the deny that emptied
    /// `meta.pending` left the name nowhere else to be found (RR-1).
    denied: HashMap<String, String>,
    /// Whether `system/init` has been seen at all on this process.
    seen_init: bool,
    /// Whether this task has written its binding yet.
    ///
    /// Its first write is the fresh record's own, and that is the one that
    /// clears `refused`: the record this task holds is new by construction,
    /// so it has not been refused. `refused_streak` is **not** cleared with
    /// it — a record that has not yet been served has not yet shown the
    /// streak is over.
    wrote_binding: bool,
}

/// Which deadline fires next, and what it means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Deadline {
    /// A turn is running and no frame has arrived for [`SILENCE_BOUND`].
    Silence,
    /// Nothing is running and nothing is parked, for `idle_bound`.
    Idle,
    /// An ask has been parked for [`STRANDED_BOUND`].
    Stranded,
}

/// Runs one child for as long as it lives.
///
/// Returns when the child is gone: the caller's `JoinHandle` resolving is
/// what says the entry is finished with.
pub async fn run(
    io: super::process::ChildIo,
    mut inputs: mpsc::Receiver<Input>,
    mut wiring: Wiring,
    idle_bound: Duration,
) {
    use tokio::io::AsyncWriteExt as _;

    let super::process::ChildIo { mut stdin, stdout, stderr, exit, kill } = io;

    // The vendor's own diagnostics, logged verbatim. The first lines are kept
    // because an exit before `system/init` is told from a transport failure
    // by what the CLI said on the way out, and by nothing else.
    let said = Arc::new(Mutex::new(Vec::<String>::new()));
    // Kept, so an exit before `system/init` can wait for the reader to reach
    // the end of what the CLI said before that exit is decided by it.
    let mut stderr_reader = stderr.map(|stderr| {
        let said = Arc::clone(&said);
        tokio::spawn(async move {
            let mut lines = super::frame::Lines::new(tokio::io::BufReader::new(stderr));
            while let Ok(Some(line)) = lines.next().await {
                // Bounded for stdout's reason: these bytes are the peer's too,
                // and this one is logged verbatim (CC-12).
                let super::frame::Line::Read(line) = line else {
                    continue;
                };

                tracing::debug!(target: "ganja_provider::claude_code::stderr", %line);
                let mut said = said.lock().expect("the stderr buffer is never poisoned");
                if said.len() < 16 {
                    said.push(line);
                }
            }
        })
    });

    // A channel rather than the future itself: a completed future must not be
    // polled again, and a closed channel may be, forever.
    let (exited_tx, mut exited) = mpsc::channel(1);
    tokio::spawn(async move {
        let _ = exited_tx.send(exit.await).await;
    });

    let mut lines = super::frame::Lines::new(tokio::io::BufReader::new(stdout));
    let mut turn = Turn::default();
    // Whether anybody can still send this task work. A one-shot's caller
    // drops its sender the moment it hands the stream back, and the turn it
    // asked for has not been answered yet — so a closed input channel ends
    // *listening*, never the turn in flight. What ends a one-shot is its own
    // `result`.
    let mut inputs_open = true;
    // What killed the child, for a turn that arrives after it is gone.
    let mut died: Option<crate::provider::ProviderError> = None;

    if let Err(error) = stdin.write_all(wiring.opening.as_bytes()).await {
        tracing::warn!(provider = super::ID, key = %wiring.key, %error, "could not open the CLI");
    }

    loop {
        let next = deadline(&wiring.meta, &turn, idle_bound);

        tokio::select! {
            input = inputs.recv(), if inputs_open => match input {
                Some(Input::Turn { frame, sent, events }) => {
                    drop_previous_turn(&mut turn);
                    open_turn(&wiring, &mut turn, events);
                    if let Err(error) = stdin.write_all(frame.as_bytes()).await {
                        // The exit decides, not whichever of the two this task
                        // noticed first: a refused write is a child that has
                        // usually exited already, and only its exit knows what
                        // it said on the way out. The write's own error is the
                        // answer only for a child still running past the bound.
                        if let Ok(Some(status)) =
                            tokio::time::timeout(EXIT_SETTLE_BOUND, exited.recv()).await
                        {
                            finish_reading_stderr(&mut stderr_reader, &turn).await;
                            died = report_exit(&wiring, &mut turn, &said, status);
                            break;
                        }
                        fail(&wiring, &mut turn, format!("could not write the turn: {error}"));
                        continue;
                    }
                    // The write happened; only now does the record of it.
                    record_sent(&wiring, &mut turn, sent);
                }
                Some(Input::Resolve { answers, carried_id, events }) => {
                    open_turn(&wiring, &mut turn, events);
                    answer_asks(&wiring, &mut turn, &mut stdin, answers).await;
                    if let Some(id) = carried_id {
                        let mut sent = wiring.meta.lock().expect("meta").sent.clone();
                        sent.push(id);
                        record_sent(&wiring, &mut turn, sent);
                    }
                }
                Some(Input::Roster { tools }) => {
                    announce_roster(&mut wiring, &mut turn, &mut stdin, tools).await;
                }
                Some(Input::SetModel { model }) => {
                    switch_model(&mut wiring, &mut turn, &mut stdin, model).await;
                }
                Some(Input::Cancel) => cancel(&wiring, &mut turn, &mut stdin).await,
                Some(Input::Close) => break,
                None => inputs_open = false,
            },
            // Cancel-safe, which is what a `select!` branch has to be: the
            // partial line lives in `lines`, never in this future.
            line = lines.next() => match line {
                Ok(Some(super::frame::Line::Read(line))) => {
                    touch(&wiring);
                    if let Some(reason) = handle(&wiring, &mut turn, &mut stdin, &line).await {
                        evict(&wiring, reason);
                        break;
                    }
                }
                // A frame past the bound is skipped by length, the way one
                // that will not parse is skipped by reason (CC-12). The turn
                // is not failed: the watchdog is what answers a process whose
                // frames stopped arriving.
                Ok(Some(super::frame::Line::TooLong(bytes))) => {
                    touch(&wiring);
                    tracing::debug!(
                        provider = super::ID,
                        key = %wiring.key,
                        bytes,
                        bound = super::frame::MAX_LINE,
                        "skipped a frame past the line bound"
                    );
                }
                // The child closed its stdout: it is on its way out, and the
                // exit arm below says with what.
                Ok(None) => {
                    if let Some(status) = exited.recv().await {
                        finish_reading_stderr(&mut stderr_reader, &turn).await;
                        died = report_exit(&wiring, &mut turn, &said, status);
                    }
                    break;
                }
                Err(error) => {
                    fail(&wiring, &mut turn, format!("could not read the CLI: {error}"));
                    break;
                }
            },
            status = exited.recv() => {
                if let Some(status) = status {
                    finish_reading_stderr(&mut stderr_reader, &turn).await;
                    died = report_exit(&wiring, &mut turn, &said, status);
                }
                break;
            }
            () = sleep_until(next), if next.is_some() => {
                let (_, which) = next.expect("the guard proved it is Some");
                match which {
                    Deadline::Silence => {
                        fail(&wiring, &mut turn, format!(
                            "no frame for {}s from the claude CLI",
                            SILENCE_BOUND.as_secs()
                        ));
                    }
                    Deadline::Idle => {
                        evict(&wiring, Reason::IdleEvicted);
                        break;
                    }
                    Deadline::Stranded => {
                        evict(&wiring, Reason::Stranded);
                        break;
                    }
                }
            }
        }
    }

    // A turn already queued when the child died has a stream somebody is
    // reading. It is failed by name rather than left to end empty, which
    // would read as a model that finished talking.
    if let Some(error) = died {
        while let Ok(input) = inputs.try_recv() {
            match input {
                Input::Turn { events, .. } | Input::Resolve { events, .. } => {
                    let _ = events.try_send(ProviderEvent::Failed(error.clone()));
                }
                Input::Roster { .. } | Input::SetModel { .. } | Input::Cancel | Input::Close => {}
            }
        }
    }

    // The only orderly exit is EOF, and dropping stdin is EOF. The two
    // signals are bounds on a child that ignored it, never the way out.
    drop(stdin);
    for (wait, signal) in [
        (Duration::from_secs(5), super::process::Signal::Term),
        (Duration::from_secs(5), super::process::Signal::Kill),
    ] {
        match tokio::time::timeout(wait, exited.recv()).await {
            Ok(_) => break,
            Err(_) => {
                tracing::info!(
                    provider = super::ID,
                    key = %wiring.key,
                    signal = ?signal,
                    "the CLI did not take EOF"
                );
                // Both bounds, and neither consumes the sender: this loop used
                // to `take()` a `FnOnce`, so the second iteration found `None`
                // and `SIGKILL` was never sent to any child (CC-3). A signal
                // to a child that has already exited is `ESRCH`, and the arm
                // is reached only when `exited.recv()` timed out anyway.
                kill(signal);
            }
        }
    }

    // The per-key scratch directory goes with the entry that owned it.
    if let Some(cwd) = &wiring.cwd {
        let _ = std::fs::remove_dir_all(cwd);
    }
}

/// Sleeps until `next`, or forever when there is nothing to wait for.
async fn sleep_until(next: Option<(Instant, Deadline)>) {
    match next {
        Some((at, _)) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Which of the three bounds applies right now.
///
/// Exactly one does: a parked ask suspends the idle rule and the silence
/// watchdog both — a dialog somebody is reading and a twelve-minute `bash`
/// are neither idleness nor silence — and a running turn is not idle.
fn deadline(
    meta: &Arc<Mutex<Meta>>,
    turn: &Turn,
    idle_bound: Duration,
) -> Option<(Instant, Deadline)> {
    let meta = meta.lock().expect("an entry's meta is never poisoned");

    if !meta.pending.is_empty() {
        return Some((meta.last_frame_at + STRANDED_BOUND, Deadline::Stranded));
    }
    if turn.events.is_some() {
        return Some((meta.last_frame_at + SILENCE_BOUND, Deadline::Silence));
    }

    Some((meta.last_frame_at + idle_bound, Deadline::Idle))
}

/// A frame crossed; every deadline is measured from now.
fn touch(wiring: &Wiring) {
    wiring.meta.lock().expect("an entry's meta is never poisoned").last_frame_at = Instant::now();
}

/// Drops what the turn before this one left behind.
///
/// The three maps [`open_turn`] cannot clear, because it runs on a **resolve**
/// too and a resolve continues the CLI turn that parked the asks — its
/// outcomes, its minted ids and its denials are that same turn's, and clearing
/// them there would lose an outcome whose `tools/call` had not yet arrived. So
/// the reset is here, on the one arm that is a new turn (CC-6).
///
/// What it is worth: `outcomes` holds a whole `CallToolResult` — a tool's
/// output, up to `truncate::MAX_CHARS` — and an allowed ask whose `tools/call`
/// never arrived left one resident for the life of a held process, which under
/// posture C is the life of the conversation.
fn drop_previous_turn(turn: &mut Turn) {
    turn.outcomes.clear();
    turn.minted.clear();
    turn.denied.clear();
}

/// Starts a turn on `events`.
fn open_turn(wiring: &Wiring, turn: &mut Turn, events: mpsc::Sender<ProviderEvent>) {
    turn.events = Some(events);
    turn.expected_calls = 0;
    turn.emitted_calls = 0;
    turn.reasoning_open = false;
    turn.rate_noticed = false;
    turn.refusal = None;
    turn.suppress_assistant = false;

    let mut meta = wiring.meta.lock().expect("an entry's meta is never poisoned");
    meta.busy = true;
    meta.last_frame_at = Instant::now();
}

/// Ends the turn, keeping the process.
fn close_turn(wiring: &Wiring, turn: &mut Turn) {
    turn.events = None;
    wiring.meta.lock().expect("an entry's meta is never poisoned").busy = false;
}

/// Emits one event, if anybody is listening.
fn emit(turn: &Turn, event: ProviderEvent) {
    if let Some(events) = &turn.events {
        // The receiver is the caller's stream. A full queue means the caller
        // has stopped reading, which a dropped event is the honest answer to:
        // blocking here would stall the child's reader for a stream nobody
        // holds.
        let _ = events.try_send(event);
    }
}

/// Fails the running turn, terminally.
///
/// Through [`close_turn`], because a failed turn is an **ended** turn: `busy`
/// says a turn is running, and the table's own two views of idleness —
/// [`HeldProcesses::idle`] and [`HeldProcesses::evictable`] — both filter on
/// it. A failure that cleared the stream and left the flag set made the entry
/// invisible to the idle sweep and to the cap forever, so enough of them and
/// the stated cap on authenticated runtimes stopped holding (CC-5).
fn fail(wiring: &Wiring, turn: &mut Turn, message: String) {
    emit(turn, ProviderEvent::Failed(crate::provider::ProviderError::Transport(message)));
    close_turn(wiring, turn);
}

/// Records what `sent` became, in memory and on disk.
fn record_sent(wiring: &Wiring, turn: &mut Turn, sent: Vec<String>) {
    wiring.meta.lock().expect("an entry's meta is never poisoned").sent = sent.clone();

    let opening = !std::mem::replace(&mut turn.wrote_binding, true);
    amend(wiring, |binding| {
        binding.sent = sent;
        if opening {
            binding.refused = false;
        }
    });
}

/// Loads, edits and stores this key's binding, or does nothing at all on the
/// `locked-elsewhere` arm, which reads none and writes none.
fn amend(wiring: &Wiring, edit: impl FnOnce(&mut super::binding::Binding)) {
    let Some(path) = &wiring.binding else {
        return;
    };

    let mut binding = super::binding::load(path).unwrap_or_default();
    binding.cli_session_id =
        wiring.meta.lock().expect("an entry's meta is never poisoned").session_id.clone();
    edit(&mut binding);

    if let Err(error) = super::binding::store(path, &binding) {
        tracing::warn!(provider = super::ID, key = %wiring.key, %error, "binding not written");
    }
}

/// Closes this entry under a release rule the task itself ran.
fn evict(wiring: &Wiring, reason: Reason) {
    if let Some(table) = wiring.table.upgrade() {
        table.forget(&wiring.key, reason);
    }

    if reason == Reason::IdleEvicted {
        *wiring.slots.eviction.lock().expect("the eviction slot is never poisoned") =
            Some(Eviction { key: wiring.key.clone(), at: std::time::SystemTime::now() });
    }
}

/// Waits, bounded, for the stderr reader to reach the end of what the CLI
/// wrote, so an exit before `system/init` is decided by all of it.
///
/// [`report_exit`] reads the buffer at once and another task fills it, so an
/// exit handled first used to read a CLI that had said nothing — the second of
/// the two orders [`EXIT_SETTLE_BOUND`] exists for. After `system/init` the
/// buffer decides nothing, so nothing waits. Bounded because a descendant that
/// inherited the pipe can hold it open past the exit.
async fn finish_reading_stderr(reader: &mut Option<tokio::task::JoinHandle<()>>, turn: &Turn) {
    if turn.seen_init {
        return;
    }
    let Some(reader) = reader.take() else {
        return;
    };

    // Past the bound, or with a reader that panicked, the exit is decided by
    // whatever had been read by then.
    let _ = tokio::time::timeout(EXIT_SETTLE_BOUND, reader).await;
}

/// What an exit that nothing here asked for means.
fn report_exit(
    wiring: &Wiring,
    turn: &mut Turn,
    said: &Arc<Mutex<Vec<String>>>,
    status: std::io::Result<std::process::ExitStatus>,
) -> Option<crate::provider::ProviderError> {
    let code = status.as_ref().ok().and_then(std::process::ExitStatus::code);

    // An exit **before** `system/init` never spent a turn, so what it means
    // is decided by what the CLI said on the way out: its own not-logged-in
    // sentence is an `Auth`, and anything else is a transport failure naming
    // the first line — run 7's `Session ID … is already in use.` is that arm,
    // exit 1 in 141 ms.
    let reported = if turn.seen_init {
        let error = crate::provider::ProviderError::Transport(format!(
            "the claude CLI exited {code:?} mid-turn"
        ));
        if turn.events.is_some() {
            emit(turn, ProviderEvent::Failed(error.clone()));
            turn.events = None;
        }

        error
    } else {
        let said = said.lock().expect("the stderr buffer is never poisoned").clone();
        let error = if said.iter().any(|line| not_logged_in(line)) {
            crate::provider::ProviderError::Auth(NO_LOGIN.to_owned())
        } else {
            crate::provider::ProviderError::Transport(said.first().map_or_else(
                || format!("the claude CLI exited {code:?} before it said anything"),
                |line| surfaced(line),
            ))
        };

        emit(turn, ProviderEvent::Failed(error.clone()));
        turn.events = None;

        error
    };

    // An exit 1 that follows an errored `result` is that result's own kind
    // and **not** a transport failure — the CLI's exit code is the last
    // `result`'s `is_error` (L1112681), so the turn has already been reported
    // and this line is the whole of what the exit adds.
    tracing::info!(provider = super::ID, key = %wiring.key, ?code, "the CLI exited");
    evict(wiring, Reason::Exited);

    Some(reported)
}

/// The sentence a person is given when the CLI holds no login.
///
/// Kept by construction and marked unmeasured: `auth_status` carries no login
/// state — it is `{isAuthenticating: false, output: []}` on every run of the
/// recording, a fully logged-in CLI included — so this arm is reached by the
/// exit path and by an `auth_status` that ever says otherwise.
pub const NO_LOGIN: &str = "the claude CLI has no login; run `claude login` in a terminal — \
     ganja never holds this credential";

/// Whether a stderr line is the CLI saying it is not logged in.
fn not_logged_in(line: &str) -> bool {
    let line = line.to_ascii_lowercase();

    line.contains("not logged in") || line.contains("claude login") || line.contains("/login")
}

/// How many bytes of the CLI's first stderr line a failed turn carries.
///
/// That line is the vendor's own and the most useful thing a person can be
/// shown — it is how an exit before `system/init` is told apart — but it
/// travels into a transcript and a log, and this wire has no
/// `Presented::redact` seam to catch a token a future diagnostic might print,
/// because it holds no credential to scrub by value. A bound is the guard that
/// does not need to know what that diagnostic will say (CC-11). Run 7's line is
/// under a hundred bytes, so every sentence the recording saw arrives whole.
const SAID_LIMIT: usize = 256;

/// What the surfaced line opens with, so it reads as the CLI's own words and
/// never as ganja's diagnosis of them.
const SAID_LABEL: &str = "the claude CLI said: ";

/// The CLI's own line as a failed turn carries it: labelled, and cut at
/// [`SAID_LIMIT`] on a char boundary with the elision counted.
fn surfaced(line: &str) -> String {
    let cut = line.floor_char_boundary(SAID_LIMIT);
    if cut == line.len() {
        return format!("{SAID_LABEL}{line}");
    }

    let omitted = line.len() - cut;
    format!("{SAID_LABEL}{}… [+{omitted} bytes]", &line[..cut])
}

/// One frame, read.
///
/// Returns the reason to close this entry when the frame is one that ends the
/// process — today only a vendor-safeguard refusal, whose `result` closes the
/// record so that the next turn opens fresh.
async fn handle(
    wiring: &Wiring,
    turn: &mut Turn,
    stdin: &mut Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    line: &str,
) -> Option<Reason> {
    use super::frame::{Block, Inbound, Request};

    let frame = match super::frame::decode(line) {
        Ok(frame) => frame,
        Err(error) => {
            tracing::debug!(provider = super::ID, key = %wiring.key, %error, "unreadable frame");

            return None;
        }
    };

    match frame {
        // Re-emitted at the head of **every** turn (M10), differing in `uuid`
        // alone, so nothing here treats it as first.
        Inbound::Init(init) => {
            turn.seen_init = true;
            served(wiring, &init.model);

            // The switch's answer: the first `init` after a `set_model`.
            if let Some(asked) = turn.model_check.take() {
                turn.model_unhonoured = !honoured(&asked, &init.model);
                tracing::info!(
                    provider = super::ID,
                    key = %wiring.key,
                    asked = %asked,
                    served_model = %init.model,
                    honoured = !turn.model_unhonoured,
                    "the process answered the model switch"
                );
            }

            let mut meta = wiring.meta.lock().expect("an entry's meta is never poisoned");
            meta.session_id.clone_from(&init.session_id);
            drop(meta);

            tracing::info!(
                provider = super::ID,
                key = %wiring.key,
                session_id = %init.session_id,
                served_model = %init.model,
                capabilities = ?init.capabilities,
                "the CLI opened a turn"
            );
        }
        Inbound::ModelFallback { model } => {
            served(wiring, &model);
            tracing::info!(provider = super::ID, key = %wiring.key, served_model = %model, "fell back");
        }
        Inbound::Refusal(refusal) => {
            // The `assistant` frame that follows is the CLI's own `API Error:`
            // banner. Suppressing it is what keeps a refusal from seeding the
            // next one: emitted, it would enter ganja's transcript as
            // assistant text, which a later preamble would render as an
            // `[Assistant]` line — the exact shape the safeguard refused 3/3.
            turn.suppress_assistant = true;
            turn.refusal = Some(refusal);
        }
        Inbound::KnownSystem { subtype } => {
            tracing::debug!(provider = super::ID, key = %wiring.key, %subtype, "system frame skipped");
        }
        Inbound::Assistant(blocks) => {
            if turn.suppress_assistant {
                turn.suppress_assistant = false;
                tracing::debug!(
                    provider = super::ID,
                    key = %wiring.key,
                    "the CLI's own error banner is not model speech and is not emitted"
                );

                return None;
            }

            for block in blocks {
                match block {
                    Block::Text(text) => emit(turn, ProviderEvent::TextDelta(text)),
                    // Empty under no `--thinking-display`: the text is
                    // withheld and only the signature arrives, and there is
                    // nothing for a transcript to carry.
                    Block::Thinking(text) if !text.is_empty() => {
                        if turn.reasoning_open {
                            emit(turn, ProviderEvent::ReasoningBreak);
                        }
                        turn.reasoning_open = true;
                        emit(turn, ProviderEvent::ReasoningDelta(text));
                    }
                    Block::Thinking(_) => {}
                    // The block set for this step is what says how many asks
                    // to expect; each ask itself arrives as `can_use_tool`.
                    Block::ToolUse { .. } => turn.expected_calls += 1,
                }
            }
        }
        Inbound::User { is_replay } => {
            tracing::debug!(provider = super::ID, key = %wiring.key, is_replay, "the CLI recorded a user frame");
        }
        Inbound::RateLimit(info) => {
            let notice = super::draw_notice(&info);
            *wiring.slots.rate.lock().expect("the rate slot is never poisoned") = Some(info);

            let mut slot =
                wiring.slots.rate_notice.lock().expect("the rate-notice slot is never poisoned");
            match notice {
                // The sentence is about now, so ordinary draw takes it down.
                None => *slot = None,
                // Once per turn: a second unusual event in the same turn is
                // the same news, and the first one's words stand.
                Some(_) if turn.rate_noticed => {}
                Some(sentence) => {
                    turn.rate_noticed = true;
                    tracing::info!(
                        provider = super::ID,
                        key = %wiring.key,
                        notice = %sentence,
                        "the vendor says this draw is not ordinary"
                    );
                    *slot = Some(sentence);
                }
            }
        }
        Inbound::AuthStatus { is_authenticating, output } => {
            // Kept by construction: on every run of the recording this frame
            // is `{isAuthenticating: false, output: []}`, a fully logged-in
            // CLI included, so the arm below has never been observed.
            if is_authenticating || !output.is_empty() {
                emit(
                    turn,
                    ProviderEvent::Failed(crate::provider::ProviderError::Auth(
                        NO_LOGIN.to_owned(),
                    )),
                );
                turn.events = None;
            }
        }
        Inbound::Result(result) => return finish(wiring, turn, result),
        Inbound::ControlRequest { request_id, request } => match request {
            Request::CanUseTool { tool_name, input, tool_use_id } => {
                ask(wiring, turn, request_id, tool_name, input, tool_use_id);
            }
            Request::Mcp { server_name, message } => {
                mcp(wiring, turn, stdin, &request_id, &server_name, &message).await;
            }
            // Never silence: a `control_request` the CLI is waiting on does
            // not time out, so an unanswered one hangs the turn where a named
            // refusal ends it readably.
            Request::Unknown { subtype } => {
                tracing::debug!(provider = super::ID, key = %wiring.key, %subtype, "unknown control request");
                write(
                    stdin,
                    &super::frame::control_error_line(
                        &request_id,
                        &format!("this build does not implement the control request `{subtype}`"),
                    ),
                )
                .await;
            }
        },
        Inbound::ControlResponse { request_id, error, .. } => {
            // An answer to a request written ahead of this turn. A refused
            // switch is logged and left to the `init` check, which is what
            // decides; a refused roster notice changes nothing this side did.
            if turn.ahead.remove(&request_id) {
                if let Some(error) = error {
                    tracing::info!(
                        provider = super::ID,
                        key = %wiring.key,
                        %error,
                        "the CLI refused a request written ahead of the turn"
                    );
                }

                return None;
            }
            // The CLI echoes every `control_response` this side sends, so an
            // id this side did not mint is that echo and never an answer.
            if !turn.minted.remove(&request_id) {
                tracing::debug!(
                    provider = super::ID,
                    key = %wiring.key,
                    %request_id,
                    "dropped a control_response this wire did not mint"
                );
            }
        }
        Inbound::Unknown { kind, subtype } => {
            tracing::debug!(provider = super::ID, key = %wiring.key, %kind, ?subtype, "unknown frame");
        }
    }

    None
}

/// The turn ended.
fn finish(wiring: &Wiring, turn: &mut Turn, result: super::frame::Result_) -> Option<Reason> {
    if let Some(refusal) = turn.refusal.take() {
        emit(
            turn,
            ProviderEvent::Failed(crate::provider::ProviderError::Transport(format!(
                "refused by the vendor safeguard: {} — {}",
                refusal.category, refusal.explanation
            ))),
        );
        close_turn(wiring, turn);

        // The streak is what bounds what this key may spend: one record with
        // the transcript rendered, one with the prompts alone, and then the
        // wire stops spending rather than trying a third.
        amend(wiring, |binding| {
            binding.refused = true;
            binding.refused_streak = binding.refused_streak.saturating_add(1);
        });

        tracing::info!(
            provider = super::ID,
            key = %wiring.key,
            category = %refusal.category,
            original_model = %refusal.original_model,
            "the vendor safeguard refused this turn"
        );

        return Some(Reason::Refused);
    }

    // **`is_error`, never `subtype`** — every `result` in the recording says
    // `subtype: "success"`, refused ones included.
    if result.is_error {
        emit(turn, ProviderEvent::Failed(crate::provider::ProviderError::Transport(result.text)));
        close_turn(wiring, turn);

        return None;
    }

    emit(
        turn,
        ProviderEvent::Usage(crate::protocol::Usage {
            input_tokens: result.usage.input,
            output_tokens: result.usage.output,
            // The CLI reports thinking under `output_tokens_details` and this
            // wire does not read it: a count nothing renders is a field to
            // keep honest rather than to guess at.
            reasoning_tokens: 0,
            cache_read_tokens: result.usage.cache_read,
            cache_write_tokens: result.usage.cache_write,
        }),
    );
    emit(turn, ProviderEvent::Finish(crate::protocol::FinishReason::Completed));
    close_turn(wiring, turn);

    if wiring.one_shot {
        // One process, one turn, stdin closed at the `result`.
        return Some(Reason::Close);
    }

    // A served turn is the only thing that resets the streak. The fresh
    // record's own binding write carries it forward, because a record that
    // has not yet been served has not yet shown the streak is over.
    amend(wiring, |binding| {
        binding.refused = false;
        binding.refused_streak = 0;
    });

    // A switch this turn could not confirm — a different model, or no `init`
    // at all, so the check is still armed — closes the entry now, and the
    // next request opens a fresh record under `--model` (`eawi`). The turn
    // itself finished: what it cost is one turn, shown by the served slot.
    if turn.model_unhonoured || turn.model_check.take().is_some() {
        turn.model_unhonoured = false;
        tracing::info!(
            provider = super::ID,
            key = %wiring.key,
            "the model switch was not confirmed; the next turn opens a fresh record"
        );

        return Some(Reason::Divergence);
    }

    None
}

/// A `can_use_tool` arrived: surface it as an ordinary tool call and park it.
fn ask(
    wiring: &Wiring,
    turn: &mut Turn,
    request_id: String,
    tool_name: String,
    input: serde_json::Value,
    tool_use_id: String,
) {
    let name = super::bridge::registry_name(&tool_name);
    surface(turn, &tool_use_id, &name, &input);

    wiring.meta.lock().expect("an entry's meta is never poisoned").pending.push(
        super::bridge::Pending {
            request_id: Some(request_id),
            tool_use_id,
            name,
            input,
            call_request_id: None,
            call_rpc_id: None,
        },
    );

    step_ends(wiring, turn);
}

/// The three events one ask becomes.
fn surface(turn: &mut Turn, tool_use_id: &str, name: &str, input: &serde_json::Value) {
    emit(turn, ProviderEvent::ToolCallStart { id: tool_use_id.to_owned(), name: name.to_owned() });
    emit(
        turn,
        ProviderEvent::ToolCallDelta {
            id: tool_use_id.to_owned(),
            // The event carries a `String`, so the ask's object is serialized at
            // the emission site rather than carried as a value.
            json: input.to_string(),
        },
    );
    emit(turn, ProviderEvent::ToolCallEnd { id: tool_use_id.to_owned() });
    turn.emitted_calls += 1;
}

/// Ends the step once every ask its `assistant` frame declared has been
/// surfaced.
///
/// The step ends with `Finish` and the process stays held: the CLI's turn has
/// not ended — it is waiting on `tools/call` — and this side's step has, which
/// is exactly the shape the engine runs tools between.
fn step_ends(wiring: &Wiring, turn: &mut Turn) {
    if turn.expected_calls == 0 || turn.emitted_calls < turn.expected_calls {
        return;
    }

    emit(turn, ProviderEvent::Finish(crate::protocol::FinishReason::Completed));
    close_turn(wiring, turn);
    turn.expected_calls = 0;
    turn.emitted_calls = 0;
}

/// The roster moved: `tools/list` answers with the new one from here on, and
/// the CLI is told so once (`i5oi`).
///
/// The notification is a host→CLI `mcp_message` control request whose answer
/// is the CLI's own empty success, so its id joins `ahead` and that answer is
/// consumed rather than logged as a stranger's. **Unmeasured live**: what the
/// CLI does next — a fresh `tools/list`, answered here from `wiring.tools` — is
/// the bundle's reading (W1a Q3.9-3.10), and no recorded run sent this.
async fn announce_roster(
    wiring: &mut Wiring,
    turn: &mut Turn,
    stdin: &mut Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    tools: Vec<crate::tool::ToolDefinition>,
) {
    wiring.tools = tools;

    let request_id = crate::protocol::uuidv7();
    turn.ahead.insert(request_id.clone());
    write(
        stdin,
        &super::frame::mcp_message_line(
            &request_id,
            super::rpc::SERVER,
            &super::rpc::list_changed(),
        ),
    )
    .await;

    tracing::info!(
        provider = super::ID,
        key = %wiring.key,
        tools = wiring.tools.len(),
        "told the process its roster changed"
    );
}

/// Asks the process to answer as `model`, and arms the check its next
/// `system/init` answers (`eawi`).
///
/// The served-model pair's `requested` half moves with it, so the bar reads
/// the model this switch asked for beside whatever the vendor says it served.
async fn switch_model(
    wiring: &mut Wiring,
    turn: &mut Turn,
    stdin: &mut Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    model: String,
) {
    let request_id = crate::protocol::uuidv7();
    turn.ahead.insert(request_id.clone());
    let named = (model != super::DEFAULT_MODEL).then_some(model.as_str());
    write(stdin, &super::frame::set_model_line(&request_id, named)).await;

    tracing::info!(
        provider = super::ID,
        key = %wiring.key,
        from = %wiring.requested_model,
        to = %model,
        "asked the process to switch model"
    );
    wiring.requested_model.clone_from(&model);
    turn.model_check = Some(model);
    turn.model_unhonoured = false;
}

/// Whether a `system/init`'s `served` model is the one a switch to `asked`
/// could have produced (`eawi`).
///
/// Compared leniently, because the vendor's spelling is not ganja's and a
/// request cannot ask for one: `default` is the vendor's choice and matches
/// anything; a trailing `[…]` context marker is the vendor's (`claude-opus-5`
/// is served as `claude-opus-5[1m]`); and an alias with no `-` in it —
/// `opus`, `sonnet` — matches a served id naming it. Anything else must be
/// equal. Lenient in the direction that keeps a process, which a wrong answer
/// here costs nothing more than the served-model slot already shows; strict
/// would respawn on every alias, which is the cost this bead removes.
#[must_use]
pub fn honoured(asked: &str, served: &str) -> bool {
    if asked == super::DEFAULT_MODEL {
        return true;
    }

    let served = served.split_once('[').map_or(served, |(bare, _)| bare);
    if served == asked {
        return true;
    }

    !asked.is_empty() && !asked.contains('-') && served.contains(asked)
}

/// A JSON-RPC message for a server this side declared.
async fn mcp(
    wiring: &Wiring,
    turn: &mut Turn,
    stdin: &mut Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    request_id: &str,
    server_name: &str,
    message: &serde_json::Value,
) {
    if server_name != super::rpc::SERVER {
        write(
            stdin,
            &super::frame::control_error_line(
                request_id,
                &format!("this wire declares no server named `{server_name}`"),
            ),
        )
        .await;

        return;
    }

    match super::rpc::answer(message, &wiring.tools, &wiring.version) {
        super::rpc::Answer::Reply(reply) => {
            answered(turn, request_id);
            write(
                stdin,
                &super::frame::control_response_line(request_id, &super::rpc::wrapped(reply)),
            )
            .await;
        }
        super::rpc::Answer::Empty => {
            answered(turn, request_id);
            write(stdin, &super::frame::control_response_line(request_id, &serde_json::json!({})))
                .await;
        }
        super::rpc::Answer::Call(call) => {
            call_arrived(wiring, turn, stdin, request_id, call).await;
        }
    }
}

/// A `tools/call` arrived.
///
/// Three ways it can land: after its ask was answered (the ordinary one, and
/// it is answered from what the resolve recorded); before the resolve, in
/// which case it waits on the parked ask; or with no ask at all — the
/// secondary path, where the call **is** the ask.
async fn call_arrived(
    wiring: &Wiring,
    turn: &mut Turn,
    stdin: &mut Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    request_id: &str,
    call: super::rpc::ToolCall,
) {
    let matched = match &call.tool_use_id {
        Some(id) => Some(id.clone()),
        // FIFO by name is the fallback for a call carrying no `_meta`; the
        // recording's every call carries one.
        None => wiring
            .meta
            .lock()
            .expect("an entry's meta is never poisoned")
            .pending
            .iter()
            .find(|pending| pending.name == call.name)
            .map(|pending| pending.tool_use_id.clone()),
    };

    // **A denied call is never called**, and this is where that is enforced
    // rather than trusted (CC-2). The permission answer already told the CLI
    // the call may not run, and its own contract is that it then sends no
    // `tools/call` — but this module's doc says the peer moves every release,
    // which is the reason no frame struct here uses `deny_unknown_fields`, and
    // the same reasoning applies to a contract. Answered from what this side
    // recorded, the call reaches neither the engine's permission ladder nor
    // `meta.pending`; the id stays in the set, so a second one is refused too.
    //
    // A call carrying no id is judged by its name, because the name is all it
    // has: `matched` searched `meta.pending`, which the deny itself emptied, so
    // without this arm the refused call came back as a fresh one the moment a
    // peer dropped `_meta` (RR-1). A name another ask of this turn was
    // *allowed* under is refused as well — with no id nothing tells the two
    // apart, and refusing is the side a guard fails on.
    let denied = match &matched {
        Some(id) => turn.denied.contains_key(id),
        None => {
            let name = super::bridge::registry_name(&call.name);

            turn.denied.values().any(|denied| *denied == name)
        }
    };
    if denied {
        refuse_call(turn, stdin, request_id, &call.id, DENIED_CALL).await;

        return;
    }

    if let Some(id) = &matched
        && let Some(result) = turn.outcomes.remove(id)
    {
        answered(turn, request_id);
        write(
            stdin,
            &super::frame::control_response_line(
                request_id,
                &super::rpc::wrapped(super::rpc::reply(&call.id, &result)),
            ),
        )
        .await;

        return;
    }

    let known = matched.as_ref().is_some_and(|id| {
        let mut meta = wiring.meta.lock().expect("an entry's meta is never poisoned");

        meta.pending
            .iter_mut()
            .find(|pending| &pending.tool_use_id == id)
            .map(|pending| {
                pending.call_request_id = Some(request_id.to_owned());
                pending.call_rpc_id = Some(call.id.clone());
            })
            .is_some()
    });

    if known {
        return;
    }

    // The secondary path: no ask preceded this call, so the call is the ask.
    // The recording never produced it — `can_use_tool` fired 3 ms, 1 ms and
    // 1 ms first on all three tool-calling runs — and the arm is here so the
    // design absorbs either order rather than assuming one.
    let name = super::bridge::registry_name(&call.name);

    // It is also the one arm where a name this turn never advertised could
    // reach the engine as a call, since the primary path only ever answers an
    // ask this side surfaced. Refused here for the deny arm's reason: what may
    // run is decided by the roster this side declared, never by the name a
    // call arrives under.
    if !wiring.tools.iter().any(|tool| tool.name == name) {
        refuse_call(
            turn,
            stdin,
            request_id,
            &call.id,
            &format!("this turn declared no tool named `{name}`"),
        )
        .await;

        return;
    }

    let tool_use_id = call.tool_use_id.clone().unwrap_or_else(|| request_id.to_owned());
    let input = serde_json::json!({});
    surface(turn, &tool_use_id, &name, &input);

    wiring.meta.lock().expect("an entry's meta is never poisoned").pending.push(
        super::bridge::Pending {
            request_id: None,
            tool_use_id,
            name,
            input,
            call_request_id: Some(request_id.to_owned()),
            call_rpc_id: Some(call.id),
        },
    );

    step_ends(wiring, turn);
}

/// What a `tools/call` for an ask already answered `deny` is answered with.
///
/// Terse on purpose: the refusal the person's dialog produced already reached
/// the model as the `can_use_tool`'s own `deny.message`, and this line only
/// has to say that the call did not happen either.
const DENIED_CALL: &str = "this call was refused and did not run";

/// Answers one `tools/call` from this side alone: a failed result, no ask, no
/// entry in `meta.pending`, nothing for the engine to see.
///
/// The failure travels as the tool's own `CallToolResult{is_error: true}`
/// rather than as a JSON-RPC error, which is the shape the bridge's own
/// ran-and-failed arm uses and the one the model reads as a tool's answer.
async fn refuse_call(
    turn: &mut Turn,
    stdin: &mut Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    request_id: &str,
    rpc_id: &serde_json::Value,
    said: &str,
) {
    let result = super::rpc::CallToolResult {
        content: vec![super::rpc::Content::text(said)],
        is_error: true,
    };

    answered(turn, request_id);
    write(
        stdin,
        &super::frame::control_response_line(
            request_id,
            &super::rpc::wrapped(super::rpc::reply(rpc_id, &result)),
        ),
    )
    .await;
}

/// Answers every parked ask this resolve carries.
async fn answer_asks(
    wiring: &Wiring,
    turn: &mut Turn,
    stdin: &mut Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    answers: Vec<super::bridge::Resolution>,
) {
    for answer in answers {
        let parked = {
            let mut meta = wiring.meta.lock().expect("an entry's meta is never poisoned");
            let at =
                meta.pending.iter().position(|pending| pending.tool_use_id == answer.tool_use_id);

            at.map(|at| meta.pending.remove(at))
        };
        let Some(parked) = parked else {
            continue;
        };

        if let Some(request_id) = &parked.request_id {
            answered(turn, request_id);
            write(
                stdin,
                &super::frame::control_response_line(request_id, &answer.permission.payload()),
            )
            .await;
        }

        match answer.result {
            // A denied call is never called, so there is nothing to answer.
            // The name goes in beside the id because this removal is what
            // leaves a call carrying no id nothing else to match against.
            None => {
                turn.denied.insert(parked.tool_use_id.clone(), parked.name.clone());
            }
            Some(result) => match (&parked.call_request_id, &parked.call_rpc_id) {
                // The call already arrived and was waiting on this.
                (Some(request_id), Some(rpc_id)) => {
                    answered(turn, request_id);
                    write(
                        stdin,
                        &super::frame::control_response_line(
                            request_id,
                            &super::rpc::wrapped(super::rpc::reply(rpc_id, &result)),
                        ),
                    )
                    .await;
                }
                // The ordinary order: the call follows the answer.
                _ => {
                    turn.outcomes.insert(parked.tool_use_id.clone(), result);
                }
            },
        }
    }
}

/// The user cancelled.
///
/// The parked asks are answered **first**: the CLI is never left holding a
/// question ganja will not answer, because an unanswered `can_use_tool` does
/// not time out and would wedge the process for the life of the session.
async fn cancel(
    wiring: &Wiring,
    turn: &mut Turn,
    stdin: &mut Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
) {
    let parked: Vec<super::bridge::Pending> = {
        let mut meta = wiring.meta.lock().expect("an entry's meta is never poisoned");

        std::mem::take(&mut meta.pending)
    };

    for pending in parked {
        if let Some(request_id) = &pending.request_id {
            answered(turn, request_id);
            write(
                stdin,
                &super::frame::control_response_line(
                    request_id,
                    &super::bridge::cancelled().payload(),
                ),
            )
            .await;
        }
    }

    // `interrupt` ends a **turn**, not a process: run 8's process kept
    // reading stdin afterwards and its second turn completed. Whether it ends
    // a *running* turn with a `result` is unmeasured — the recording's
    // interrupt landed 63 ms after an already-finished turn — so the watchdog
    // is the fallback if no `result` follows.
    let request_id = crate::protocol::MessageId::ascending().as_str().to_owned();
    turn.minted.insert(request_id.clone());
    write(
        stdin,
        &super::frame::control_request_line(&request_id, super::frame::ControlRequest::Interrupt),
    )
    .await;
}

/// Records that this side answered `request_id`, so the CLI's echo of that
/// answer is told from an answer of its own.
fn answered(turn: &mut Turn, request_id: &str) {
    turn.minted.insert(request_id.to_owned());
}

/// The newest `{requested, served}` pair, provider-wide and newest-wins.
fn served(wiring: &Wiring, model: &str) {
    *wiring.slots.served_model.lock().expect("the served-model slot is never poisoned") =
        Some(ServedModel { requested: wiring.requested_model.clone(), served: model.to_owned() });
}

/// One line to the CLI's stdin.
async fn write(stdin: &mut Box<dyn tokio::io::AsyncWrite + Send + Unpin>, line: &str) {
    use tokio::io::AsyncWriteExt as _;

    if let Err(error) = stdin.write_all(line.as_bytes()).await {
        tracing::warn!(provider = super::ID, %error, "could not write to the CLI");
    }
}
