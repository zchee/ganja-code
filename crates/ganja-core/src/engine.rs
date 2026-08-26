//! The engine frontends drive: commands in, an ordered event stream out.
//!
//! Every subscriber has a bounded queue of its own, and chose a policy when it
//! registered. A **lossless** subscriber ([`Engine::subscribe`]) is never
//! dropped: a full queue makes the publisher wait, so backpressure lands on
//! the turn task and never on a render loop. A **droppable** subscriber
//! ([`Engine::subscribe_droppable`]) is the reverse trade: the publisher never
//! waits for it, and one that stops draining is evicted whole — its stream
//! ends with [`Evicted`] rather than silently, so a consumer can tell a
//! finished turn from a torn one. The first lossless subscriber inherits the
//! queue the engine was born with, buffered since construction; every later
//! one sees events from the moment it registered. One turn at a time,
//! unchanged.
//!
//! The engine owns the transcript. A turn appends the user's message, runs the
//! agent loop in [`crate::session`] — streaming the reply, executing the tool
//! calls it asks for, asking again until a request ends without any — and
//! reports every part of it through the event stream, so a frontend that
//! applies every event holds exactly what the next
//! [`ChatRequest`](crate::provider::ChatRequest) will carry.
//!
//! A **persistent** engine ([`Engine::persistent`]) additionally writes every
//! turn through to a [`Storage`] as it streams, and exposes the session
//! operations — [`Engine::sessions`], [`Engine::resume`],
//! [`Engine::current_session`] — as plain request/response methods, the
//! in-process analog of upstream's REST routes. They are deliberately not bus
//! events: the wire protocol is pinned, and P7 owns the transport. An engine
//! built with [`Engine::new`] has none of this — no store, no auto-title, no
//! compaction — which is what keeps golden, scripted and PTY runs
//! deterministic.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use futures::{
    StreamExt as _,
    stream::{self, BoxStream},
};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::{
    agent::{self, Agent},
    catalog, command,
    config::{AgentMode, TeamlessSend},
    hook, job, lsp, mcp,
    permission::{Permissions, Rule},
    protocol::{
        Command, Event, Message, MessageId, PartBody, PermissionId, PermissionMode,
        PermissionReply, RevertInfo, RevertScope, Role, ToolState, Usage, now,
    },
    provider::Provider,
    session::{
        Answered, LiveSession, PendingReplies, Persist, SessionState, SteerInput, Steering, Turn,
        TurnHandle, TurnKind, run_turn,
    },
    snapshot,
    storage::{self, SessionId, SessionInfo, Storage, StorageError},
    subagent,
    teammate::{self, identity::Identity},
    tool::{Credentials, FileTimes, Registry, Tool, plan, send_message, task, team::Peer},
    watch,
};

/// Events each subscriber's queue holds before its policy decides what a full
/// queue means: a lossless subscriber makes the publisher wait, a droppable
/// one is evicted.
pub const EVENT_CAPACITY: usize = 1024;

/// How long [`Engine::shutdown_settle`] waits for the gate's event drain to
/// land the shutdown settlements in the fanout before proceeding regardless —
/// the reference's own shutdown bound (v2 §"Shutdown", evidence
/// 620390-620431).
const SHUTDOWN_SETTLE_FLUSH: std::time::Duration = std::time::Duration::from_millis(750);

/// Teammate permission dialogs the lead may fall behind on before a teammate's
/// forwarding waits for it (**D-5**).
///
/// Small on purpose, and much smaller than the event queue above: a dialog is
/// a question somebody has to answer, so a lead holding dozens unanswered is a
/// lead nobody is at.
///
/// A full queue **refuses** the ask rather than waiting for room, and nothing
/// on that path ever blocks: the forwarding offers a question with
/// `try_send`, and a question that will not fit is answered
/// [`PermissionReply::Reject`] exactly as a question with nowhere to go at all
/// is. Waiting was the alternative and it was not a smaller bug — an awaited
/// send on this channel hangs the teammate's turn on a receiver nobody
/// claimed, with no timeout and no cancel path.
const TEAMMATE_DIALOGS: usize = 16;

/// Why a session with no store has no in-process teammates; see [`Storeless`].
const STORELESS: &str = "this session has no store to keep a teammate's \
    conversation in, and a teammate's transcript is a session somebody may \
    resume";

/// How full the current session's context is, as [`Engine::context_estimate`]
/// answers it: the estimate the last request stamped, and the window the
/// catalog sizes the active model at — absent for a model it does not know,
/// which is also the session that never auto-compacts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContextEstimate {
    /// Estimated tokens the next request will carry —
    /// [`SessionInfo::context_tokens`], the measure compaction compares
    /// against the window. Zero before a first turn, and always zero on an
    /// engine built without storage, which stores no measure to read.
    pub tokens: u64,
    /// The catalog's context window for the active model, or [`None`] for an
    /// uncataloged one.
    pub window: Option<u64>,
}

/// What fills the next request, category by category, as
/// [`Engine::context_breakdown`] answers it — the `/context` dialog's data
/// (**D470**, `slash-context`: upstream opencode has no such surface; the
/// categories are Claude Code's own panel mapped onto what ganja assembles).
///
/// Every count is an **estimate** by the compaction fit guard's own
/// chars-per-token convention ([`crate::session`]'s `estimate_tokens`), except
/// where a turn reported actuals — see [`Engine::context_breakdown`] on the
/// conversation shares. [`ContextEstimate`] reads the stored measure the last
/// request stamped; this computes from the same inputs the *next* request
/// will be assembled from, which is why it answers on a fresh session with
/// zero turns and immediately after a revert.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContextBreakdown {
    /// The active model's id — the same string [`Engine::model`] answers,
    /// carried so the `/context` panel can name what it measured (and resolve
    /// a catalog display name) without a second accessor round trip.
    pub model: String,
    /// The system prompt's fixed half: the agent's own prompt (or the model
    /// family's base prompt), plus the environment block.
    pub system_prompt: u64,
    /// The instruction files — the `AGENTS.md` family — headers included,
    /// the ones a session walked in from below the root (**D480**) among them.
    pub instructions: u64,
    /// The builtin tools' schemas, names and descriptions.
    pub tools_builtin: u64,
    /// What the connected MCP servers add: their tools' schemas, and the
    /// instructions the servers sent about themselves — both exist only
    /// because a server is connected, so both are its cost.
    pub tools_mcp: u64,
    /// How many builtin tools [`ContextBreakdown::tools_builtin`] priced.
    ///
    /// The counts are **metadata for the panel's detail sections** (P14 W7),
    /// never token figures: [`ContextBreakdown::total`] sums nothing from
    /// them. Only the two tool counts exist because only the tools are walked
    /// item by item here — the instruction-file and skill shares are measured
    /// off the composed suffix as one string
    /// ([`crate::instruction`]'s `suffix_measure`), so no honest item count
    /// passes through for them and none is invented.
    pub tools_builtin_count: usize,
    /// How many MCP-lent tools [`ContextBreakdown::tools_mcp`] priced — the
    /// tools alone, though the token figure also carries the servers' own
    /// instructions.
    pub tools_mcp_count: usize,
    /// The skills block of the system prompt.
    pub skills: u64,
    /// The conversation's user half.
    pub conversation_user: u64,
    /// The conversation's assistant half, tool traffic included.
    pub conversation_assistant: u64,
    /// The catalog's context window for the active model, or [`None`] for an
    /// uncataloged one — the same honest absence [`ContextEstimate::window`]
    /// reports.
    pub window: Option<u64>,
    /// Tokens auto-compaction holds back — the top tenth of the window, the
    /// complement of [`crate::session`]'s 90% trigger. Carried on the result
    /// so a free-space consumer never re-derives the trigger; absent exactly
    /// when the window is.
    pub reserve: Option<u64>,
}

impl ContextBreakdown {
    /// Every category summed — what the grid's legend must add up to.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.system_prompt
            .saturating_add(self.instructions)
            .saturating_add(self.tools_builtin)
            .saturating_add(self.tools_mcp)
            .saturating_add(self.skills)
            .saturating_add(self.conversation_user)
            .saturating_add(self.conversation_assistant)
    }

    /// What is left: window − used − reserve, or [`None`] for a model whose
    /// window nobody can size.
    #[must_use]
    pub fn free(&self) -> Option<u64> {
        let window = self.window?;

        Some(
            window
                .saturating_sub(self.total())
                .saturating_sub(self.reserve.unwrap_or(0)),
        )
    }
}

/// A command the engine refused.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// A turn is streaming — or waiting on a permission — and the engine runs
    /// one turn at a time. Session switches wait for the same reason: the
    /// turn in flight is writing into the session it started on.
    #[error("a turn is already streaming; cancel it before sending another prompt")]
    Busy,
    /// A [`Command::Steer`] arrived with the slot empty. The mirror image of
    /// [`EngineError::Busy`]: steering is the one command whose precondition
    /// is an *occupied* slot, and a frontend racing the turn boundary in that
    /// direction gets a typed answer rather than a message quietly promoted to
    /// a turn of its own.
    #[error("no turn is streaming; there is nothing to steer")]
    NotStreaming,
    /// A session operation reached an engine built with [`Engine::new`],
    /// which keeps no sessions: its transcript lives and dies with the
    /// process.
    #[error("this engine keeps no sessions; it was built without storage")]
    Ephemeral,
    /// [`Engine::resume`] named a session the store does not hold — never
    /// created, or quarantined as corrupt.
    #[error("no stored session named {}", id.as_str())]
    SessionNotFound {
        /// The id nothing answers to.
        id: SessionId,
    },
    /// The storage layer refused to act. Reads never fail on content — a
    /// corrupt file is quarantined and skipped — so this is the filesystem
    /// itself refusing.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// A [`Command::SwitchAgent`] reached an engine built without an agent
    /// registry, which is every engine a test or a golden run builds.
    #[error("this engine has no agents; it was built without a registry")]
    NoAgents,
    /// [`Command::SwitchAgent`] named an agent the registry does not hold.
    #[error("no agent named {name}")]
    UnknownAgent {
        /// The name nothing answers to.
        name: String,
    },
    /// [`Command::SwitchAgent`] named a subagent. Those exist to be spawned by
    /// the task tool, and a session that ran as one would have no way back to
    /// the tools it gave up.
    #[error("{name} is a subagent, which only the task tool runs")]
    SubagentNotSelectable {
        /// The subagent that was asked for.
        name: String,
    },
    /// [`Command::SwitchModel`] named a model this session's provider does not
    /// serve. The provider is fixed when the engine is built, so a model
    /// belonging to another one is not a switch this build can make.
    #[error("{provider} does not serve a model named {model}")]
    UnknownModel {
        /// The model that was asked for.
        model: String,
        /// The provider that was asked for it.
        provider: String,
    },
    /// [`Command::SwitchEffort`] named an effort the active model's catalog
    /// row does not carry. The message lists the row's real names, because the
    /// useful half of "no such effort" is which ones there are.
    #[error("{model} has no effort named {effort}; {}", spell_efforts(available))]
    UnknownEffort {
        /// The name nothing answers to.
        effort: String,
        /// The model it was asked of.
        model: String,
        /// Every effort that would have worked, in the catalog's order.
        available: Vec<String>,
    },
    /// [`Command::SwitchEffort`] reached a provider the catalog has no rows
    /// for. Efforts are catalog rows (upstream `provider.ts:1049`), so a
    /// session already running without sizing or pricing has no names to
    /// select from — the same no-catalog posture, stated for this command.
    #[error("{provider} is not in the catalog, so its models have no efforts to select")]
    UncatalogedEffort {
        /// The provider whose models the catalog cannot describe.
        provider: String,
    },
    /// [`Command::RunCommand`] named a command nothing answers to. The message
    /// carries the roster, because the useful half of "no such command" is
    /// which ones there are.
    #[error("no command named /{name}; this session has {}", available.join(", "))]
    UnknownCommand {
        /// The name nothing answers to.
        name: String,
        /// Every command that would have worked, sorted.
        available: Vec<String>,
    },
    /// [`Command::RunCommand`] named a command whose `agent` is a subagent.
    /// Those exist to be spawned by the task tool, and a command running as one
    /// would be a turn with no way back.
    #[error("the /{name} command runs as {agent}, which only the task tool runs")]
    CommandSubagent {
        /// The command that cannot run.
        name: String,
        /// The subagent it named.
        agent: String,
    },
    /// [`Command::Undo`] or [`Command::Redo`] reached a session that takes no
    /// snapshots. Moving the transcript without putting the files back would
    /// be an undo that only half happened, and saying so is the honest half.
    #[error("this session takes no snapshots, so there is nothing to undo")]
    NoSnapshots,
    /// [`Command::Undo`] walked back past the first prompt of the session.
    #[error("nothing to undo")]
    NothingToUndo,
    /// [`Command::Redo`] reached a session that is not reverted.
    #[error("nothing to redo")]
    NothingToRedo,
    /// [`Command::RevertTo`] named something that is not a checkpoint: a
    /// message id nothing in the live window answers to, or one that answers to
    /// an assistant message rather than to a prompt. Reverting to the nearest
    /// thing that *is* one would be moving a conversation somewhere nobody
    /// asked for, so the id is named back instead.
    #[error(
        "no checkpoint named {}; a rewind stops at a prompt this session still holds",
        id.as_str()
    )]
    NoSuchCheckpoint {
        /// The id nothing answers to.
        id: crate::protocol::MessageId,
    },
    /// A hook of the user's own refused what they just asked for — today, a
    /// `UserPromptSubmit` hook that exited 2 or denied the prompt.
    ///
    /// Typed rather than folded into an existing variant because a frontend has
    /// to be able to say *whose* refusal this was: the words are a program's,
    /// written by the person now reading them, and nothing about the model or
    /// the session went wrong. The prompt never reached the model and the
    /// engine stayed idle, so the text is still the frontend's to keep.
    #[error("a {event} hook refused this prompt: {reason}")]
    HookRefused {
        /// The event whose hook refused, by its config-file name.
        event: &'static str,
        /// What the hook said — its stderr, or the reason it denied with.
        reason: String,
    },
}

/// The half of [`EngineError::UnknownEffort`]'s sentence that names what
/// *would* have worked — or owns that nothing would, which a model with no
/// efforts at all has to say rather than trailing an empty list.
fn spell_efforts(available: &[String]) -> String {
    if available.is_empty() {
        return "it has no efforts at all".to_owned();
    }

    format!("it has {}", available.join(", "))
}

/// How a droppable subscriber's stream ends when it fell behind.
///
/// Yielded as the stream's final item, after whatever its queue still held:
/// everything before it is real and in order, and everything after it was
/// never queued. It is an error value rather than a silent end because the
/// two look identical otherwise, and a consumer that mistook an eviction for
/// a finished turn would render a torn transcript as a whole one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "this subscriber fell behind and was evicted; the events after its last one were never queued"
)]
pub struct Evicted;

/// Every destination is gone: the engine-birth queue's receiver was dropped
/// and no registered subscriber survives. The turn has nobody left to tell,
/// which is the one condition under which it stops reporting.
#[derive(Debug)]
pub(crate) struct NoSubscribers;

/// Where events leave the engine: every subscriber's queue, behind one lock,
/// each carrying the policy its subscriber chose.
///
/// Ordering rests on there being **one publisher at a time**: the turn task
/// while a turn streams, or a command path holding the turn slot while the
/// engine is idle. Each queue is FIFO, so under that invariant every
/// subscriber sees the events of a turn in emission order — two lossless
/// subscribers of one turn hold the same transcript frame for frame.
///
/// **That invariant is now enforced here rather than assumed.** It used to hold
/// by construction: a turn task blocked inside a `task` call published nothing,
/// so the one child's watcher was the only publisher for as long as it ran. A
/// step that fans several children out has several watchers crossing dialogs and
/// progress at once (**D462**), and two of them delivering concurrently could
/// otherwise reach two subscribers in two different orders — each queue FIFO,
/// and the queues disagreeing. [`Fanout::publish`] is what stops that: one
/// delivery reaches every outlet before the next begins.
pub(crate) struct Fanout {
    outlets: std::sync::Mutex<Outlets>,
    /// Held for the whole of one delivery, so concurrent publishers interleave
    /// between events and never inside one.
    ///
    /// Async because a lossless outlet's send waits on its subscriber, and the
    /// waiting is the point: backpressure has always landed on whoever is
    /// publishing.
    publish: tokio::sync::Mutex<()>,
    /// Who answers a permission dialog when nobody is going to be asked one.
    /// Inert on every fanout but the engine's own; see [`Bypass`].
    bypass: Bypass,
}

/// The answer a bypassed turn gives its own permission dialogs (**D496**).
///
/// D479's bypass lives in the two frontends: a `--yolo` session answers every
/// `PermissionRequested` with "allow once" instead of drawing it. A teammate
/// has no frontend, and a lead's `mode_set_request` can reach any engine, so
/// the same answer has to exist *inside* the engine — and this is the value
/// that gives it.
///
/// # Why it hangs off the fanout
///
/// A dialog passes exactly one funnel. Every request a turn raises — a root
/// turn's, and a child's re-addressed and republished by
/// [`subagent`](crate::subagent)'s watcher — is delivered through
/// [`Fanout::send`], and nothing else in the engine sees one. Answering there
/// also puts the reply in the wait's own channel *before* the turn wakes from
/// publishing the request, so a bypassed dialog costs the turn nothing and no
/// subscriber ever sees a reply ahead of the request it answers.
///
/// # What it does not do
///
/// It answers a dialog; it does not repeal a rule. A `deny` raises no request
/// at all — `session.rs`'s gate fails the call outright — so nothing here can
/// launder one, and the answer is always [`PermissionReply::Once`], which
/// remembers nothing and writes nothing to this project's store. That is
/// D479's posture exactly, reached at a turn boundary instead of at launch.
#[derive(Default)]
struct Bypass {
    /// The running turn's wait registry, present exactly when that turn
    /// *began* under [`PermissionMode::Bypass`].
    ///
    /// Installed at a turn's start, which is what makes D496's "applied at the
    /// next turn's start" true rather than aspirational: a mode set mid-turn
    /// changes what the engine holds and not this, so the turn that is
    /// streaming keeps the posture it began with — D474's discipline, the same
    /// one the hooks and the base tools a turn clones at its start keep.
    ///
    /// The registry is held here directly rather than reached through
    /// [`TurnSlot`]: that slot is an async mutex a publisher may already be
    /// inside, and an answer that had to take it could end up waiting on the
    /// very publish that asked for it.
    answering: std::sync::Mutex<Option<Arc<std::sync::Mutex<PendingReplies>>>>,
}

impl Bypass {
    /// What a turn beginning under `mode` installs — and what one beginning
    /// under [`PermissionMode::Ask`] takes away again.
    fn begin(&self, mode: PermissionMode, pending: &Arc<std::sync::Mutex<PendingReplies>>) {
        *self.lock() = match mode {
            PermissionMode::Ask => None,
            PermissionMode::Bypass => Some(Arc::clone(pending)),
        };
    }

    /// Answers the request `id` with "allow once", if this turn is bypassed.
    fn answer(&self, id: &PermissionId) {
        let Some(pending) = self.lock().clone() else {
            return;
        };
        let answered = pending
            .lock()
            .expect("the pending replies are never poisoned")
            .answer_permission(id, PermissionReply::Once);
        if answered {
            tracing::debug!(
                id = id.as_str(),
                "a bypassed turn allowed this call once without asking anybody"
            );
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Arc<std::sync::Mutex<PendingReplies>>>> {
        self.answering
            .lock()
            .expect("the bypassed turn's registry is never poisoned")
    }
}

/// The registry half of [`Fanout`]: the queues, and the counter that names
/// them so the cleanup after an unlocked delivery removes exactly the ones
/// whose receivers turned out to be gone.
struct Outlets {
    entries: Vec<Outlet>,
    minted: u64,
}

/// One subscriber's queue.
struct Outlet {
    id: u64,
    lane: Lane,
}

/// The policy a subscriber chose when it registered.
enum Lane {
    /// A full queue makes the publisher wait: nothing is ever dropped, and
    /// the backpressure lands on the turn task.
    Lossless(mpsc::Sender<Event>),
    /// The publisher never waits: a full queue evicts the subscriber, and
    /// `loss` is how its stream is told it did not simply end.
    Droppable {
        queue: mpsc::Sender<Event>,
        loss: oneshot::Sender<Evicted>,
    },
}

impl Fanout {
    /// A fanout of one: `first` is registered as a lossless outlet. The
    /// engine seeds this with its birth queue's sender; a subagent's turn
    /// seeds it with the private queue its watcher reads.
    pub(crate) fn new(first: mpsc::Sender<Event>) -> Self {
        Self {
            outlets: std::sync::Mutex::new(Outlets {
                entries: vec![Outlet {
                    id: 0,
                    lane: Lane::Lossless(first),
                }],
                minted: 1,
            }),
            publish: tokio::sync::Mutex::new(()),
            bypass: Bypass::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Outlets> {
        self.outlets
            .lock()
            .expect("the subscriber registry is never poisoned")
    }

    /// Adds a subscriber. Registration is the atomic point after which
    /// nothing published is lost: it takes the same lock every delivery
    /// snapshots under, so an event published after this returns reaches the
    /// new queue.
    fn register(&self, lane: Lane) {
        let mut outlets = self.lock();
        let id = outlets.minted;
        outlets.minted += 1;
        outlets.entries.push(Outlet { id, lane });
    }

    /// Delivers `event` to every subscriber, each under its own policy.
    ///
    /// The registry lock is never held across an await: droppable queues are
    /// served inline under it — `try_send` cannot wait — and the lossless
    /// senders are snapshotted, then awaited one by one outside it. A
    /// droppable queue that is full costs its subscriber the subscription,
    /// never the turn a moment of waiting; a receiver that was dropped is
    /// removed on its first failed send.
    ///
    /// # Errors
    ///
    /// [`NoSubscribers`] when no outlet remains after this delivery, which is
    /// the caller's cue that the turn has nobody left to tell.
    pub(crate) async fn send(&self, event: Event) -> Result<(), NoSubscribers> {
        // One delivery at a time, across every publisher this fanout has —
        // see the type's own docs for whose orders would otherwise disagree.
        let _publishing = self.publish.lock().await;

        let lossless: Vec<(u64, mpsc::Sender<Event>)> = {
            let mut outlets = self.lock();
            let mut index = 0;
            while index < outlets.entries.len() {
                let full = match &outlets.entries[index].lane {
                    Lane::Lossless(_) => {
                        index += 1;
                        continue;
                    }
                    Lane::Droppable { queue, .. } => match queue.try_send(event.clone()) {
                        Ok(()) => {
                            index += 1;
                            continue;
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => true,
                        Err(mpsc::error::TrySendError::Closed(_)) => false,
                    },
                };
                let removed = outlets.entries.swap_remove(index);
                // An eviction is announced — that is the whole contract — but
                // a receiver that is simply gone gets nothing: there is nobody
                // on the other end to mislead.
                if full && let Lane::Droppable { loss, .. } = removed.lane {
                    let _ = loss.send(Evicted);
                }
            }

            outlets
                .entries
                .iter()
                .filter_map(|outlet| match &outlet.lane {
                    Lane::Lossless(sender) => Some((outlet.id, sender.clone())),
                    Lane::Droppable { .. } => None,
                })
                .collect()
        };

        let mut dead = Vec::new();
        for (id, sender) in &lossless {
            if sender.send(event.clone()).await.is_err() {
                dead.push(*id);
            }
        }

        // A bypassed turn answers its own dialog here: after every subscriber
        // has the request, and before the turn wakes from this call. See
        // [`Bypass`] for why this funnel is where that answer belongs.
        if let Event::PermissionRequested { id, .. } = &event {
            self.bypass.answer(id);
        }

        let mut outlets = self.lock();
        if !dead.is_empty() {
            outlets.entries.retain(|outlet| !dead.contains(&outlet.id));
        }
        if outlets.entries.is_empty() {
            Err(NoSubscribers)
        } else {
            Ok(())
        }
    }
}

/// What one turn runs as, when it is not what the session runs as.
///
/// A `/command` naming an agent or a model is the only source: both are per
/// message upstream, so neither changes what the session is.
struct Overrides {
    /// The agent this one turn runs as: its prompt, and its rules.
    agent: Option<Agent>,
    /// The model this one turn asks.
    model: Option<String>,
}

/// What the next turn runs as.
///
/// Both halves are switchable mid-session and both take effect at the next
/// turn, never the one in flight: upstream re-resolves them per prompt, and a
/// turn that changed model halfway would be one conversation asked of two.
#[derive(Debug, Default)]
struct Active {
    /// Model the next request asks for.
    model: String,
    /// Catalog effort the next request runs under, [`None`] for upstream's
    /// "Default". Only ever a name the active model's catalog row carries:
    /// [`Engine::switch_effort`] validates on the way in, and every model
    /// change runs [`Engine::reconcile_effort`] so a model that lacks the
    /// name clears it (upstream `prompt.ts:654`).
    effort: Option<String>,
    /// Agent whose prompt and rules the next turn runs under. [`None`] on an
    /// engine built without a registry, where there is nothing to run as.
    agent: Option<String>,
    /// Agent the *previous* turn ran under, which is the whole of what the
    /// plan-to-build reminder needs to know. In memory only: a message does
    /// not record the agent that produced it, so a resumed session starts
    /// with no opinion about what came before.
    previous_agent: Option<String>,
}

/// Which agent a pending switch leads to — the two plan doors, named as the
/// directions they are (**D477**).
///
/// The cell below carries this rather than assuming build, because
/// `plan_enter` records through the very same seam `plan_exit` does and the
/// boundary has to know which agent to persist and announce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SwitchTo {
    /// `plan_exit`'s direction: the plan agent handed the wheel to build.
    Build,
    /// `plan_enter`'s: the build agent asked to plan first.
    Plan,
}

impl SwitchTo {
    /// The agent this direction names.
    pub(crate) fn agent(self) -> &'static str {
        match self {
            Self::Build => agent::BUILD,
            Self::Plan => agent::PLAN,
        }
    }
}

/// Where a plan approval stands, from the tool's Yes to the prompt that
/// finally reads the approval sentence.
///
/// Four states, because three could not express the policy table on
/// [`Engine::lock_entry`]: "switch applied, sentence still riding" is a real
/// place a session stands after a shell turn or a model switch, and folding it
/// into either neighbour would make one of them mean two things. The sentence
/// itself is a constant ([`APPROVAL_SENTENCE`]), so no state carries payload
/// beyond the direction the live two carry.
///
/// Only the build direction has a sentence to ride: upstream writes a
/// synthetic approval message for its exit door and has no enter door at all,
/// so ganja's `plan_enter` lands with no message of its own (**D477**) — the
/// plan agent's standing per-turn reminder is already what tells it what it
/// is. An applied `Plan` switch therefore returns the cell straight to
/// [`PendingSwitch::None`] where an applied `Build` one moves to
/// [`PendingSwitch::SentencePending`].
///
/// Two owned deviations live on this cell. **approval-persists-at-the-boundary**:
/// upstream persists its synthetic approval message the moment the tool
/// returns, so its process-death window is zero; ganja persists at the turn
/// boundary, so a crash between the Yes and the end of that turn loses both
/// the switch and the sentence — the window is the remainder of one turn,
/// typically seconds, because the model was told to wait. And on an engine
/// with no persistence at all (`Turn::persist` is [`None`] — scripted and
/// test engines) the boundary degrades to announce-only while the
/// apply-at-entry half still runs: correct by construction, since such an
/// engine has no row for a restart to read. **approval-rides-the-request**:
/// upstream stores a synthetic user message; ganja's reminders are
/// request-time, so the sentence does not survive a restart, a `NewSession`,
/// or a manual supersede — the same family as build-switch-once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingSwitch {
    /// Nothing pending.
    None,
    /// The tool's Yes, recorded while its turn is still in flight. Only ever
    /// observable from inside that turn: the boundary moves it on before the
    /// slot is released.
    Requested(SwitchTo),
    /// The boundary persisted and announced the switch; the next engine entry
    /// still owes the in-memory half.
    Announced(SwitchTo),
    /// The switch is applied in memory; the approval sentence has not yet
    /// reached a prompt that could deliver it. Build-only, for the reason
    /// stated above.
    SentencePending,
}

/// What the first build prompt after an approval reads, ported verbatim from
/// upstream `tool/plan.ts` minus its plan-file path — ganja's plan is prose
/// already in the transcript (deviation: plan-is-prose).
pub(crate) const APPROVAL_SENTENCE: &str =
    "The plan has been approved, you can now edit files. Execute the plan";

/// The engine's side of the [`plan::Switcher`] seam: a Yes writes
/// [`PendingSwitch::Requested`] with its direction and nothing else.
///
/// Synchronous and infallible because it records intent and waits for
/// nothing — the boundary announces, the next entry applies. Wired into a
/// [`crate::tool::ToolCtx`] only on a parent turn of an engine whose registry
/// holds an agent a door leads to; which *direction* can actually be recorded
/// is decided by which door [`Engine::install`] registered, and that is what
/// makes the trait's presence-is-ability promise true per direction.
#[derive(Debug)]
pub(crate) struct RecordSwitch {
    pub(crate) pending: Arc<std::sync::Mutex<PendingSwitch>>,
}

impl RecordSwitch {
    fn record(&self, target: SwitchTo) {
        *self
            .pending
            .lock()
            .expect("the pending switch is never poisoned") = PendingSwitch::Requested(target);
    }
}

impl plan::Switcher for RecordSwitch {
    fn switch_to_build(&self) {
        self.record(SwitchTo::Build);
    }

    fn switch_to_plan(&self) {
        self.record(SwitchTo::Plan);
    }
}

/// How an engine entry disposes of a pending plan approval — the misfire
/// policy, named at every entry because [`TurnSlot::entry`] cannot be reached
/// without one.
#[derive(Clone, Copy, Debug)]
enum PendingPolicy {
    /// `start_turn` and `switch_model`: an announced switch is applied before
    /// the entry's own work, so the task-roster rebuild, the reminder gates
    /// and the entry's `remember_selection` all see the agent the boundary
    /// announced rather than the stale one it replaced. A riding sentence
    /// keeps riding; only the reminder assembly consumes it.
    Apply,
    /// `switch_agent`: the person's later explicit choice outranks the
    /// earlier Yes, so both the switch and the sentence are dropped
    /// (deviation: a-later-switch-outranks-a-yes — upstream's stored
    /// synthetic message has no supersede).
    Discard,
    /// `new_session`, `resume`, `redo`: the Yes belonged to a conversation
    /// that is over — or, for `redo`, was already cleared by the `undo` that
    /// made a redo possible, so this is defensive totality. The row a
    /// boundary already wrote is untouched: resuming that session later still
    /// resumes as build.
    Clear,
    /// `undo`: the approval goes back with the plan it approved. An
    /// unapplied switch is revoked two-sidedly — cell cleared *and* the row
    /// re-asserted to the still-active selection, without which the row would
    /// keep naming the agent the boundary wrote for a session that continues
    /// as the other one, and a restart would resume wrong. An
    /// already-applied switch survives, exactly as a
    /// manual switch survives an undo; only the sentence is dropped.
    Revoke,
}

/// The engine-side follow-up a [`TurnSlot::entry`] leaves its caller owing,
/// because the cell transition lives in the newtype and `apply_agent` /
/// `remember_selection` live on the [`Engine`]. Settled by
/// [`Engine::lock_entry`] while the guard is still held.
#[must_use]
enum Owed {
    Nothing,
    /// Apply the announced switch in memory through the non-emitting
    /// [`Engine::apply_agent`] — the boundary already announced.
    Apply(SwitchTo),
    /// Re-run [`Engine::remember_selection`] so the row returns to the
    /// selection that is actually active.
    ReassertRow,
}

/// The turn slot behind the engine's Busy discipline, and the pending plan
/// approval beside it.
///
/// The slot holds the handle of the turn in flight and doubles as the
/// idle/busy flag; the handle carries the turn's cancellation token and the
/// permission wait a [`Command::ReplyPermission`] routes into. It is wrapped
/// in this newtype so its **only acquisition paths are [`TurnSlot::entry`]
/// and the read-only [`TurnSlot::observe`]**: a state-changing entry cannot
/// reach the slot without naming its [`PendingPolicy`], which turns the
/// per-entry misfire table from a prose convention — one that had already
/// lost two rows to drift — into structure. The one thing threaded past the
/// methods is a clone of the raw `slot` Arc into each root [`Turn`]'s
/// literal, and that is a *release* handle, not an acquisition path:
/// the boundary in `run_turn` only ever writes [`None`] into it.
///
/// **Race-freedom proof.** The cell is only ever
/// [`PendingSwitch::Requested`] while the slot is occupied — the door that
/// writes it runs inside a turn — and the boundary moves it to
/// [`PendingSwitch::Announced`] before the slot is released. Every entry
/// Busy-checks the slot first, through [`TurnSlot::entry`], which cannot be
/// bypassed without bypassing the Busy check itself. Therefore no entry can
/// observe `Requested`, and no turn can start with an unapplied `Announced`.
/// `SentencePending` is post-apply: the in-memory selection and the row
/// already agree there, and the state gates only the one-shot sentence.
struct TurnSlot {
    slot: Arc<Mutex<Option<TurnHandle>>>,
    pending: Arc<std::sync::Mutex<PendingSwitch>>,
}

impl TurnSlot {
    fn new() -> Self {
        Self {
            slot: Arc::default(),
            pending: Arc::new(std::sync::Mutex::new(PendingSwitch::None)),
        }
    }

    /// The Busy check every state-changing entry starts with, plus the cell
    /// half of `policy`. The engine half comes back as the [`Owed`] the
    /// caller must settle while still holding the guard.
    async fn entry(
        &self,
        policy: PendingPolicy,
    ) -> Result<(tokio::sync::MutexGuard<'_, Option<TurnHandle>>, Owed), EngineError> {
        let guard = self.slot.lock().await;
        if guard.is_some() {
            return Err(EngineError::Busy);
        }

        let mut pending = self
            .pending
            .lock()
            .expect("the pending switch is never poisoned");
        let owed = match (policy, *pending) {
            // By the proof on the type: `Requested` exists only while the
            // slot is occupied, and this entry just found it empty.
            (_, PendingSwitch::Requested(_)) => {
                unreachable!("a plan approval can only be Requested while a turn holds the slot")
            }
            (_, PendingSwitch::None) => Owed::Nothing,
            // Only the build direction leaves a sentence riding; the enter
            // door writes no message of its own, so applying it finishes it
            // (**D477**).
            (PendingPolicy::Apply, PendingSwitch::Announced(SwitchTo::Build)) => {
                *pending = PendingSwitch::SentencePending;
                Owed::Apply(SwitchTo::Build)
            }
            (PendingPolicy::Apply, PendingSwitch::Announced(SwitchTo::Plan)) => {
                *pending = PendingSwitch::None;
                Owed::Apply(SwitchTo::Plan)
            }
            (PendingPolicy::Apply, PendingSwitch::SentencePending) => Owed::Nothing,
            (PendingPolicy::Discard | PendingPolicy::Clear, _) => {
                *pending = PendingSwitch::None;
                Owed::Nothing
            }
            (PendingPolicy::Revoke, PendingSwitch::Announced(_)) => {
                *pending = PendingSwitch::None;
                Owed::ReassertRow
            }
            (PendingPolicy::Revoke, PendingSwitch::SentencePending) => {
                *pending = PendingSwitch::None;
                Owed::Nothing
            }
        };
        drop(pending);

        Ok((guard, owed))
    }

    /// A read-only look at the turn in flight, for the cancel, reply and steer
    /// observers — the paths that change no engine state and therefore owe no
    /// policy. What they reach through the handle is the running turn's own
    /// cells; the slot itself and the approval beside it are untouched.
    async fn observe<R>(&self, look: impl FnOnce(Option<&TurnHandle>) -> R) -> R {
        look(self.slot.lock().await.as_ref())
    }
}

/// Composes the environment half of the system prompt for a model.
///
/// Taken as a function rather than as the config and directory it is composed
/// from, so that the engine's dependency here is exactly what it uses — a
/// model's name in, a prompt half out — and not a whole config a later reader
/// would start reading other answers out of.
type Environment = dyn Fn(&str) -> Option<String> + Send + Sync;

/// Owns the turn lifecycle and publishes what happens during it.
pub struct Engine {
    provider: Arc<dyn Provider>,
    /// The model and agent the next turn runs as; see [`Active`].
    ///
    /// Between a plan approval's boundary and the next engine entry this can
    /// lag: the session row and the emitted `AgentChanged` already say build
    /// while the agent here still says plan. **In that bounded window the row
    /// and the event are the truth about the agent**, and nothing reads this
    /// field for the agent without first passing an entry that applies the
    /// switch — see the proof on [`TurnSlot`].
    active: std::sync::Mutex<Active>,
    /// The half of the system prompt an agent replaces: the base prompt for
    /// the model's family, composed by [`crate::instruction::base_prompt`].
    /// [`None`] is an engine nobody configured, which every scripted and
    /// golden run relies on.
    ///
    /// Behind a lock for the reason the suffix is: which prompt this is depends
    /// on the model's family, and the model can change under a session that is
    /// already assembled; see [`Engine::with_base_for_model`].
    base_prompt: std::sync::Mutex<Option<String>>,
    /// Whether that half is recomposed for the family of whatever model is
    /// active. `false` leaves whatever [`Engine::with_system_parts`] was given
    /// standing for the session, which is what every scripted and golden run
    /// wants.
    base_follows_model: bool,
    /// The half no agent replaces — the environment block and the instruction
    /// files — which is why it is held apart from the base prompt rather than
    /// concatenated into it: switching agents swaps one and keeps the other.
    ///
    /// Behind a lock because the environment block states the model as fact,
    /// and the model can change under a session that is already assembled; see
    /// [`Engine::with_environment`].
    prompt_suffix: std::sync::Mutex<Option<String>>,
    /// How that half is composed for a given model, when the caller handed a
    /// way to compose it. [`None`] leaves whatever
    /// [`Engine::with_system_parts`] was given standing for the session, which
    /// is what every scripted and golden run wants.
    ///
    /// Behind a lock, like the three siblings below that the `/plugin`
    /// dialog's Reload swaps (**D474**): a lead's engine is shared with the
    /// socket that serves it (**D505**), so the frontend no longer holds it
    /// exclusively and a `&mut` seam would be one it could not reach. The
    /// lock is a `std::sync::Mutex` and is **never held across an await**:
    /// every reader clones the value out and drops the guard, and every
    /// writer is a synchronous swap — three of the four are read on the
    /// turn path, so a guard that lived across an await would be a guard a
    /// turn could block on.
    environment: std::sync::Mutex<Option<Arc<Environment>>>,
    /// Agents this session may run as. [`None`] leaves every turn on the base
    /// prompt with no agent rules, which is what an engine built for a golden
    /// run wants.
    agents: Option<Arc<agent::Registry>>,
    /// Tools as the caller handed them over, without the task tool and
    /// without anything an MCP server lent. What every rebuild below starts
    /// from. Locked for `environment`'s reason.
    base_tools: std::sync::Mutex<Arc<Registry>>,
    /// Where a turn's `$name` invocations load skills from — the same value
    /// the frontend's `skill` tool was installed over, handed here through
    /// [`Engine::with_skill_roots`] so a user invocation and a model's
    /// `skill` call read one list. Empty by default, which keeps every
    /// fixture and golden run off the machine it happens to be on. Locked
    /// for `environment`'s reason.
    skill_roots: std::sync::Mutex<crate::tool::skill::Roots>,
    /// [`Engine::base_tools`] plus whatever the connected MCP servers are
    /// currently lending. What a subagent is offered — the same set the parent
    /// has, minus the task tool it never gets.
    ///
    /// Behind its own lock because a connect finishing has to change it
    /// without disturbing a turn that is already holding a snapshot, and
    /// behind an [`Arc`] because one thing outside this engine has to read it
    /// *live*: the in-process teammate backend composes each teammate's set at
    /// the spawn rather than at the install, and a handle is the only way to
    /// do that without the backend holding the engine that holds the backend.
    lent_tools: Arc<std::sync::Mutex<Arc<Registry>>>,
    /// MCP servers this session was configured with, once somebody installed
    /// them. [`None`] is every engine that was never given any, which is every
    /// scripted and golden run.
    mcp: Option<Arc<mcp::Servers>>,
    /// Which [`mcp::Servers::generation`] the registries above were built
    /// from, so a rebuild happens exactly when the tool surface moved.
    mcp_installed: std::sync::Mutex<u64>,
    /// The session-lifetime activated set (**D492**): names a `tool_search`
    /// hit, an executed `mcp__*` call, or resume seeding put back on the
    /// advertised roster. Insert-only between `NewSession`s, and the one
    /// shared handle every [`deferral::Deferral`](crate::tool::deferral)
    /// clone writes through — which is what makes "a tool this conversation
    /// has touched is never un-advertised" a property of the handle rather
    /// than a discipline.
    activated_tools: Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
    /// Which names defer, over the handle above, as last computed on the
    /// shared composition path ([`Engine::compose`]). What every turn clones.
    /// Empty candidates on every engine whose registry holds no `mcp__*`
    /// names — every scripted and golden run.
    deferral: std::sync::Mutex<crate::tool::deferral::Deferral>,
    /// The composed registry's definitions, as last rebuilt — the snapshot
    /// `tool_search` answers from, shared rather than copied so a
    /// reconnect's recomposition is what a later search reads.
    tool_definitions: Arc<std::sync::Mutex<Vec<crate::tool::ToolDefinition>>>,
    /// The advertised `mcp__*` budget, from the config's
    /// `tool_defer_threshold`; see [`Engine::with_defer_threshold`].
    defer_threshold: usize,
    /// Language servers this session may run. [`None`] is a session whose
    /// config asked for none, which is the default and every scripted and
    /// golden run. Nothing starts here: a server is spawned by the first touch
    /// of a file it claims, and nothing else ever touches one.
    lsp: Option<Arc<lsp::Lsp>>,
    /// What every turn's file changes are recorded against, so `/undo` can put
    /// them back. [`None`] is an engine nobody installed any on — every
    /// scripted, golden and PTY run — where `/undo` refuses rather than
    /// silently moving the transcript.
    snapshots: Option<Arc<snapshot::Snapshots>>,
    /// How far back an `/undo` has walked, when one has.
    ///
    /// Held here rather than only on the session record because an in-memory
    /// engine has no record: the store is where this *outlives* the process,
    /// not where it lives.
    revert: std::sync::Mutex<Option<snapshot::RevertState>>,
    /// Tools the model is offered, and the agent loop executes.
    ///
    /// Behind a lock because the task tool's *description* is the roster of
    /// agents the current one may delegate to, so switching agents rebuilds
    /// the set rather than mutating a tool that several turns may be reading.
    tools: std::sync::Mutex<Arc<Registry>>,
    /// Slash commands this session can run: the builtins plus whatever the
    /// config described.
    commands: Arc<command::Registry>,
    /// Rules deciding which tool calls wait for the user.
    permissions: Arc<std::sync::Mutex<Permissions>>,
    /// Rules the frontend imposed for the life of this engine, which sit above
    /// whatever agent a turn runs as.
    ///
    /// The permission set has two tiers — the agent's ruleset beneath, the
    /// answers a person gave on top — and neither is these. A headless run
    /// refuses the tools that would ask a question nobody is there to answer:
    /// that is not the agent's opinion, so it must survive the agent changing,
    /// and it is not a person's answer, so it must not outrank one. Held here
    /// and appended by [`Engine::baseline_for`] to every baseline the engine
    /// installs or derives, which is what keeps it true through all five of
    /// them (deviation: standing-rules-outlive-the-agent).
    standing: std::sync::Mutex<Vec<Rule>>,
    /// Directory tool calls resolve relative paths against, captured once so
    /// every call in a session agrees on where it is.
    cwd: PathBuf,
    /// Where the project starts. A `!` command runs here, a mentioned file is
    /// named relative to here, and `/init`'s `${path}` is this.
    root: PathBuf,
    /// Which files this session has read, shared by every tool call in it.
    files: Arc<FileTimes>,
    /// Where this build keeps its credentials, handed to every tool call so
    /// that `read` and `grep` can refuse the file.
    ///
    /// Resolved once per engine, at construction: the store cannot move while
    /// ganja runs, a guard that could be pointed somewhere harmless by setting
    /// an environment variable mid-run would not be worth much, and `grep`
    /// would otherwise re-derive the path for every file it walks past.
    /// [`None`] is a machine with no home directory to resolve a store
    /// against, where there is nothing here to protect.
    credentials: Credentials,
    /// What reports changes to those files, once somebody started one.
    /// [`None`] is an engine nobody asked to watch — every scripted, golden
    /// and PTY run — where a file changed outside the session is noticed by
    /// the next write that touches it and not before.
    ///
    /// Held only so that it is not dropped: dropping it ends the watch.
    watcher: std::sync::Mutex<Option<watch::Watcher>>,
    /// The id of the session this engine is on — the one every event names.
    ///
    /// Minted at construction, so an ephemeral engine's session has a name
    /// even though it has no row; adopted by the first prompt's lazy create
    /// on a persistent engine, so turn-1's events and the stored session
    /// agree; replaced by [`Engine::resume`] before the resumed revert is
    /// announced; re-minted by [`Command::NewSession`], because the next
    /// conversation is a different session and a stale id here would have
    /// its lazy create upsert over the previous one's row. Agent and model
    /// switches never touch it — they change what the session runs as, not
    /// which session it is.
    ///
    /// Behind an [`Arc`] since the admission gate landed (**D524**): the
    /// gate's event drain stamps each hold transition with the current id
    /// **at publish**, exactly where every non-turn publish stamps, and that
    /// task outlives any borrow of this struct it could hold.
    session: Arc<std::sync::Mutex<SessionId>>,
    /// Every subscriber's queue; the one place events leave the engine.
    fanout: Arc<Fanout>,
    /// The receiver of the queue the engine was born with, waiting for the
    /// first subscriber to claim it. Everything published before that claim —
    /// a resume's `RevertChanged` most of all — is buffered here rather than
    /// lost, which is what lets a frontend resume first and subscribe second.
    unclaimed: Mutex<Option<mpsc::Receiver<Event>>>,
    /// The turn in flight and the pending plan approval, behind the two-path
    /// discipline documented on [`TurnSlot`].
    turn: TurnSlot,
    /// The conversation the next request carries. On a persistent engine this
    /// is the live window — everything from the compaction summary onward —
    /// rather than the whole stored transcript.
    history: Arc<Mutex<Vec<Message>>>,
    /// The store and the live session, when this engine persists. [`None`]
    /// is [`Engine::new`]'s in-memory engine, and with it every P4 behaviour
    /// is absent: no write-through, no auto-title, no compaction.
    persistence: Option<Arc<SessionState>>,
    /// Background jobs this session has started — `bash` calls run with
    /// `run_in_background: true`. Always present, unlike `mcp`/`lsp`: there
    /// is no engine a background job cannot outlive.
    jobs: Arc<job::JobRegistry>,
    /// The team this session leads, once somebody gave it one (**D500**,
    /// **D501**) — the registry that owns every teammate's lifetime, behind
    /// the door a `task` call carrying a `name` goes through. [`None`] is
    /// every session that leads no team, which is every scripted, golden and
    /// PTY run — and every teammate's own engine, since a teammate leads
    /// nobody.
    ///
    /// Beside [`Engine::jobs`] rather than inside a turn for the reason that
    /// registry is: a teammate's lifetime is not a turn's, so a turn's cancel
    /// must not end one. Unlike jobs it is optional, because a session with no
    /// team has no directory on disk and should leave none.
    teammates: Option<Arc<subagent::Teammates>>,
    /// Where this session's own `send_message` calls are posted, stamped with
    /// **this engine's own** outbound identity (**D498**) — which is the lead's
    /// name on a lead, and the member's own on either kind of member.
    ///
    /// Three installers, one per kind of session, and every one of them binds
    /// the name at construction so nothing can choose it per send:
    /// [`Engine::with_teammates`] for the lead, [`Engine::with_postbox`] for a
    /// pane teammate speaking as itself, and [`Engine::install_postbox`] for an
    /// in-process teammate — whose postbox only exists once
    /// [`teammate::TeammateRegistry`] holds both the team and the teammate, which
    /// is after this engine was built. That last one's own doc says why it is the
    /// exception and why it is not public.
    postbox: std::sync::Mutex<Option<Arc<dyn crate::tool::team::Postbox>>>,
    /// Every teammate's permission dialogs, waiting for the lead side to claim
    /// the receiver (**D-5**). See [`Engine::teammate_dialogs`].
    teammate_dialogs: std::sync::Mutex<Option<mpsc::Receiver<teammate::posture::Forwarded>>>,
    /// The roster the offered `send_message` was last described against, so a
    /// teammate spawned mid-session is addressable at the next turn rather
    /// than never. The same once-per-turn-start memo `mcp_installed` keeps,
    /// for the same reason: a rebuild happens exactly when what it renders
    /// moved.
    team_roster: std::sync::Mutex<Vec<Peer>>,
    /// The posture the **next** turn runs under (**D-15**, **D496**).
    ///
    /// Written the moment a [`Command::SetPermissionMode`] arrives, mid-turn
    /// included, and read once at a turn's start into [`Bypass`]. Not reset by
    /// [`Command::NewSession`], for the reason the model and the agent are
    /// not: it is what this engine runs as, and neither the person nor the
    /// lead who set it asked for it back.
    permission_mode: std::sync::Mutex<PermissionMode>,
    /// What a config asked to be run at the nine moments [`crate::hook`]
    /// names. [`None`] is an engine whose config asked for none, which does no
    /// hook work at all rather than inert hook work at nine seams. Locked
    /// for `environment`'s reason.
    hooks: std::sync::Mutex<Option<Arc<hook::Hooks>>>,
    /// What a `SessionStart` hook asked to put in front of the model, waiting
    /// for a turn that can deliver it.
    ///
    /// Queued rather than standing (**D460**): the reminders channel appends to
    /// the last user message of *every* request, so a standing entry would
    /// repeat a session's opening context on each one. Delivered by the next
    /// turn that asks the model and then gone — the same discipline the stale-
    /// file notice already keeps, at the same seam.
    hook_context: std::sync::Mutex<Vec<String>>,
    /// How many `task` calls from one assistant step may run at the same time.
    ///
    /// A plain number rather than an [`Option`], because the config's own
    /// default is resolved before it gets here
    /// ([`crate::config::AgentsConfig::concurrency`]) and an engine nobody
    /// configured still has to have an answer.
    concurrency: usize,
    /// The config's `small_model`, handed to every turn this engine starts so
    /// that the title request can prefer it over the catalog's cheapest row.
    ///
    /// Held verbatim, prefix and all: whether the spec binds is a question
    /// about the provider, and this engine's provider is fixed while its
    /// *model* is not — so the answer is taken where the request is built
    /// rather than cached here. [`None`] is a config that named none, which is
    /// the default and every scripted and golden run.
    small_model: Option<String>,
    /// The receiver-side admission gate (**D523**–**D525**): what this
    /// session, where it leads a team, does with a peer message from outside
    /// that team before anything is delivered. Always present — an engine
    /// with no team simply never feeds it — and shared, because the socket
    /// door, the lead's §6.2 pass and the settlement commands all read one
    /// buffer and two sets.
    inbound: Arc<teammate::inbound::Inbound>,
    /// The receiving half of the gate's ordered transition queue (M10),
    /// waiting for [`Engine::with_teammates`] to spawn the drain task that
    /// turns each transition into a published [`Event`]. Take-once, like the
    /// birth event queue; still here on a session that never leads a team,
    /// where nothing can enqueue onto it.
    inbound_drain:
        std::sync::Mutex<Option<mpsc::UnboundedReceiver<teammate::inbound::HoldTransition>>>,
    /// The flush handshake into that drain task: [`Engine::shutdown_settle`]
    /// sends a one-shot acknowledger and the drain answers it only once its
    /// queue is empty, which is what makes the shutdown flush a bounded wait
    /// on real publication rather than a sleep. [`None`] until the drain task
    /// exists.
    inbound_flush: std::sync::Mutex<Option<mpsc::UnboundedSender<oneshot::Sender<()>>>>,
    /// Whether this session started under the D479 bypass trio
    /// (`--auto`/`--yolo`/`--dangerously-skip-permissions`) — **for the
    /// receiver classifier only** (**D523**, user-ratified 2026-08-25).
    /// Dialog auto-answering stays the frontend's; D479's "answers a dialog;
    /// repeals no rule" semantics are untouched by this bit.
    inbound_bypass: bool,
    /// Which session a name points at, and what a person is told about the
    /// name they typed (**D528**). Always present, the admission gate's own
    /// posture: a session that registers nothing simply feeds it no records
    /// of its own, and a resolution consulted from a session with no name of
    /// its own excludes nothing new. Seeded at assembly from the socket
    /// directory the binder and the lister read
    /// ([`Engine::with_socket_directory`]), so the hidden `--socket-dir`
    /// override reaches every reader the same way.
    identity: Arc<Identity>,
    /// This session's own self-name (**D530**, **ADJ-2**): the value
    /// [`Engine::with_solo_postbox`]'s postbox stamps `from` with as
    /// `<self-name>@solo`, read fresh at send time so `/rename` moves the
    /// *next* send without this cell holding a stale copy. Every interactive
    /// session gets one — a lead's own registration name rides the same
    /// value — set through the one seam, [`Engine::set_self_name`], that
    /// only a frontend calls; a model's arguments cannot reach it. Defaults
    /// to [`crate::tool::registry::FALLBACK_NAME`] until seeded, the same
    /// fallback the D527 name grammar's own sanitizer falls back to.
    self_name: Arc<std::sync::Mutex<String>>,
    /// Whether this session's `send_message` posts through the solo postbox
    /// rather than a team's (**D530**, **D531**): the input the call-time
    /// posture computation reads alongside [`Engine::teamless_send`]. Set
    /// `false` by every [`Engine::install_postbox`] call and `true`
    /// afterward by [`Engine::install_solo_postbox`] alone, so installing
    /// any other kind of postbox — a lead's, a member's — always clears it.
    teamless: AtomicBool,
    /// The resolved `teamless_send` posture (**D531**): `Unasked` unless a
    /// frontend's config named `ask` through
    /// [`Engine::with_teamless_send`]. Consulted only while
    /// [`Engine::teamless`] is set — in-team `send_message` stays D498's
    /// static ladder regardless of this value, the ruling's own "in a
    /// session that holds a team, this key has no effect at all".
    teamless_send: TeamlessSend,
}

impl Engine {
    /// Builds an engine that answers through `provider`, asking it for
    /// `model`, executing calls to `tools` under `permissions`.
    ///
    /// The engine is in-memory: the transcript lives and dies with the
    /// process, and session operations answer [`EngineError::Ephemeral`].
    /// Tests and demos rely on that absence — nothing here ever touches a
    /// disk or spends a provider request on bookkeeping.
    #[must_use]
    pub fn new(
        provider: Arc<dyn Provider>,
        model: impl Into<String>,
        tools: Arc<Registry>,
        permissions: Permissions,
    ) -> Self {
        Self::assemble(provider, model.into(), tools, permissions, None)
    }

    /// Builds an engine whose sessions live in `storage`.
    ///
    /// The first prompt creates a session (or [`Engine::resume`] installs a
    /// stored one), every turn writes itself through as it streams, a
    /// completed first turn earns the session a title, and a session whose
    /// last request filled 90% of its model's context window is compacted
    /// before the next turn.
    #[must_use]
    pub fn persistent(
        provider: Arc<dyn Provider>,
        model: impl Into<String>,
        tools: Arc<Registry>,
        permissions: Permissions,
        storage: Storage,
    ) -> Self {
        Self::assemble(
            provider,
            model.into(),
            tools,
            permissions,
            Some(Arc::new(SessionState {
                storage,
                live: std::sync::Mutex::new(LiveSession::default()),
            })),
        )
    }

    fn assemble(
        provider: Arc<dyn Provider>,
        model: String,
        tools: Arc<Registry>,
        permissions: Permissions,
        persistence: Option<Arc<SessionState>>,
    ) -> Self {
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        // Captured at construction so a process whose directory later moves
        // keeps resolving paths where the session began. A process with no
        // readable directory falls back to relative resolution.
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let root = crate::project::Project::resolve(&cwd).root().to_owned();
        // Unset policy and the default review window, until a frontend hands
        // the config's answer through `with_inbound_policy`; inert either way
        // on a session that never leads a team.
        let (inbound, inbound_drain) = teammate::inbound::Inbound::new(
            teammate::inbound::ResolvedInbound::new(None),
            crate::config::DialogExpiry::default(),
        );

        let engine = Self {
            provider,
            active: std::sync::Mutex::new(Active {
                model,
                effort: None,
                agent: None,
                previous_agent: None,
            }),
            base_prompt: std::sync::Mutex::new(None),
            base_follows_model: false,
            prompt_suffix: std::sync::Mutex::new(None),
            environment: std::sync::Mutex::new(None),
            agents: None,
            // The task tool is never one of these: it exists only once the
            // engine knows which agents it may spawn, which is
            // `with_agents`'s business.
            base_tools: std::sync::Mutex::new(Arc::clone(&tools)),
            skill_roots: std::sync::Mutex::new(crate::tool::skill::Roots::none()),
            lent_tools: Arc::new(std::sync::Mutex::new(Arc::clone(&tools))),
            mcp: None,
            mcp_installed: std::sync::Mutex::new(0),
            activated_tools: Arc::default(),
            deferral: std::sync::Mutex::new(crate::tool::deferral::Deferral::none()),
            tool_definitions: Arc::default(),
            defer_threshold: crate::config::DEFAULT_TOOL_DEFER_THRESHOLD,
            lsp: None,
            snapshots: None,
            revert: std::sync::Mutex::new(None),
            tools: std::sync::Mutex::new(tools),
            commands: Arc::new(command::Registry::builtin(&root)),
            permissions: Arc::new(std::sync::Mutex::new(permissions)),
            standing: std::sync::Mutex::new(Vec::new()),
            cwd,
            root,
            files: Arc::new(FileTimes::default()),
            // An engine whose store cannot be resolved has nothing to guard,
            // and says so in a form a construction site cannot leave blank.
            credentials: crate::auth::store_path()
                .ok()
                .map_or(Credentials::Unguarded, Credentials::Guarded),
            watcher: std::sync::Mutex::new(None),
            session: Arc::new(std::sync::Mutex::new(SessionId::ascending())),
            fanout: Arc::new(Fanout::new(events)),
            unclaimed: Mutex::new(Some(receiver)),
            turn: TurnSlot::new(),
            history: Arc::default(),
            persistence,
            jobs: Arc::new(job::JobRegistry::new()),
            teammates: None,
            postbox: std::sync::Mutex::new(None),
            teammate_dialogs: std::sync::Mutex::new(None),
            team_roster: std::sync::Mutex::new(Vec::new()),
            permission_mode: std::sync::Mutex::new(PermissionMode::Ask),
            hooks: std::sync::Mutex::new(None),
            hook_context: std::sync::Mutex::new(Vec::new()),
            concurrency: crate::config::AgentsConfig::DEFAULT_CONCURRENCY,
            small_model: None,
            inbound: Arc::new(inbound),
            inbound_drain: std::sync::Mutex::new(Some(inbound_drain)),
            inbound_flush: std::sync::Mutex::new(None),
            inbound_bypass: false,
            identity: Arc::new(Identity::default()),
            self_name: Arc::new(std::sync::Mutex::new(
                crate::tool::registry::FALLBACK_NAME.to_owned(),
            )),
            teamless: AtomicBool::new(false),
            teamless_send: TeamlessSend::default(),
        };
        // The builder-time run of the shared composition path, for the one
        // engine no rebuild ever reaches: a fixture whose `mcp__*` names
        // arrived as base tools. A registry without such names composes to
        // an empty candidate set and the same `Arc`, so every other caller
        // is untouched by construction.
        engine.recompose_tools();

        engine
    }

    /// Sets what the model is told before it is told anything else, as its two
    /// halves.
    ///
    /// Consuming rather than a setter, so it composes with either constructor
    /// and cannot be called on an engine that is already streaming a turn. The
    /// prompt is captured once and carried by every request a turn makes —
    /// including the one that summarizes a conversation for compaction, which
    /// is what stops a compacted session from losing the instructions the rest
    /// of it was written under. Two [`None`]s leave the requests without one,
    /// which is what [`Engine::new`]'s scripted and golden runs depend on.
    ///
    /// `base` is the half an agent replaces — the prompt for the model's
    /// family — and `suffix` is the half none of them do: the environment
    /// block and the instruction files, which describe where the session is
    /// working and are true of every agent that works there. They are kept
    /// apart rather than concatenated because switching agents has to swap one
    /// and keep the other, and a single string cannot be taken back apart.
    ///
    /// Joined by a bare newline, as upstream's `session/llm/request.ts` joins
    /// them, and [`None`] only when neither half says anything.
    #[must_use]
    pub fn with_system_parts(mut self, base: Option<String>, suffix: Option<String>) -> Self {
        self.base_prompt = std::sync::Mutex::new(base);
        self.prompt_suffix = std::sync::Mutex::new(suffix);

        self
    }

    /// Keeps the base half composed for the family of whichever model the
    /// session is asking, rather than for the one it launched on.
    ///
    /// The base prompt is chosen by family — Anthropic's, OpenAI's, or the one
    /// for everything else — so a session that switches across families and
    /// keeps the prompt it launched with runs the new model under another
    /// family's instructions, inside a prompt whose environment block has
    /// already moved on and names the new one. Installing this composes that
    /// half now, and again after anything that moves the active model.
    ///
    /// Takes no way to compose one, where [`Engine::with_environment`] does:
    /// the environment half is composed from a config and a working directory
    /// the engine does not hold, while the base half is composed from the
    /// model's name alone and [`crate::instruction::base_prompt`] is already in
    /// this crate. A closure here would be indirection that bought nothing —
    /// and would let a caller install a base that disagrees with the family
    /// table.
    ///
    /// Supersedes whatever base [`Engine::with_system_parts`] was given, so the
    /// two cannot disagree; a caller with a base of its own — a scripted run, a
    /// golden run — simply does not ask for this.
    #[must_use]
    pub fn with_base_for_model(mut self) -> Self {
        self.base_follows_model = true;
        self.recompose_base();

        self
    }

    /// The base half as it currently stands.
    fn base_half(&self) -> Option<String> {
        self.base_prompt
            .lock()
            .expect("the system prompt is never poisoned")
            .clone()
    }

    /// Composes the base half again for the family of the model that is active
    /// now.
    ///
    /// Does nothing unless [`Engine::with_base_for_model`] asked for it, which
    /// is what leaves a scripted engine's own base alone. Called beside
    /// [`Engine::recompose_environment`] at every site that moves the active
    /// model: the two halves are written against the same model and a site that
    /// moved one without the other would leave the prompt describing two.
    fn recompose_base(&self) {
        if !self.base_follows_model {
            return;
        }
        let composed = crate::instruction::base_prompt(&self.model()).to_owned();

        *self
            .base_prompt
            .lock()
            .expect("the system prompt is never poisoned") = Some(composed);
    }

    /// Keeps the suffix half composed for whichever model the session is
    /// asking, rather than for the one it launched on.
    ///
    /// The environment block states the model as fact — twice, in the sentence
    /// above `<env>` — so a session that switches model mid-conversation and
    /// keeps the block it started with tells the new model it is the old one.
    /// Installing this recomposes that half now, and again after anything that
    /// moves the active model.
    ///
    /// Supersedes whatever suffix [`Engine::with_system_parts`] was given, so
    /// the two cannot disagree; a caller with a fixed suffix simply does not
    /// install one of these.
    #[must_use]
    pub fn with_environment(
        self,
        compose: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        *self
            .environment
            .lock()
            .expect("the environment composer is never poisoned") = Some(Arc::new(compose));
        self.recompose_environment();

        self
    }

    /// Replaces how the environment half is composed, in place, and
    /// recomposes it now — [`Engine::with_environment`]'s in-session twin,
    /// and one third of the `/plugin` dialog's Reload seam (**D474**,
    /// declared at that action): a reload that moves the skill roots has to
    /// move the `<available_skills>` block the closure composes too, or the
    /// prompt keeps advertising skills the tool no longer serves.
    ///
    /// `&self` over the field's lock rather than `&mut self`, since P25: the
    /// one caller is a frontend, but a lead's engine is also the one its
    /// session socket serves (**D505**), held through an [`Arc`] by both, so
    /// an exclusive borrow is not something that frontend can produce. The
    /// contract is unchanged — a turn clones what it needs at its start and
    /// keeps it, so a swap lands at the next turn and never under one.
    pub fn replace_environment(
        &self,
        compose: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) {
        *self
            .environment
            .lock()
            .expect("the environment composer is never poisoned") = Some(Arc::new(compose));
        self.recompose_environment();
    }

    /// The environment half as it currently stands.
    fn environment_half(&self) -> Option<String> {
        self.prompt_suffix
            .lock()
            .expect("the system prompt is never poisoned")
            .clone()
    }

    /// Composes the suffix half again for the model that is active now.
    ///
    /// Does nothing when no way to compose one was installed, which is what
    /// leaves a scripted engine's literal suffix alone.
    fn recompose_environment(&self) {
        let compose = self
            .environment
            .lock()
            .expect("the environment composer is never poisoned")
            .clone();
        let Some(compose) = compose else {
            return;
        };
        let composed = compose(&self.model());

        *self
            .prompt_suffix
            .lock()
            .expect("the system prompt is never poisoned") = composed;
    }

    /// Sets the agents this session may run as, and starts it on the
    /// registry's default.
    ///
    /// The default's ruleset becomes the permission baseline immediately: an
    /// engine that had agents but judged its first turn without them would be
    /// running the agent's prompt under somebody else's rules.
    #[must_use]
    pub fn with_agents(mut self, agents: Arc<agent::Registry>) -> Self {
        self.agents = Some(Arc::clone(&agents));

        let start = agents.default_agent().to_owned();
        if let Some(agent) = agents.get(&start) {
            self.install(agent);
            {
                let mut active = self.active();
                active.agent = Some(start);
                if let Some(model) = agent.model.as_deref().and_then(|model| self.adopt(model)) {
                    active.model = model;
                }
            }
            // The default agent may prefer a model of another family, and both
            // halves are written against whichever one the session ends up on:
            // the base prompt is that family's, and the environment block names
            // the model.
            self.recompose_environment();
            self.recompose_base();
        }

        self
    }

    /// Sets how many `task` calls from one assistant step may run at once.
    ///
    /// Clamped to at least one rather than refused: the config path already
    /// refuses a zero by name ([`crate::config`]'s `check_agents`), and a
    /// caller reaching this builder with one anyway means a batch that never
    /// starts — which is the one outcome nobody can have wanted.
    #[must_use]
    pub fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = concurrency.max(1);

        self
    }

    /// Gives the admission gate the config's answer (**D523**): the explicit
    /// `cross_session_inbound` value with the tier that established it —
    /// [`crate::config::Config::inbound_policy`]'s pair, since the merged
    /// config keeps only the winner — and the `dialog_expiry` review window.
    ///
    /// Consuming, and it must be: the expiry is baked into the gate at its
    /// construction, so this **rebuilds** the gate and its transition queue,
    /// which is only sound while nothing else holds them — exactly what a
    /// builder-phase engine guarantees and a shared one could not.
    #[must_use]
    pub fn with_inbound_policy(
        mut self,
        explicit: Option<(crate::config::InboundPolicy, ganja_protocol::PolicySource)>,
        expiry: crate::config::DialogExpiry,
    ) -> Self {
        let (inbound, drain) = teammate::inbound::Inbound::new(
            teammate::inbound::ResolvedInbound::new(explicit),
            expiry,
        );
        self.inbound = Arc::new(inbound);
        *self
            .inbound_drain
            .lock()
            .expect("the inbound drain slot is never poisoned") = Some(drain);

        self
    }

    /// Records that this session started under the D479 bypass trio — **for
    /// the receiver classifier only** (**D523**, user-ratified 2026-08-25).
    ///
    /// A classification seed, not a posture: dialog auto-answering stays the
    /// frontend's own (`--yolo` answers dialogs in the TUI, and repeals no
    /// rule), and nothing about [`Command::SetPermissionMode`]'s slot moves.
    /// What it changes is one reading — [`Engine::receiver_class`] answers
    /// bypass for a session so started, which is exactly the session whose
    /// every unset-policy inbound then holds `no_mode_asserted` rather than
    /// delivering into a run nobody is gating.
    #[must_use]
    pub fn with_inbound_bypass(mut self, seeded: bool) -> Self {
        self.inbound_bypass = seeded;

        self
    }

    /// Sets the advertised `mcp__*` budget — the config's
    /// `tool_defer_threshold` (**D492**) — and recomputes the deferral over
    /// the current set, so a builder-time registry already carrying `mcp__*`
    /// names defers under the budget it was just given rather than the
    /// default it was born with.
    #[must_use]
    pub fn with_defer_threshold(mut self, threshold: usize) -> Self {
        self.defer_threshold = threshold;
        self.recompose_tools();

        self
    }

    /// Sets the config's `small_model` — the model this session's title
    /// request prefers over the catalog's cheapest chat-capable row.
    ///
    /// Taken as written, including its `provider/` prefix: a spec naming
    /// another provider's model is passed over at the request, with a line
    /// saying so ([`crate::config::model_bound_to`]). Nothing is refused here
    /// and nothing is validated — a title is the one request in a session that
    /// may name a model this build never checked, and the wire's refusal is
    /// already caught by the retry that asks the session's own model.
    #[must_use]
    pub fn with_small_model(mut self, small_model: Option<String>) -> Self {
        self.small_model = small_model;

        self
    }

    /// How many `task` calls from one assistant step may run at once.
    ///
    /// The read side of [`with_concurrency`](Self::with_concurrency), and the
    /// only one: nothing in the engine asks an engine what its cap is. It is
    /// here so an assembly seam's own test can see whether the config's cap
    /// reached the engine a real session runs on — the runtime half is pinned
    /// over a hand-built engine in `tests/parallel_subagents.rs`, which cannot
    /// see how a frontend built one.
    #[must_use]
    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    /// Sets the MCP servers this session may use.
    ///
    /// Installing them connects nothing: [`Engine::connect_mcp`] is what
    /// starts that, and it is a separate call because a caller may want the
    /// engine assembled before anything reaches the network.
    #[must_use]
    pub fn with_mcp(mut self, servers: Arc<mcp::Servers>) -> Self {
        self.mcp = Some(servers);

        self
    }

    /// Connects every enabled MCP server, in the background.
    ///
    /// Returns immediately. A server that connects lends its tools to the
    /// registry the *next* turn is built with; a server that fails says so
    /// through [`Engine::mcp_status`] and costs nothing else. Nothing here can
    /// fail the engine, and nothing here can end a turn.
    pub fn connect_mcp(&self) {
        let Some(servers) = self.mcp.clone() else {
            return;
        };
        if servers.is_empty() {
            return;
        }

        tokio::spawn(async move { servers.connect_all().await });
    }

    /// Where every configured MCP server stands.
    ///
    /// Empty on an engine with no servers, and on one whose servers are all
    /// still being dialled — a server with no status yet is one nothing has
    /// finished trying.
    ///
    /// A connection that has gone away is noticed here as well as at the turn
    /// seam, so that a frontend polling this is never shown a `connected` that
    /// stopped being true. Its tools still leave the registry at the next turn
    /// and not at this call: what a turn is offered is decided once, before it
    /// starts.
    #[must_use]
    pub fn mcp_status(&self) -> BTreeMap<String, mcp::Status> {
        let Some(servers) = &self.mcp else {
            return BTreeMap::new();
        };
        servers.reap();

        servers.status()
    }

    /// How many tools each connected MCP server currently lends — the same
    /// count [`Engine::mcp_status`]'s companion for a `/mcp` row or `ganja
    /// mcp`'s listing, [`mcp::Servers::tool_counts`] under the engine's own
    /// servers.
    #[must_use]
    pub fn mcp_tool_counts(&self) -> BTreeMap<String, usize> {
        let Some(servers) = &self.mcp else {
            return BTreeMap::new();
        };

        servers.tool_counts()
    }

    /// Every configured MCP server, by name — including one still on its
    /// first dial, absent from [`Engine::mcp_status`]'s map until that
    /// resolves; see [`mcp::Servers::names`].
    #[must_use]
    pub fn mcp_names(&self) -> Vec<String> {
        self.mcp
            .as_ref()
            .map_or_else(Vec::new, |servers| servers.names())
    }

    /// Re-dials one MCP server by name; see [`mcp::Servers::reconnect`].
    ///
    /// `Err` when there are no MCP servers configured at all, naming the
    /// server the same way [`mcp::Servers::reconnect`]'s own refusals do —
    /// there is nothing this engine could mean by the name either way.
    pub async fn reconnect_mcp(&self, name: &str) -> Result<(), String> {
        let Some(servers) = &self.mcp else {
            return Err(format!("mcp server \"{name}\" is not configured"));
        };

        servers.reconnect(name).await
    }

    /// Whether `name` is a remote MCP server configured with `oauth` — what
    /// gates a Login action on it; see [`mcp::Servers::has_oauth`].
    #[must_use]
    pub fn mcp_has_oauth(&self, name: &str) -> bool {
        self.mcp
            .as_ref()
            .is_some_and(|servers| servers.has_oauth(name))
    }

    /// The URL a login for `name` wants opened, while one is in flight; see
    /// [`mcp::Servers::login_url`].
    #[must_use]
    pub fn mcp_login_url(&self, name: &str) -> Option<String> {
        self.mcp
            .as_ref()
            .and_then(|servers| servers.login_url(name))
    }

    /// Starts an OAuth login for one MCP server; see [`mcp::Servers::start_login`].
    pub async fn login_mcp(&self, name: &str) -> Result<(), String> {
        let Some(servers) = &self.mcp else {
            return Err(format!("mcp server \"{name}\" is not configured"));
        };

        servers.start_login(name).await
    }

    /// Closes every MCP connection and ends every local server's process
    /// group.
    ///
    /// The door for a caller holding only an engine, which is what a test that
    /// asked for one with servers already connected has. A frontend keeps its
    /// own `Arc<Servers>` and shuts down through that instead — one layer
    /// down, the same call — because `App::run` consumes the engine.
    pub async fn shutdown_mcp(&self) {
        if let Some(servers) = &self.mcp {
            servers.shutdown().await;
        }
    }

    /// The background jobs this session has started — `bash` calls run with
    /// `run_in_background: true` — for a status display to poll.
    #[must_use]
    pub fn jobs(&self) -> &Arc<job::JobRegistry> {
        &self.jobs
    }

    /// What the vendor last said is left of this account's budget — the
    /// rate-limit headers its responses carry (**D484**,
    /// `rate-limit-visibility`), for a status display to poll on its tick
    /// exactly as [`Engine::jobs`] and [`Engine::context_estimate`] are.
    ///
    /// Deliberately not a protocol event, and deliberately not engine-held
    /// state: the buckets live on the wire that received them
    /// ([`Provider::rate_windows`]) because what they measure is the
    /// *credential's* budget, not a conversation's. So this reads through
    /// rather than caching, a resume neither clears nor rebuilds anything —
    /// the account did not change when the session did — and a wire that
    /// receives no such headers answers with an empty set that every surface
    /// renders as nothing.
    ///
    /// A bucket past its own reset is still returned, carrying that reset:
    /// deciding what an expired window looks like belongs to whoever draws it,
    /// and dropping it here would hide the fact that the vendor once spoke.
    /// Since P22 a bucket may carry **no** reset at all — grok sends the two
    /// counts and no `-reset-` field — and such a window is never expired,
    /// because there is no clock against which it could be: it lives until the
    /// next response replaces the whole set. A surface that draws a reset must
    /// therefore render its absence rather than assume one
    /// ([`crate::provider::RateWindow::reset`] is an [`Option`], and that is
    /// the amendment recorded at the field).
    ///
    /// [`Provider::rate_windows`]: crate::provider::Provider::rate_windows
    #[must_use]
    pub fn rate_windows(&self) -> Vec<crate::provider::RateWindow> {
        self.provider.rate_windows()
    }

    /// What the vendor last said is left of this account's **plan** — the 5h
    /// and weekly analogues D471 could not draw and the W-A1 probe found in
    /// headers after all (**D485**, declared in
    /// [`crate::provider::rate`]).
    ///
    /// Additive beside [`Engine::rate_windows`], never a widening of it: that
    /// accessor's shape and every caller of it are untouched, because a plan
    /// bucket is a different measurement rather than a fuller one. Everything
    /// its neighbour's doc says about *how* it is held — on the wire, per
    /// credential, read through rather than cached, surviving a resume — holds
    /// here for the same reasons.
    ///
    /// A window past its own reset is still returned for that neighbour's
    /// reason; a window whose vendor sent no reset is returned too, and means
    /// exactly that.
    ///
    /// [`Provider::plan_windows`]: crate::provider::Provider::plan_windows
    #[must_use]
    pub fn plan_windows(&self) -> Vec<crate::provider::PlanWindow> {
        self.provider.plan_windows()
    }

    /// Ends every background job's whole process group. Mirrors
    /// [`Engine::shutdown_mcp`]/[`Engine::shutdown_lsp`]: idempotent, and
    /// safe to call on an engine that started none.
    pub async fn shutdown_jobs(&self) {
        self.jobs.shutdown().await;
    }

    /// Gives this session the team `registry` holds (**D500**, **D501**).
    ///
    /// Consuming, like every other installer here: whether a session leads a
    /// team is decided once, before anything can be streaming. What the team
    /// then *holds* is not — teammates come and go — which is why the two
    /// surfaces that draw it poll [`Engine::teammates`] rather than being
    /// told.
    ///
    /// Four things are wired here because this is the one moment they all
    /// exist: the three backends a `task` call may name, of which only the
    /// in-process one holds anything of this engine's; the lead's own postbox,
    /// stamped with the lead's name and no other; the dialog channel a
    /// teammate's asks travel to the lead on ([`Engine::teammate_dialogs`]),
    /// attached before anything can be spawned into it; and the tool set,
    /// recomposed because a team is what puts `send_message` in front of the
    /// model at all — presence is ability, the same rule `task` is registered
    /// under.
    ///
    /// **What a teammate is offered** is the *lent* set — this build's tools
    /// plus whatever the MCP servers are lending **at the moment that teammate
    /// is spawned**, which is not the moment the team was installed: the
    /// servers dial in the background, and a set snapshotted here would be the
    /// empty one they had not filled yet — with `send_message` on it and
    /// nothing else: no `task`, for the reason
    /// a subagent has none (a teammate is not a place to nest a second team),
    /// and no plan doors, which want an agent roster a teammate engine has
    /// not got. Its description names the lead, the one peer that exists
    /// before any teammate does and cannot go away; who *else* it may address
    /// is answered per call by its own postbox, which is live. That set is
    /// then composed under this session's own `tool_defer_threshold`, so a
    /// teammate holding the lead's `mcp__*` names defers exactly what the lead
    /// defers and is offered the same `tool_search` back in (**D492**).
    ///
    /// **A session with no store gets the team and no in-process backend.** A
    /// teammate's conversation is a root row somebody may resume (D-8), and
    /// there is nowhere to write one; the surface refuses by name rather than
    /// running a teammate whose transcript evaporates.
    #[must_use]
    pub fn with_teammates(mut self, registry: Arc<teammate::TeammateRegistry>) -> Self {
        let lead = Arc::new(subagent::Postbox::lead(
            &registry,
            Some((&self.identity, Arc::clone(&self.session))),
        ));
        let in_process: Arc<dyn teammate::TeammateBackend> = match self.storage() {
            Some(storage) => Arc::new(teammate::InProcess::lending(
                Arc::clone(&self.provider),
                self.teammate_tools(&registry),
                storage,
                self.teammate_permissions(),
                // The lead's own budget, so a teammate offered the lead's MCP
                // tools defers the same set of them (**D492**).
                self.defer_threshold,
            )),
            None => Arc::new(Storeless),
        };

        let (dialogs, waiting) = mpsc::channel(TEAMMATE_DIALOGS);
        registry.forward_dialogs_to(dialogs);
        *self
            .teammate_dialogs
            .lock()
            .expect("the dialog queue is never poisoned") = Some(waiting);

        self.teammates = Some(Arc::new(subagent::Teammates::new(
            registry,
            subagent::Backends {
                in_process,
                pane: Arc::new(teammate::pane::GanjaPane),
                claude: Arc::new(teammate::claude::ClaudePane),
                // **D512 (P28)**: all three shim slots open the CLI's own
                // native TUI in a pane (`teammate::shim_tui`), spoken to
                // through bracketed paste, and **no spawn door in this build
                // reaches the headless `teammate::shim::ShimBackend`** any
                // more — that machinery stays in the tree, unit-tested,
                // reachable only by the tests that drive it against a fake
                // CLI. Which is also why `teammates.shim_turn_timeout` is
                // not read here: a pane-mode shim has no per-turn deadline
                // (the module doc owns why), and the key governs only the
                // headless machinery it was written for (**D509**).
                //
                // This slot searches the real `PATH`; a test that reaches it
                // spawns the developer's own `codex`. Tests spawn shim
                // teammates through `ganja_testkit::backends` or
                // `tests/shim_support::lead`, never through `with_teammates`.
                //
                // The trap worth naming: a `#[cfg(test)]` guard would not fire
                // here. Integration tests link this lib as an ordinary
                // dependency with `cfg(test)` unset, so the guard is this
                // comment and those two safe doors rather than anything the
                // compiler checks.
                codex: Arc::new(teammate::shim_tui::ShimTui::new(Arc::new(
                    teammate::codex::Codex::new(),
                ))),
                // Real as of Dv-7, and carrying the codex slot's hazard for
                // the same reason. No deadline is handed over any more: the
                // `--print-timeout` a resident launch line derived from one
                // belongs to the headless shape, and a TUI is launched with
                // its floors alone.
                agy: Arc::new(teammate::shim_tui::ShimTui::new(Arc::new(
                    teammate::agy::Agy::new(),
                ))),
                // Real as of W5, and carrying the codex slot's hazard for the
                // same reason: it searches the real `PATH`, so a test that
                // reached it would spawn the developer's own `grok`. The two
                // safe doors are the same two.
                grok: Arc::new(teammate::shim_tui::ShimTui::new(Arc::new(
                    teammate::grok::Grok::new(),
                ))),
            },
        )));
        self.install_postbox(lead);
        self.recompose_tools();
        // A team is what makes the gate reachable — the socket exists only
        // for a lead session — so this is where its event drain starts.
        self.spawn_inbound_drain();

        self
    }

    /// Starts the gate's ordered event forwarder and its expiry timers (M10,
    /// **D524**): one task that drains [`teammate::inbound::HoldTransition`]s
    /// into the fanout — stamping each with the **current** session id at
    /// publish, the same slot every non-turn publish stamps — spawns a
    /// deadline timer per parity hold, aborts it on that hold's settlement,
    /// and answers [`Engine::shutdown_settle`]'s flush handshake only when
    /// its queue is empty.
    ///
    /// Decoupled from the socket door's request path on purpose: a lossless
    /// subscriber makes a publisher wait, so publishing inside
    /// [`Engine::receive_peer_message`] would let one wedged SSE client delay
    /// — and jitter — every peer's POST. What a stalled drain delays is event
    /// visibility, never a decision.
    ///
    /// Guarded on a live runtime rather than assumed into one: the one
    /// builder-path caller outside a runtime is a synchronous unit test's
    /// engine, which spawns no teammates and admits nothing, so skipping the
    /// drain there loses observability of events that cannot occur.
    fn spawn_inbound_drain(&self) {
        if tokio::runtime::Handle::try_current().is_err() {
            tracing::debug!(
                "no async runtime at team install; the inbound gate's events will not publish"
            );

            return;
        }
        let Some(mut transitions) = self
            .inbound_drain
            .lock()
            .expect("the inbound drain slot is never poisoned")
            .take()
        else {
            // A second team install on one engine; the first task stands.
            return;
        };
        let (flush, mut flushes) = mpsc::unbounded_channel::<oneshot::Sender<()>>();
        *self
            .inbound_flush
            .lock()
            .expect("the inbound flush slot is never poisoned") = Some(flush);

        let fanout = Arc::clone(&self.fanout);
        let session = Arc::clone(&self.session);
        // Weak on both counts, so the drain task keeps neither the gate nor
        // the team alive: when the engine goes, the gate's sender drops, the
        // queue ends, and the task ends with it.
        let inbound = Arc::downgrade(&self.inbound);
        let teammates = self.teammates.as_ref().map(Arc::downgrade);

        tokio::spawn(async move {
            let mut timers: BTreeMap<String, tokio::task::JoinHandle<()>> = BTreeMap::new();
            let mut flushes_open = true;
            loop {
                let transition = if flushes_open {
                    tokio::select! {
                        // Biased so a flush is answered only once every
                        // transition enqueued before it has been published —
                        // the whole meaning of the handshake.
                        biased;
                        transition = transitions.recv() => transition,
                        request = flushes.recv() => match request {
                            Some(ack) => {
                                // Reached only while the transition queue is
                                // empty at this poll: everything earlier is
                                // in the fanout.
                                let _ = ack.send(());
                                continue;
                            }
                            None => {
                                // The engine dropped its flush handle; a
                                // closed channel answers `None` instantly, so
                                // leave the select before it becomes a spin.
                                flushes_open = false;
                                continue;
                            }
                        },
                    }
                } else {
                    transitions.recv().await
                };
                let Some(transition) = transition else {
                    break;
                };

                match &transition {
                    teammate::inbound::HoldTransition::Held {
                        id,
                        expires_in_ms: Some(wait),
                        ..
                    } => {
                        // The expiry re-check's narrowness arrives as data:
                        // only a parity hold carries a deadline, so only a
                        // parity hold gets a timer.
                        let timer = tokio::spawn(expire_hold(
                            *wait,
                            id.clone(),
                            inbound.clone(),
                            teammates.clone(),
                        ));
                        timers.insert(id.as_str().to_owned(), timer);
                    }
                    teammate::inbound::HoldTransition::Held { .. } => {}
                    teammate::inbound::HoldTransition::Settled { id, .. } => {
                        if let Some(timer) = timers.remove(id.as_str()) {
                            timer.abort();
                        }
                    }
                }

                let current = session
                    .lock()
                    .expect("the session id is never poisoned")
                    .clone();
                let _ = fanout.send(transition.into_event(current)).await;
            }
            for (_, timer) in timers {
                timer.abort();
            }
        });
    }

    /// The team this session leads, for a status display, the `/team` dialog
    /// or a turn's `task` call — polled exactly as [`Engine::jobs`] is.
    ///
    /// [`None`] is a session that leads no team, which is a different answer
    /// from a team with nobody in it: the first has no directory on disk and
    /// the second does. What a frontend draws comes from
    /// [`subagent::Teammates::registry`]'s
    /// [`view`](teammate::TeammateRegistry::view).
    #[must_use]
    pub fn teammates(&self) -> Option<&Arc<subagent::Teammates>> {
        self.teammates.as_ref()
    }

    /// The team this session leads, as anything that only *renders* it reads
    /// it — `GET /team` on either of `ganja-serve`'s transports (D-13,
    /// **D505**), and the same value the `/team` dialog draws.
    ///
    /// Polled, exactly as [`Engine::teammates`] is, and derived from it: one
    /// [`view`](teammate::TeammateRegistry::view) over the registry, so a
    /// socket and a terminal looking at the same session at the same moment
    /// see the same roster. [`None`] is a session that leads no team, which
    /// is a different answer from a team of nobody — the first has no
    /// directory on disk, and a route serves it as "not found" rather than as
    /// an empty roster.
    #[must_use]
    pub fn team_view(&self) -> Option<ganja_protocol::team::TeamView> {
        self.teammates.as_ref().map(|team| team.registry().view())
    }

    /// A plain message another session sent over **this** session's socket,
    /// delivered into this team (**D505**) — the engine's side of
    /// `ganja-serve`'s socket-only `POST /team/{name}/message`, and since
    /// **D523** the seam the admission gate sits in.
    ///
    /// The route reaches the team through this rather than through the
    /// registry so that serve invents no state and holds no team: the rungs
    /// a peer's message climbs, and the write it is delivered through, are
    /// `subagent`'s, the same code a local teammate's message goes through,
    /// and the identity it is stamped with is decided there (`Postbox::peer`)
    /// and nowhere a route could choose. Between the two halves the gate
    /// decides — synchronously, because this answer **is** the receipt
    /// channel: a `Deliver` writes and answers the uniform arrival note, a
    /// `Held` writes nothing and names its cause (the reference's own held
    /// receipt), and a `Silent` — an explicit refuse, a guard drop, the
    /// queue cap — writes nothing and answers **byte-identically** to the
    /// accept, traced with its typed reason and told to nobody (v2
    /// §"Explicit outcomes (`P8a`)", evidence 620644-620683).
    ///
    /// The `NoTeam` refusal stays first, ahead of every rung and any policy:
    /// it is the structural analogue of the reference's kill-switch refuse —
    /// a session with no team has no gate to consult (v2 §"Bundle gate
    /// (`Hg()`)", evidence 220730-220742, and the divergence note in
    /// [`teammate::inbound`]'s module doc).
    ///
    /// # Errors
    ///
    /// [`NotReceived::NoTeam`](crate::NotReceived::NoTeam) when this session
    /// leads no team, and otherwise whichever rung the message failed — the
    /// ladder's shape errors predate policy and keep their own sentences.
    pub async fn receive_peer_message(
        &self,
        incoming: crate::Incoming,
    ) -> Result<crate::tool::team::Sent, crate::NotReceived> {
        let Some(team) = &self.teammates else {
            return Err(crate::NotReceived::NoTeam);
        };
        let registry = team.registry();

        let message = subagent::receive_ladder(registry, incoming)?;
        let lead = message.lead.clone();
        let admission = self.inbound.admit_socket(
            self.receiver_class(),
            &message.from,
            &message.text,
            message.summary.as_deref(),
        );
        match admission {
            teammate::inbound::SocketAdmission::Deliver => {
                let (sent, identity) = subagent::deliver_to_lead(registry, message).await?;
                // After the write that minted it (M6): only now does the
                // identity exist to record, and the §6.2 pass delivers what
                // the set holds without re-running policy or guards (D525).
                self.inbound.admit_identity(identity);

                Ok(sent)
            }
            teammate::inbound::SocketAdmission::Held {
                cause,
                evicted_prune,
            } => {
                if let Some(identity) = evicted_prune {
                    // A capacity eviction's mailbox-door victim, pruned
                    // best-effort: its record is already settled `expired`,
                    // and an entry a failed prune leaves behind re-gates as a
                    // fresh hold — fail-closed — never delivers.
                    if !prune_lead_inboxes(registry, &identity).await {
                        tracing::warn!(
                            ?identity,
                            "an evicted hold's inbox entry could not be pruned; it re-gates as a fresh hold"
                        );
                    }
                }

                Ok(crate::tool::team::Sent {
                    to: lead,
                    note: subagent::held_note(cause),
                })
            }
            teammate::inbound::SocketAdmission::Silent(reason) => {
                // The typed reason is the whole receiver-side record; the
                // sender reads the same bytes an accept answers.
                tracing::info!(
                    from = message.from,
                    reason = ?reason,
                    "an inbound peer message was dropped without telling the sender"
                );

                Ok(crate::tool::team::Sent {
                    to: lead,
                    note: subagent::RECEIVED.to_owned(),
                })
            }
        }
    }

    /// This session's receiver permission class, as the admission gate's
    /// parity matrix reads it (**D523**): bypass iff the engine's mode is
    /// [`PermissionMode::Bypass`] or the session started under the D479 trio
    /// ([`Engine::with_inbound_bypass`]); prompting otherwise.
    ///
    /// Total today — both sources are plain engine state — so this never
    /// answers [`None`]; the [`Option`] is the resolver's fail-closed slot
    /// (an unreadable mode holds `mode_unknown`), carried so every caller
    /// hands the gate the shape it decides on.
    #[must_use]
    pub fn receiver_class(&self) -> Option<teammate::inbound::ReceiverClass> {
        Some(teammate::inbound::classify_receiver(
            self.permission_mode(),
            self.inbound_bypass,
        ))
    }

    /// The admission gate itself, for the one consumer outside this file
    /// that shares its state: the lead's §6.2 pass
    /// ([`teammate::lead_inbox::LeadInbox`]'s gated construction), whose
    /// mailbox door, held-index and admitted set are the same ones the
    /// socket door above feeds.
    #[must_use]
    pub fn inbound(&self) -> &Arc<teammate::inbound::Inbound> {
        &self.inbound
    }

    /// The messages the admission gate is holding for review — what `/held`,
    /// the approval dialog's countdown and the `N held` status segment poll,
    /// in the [`Engine::team_view`]/[`Engine::jobs`] family (**D524**).
    #[must_use]
    pub fn held_messages(&self) -> Vec<teammate::inbound::HeldEntry> {
        self.inbound.held_messages()
    }

    /// The queue this session's teammates raise their permission dialogs on
    /// (**D-5**), claimed once by whoever is going to answer them.
    ///
    /// Take-once, exactly like the birth event queue: there is one receiver
    /// and handing it to two readers would split the dialogs between them.
    /// [`None`] on a session with no team, and on a second call. A lead that
    /// never claims it leaves its teammates' asks refused rather than hanging
    /// — the channel fills, and [`teammate::posture::Forwarding`] answers what
    /// it cannot deliver.
    ///
    /// Each [`teammate::posture::Forwarded`] carries the teammate's name, the
    /// `PermissionRequested` its turn published, and the sender the answer
    /// goes back on; dropping that sender is the refusal a dialog nobody could
    /// show means.
    #[must_use]
    pub fn teammate_dialogs(&self) -> Option<mpsc::Receiver<teammate::posture::Forwarded>> {
        self.teammate_dialogs
            .lock()
            .expect("the dialog queue is never poisoned")
            .take()
    }

    /// Gives this session the postbox its own `send_message` calls are posted
    /// through — the door for a process that **is** a teammate (§4.1, §10.3).
    ///
    /// Consuming, like [`Engine::with_teammates`], and for the same reason:
    /// whose name a session writes under is decided once, before anything can
    /// be streaming. A pane launched by some other session's lead installs a
    /// [`teammate::member::MemberPostbox`] here, stamped with the name its
    /// launch line carried, and is offered `send_message` from its first turn
    /// — presence is ability, the same rule [`Engine::with_teammates`]
    /// registers the tool under, and the same composition path, so a
    /// `/plugin` Reload cannot quietly drop it. Nothing else changes: this
    /// session leads no team, [`Engine::teammates`] stays [`None`], and no
    /// dialog channel is opened, because a member's asks travel the other way.
    ///
    /// The identity is the postbox's own — bound when it was built, never a
    /// parameter here — so what this hands the engine is a value that already
    /// cannot say anything but its own name.
    #[must_use]
    pub fn with_postbox(self, postbox: Arc<dyn crate::tool::team::Postbox>) -> Self {
        self.install_postbox(postbox);
        self.recompose_tools();

        self
    }

    /// Installs the postbox one engine's `send_message` calls are posted
    /// through.
    ///
    /// `&self` for the one caller a consuming builder could not serve: a
    /// teammate's own postbox is built by [`teammate::TeammateRegistry`] once
    /// it holds both the team and the teammate, and a teammate engine is
    /// reachable only by shared reference (which is what keeps it from being
    /// given snapshots). [`Engine::with_teammates`] and
    /// [`Engine::with_postbox`] land here too, before the engine is shared.
    ///
    /// `pub(crate)` because a postbox is an engine's **outbound identity** —
    /// the name every message it writes is stamped with, bound at construction
    /// precisely so nothing can choose it per send. A public setter on `&self`
    /// would hand that choice back: any caller holding an `&Engine` could
    /// re-stamp a running conversation as somebody else, which is the forgery
    /// [`crate::subagent::Postbox`] exists to make impossible. The public door
    /// is the consuming builder above, which can only run before the engine
    /// is anybody's to hold.
    ///
    /// Also clears [`Engine::teamless`] (**D530**, **D531**): every kind
    /// this hands the postbox mutex — a lead's, a member's, a real
    /// teammate's — is a session that holds *something other than* the solo
    /// postbox, so the call-time posture computation must stop reading
    /// [`Engine::teamless_send`] the moment any of them lands. Only
    /// [`Engine::install_solo_postbox`] sets the flag back, and it does so
    /// **after** calling this, which is what makes the two calls compose
    /// into "team spawns ⇒ this clears it; team ends ⇒ that sets it again"
    /// without either seam needing to know the other ran.
    pub(crate) fn install_postbox(&self, postbox: Arc<dyn crate::tool::team::Postbox>) {
        *self.postbox.lock().expect("the postbox is never poisoned") = Some(postbox);
        self.teamless.store(false, Ordering::Relaxed);
    }

    /// Installs the solo postbox (**D530**): a session that leads no team,
    /// addressing other live sessions by name or by `uds:` address, bound to
    /// this engine's own self-name cell, identity resolver and live session
    /// id. Consuming, like every other assembly-time installer here.
    #[must_use]
    pub fn with_solo_postbox(self) -> Self {
        self.install_solo_postbox();

        self
    }

    /// [`Engine::with_solo_postbox`]'s mechanism, and [`Engine::retire_team`]'s
    /// — the one place a [`subagent::SoloPostbox`] is built, so the two
    /// callers cannot drift into building it two different ways.
    fn install_solo_postbox(&self) {
        let solo = Arc::new(subagent::SoloPostbox::new(
            Arc::clone(&self.self_name),
            Arc::clone(&self.identity),
            Arc::clone(&self.session),
        ));
        self.install_postbox(solo);
        // After `install_postbox`'s own reset, which is what makes this the
        // "swaps back" half of D530/F10's bidirectional seam.
        self.teamless.store(true, Ordering::Relaxed);
        self.recompose_tools();
    }

    /// Installs `registry`'s lead postbox on this session (**D530**, **F10**:
    /// "a team spawning mid-session flips the **D531** computed default back
    /// to allow, with no rule mutation"), reusing `Engine::install_postbox`'s
    /// own anti-forgery shape — the caller hands a [`teammate::TeammateRegistry`],
    /// never a bare identity, so nothing here can stamp an arbitrary sender
    /// name. [`Engine::with_teammates`] calls this at assembly; nothing else
    /// in this build calls it mid-session yet, because nothing yet *has* a
    /// team to hand over after assembling teamless — the seam exists so the
    /// posture computation is provably correct the day something does, and
    /// [`Engine::retire_team`] is its exact reverse.
    ///
    /// Narrower than [`Engine::with_teammates`] on purpose: it does not touch
    /// [`Engine::teammates`] (the `task` door) or the dialog-forwarding
    /// channel, so a caller after this method alone still cannot spawn a
    /// teammate — only `send_message`'s posture and roster description move.
    pub fn install_team(&self, registry: &Arc<teammate::TeammateRegistry>) {
        let lead = Arc::new(subagent::Postbox::lead(
            registry,
            Some((&self.identity, Arc::clone(&self.session))),
        ));
        self.install_postbox(lead);
        self.recompose_tools();
    }

    /// The reverse of [`Engine::install_team`] (**D530**, **F10**'s "team
    /// ends ⇒ solo postbox swaps back"): reinstalls the solo postbox, so a
    /// send afterward stamps `from` as `<self-name>@solo` again and carries
    /// the one-way note. `subagent::TEAM_GONE` cannot answer such a
    /// send — `subagent::SoloPostbox` holds no
    /// `Weak<teammate::TeammateRegistry>` to fail upgrading in the first
    /// place, which is the structural half of **AC-42**.
    pub fn retire_team(&self) {
        self.install_solo_postbox();
    }

    /// Sets this session's self-name (**D530**, **ADJ-2**): the value the
    /// solo postbox stamps `from` with, and — for a lead — the name a
    /// frontend's own registration record carries. The one seam every
    /// `/rename` calls, whether or not this session leads: a lead's own
    /// wire identity (`<name>@<team>`) is untouched, because `/rename` never
    /// renames a team member, but its self-name still moves so a team-end
    /// swap-back sends under the name the person last chose. Only a
    /// frontend calls this — a model's arguments cannot reach it, the same
    /// property [`crate::tool::team::Postbox`]'s own doc states of `from`.
    pub fn set_self_name(&self, name: impl Into<String>) {
        *self
            .self_name
            .lock()
            .expect("the self-name cell is never poisoned") = name.into();
    }

    /// The self-name [`Engine::set_self_name`] last set, or the derived
    /// fallback no frontend has replaced yet.
    #[must_use]
    pub fn self_name(&self) -> String {
        self.self_name
            .lock()
            .expect("the self-name cell is never poisoned")
            .clone()
    }

    /// Seeds the identity resolver's socket directory (**D528**): the same
    /// directory the binder binds under and the lister lists from, so the
    /// hidden `--socket-dir` override reaches every reader identically.
    /// Consuming, like every other assembly-time seed here.
    #[must_use]
    pub fn with_socket_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.identity = Arc::new(Identity::new(directory));

        self
    }

    /// Sets the resolved `teamless_send` posture (**D531**) a teamless
    /// session's `send_message` calls run under. Consuming, like the config
    /// values this mirrors (`Engine::concurrency`'s own pattern): resolved
    /// once by a frontend from its config and handed in, never read from a
    /// `Config` here — the engine stays as ignorant of *where* the value came
    /// from as `ganja-permission`'s own `gate_with_default` stays of this
    /// value's own source.
    #[must_use]
    pub fn with_teamless_send(mut self, policy: TeamlessSend) -> Self {
        self.teamless_send = policy;

        self
    }

    /// Ends every teammate this session started, waiting for each one's turn
    /// to settle rather than killing it.
    ///
    /// Idempotent like [`Engine::shutdown_mcp`] and [`Engine::shutdown_jobs`]
    /// — the engine-side twin of the registry shutdown the lead frontend runs
    /// through the handle it kept; `run` and `serve` lead no team, so nothing
    /// else calls it. What it must never
    /// do is shut down what a teammate *shares*: an in-process teammate holds
    /// the lead's MCP servers and language servers when it holds any at all,
    /// and those die with the lead one layer below the engine. So this calls
    /// through to [`teammate::TeammateRegistry::shutdown`], which settles each
    /// teammate's own turn and ends its own background jobs — and nothing
    /// else's.
    pub async fn shutdown_teammates(&self) {
        // Held inbound settles first (**D524**): the holds are a lead's
        // surfaces and the lead is what is ending, so their `expired` events
        // are flushed while the fanout is still being read.
        self.shutdown_settle().await;
        if let Some(teammates) = &self.teammates {
            teammates.registry().shutdown().await;
        }
    }

    /// Settles every held inbound message `expired` and gives the event
    /// drain a **bounded** wait — `SHUTDOWN_SETTLE_FLUSH`, 750 ms — to move
    /// the settlements into the fanout, then proceeds regardless: the
    /// reference's own shutdown bound (v2 §"Shutdown", evidence
    /// 620390-620431).
    ///
    /// Idempotent like [`Engine::shutdown_mcp`] and [`Engine::shutdown_jobs`]
    /// — a second call finds nothing held and an empty queue to flush. Per
    /// C1, `expired` here settles the **review record**: a mailbox-door
    /// hold's durable inbox entry is deliberately left in place for
    /// next-start re-gating (the no-lost-mail half), while a socket-door
    /// hold is gone with the process — a crash provides no settlement at all,
    /// and the same split applies.
    ///
    /// The wait is a handshake, not a sleep: the drain task acknowledges
    /// only once its queue is empty, so a healthy fanout completes in
    /// microseconds and only a wedged lossless subscriber spends the bound.
    pub async fn shutdown_settle(&self) {
        self.inbound.shutdown_settle();
        let flush = self
            .inbound_flush
            .lock()
            .expect("the inbound flush slot is never poisoned")
            .clone();
        let Some(flush) = flush else {
            // No drain task — a session that never led a team, whose gate
            // never held anything to announce.
            return;
        };
        let (ack, flushed) = oneshot::channel();
        if flush.send(ack).is_err() {
            return;
        }
        let _ = tokio::time::timeout(SHUTDOWN_SETTLE_FLUSH, flushed).await;
    }

    /// The store this engine persists into, for a second engine that shares
    /// it: a **clone** of the handle, never a second open of the same path,
    /// because the connection, the writer thread and the migration all live
    /// behind the `Arc` a clone shares.
    fn storage(&self) -> Option<Storage> {
        self.persistence.as_ref().map(|state| state.storage.clone())
    }

    /// The tool set a teammate of this session is offered, as a factory the
    /// backend runs **at each spawn**; see [`Engine::with_teammates`] for what
    /// is in it and why.
    ///
    /// A factory rather than a set, and the reason is timing: MCP servers are
    /// dialled in the background at startup, and a team is installed while the
    /// engine is being assembled. A set built here would therefore be whatever
    /// the servers had lent by the time the *builder* ran — in practice
    /// nothing — and every teammate of that session would be offered no MCP
    /// tools at all, for the whole session, however long the lead had been
    /// using them. Read per spawn, a teammate gets what the lead's own next
    /// turn would get.
    ///
    /// The roster it describes `send_message` against is fixed, and may be:
    /// the lead is the one peer that exists before any teammate does and
    /// cannot go away, and who *else* a teammate may address is answered per
    /// call by its own postbox.
    fn teammate_tools(
        &self,
        registry: &Arc<teammate::TeammateRegistry>,
    ) -> impl Fn() -> Arc<Registry> + Send + Sync + use<> {
        let lead = Peer {
            name: registry.lead().as_str().to_owned(),
            description: None,
            lead: true,
        };
        let lent = Arc::clone(&self.lent_tools);

        move || {
            let base = Arc::clone(&lent.lock().expect("the tool registry is never poisoned"));

            Arc::new(base.with(Arc::new(send_message::SendMessageTool::new(
                std::slice::from_ref(&lead),
            ))))
        }
    }

    /// The ruleset each teammate engine is built with (**D-5**).
    ///
    /// A factory rather than one ruleset bound up front, because the agent
    /// is per spawn: a `task` call names a `subagent_type`, and a teammate
    /// started as `plan` that ran under `build`'s rules would be a plan
    /// agent that can write. So each spawn resolves its own agent, and that
    /// agent's rules are what the lead's own refusals are appended *after* —
    /// [`teammate::posture::permissions_for`]'s anti-laundering order.
    ///
    /// The lead's rules are read through the live handle at each spawn rather
    /// than copied here, so a teammate started after somebody answered
    /// "always" runs with that answer.
    fn teammate_permissions(&self) -> impl Fn(&teammate::SpawnSpec) -> Permissions + use<> {
        let lead = Arc::clone(&self.permissions);
        let agents = self.agents.clone();

        move |spec| {
            let rules = agents
                .as_ref()
                .and_then(|registry| registry.get(&spec.agent_type))
                .map(|agent| agent.rules.clone())
                .unwrap_or_default();
            let held = lead
                .lock()
                .expect("the permission rules are never poisoned");

            teammate::posture::permissions_for(&held, rules)
        }
    }

    /// Sets what this session runs at the nine moments [`crate::hook`] names.
    ///
    /// Consuming, like every other installer here: what a session runs around
    /// its own turns is decided once, before anything can be streaming.
    ///
    /// Installing them fires nothing. [`Engine::session_start`] is what opens
    /// the session, and it is a separate call for [`Engine::connect_mcp`]'s
    /// reason — the engine is assembled before anything of its own runs, and a
    /// constructor that spawned somebody's shell command would be a constructor
    /// that can hang.
    #[must_use]
    pub fn with_hooks(self, hooks: Arc<hook::Hooks>) -> Self {
        self.replace_hooks(Some(hooks));

        self
    }

    /// Where a turn's `$name` invocations load skills from.
    ///
    /// The caller hands the same `instruction::skill_roots` value it built
    /// the `skill` tool over, which is what keeps the prompt's
    /// `<available_skills>`, the tool's answers and a `$` invocation one
    /// list. An engine never given roots expands every name to the tool's
    /// own not-found sentence — honest, and exactly what a fixture wants.
    #[must_use]
    pub fn with_skill_roots(self, roots: crate::tool::skill::Roots) -> Self {
        self.replace_skill_roots(roots);

        self
    }

    /// Replaces the skill roots in place — the `/plugin` dialog's Reload,
    /// which swaps these beside [`Engine::replace_base_tools`] so the next
    /// turn's `$` invocations read the same list its rebuilt `skill` tool
    /// does. A running turn keeps the roots it started with, like every
    /// other value a turn clones at its start.
    pub fn replace_skill_roots(&self, roots: crate::tool::skill::Roots) {
        *self
            .skill_roots
            .lock()
            .expect("the skill roots are never poisoned") = roots;
    }

    /// The roots [`Engine::with_skill_roots`] installed, for a frontend
    /// composing `$name` invocations against the same list a turn will
    /// expand them from.
    #[must_use]
    pub fn skill_roots(&self) -> crate::tool::skill::Roots {
        self.skill_roots
            .lock()
            .expect("the skill roots are never poisoned")
            .clone()
    }

    /// Replaces what runs at the nine hook moments, in place — the hooks
    /// half of the `/plugin` dialog's Reload seam (**D474**, declared at
    /// that action). A turn already in flight keeps the [`Arc`] it cloned
    /// when it started, so the swap lands at the next fire — the next
    /// turn's seams and the session-level hooks alike — never under a call
    /// that is already bracketed.
    ///
    /// [`None`] uninstalls: a reload that leaves no hooks table behind
    /// leaves an engine that does no hook work, exactly like one whose
    /// config never asked.
    pub fn replace_hooks(&self, hooks: Option<Arc<hook::Hooks>>) {
        *self.hooks.lock().expect("the hooks are never poisoned") = hooks;
    }

    /// The hooks as they currently stand, cloned out from under the lock so a
    /// caller never holds it across an await.
    fn hooks(&self) -> Option<Arc<hook::Hooks>> {
        self.hooks
            .lock()
            .expect("the hooks are never poisoned")
            .clone()
    }

    /// The base tools as they currently stand.
    fn base_tools(&self) -> Arc<Registry> {
        Arc::clone(
            &self
                .base_tools
                .lock()
                .expect("the base tools are never poisoned"),
        )
    }

    /// Fires `SessionStart` with the source a fresh session has, and keeps
    /// whatever context it asked for until a turn can deliver it.
    ///
    /// Called by a frontend once it has assembled the engine, rather than from
    /// the constructor: the constructor is synchronous, and it runs before
    /// [`Engine::with_hooks`] has installed anything for it to fire (a
    /// divergence from the plan's stated seam, which named `assemble`). The
    /// resume half lives in [`Engine::resume`], where a resumed session is
    /// installed.
    pub async fn session_start(&self) {
        self.fire_session_hook(hook::Payload::SessionStart {
            source: hook::Source::Startup,
        })
        .await;
    }

    /// Waits until no turn is in flight, or until `limit` runs out.
    ///
    /// A turn's finish event reaches its subscribers **before** the turn has
    /// finished: `MessageFinished` is queued first on purpose, and the tail
    /// after it — the plan-approval announcement, the `Stop` hook, the slot
    /// release — runs once it is out the door. A caller that stops reading at
    /// that event and then tears the process down therefore cuts the tail off
    /// mid-sentence, which is how a headless run came to fire every hook of a
    /// turn except the one defined to run at its end.
    ///
    /// Bounded, and by an argument rather than by a constant of its own: a hook
    /// somebody wrote already has a timeout, and a wait with no ceiling here
    /// would let a wedged one stop a script from ever exiting. Returns whether
    /// the engine really went idle, so a caller that cares can say so.
    pub async fn settle(&self, limit: std::time::Duration) -> bool {
        /// Between looks. Short enough that the common case — the tail is
        /// already done — costs one poll, and long enough that a slow hook is
        /// not paid for in wakeups.
        const BETWEEN: std::time::Duration = std::time::Duration::from_millis(10);

        let deadline = std::time::Instant::now() + limit;
        loop {
            if self.turn.observe(|turn| turn.is_none()).await {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(BETWEEN).await;
        }
    }

    /// Fires `SessionEnd` as a frontend shuts down.
    ///
    /// `reason` is one word, and this build's vocabulary for it is small on
    /// purpose: every route out of every frontend is somebody ending the
    /// process, so `"exit"` is what all three pass.
    pub async fn session_end(&self, reason: &str) {
        self.fire_session_hook(hook::Payload::SessionEnd {
            reason: reason.to_owned(),
        })
        .await;
    }

    /// Runs one session-level hook and files what it said: notices to the log,
    /// context to the queue the next turn drains.
    async fn fire_session_hook(&self, payload: hook::Payload) {
        let Some(hooks) = self.hooks() else {
            return;
        };
        let event = payload.event();
        let outcome = hooks.fire(self.session_id().as_str(), &payload).await;
        outcome.report(event);
        if !outcome.context.is_empty() {
            self.hook_context
                .lock()
                .expect("the hook context is never poisoned")
                .extend(outcome.context);
        }
    }

    /// Sets the language servers this session may run.
    ///
    /// There is no `connect` beside this one, as there is for MCP: a language
    /// server is started by the first touch of a file it claims, so installing
    /// the service is the whole of the wiring. An engine given none — which is
    /// every engine whose config did not ask — does no LSP work at all rather
    /// than doing inert LSP work.
    #[must_use]
    pub fn with_lsp(mut self, lsp: Arc<lsp::Lsp>) -> Self {
        self.lsp = Some(lsp);

        self
    }

    /// Ends every language server this session started.
    ///
    /// Dropping the engine does this too; the method exists so a frontend can
    /// stop them at a moment it chooses.
    pub fn shutdown_lsp(&self) {
        if let Some(lsp) = &self.lsp {
            lsp.shutdown();
        }
    }

    /// Starts reporting changes other people make to the files this session
    /// has read.
    ///
    /// Returns immediately, and is a separate call rather than part of
    /// assembly for [`Engine::connect_mcp`]'s reason: the engine is built
    /// before anything of its own starts running. Must be called from inside a
    /// tokio runtime.
    ///
    /// A watcher that will not start is one warning and nothing else — the
    /// session then behaves exactly as it did before watching existed, which
    /// is a read-before-write gate that notices a change when a write asks
    /// about it. Calling this twice replaces the watch rather than adding a
    /// second one.
    ///
    /// **Nothing here touches the filesystem**, so a startup path may call it
    /// whatever the project contains: the platform watcher is built, and every
    /// directory registered, on the watcher's own task. That is not a detail —
    /// registering a recursive watch on Linux is a synchronous walk of the
    /// whole tree, and this call sits before a terminal takeover.
    pub fn watch_files(&self) {
        *self
            .watcher
            .lock()
            .expect("the watcher slot is never poisoned") =
            Some(watch::Watcher::new(&self.root, Arc::clone(&self.files)));
    }

    /// Sets what this session's turns snapshot the working tree with.
    ///
    /// Consuming, like the other installers: what a session can undo is
    /// decided once, before anything can be streaming. An engine given none
    /// takes no snapshots and refuses [`Command::Undo`] — which is what every
    /// scripted, golden and PTY run wants, since none of them should be
    /// spawning git.
    #[must_use]
    pub fn with_snapshots(mut self, snapshots: Arc<snapshot::Snapshots>) -> Self {
        self.snapshots = Some(snapshots);

        self
    }

    /// Sets the slash commands this session can run.
    ///
    /// Consuming for the same reason [`Engine::with_system_parts`] is: the roster is
    /// resolved once, before anything can be streaming.
    ///
    /// This is also where a command **file** naming an agent nobody has is
    /// refused, by name and by file
    /// ([`command::Registry::refusing_unknown_agents`]) — the first moment
    /// both rosters exist, and still long before a turn. It therefore has to
    /// run **after** [`Engine::with_agents`], which is the order both
    /// frontends assemble in; called before it, the roster is left whole and
    /// the dispatch-time [`EngineError::UnknownAgent`] below is what such a
    /// command meets.
    #[must_use]
    pub fn with_commands(mut self, commands: Arc<command::Registry>) -> Self {
        self.commands = match &self.agents {
            Some(agents) => {
                Arc::new(Arc::unwrap_or_clone(commands).refusing_unknown_agents(agents.as_ref()))
            }
            None => commands,
        };

        self
    }

    /// The commands this session can run, for a palette to list.
    #[must_use]
    pub fn commands(&self) -> &Arc<command::Registry> {
        &self.commands
    }

    /// The permission rules this engine consults, shared with the agent loop
    /// and with whatever persists an "always" answer.
    #[must_use]
    pub fn permissions(&self) -> Arc<std::sync::Mutex<Permissions>> {
        Arc::clone(&self.permissions)
    }

    /// Imposes `rules` above every agent's ruleset, for the life of this
    /// engine.
    ///
    /// What a frontend uses to say something no agent may take back: a
    /// headless run refuses the tools that would ask a question nobody is
    /// there to answer. They land at the end of the baseline, so last-match-
    /// wins puts them over the agent's own rules and the config's — and still
    /// beneath the answers a person gave, which is where the two-tier order
    /// already put everything a build decided.
    ///
    /// Reaching through [`Engine::permissions`] to install the same rules is
    /// what this call exists to replace: a baseline written from outside is
    /// dropped by the next agent change, and *four* things change the agent —
    /// a resume, a `/agent` switch, an MCP server finishing its dial, and the
    /// initial `with_agents`. Rules given here survive all of them, and the
    /// per-turn ruleset a `/command` naming its own agent runs under carries
    /// them too.
    ///
    /// Applies immediately: the set judging the very next call is recomposed
    /// before this returns.
    pub fn append_standing_rules(&self, rules: Vec<Rule>) {
        {
            let mut standing = self
                .standing
                .lock()
                .expect("the standing rules are never poisoned");
            standing.extend(rules);
        }

        // Recomposed through the same seam every other install goes through,
        // so there is no second answer to "what is the baseline made of". An
        // engine with no agents has no agent ruleset to sit beneath these, and
        // then the standing rules are the whole of the baseline.
        let name = self.active().agent.clone();
        match self
            .agents
            .as_ref()
            .zip(name.as_deref())
            .and_then(|(registry, name)| registry.get(name))
        {
            Some(agent) => self.install(agent),
            None => self
                .permissions
                .lock()
                .expect("the permission rules are never poisoned")
                .set_baseline(self.standing()),
        }
    }

    /// The model the next turn will ask for.
    #[must_use]
    pub fn model(&self) -> String {
        self.active().model.clone()
    }

    /// The agent the next turn will run as, or [`None`] on an engine built
    /// without a registry.
    #[must_use]
    pub fn agent(&self) -> Option<String> {
        self.active().agent.clone()
    }

    /// The catalog effort the next turn will run under, or [`None`] for
    /// upstream's "Default" — which every session starts on, and the only
    /// value an uncataloged provider's session ever holds.
    #[must_use]
    pub fn effort(&self) -> Option<String> {
        self.active().effort.clone()
    }

    /// Whether this engine's provider can carry a binary attachment of `mime`
    /// as native content, verbatim from the wire's own
    /// [`accepts_attachment`](crate::provider::Provider::accepts_attachment).
    ///
    /// What a frontend consults at submit time: a mention the wire cannot
    /// carry will be degraded to text naming the file when the request is
    /// built, and the moment to say so in the status line is before the turn,
    /// not inside it.
    #[must_use]
    pub fn accepts_attachment(&self, mime: &str) -> bool {
        self.provider.accepts_attachment(mime)
    }

    /// The agents this session may run as, for a picker to list.
    #[must_use]
    pub fn agents(&self) -> Option<&Arc<agent::Registry>> {
        self.agents.as_ref()
    }

    /// Every stored session, newest first — what a session picker lists.
    ///
    /// The listing runs on the blocking pool because it walks the store: a
    /// caller inside a render loop stays responsive however many sessions
    /// have accumulated.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Ephemeral`] on an engine built without storage,
    /// and [`EngineError::Storage`] when the filesystem refuses the listing.
    pub async fn sessions(&self) -> Result<Vec<SessionInfo>, EngineError> {
        let Some(state) = &self.persistence else {
            return Err(EngineError::Ephemeral);
        };

        let storage = state.storage.clone();
        let sessions = tokio::task::spawn_blocking(move || storage.list_sessions())
            .await
            .expect("the session listing neither panics nor is aborted")?;

        Ok(sessions)
    }

    /// The id every event this engine emits carries: its current session's.
    ///
    /// Always answers, where [`Engine::current_session`] answers only once a
    /// persistent engine holds a row — the id predates the row on purpose,
    /// so a subscriber can attribute a session's events from its very first
    /// one, and an ephemeral engine's session has a name even though nothing
    /// stores it.
    #[must_use]
    pub fn session_id(&self) -> SessionId {
        self.session
            .lock()
            .expect("the session id is never poisoned")
            .clone()
    }

    /// The session the engine is writing into, or [`None`] before the first
    /// prompt or resume — and always [`None`] on an in-memory engine.
    #[must_use]
    pub fn current_session(&self) -> Option<SessionInfo> {
        self.persistence
            .as_ref()?
            .live
            .lock()
            .expect("the live session is never poisoned")
            .info
            .clone()
    }

    /// How full the current session's context is against its model's window —
    /// the same two numbers compaction reads at turn start
    /// ([`crate::session`]'s trigger: stored [`SessionInfo::context_tokens`]
    /// against the catalog's `context_window`), exposed for a status display
    /// to poll on its tick, exactly as [`Engine::jobs`] is. Deliberately not a
    /// protocol event: the measure only moves when a request finishes, and a
    /// frontend already ticks.
    #[must_use]
    pub fn context_estimate(&self) -> ContextEstimate {
        let tokens = self
            .persistence
            .as_ref()
            .and_then(|state| {
                state
                    .live
                    .lock()
                    .expect("the live session is never poisoned")
                    .info
                    .as_ref()
                    .map(|info| info.context_tokens)
            })
            .unwrap_or(0);
        // Absent for an uncataloged model, the same answer compaction gives:
        // only the catalog can say what fits, and inventing a denominator
        // would put a percentage on a window nobody measured.
        let window = catalog::model(&self.model()).map(|model| model.context_window);

        ContextEstimate { tokens, window }
    }

    /// What the next request would carry, category by category (**D470**).
    ///
    /// Computed on demand from the same state the request assembly reads —
    /// the system halves `Engine::system_for` would join, the tool registry
    /// a turn would snapshot, the live window minus what a standing revert
    /// hides — never stashed per turn, so it answers on a fresh session with
    /// zero turns and immediately after a revert. The split of the composed
    /// suffix is [`crate::instruction`]'s own (`suffix_measure`), against the
    /// markers its composer wrote, and every estimate goes through the
    /// compaction fit guard's chars-per-token convention: one estimator, not
    /// a second tokenizer.
    ///
    /// The conversation share **prefers actuals**: an assistant message whose
    /// turn reported a [`Usage`] is priced at its reported `output_tokens` —
    /// plus the estimate for its tool results, which came back *to* the model
    /// and were never part of anything a provider counted as output.
    pub async fn context_breakdown(&self) -> ContextBreakdown {
        use crate::session::{compaction_reserve, estimate_tokens};

        // The head the next request would carry: the agent's own prompt where
        // the session runs as one, the base half where it does not —
        // `system_for`'s rule, read from the same fields.
        let head = {
            let agent = self.active().agent.clone();
            agent
                .and_then(|name| {
                    self.agents
                        .as_ref()
                        .and_then(|registry| registry.get(&name))
                        .and_then(|agent| agent.prompt.clone())
                })
                .or_else(|| self.base_half())
        };
        let suffix = self.environment_half().unwrap_or_default();
        let measure = crate::instruction::suffix_measure(&suffix);
        let head_chars = head.as_deref().map_or(0, |head| head.chars().count());

        let registry = Arc::clone(
            &self
                .tools
                .lock()
                .expect("the tool registry is never poisoned"),
        );
        let (mut builtin_chars, mut mcp_chars) = (0_usize, 0_usize);
        let (mut builtin_count, mut mcp_count) = (0_usize, 0_usize);
        for definition in registry.definitions() {
            let chars = definition.name.chars().count()
                + definition.description.chars().count()
                + definition.schema.to_string().chars().count();
            // The registry's own naming rule: everything a server lends is
            // `mcp__<server>__<tool>`, and nothing else may be.
            if definition.name.starts_with("mcp__") {
                mcp_chars += chars;
                mcp_count += 1;
            } else {
                builtin_chars += chars;
                builtin_count += 1;
            }
        }
        // What the connected servers said about themselves rides the same
        // category as their tools: both are what having the server costs.
        if let Some(instructions) = self.mcp.as_ref().and_then(|servers| servers.instructions()) {
            mcp_chars += instructions.chars().count();
        }

        // The live window, minus what a standing revert hides: the next
        // prompt's `truncate_reverted` drops everything from the anchor on,
        // so a breakdown read between the revert and that prompt must not
        // count messages the request will never carry.
        let hidden_from = self.reverted().map(|state| state.message_id);
        let (mut user, mut assistant) = (0_u64, 0_u64);
        // The messages the next request would carry, so the nested-instruction
        // walk below sees exactly what the request assembly will
        // (`session::nested_system`).
        let mut carried = Vec::new();
        {
            let history = self.history.lock().await;
            for message in history.iter() {
                if hidden_from
                    .as_ref()
                    .is_some_and(|anchor| message.id >= *anchor)
                {
                    continue;
                }
                carried.push(message.clone());
                let (generated, tool_results) = message_chars(message);
                let tokens = match (&message.role, message.usage) {
                    (Role::Assistant, Some(usage)) => usage
                        .output_tokens
                        .saturating_add(estimate_tokens(tool_results)),
                    _ => estimate_tokens(generated.saturating_add(tool_results)),
                };
                match message.role {
                    Role::User => user = user.saturating_add(tokens),
                    Role::Assistant => assistant = assistant.saturating_add(tokens),
                }
            }
        }

        // The lazily walked-in instruction files below the root (**D480**).
        // They join the request's system prompt after the composed suffix, so
        // `suffix_measure` never sees them — but they are instruction files,
        // and the whole point of D480's honesty clause is that their weight is
        // read here rather than felt later. Composed through the same function
        // the request assembly uses, over the same messages, so the two cannot
        // price different text.
        let nested = crate::instruction::nested_suffix(
            &self.root,
            &self.cwd,
            &crate::session::touched_files(&carried, &self.cwd),
        )
        .chars()
        .count();

        let model = self.model();
        let window = catalog::model(&model).map(|model| model.context_window);

        ContextBreakdown {
            model,
            system_prompt: estimate_tokens(head_chars + measure.environment),
            instructions: estimate_tokens(measure.instructions + nested),
            tools_builtin: estimate_tokens(builtin_chars),
            tools_mcp: estimate_tokens(mcp_chars),
            tools_builtin_count: builtin_count,
            tools_mcp_count: mcp_count,
            skills: estimate_tokens(measure.skills),
            conversation_user: user,
            conversation_assistant: assistant,
            window,
            reserve: window.map(compaction_reserve),
        }
    }

    /// Installs the stored session `id` as the engine's current one and
    /// returns its **full transcript**, oldest first, for a frontend to seed
    /// its view from.
    ///
    /// The engine's own request history becomes the live window: messages
    /// from [`SessionInfo::summary`] onward, all of them when no compaction
    /// has happened. Assistant messages that carry no content are left out of
    /// the window — some providers reject an empty message — but stay in the
    /// returned transcript, `time.completed` still absent, which is how a
    /// frontend shows them as aborted.
    ///
    /// A tool call the previous process never finished is closed here as
    /// [`ToolState::Error`], in the returned transcript, in the installed
    /// window, and on disk: the next request must answer every call the
    /// model opened.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Busy`] while a turn is in flight — the turn is
    /// writing into the session it started on — [`EngineError::Ephemeral`]
    /// on an engine built without storage, [`EngineError::SessionNotFound`]
    /// for an id the store does not hold, and [`EngineError::Storage`] when
    /// the filesystem refuses a read.
    pub async fn resume(&self, id: &SessionId) -> Result<Vec<Message>, EngineError> {
        let Some(state) = &self.persistence else {
            return Err(EngineError::Ephemeral);
        };

        // The slot guard is held across the whole install, so a prompt that
        // arrives mid-resume waits and then lands on the freshly installed
        // session instead of racing the old one. No turn exists while it is
        // held, so nothing on the event path can contend for it.
        //
        // A pending plan approval is cleared: it belonged to the conversation
        // being left. The switch itself still survives — the boundary already
        // wrote it onto that session's row, and the row is what a later
        // resume of *that* session reads — while the sentence does not (the
        // approval-rides-the-request family).
        let slot = self.lock_entry(PendingPolicy::Clear).await?;

        let storage = state.storage.clone();
        let wanted = id.clone();
        let (info, transcript) = tokio::task::spawn_blocking(move || {
            let Some(info) = storage.load_info(&wanted)? else {
                return Err(EngineError::SessionNotFound { id: wanted });
            };
            let mut transcript = storage.load_transcript(&wanted)?;
            close_interrupted(&storage, &wanted, &mut transcript);

            Ok((info, transcript))
        })
        .await
        .expect("the session load neither panics nor is aborted")?;

        // The activated set is re-seeded on every resume (**D492**): the
        // row's own field unioned with every `mcp__*` tool-call name in the
        // **full** stored transcript, not the post-compaction window.
        // Deliberately broader than the in-process predicate — denied and
        // unknown calls seed too, owned as benign over-seeding (grow-only;
        // advertisement is not authority; a name matching no candidate
        // changes nothing) — because it buys replay hygiene: a resumed
        // request never replays an `mcp__*` call the roster withholds. The
        // union covers *calls*; a search leaves no such part, and the flush
        // at its own finish is what wrote it onto the row this reads.
        let mut activated = info.activated_tools.clone();
        for message in &transcript {
            for part in &message.parts {
                if let crate::protocol::PartBody::Tool { tool, .. } = &part.body
                    && tool.starts_with(ganja_permission::permission::MCP_PREFIX)
                {
                    activated.insert(tool.clone());
                }
            }
        }
        if !activated.is_empty() {
            tracing::debug!(
                count = activated.len(),
                by = "seed",
                "seeded the activated tools from the resumed session"
            );
        }
        *self
            .activated_tools
            .lock()
            .expect("the activated set is never poisoned") = activated;

        let start = match &info.summary {
            None => 0,
            Some(summary) => match transcript.iter().position(|m| m.id == *summary) {
                Some(index) => index,
                None => {
                    // The summary message was quarantined or lost; the whole
                    // transcript is the only honest window left.
                    tracing::warn!(
                        session = info.id.as_str(),
                        "the compaction summary is missing from the transcript; \
                         resuming with the full history"
                    );
                    0
                }
            },
        };
        let window: Vec<Message> = transcript[start..]
            .iter()
            .filter(|message| message.role == Role::User || message.has_content())
            .cloned()
            .collect();

        *self.history.lock().await = window;
        // A resumed conversation has read nothing yet in this process: what
        // the session it replaced had open says nothing about these files.
        self.files.clear();
        self.restore_selection(&info);
        // The slot moves before anything is announced: the resumed revert
        // below must carry the resumed session's id, not the one this engine
        // minted at birth or was on a moment ago.
        *self
            .session
            .lock()
            .expect("the session id is never poisoned") = info.id.clone();
        let revert = info.revert.clone();
        {
            let mut live = state
                .live
                .lock()
                .expect("the live session is never poisoned");
            live.info = Some(info);
            live.warned_uncataloged = false;
        }
        // A session left mid-undo reopens mid-undo. The event is the only way
        // a frontend that has just started can learn which messages are hidden
        // — the transcript it was handed above still holds every one of them.
        // No prompt travels with it: reopening a conversation is not the
        // moment to put words in somebody's editor. A session that was not
        // reverted announces nothing, because a frontend seeding itself from
        // that transcript is already hiding none of it.
        *self
            .revert
            .lock()
            .expect("the revert state is never poisoned") = revert.clone();
        if let Some(revert) = &revert {
            let _ = self
                .fanout
                .send(Event::RevertChanged {
                    session_id: self.session_id(),
                    revert: Some(revert.info()),
                    prompt: None,
                })
                .await;
        }
        drop(slot);

        // After the install and after the slot is released: the envelope names
        // the session that was resumed, so it has to be fired once this engine
        // *is* on that session — and a hook holding the slot would make a
        // resume Busy for its own duration. `startup` never fires here and
        // `resume` never fires anywhere else, which is the whole of what a
        // `SessionStart` matcher selects between.
        self.fire_session_hook(hook::Payload::SessionStart {
            source: hook::Source::Resume,
        })
        .await;

        Ok(transcript)
    }

    /// Puts a resumed session back on the agent and model it was running.
    ///
    /// Either half may be refused: the agent registry is built from this
    /// process's config and may no longer hold the agent, and the provider is
    /// fixed at construction so a session stored under another one names a
    /// model this build cannot ask for (**D8**). A refusal is a warning and
    /// the engine's own selection stands — a session that reopened silently
    /// asking a model that does not exist would fail every turn instead.
    fn restore_selection(&self, info: &SessionInfo) {
        if let Some(name) = &info.agent {
            match self
                .agents
                .as_ref()
                .and_then(|registry| registry.get(name))
                .filter(|agent| agent.mode != AgentMode::Subagent)
            {
                Some(agent) => {
                    self.install(agent);
                    self.active().agent = Some(agent.name.clone());
                }
                None => tracing::warn!(
                    session = info.id.as_str(),
                    agent = name.as_str(),
                    "the stored agent is not one this build has; resuming on the default"
                ),
            }
        }

        if let Some(model) = &info.model {
            if self.serves(model) {
                self.active().model = model.clone();
            } else {
                tracing::warn!(
                    session = info.id.as_str(),
                    model = model.as_str(),
                    provider = self.provider.id(),
                    "the stored model is not one this provider serves; \
                     resuming on the one this session was started with"
                );
            }
        }

        // The stored effort is restored against whatever model the lines
        // above settled on, and dropped when that model's row no longer
        // carries it — the same rule a live model switch applies
        // (upstream `prompt.ts:654`), met again on the resume path.
        self.active().effort = info.effort.clone();
        if self.reconcile_effort() {
            tracing::warn!(
                session = info.id.as_str(),
                effort = info.effort.as_deref().unwrap_or_default(),
                "the stored effort is not one the resumed model carries; \
                 resuming without one"
            );
        }

        // Nothing in the transcript says which agent produced which message,
        // so a resumed session has no previous turn to compare against and
        // does not replay the plan-to-build reminder.
        self.active().previous_agent = None;
        // A session reopened on the model it was last asking gets that model's
        // prompt — its family's base and an environment block naming it — and
        // not the one this process happened to start on.
        self.recompose_environment();
        self.recompose_base();
    }

    /// The system prompt one turn carries: the agent's own prompt where it has
    /// one, the model family's base prompt where it does not, and the
    /// environment half after either.
    fn system_for(&self, agent: Option<&Agent>) -> Option<String> {
        let base = self.base_half();
        let head = agent
            .and_then(|agent| agent.prompt.as_deref())
            .or(base.as_deref());
        let suffix = self.environment_half();

        // What the connected servers said about themselves, after the
        // instruction files and before nothing — upstream's own position for
        // it (`session/prompt.ts:1261-1269`). Absent when no server said
        // anything, which is every session with no MCP configured, so nothing
        // that has no servers sees a change here.
        let mcp = self.mcp.as_ref().and_then(|servers| servers.instructions());

        let composed = match (head, suffix.as_deref()) {
            (None, None) => None,
            (Some(only), None) | (None, Some(only)) => Some(only.to_owned()),
            (Some(head), Some(suffix)) => Some(format!("{head}\n{suffix}")),
        };

        match (composed, mcp) {
            (composed, None) => composed,
            (None, Some(mcp)) => Some(mcp),
            (Some(composed), Some(mcp)) => Some(format!("{composed}\n{mcp}")),
        }
    }

    /// Claims a lossless event stream.
    ///
    /// The first call inherits the queue the engine was born with, so
    /// everything published since construction — a resume's `RevertChanged`
    /// most of all — is already waiting in it. Every later call registers a
    /// fresh queue that sees events from the moment this returns:
    /// registration is the atomic point after which nothing published is
    /// lost. Two lossless subscribers of one turn therefore hold the same
    /// transcript frame for frame, differing only in where each began.
    ///
    /// Every stream this returns is bounded and lossless: a full queue makes
    /// the publisher wait, so backpressure lands on the turn task and never
    /// on a render loop. A subscriber that may be abandoned instead of waited
    /// for is [`Engine::subscribe_droppable`]'s business.
    ///
    /// # Errors
    ///
    /// None today. The `Result` stays because this is the seam a
    /// transport-served engine will fail at, and every caller already treats
    /// it as fallible.
    pub async fn subscribe(&self) -> Result<BoxStream<'static, Event>, EngineError> {
        if let Some(receiver) = self.unclaimed.lock().await.take() {
            return Ok(ReceiverStream::new(receiver).boxed());
        }

        let (sender, receiver) = mpsc::channel(EVENT_CAPACITY);
        self.fanout.register(Lane::Lossless(sender));

        Ok(ReceiverStream::new(receiver).boxed())
    }

    /// Registers a subscriber the engine may drop rather than wait for.
    ///
    /// The shape an HTTP or SSE consumer needs: the agent loop never stalls
    /// on it, because a full queue costs the subscriber its subscription
    /// instead of costing the turn a wait. The stream then ends with
    /// [`Evicted`] after whatever its queue still held — an observable error,
    /// never a silent end, so the consumer knows its transcript is torn and
    /// can resynchronize rather than trust it.
    ///
    /// Like every later subscriber it sees events from the moment this
    /// returns; the birth queue belongs to the first lossless subscriber,
    /// whose loss guarantee is the one worth spending it on.
    pub fn subscribe_droppable(&self) -> BoxStream<'static, Result<Event, Evicted>> {
        let (sender, receiver) = mpsc::channel(EVENT_CAPACITY);
        let (loss, lost) = oneshot::channel();
        self.fanout.register(Lane::Droppable {
            queue: sender,
            loss,
        });

        // The queue drains first, whole; only once it ends is the loss
        // marker consulted. An engine that simply went away drops `loss`
        // unfired, and the stream ends the way it always did.
        ReceiverStream::new(receiver)
            .map(Ok)
            .chain(stream::once(lost).filter_map(|fired| async move { fired.ok().map(Err) }))
            .boxed()
    }

    /// Applies `command`.
    ///
    /// The call returns as soon as the command is accepted — a turn's work
    /// happens in a spawned task and is reported through the event stream — so
    /// a caller may await this from inside a render loop.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Busy`] when a prompt or a switch arrives while
    /// another turn is still streaming, or still waiting on a permission, and
    /// the agent and model refusals for a switch that names something this
    /// session cannot become.
    pub async fn send(&self, command: Command) -> Result<(), EngineError> {
        match command {
            Command::SendPrompt {
                text,
                mentions,
                skills,
                peers,
                session_mentions,
            } => {
                self.start_turn(
                    text,
                    TurnKind::Prompt {
                        mentions,
                        skills,
                        peers,
                        session_mentions,
                    },
                    None,
                )
                .await
            }
            Command::Steer {
                id,
                text,
                mentions,
                skills,
                peers,
                session_mentions,
            } => {
                self.steer(id, text, mentions, skills, peers, session_mentions)
                    .await
            }
            Command::CancelTurn => {
                self.cancel_turn().await;
                Ok(())
            }
            Command::ReplyPermission { id, reply } => {
                self.reply_permission(&id, reply).await;
                Ok(())
            }
            Command::ReplyQuestion { id, answers } => {
                self.answer_question(&id, Answered::Replied(answers)).await;
                Ok(())
            }
            Command::RejectQuestion { id } => {
                self.answer_question(&id, Answered::Rejected).await;
                Ok(())
            }
            Command::SwitchAgent { name } => self.switch_agent(name).await,
            Command::SwitchModel { model } => self.switch_model(model).await,
            Command::SwitchEffort { effort } => self.switch_effort(effort).await,
            Command::SetPermissionMode { mode } => self.set_permission_mode(mode).await,
            // A person's word on one held inbound message (**D524**). The
            // release re-checks current policy inside the gate — an approval
            // cannot override a policy that has since become refuse — and a
            // settle naming an id nobody holds is ignored, which is also how
            // a person racing the expiry timer loses gracefully.
            Command::SettleHeld { id, decision } => {
                let settlement = match decision {
                    crate::protocol::HeldDecision::Release => self.inbound.release(&id),
                    crate::protocol::HeldDecision::Deny => self.inbound.deny(&id),
                };
                match settlement {
                    None => tracing::debug!(
                        id = id.as_str(),
                        "a settle named a hold nobody holds; ignored"
                    ),
                    Some(settlement) => {
                        settle_side_effects(
                            &self.inbound,
                            self.teammates.as_ref(),
                            &id,
                            settlement,
                        )
                        .await;
                    }
                }
                Ok(())
            }
            Command::RunShell { command } => {
                self.start_turn(command.clone(), TurnKind::Shell { command }, None)
                    .await
            }
            Command::RunCommand { name, args } => self.run_command(&name, &args).await,
            Command::Compact => {
                self.start_turn(String::new(), TurnKind::Compact, None)
                    .await
            }
            Command::NewSession => self.new_session().await,
            Command::Undo => self.undo().await,
            Command::Redo => self.redo().await,
            Command::RevertTo { message_id, scope } => {
                self.revert_to_message(message_id, scope).await
            }
        }
    }

    /// Expands the named command and starts a turn with the result.
    async fn run_command(&self, name: &str, args: &str) -> Result<(), EngineError> {
        let Some(definition) = self.commands.get(name) else {
            return Err(EngineError::UnknownCommand {
                name: name.to_owned(),
                available: self.commands.names(),
            });
        };

        // A command that names an agent runs as it for this turn only, without
        // changing what the session is: upstream re-resolves the agent from
        // each user message, so a command's choice reaches exactly the message
        // it came with.
        let agent = match &definition.agent {
            None => None,
            Some(name) => {
                let registry = self.agents.as_ref().ok_or(EngineError::NoAgents)?;
                let agent = registry
                    .get(name)
                    .ok_or_else(|| EngineError::UnknownAgent { name: name.clone() })?;
                if agent.mode == AgentMode::Subagent {
                    return Err(EngineError::CommandSubagent {
                        name: definition.name.clone(),
                        agent: name.clone(),
                    });
                }

                Some(agent.clone())
            }
        };
        // Upstream's precedence: the command's own model, then the model of the
        // agent it named, then the session's.
        let model = definition
            .model
            .as_deref()
            .and_then(|model| self.model_named(model))
            .or_else(|| {
                agent
                    .as_ref()
                    .and_then(|agent| agent.model.as_deref())
                    .and_then(|model| self.adopt(model))
            });
        let overrides = (agent.is_some() || model.is_some()).then_some(Overrides { agent, model });

        let ctx = crate::tool::ToolCtx {
            // A person running a command means "here" as in this project, not
            // whichever process directory happened to launch ganja — the same
            // ground the `!` passthrough gives its shell.
            cwd: self.root.clone(),
            // Expansion precedes the turn, so no turn token exists to parent
            // this one. The shell tool's own default timeout bounds a runaway
            // template command; there is not yet a cancellation to receive.
            cancel: CancellationToken::new(),
            call_id: String::new(),
            files: Arc::clone(&self.files),
            credentials: self.credentials.clone(),
            spawn: None,
            postbox: None,
            ask: None,
            switch: None,
            jobs: None,
        };
        let expanded = definition.expand(args, &ctx).await;

        self.start_turn(
            expanded.prompt,
            TurnKind::Prompt {
                mentions: expanded.mentions,
                // A template's `$` words are its own vocabulary
                // (`$ARGUMENTS`), not the composer's skill grammar: nothing
                // in an expanded command is scanned for invocations.
                skills: Vec::new(),
                peers: Vec::new(),
                // A command's template carries no composer `@`-mentions of
                // its own — those are a person's prompt, not a template's
                // expansion — so an expanded command names none.
                session_mentions: Vec::new(),
            },
            overrides,
        )
        .await
    }

    /// The model a config spelling names, when this provider serves it.
    ///
    /// Config spells a model `"provider/model"` and the provider is fixed at
    /// construction, so what is left of that spelling is everything after the
    /// first slash. A model this provider does not serve is a warning and no
    /// override — the turn asks what the session was already asking, rather
    /// than failing on a model that does not exist.
    fn model_named(&self, spelled: &str) -> Option<String> {
        if let Some(model) = self.adopt(spelled) {
            return Some(model);
        }

        tracing::warn!(
            model = spelled,
            provider = self.provider.id(),
            "the command asks for a model this provider does not serve; \
             running it on the session's own"
        );

        None
    }

    /// Forgets the live session so the next prompt starts a fresh one.
    async fn new_session(&self) -> Result<(), EngineError> {
        // Held for the same reason `resume` holds it: a prompt arriving
        // mid-clear must land on the new session, not race the old one. A
        // pending approval clears with the conversation it belonged to; the
        // old session's row already says build, so reopening it later is
        // still right.
        let turn = self.lock_entry(PendingPolicy::Clear).await?;

        self.history.lock().await.clear();
        // The next conversation is a different session, so it gets a
        // different name now, before anything can be said in it. Left stale,
        // the next prompt's lazy create would adopt the old id and its
        // `save_info` would upsert over the previous conversation's row.
        *self
            .session
            .lock()
            .expect("the session id is never poisoned") = SessionId::ascending();
        if let Some(state) = &self.persistence {
            let mut live = state
                .live
                .lock()
                .expect("the live session is never poisoned");
            live.info = None;
            live.warned_uncataloged = false;
        }
        // The next conversation starts with nothing activated (**D492**),
        // and the recompute puts the names the old session had touched back
        // under the deferral arithmetic — over never-touched names only, as
        // every recompute is, which for a fresh set is all of them.
        self.activated_tools
            .lock()
            .expect("the activated set is never poisoned")
            .clear();
        self.recompose_tools();
        // A new conversation has addressed nobody (**D528**'s pin guard's own
        // `NewSession` door): carrying the old one's choices forward would be
        // guarding a history that no longer exists.
        self.identity.clear_pins();
        // Nothing before this turn to compare against, so the plan-to-build
        // reminder does not fire on the first turn of a new session.
        self.active().previous_agent = None;
        // Read-before-write is a rule about one conversation. The files the
        // last one read are no argument for writing them in this one.
        self.files.clear();
        // A revert is a position in a transcript, and this one has none. The
        // files stay where the revert left them: starting a new conversation
        // is not asking for the last one's work back.
        *self
            .revert
            .lock()
            .expect("the revert state is never poisoned") = None;
        drop(turn);

        Ok(())
    }

    /// Puts the working tree back to what it was before the last prompt, and
    /// hides that prompt and everything after it.
    async fn undo(&self) -> Result<(), EngineError> {
        // Held across the whole revert, exactly as `resume` holds it: a turn
        // must not begin on a transcript that is being rewritten under it. A
        // turn already in flight is refused rather than aborted — upstream
        // aborts and then reverts, where here the person at the terminal
        // cancels and then undoes (**D119**).
        //
        // An undo revokes a pending plan approval with the plan it approved.
        // The two candidate rules — clear on any undo, or only when the
        // revert covers the approving turn — coincide for an unapplied
        // approval: `Announced` ends at the next entry, so no prompt can have
        // intervened, and the one prompt this undo can hide *is* the
        // approving turn (upstream parity for free: its revert deletes the
        // stored approval message the same way). An applied switch survives
        // and only the sentence drops — an undo here may hide a later turn
        // than the approving one, and dropping the sentence on any undo is
        // the simple rule, owned in the build-switch-once family.
        let turn = self.lock_entry(PendingPolicy::Revoke).await?;
        let snapshots = self.snapshotting()?;

        let current = self.reverted();
        let (anchor, prompt, patches) = {
            let history = self.history.lock().await;
            let anchor =
                snapshot::undo_anchor(&history, current.as_ref().map(|state| &state.message_id))
                    .ok_or(EngineError::NothingToUndo)?;
            let prompt = snapshot::prompt_at(&history, &anchor);
            let patches = snapshot::patches_from(&history, &anchor);

            (anchor, prompt, patches)
        };

        self.revert_to(snapshots, current.as_ref(), anchor, prompt, &patches)
            .await;
        drop(turn);

        Ok(())
    }

    /// Steps one prompt forward through what an undo hid.
    async fn redo(&self) -> Result<(), EngineError> {
        // A non-empty approval cell is unreachable here — reaching `redo`
        // requires a prior `undo`, which cleared it — so the clear is
        // defensive totality. The asymmetry is owned: a revoked Yes does not
        // return on redo, the same family as the sentence's non-survival.
        let turn = self.lock_entry(PendingPolicy::Clear).await?;
        let snapshots = self.snapshotting()?;
        let current = self.reverted().ok_or(EngineError::NothingToRedo)?;

        let forward = {
            let history = self.history.lock().await;
            snapshot::redo_anchor(&history, &current.message_id).map(|anchor| {
                let prompt = snapshot::prompt_at(&history, &anchor);
                let patches = snapshot::patches_from(&history, &anchor);

                (anchor, prompt, patches)
            })
        };

        match forward {
            Some((anchor, prompt, patches)) => {
                self.revert_to(snapshots, Some(&current), anchor, prompt, &patches)
                    .await;
            }
            // Nothing left to step forward to, so the working tree goes back
            // whole: every file the tree holds, whether or not a patch named
            // it, because what is being undone is the undo itself.
            None => {
                if let Some(hash) = &current.snapshot {
                    snapshots.restore(hash).await;
                }
                self.remember_revert(None);
                let _ = self
                    .fanout
                    .send(Event::RevertChanged {
                        session_id: self.session_id(),
                        revert: None,
                        prompt: None,
                    })
                    .await;
            }
        }
        drop(turn);

        Ok(())
    }

    /// Takes the session back to the checkpoint `anchor` names, restoring
    /// whatever `scope` asks for.
    ///
    /// A superset of [`Engine::undo`] and not a replacement for it: the anchor
    /// comes from the user instead of from a walk, and the scope decides which
    /// halves of the checkpoint move. Everything underneath is the machinery
    /// `undo` already uses — the same `patches_from`, the same
    /// `Snapshots::revert`, the same `RevertState` — so hidden-not-deleted,
    /// redo and next-prompt-commits fall out unchanged for the scopes that hide
    /// anything.
    ///
    /// Refused while a turn is streaming, for `undo`'s reason (**D119**), and
    /// with the same [`PendingPolicy::Revoke`]: a rewind that hides the turn a
    /// plan approval was announced in must not leave the approval standing.
    async fn revert_to_message(
        &self,
        anchor: MessageId,
        scope: RevertScope,
    ) -> Result<(), EngineError> {
        let turn = self.lock_entry(PendingPolicy::Revoke).await?;

        // Files have to come from somewhere, so a scope that moves them is
        // refused on a session that takes no snapshots — exactly as an undo is.
        // A conversation-only rewind restores nothing from disk and is
        // therefore not that session's to refuse.
        let snapshots = match scope.touches_files() {
            true => Some(self.snapshotting()?),
            false => self
                .snapshots
                .as_deref()
                .filter(|snapshots| snapshots.enabled()),
        };

        let current = self.reverted();
        let (prompt, patches) = {
            let history = self.history.lock().await;
            // A checkpoint is a prompt. An assistant message, a part id, or an
            // id from another session all land here, and all get their own name
            // back rather than the nearest prompt this could have meant.
            if !history
                .iter()
                .any(|message| message.id == anchor && message.role == Role::User)
            {
                return Err(EngineError::NoSuchCheckpoint { id: anchor });
            }

            (
                snapshot::prompt_at(&history, &anchor),
                snapshot::patches_from(&history, &anchor),
            )
        };

        // Captured once per chain and reused by every revert after it, for
        // `revert_to`'s reason: a second capture would be taken from a tree an
        // earlier revert had already rewritten. A files-only rewind records no
        // state at all, so nothing will ever ask it for one.
        let taken_from = current.as_ref().and_then(|state| state.snapshot.clone());
        let redo = match (&taken_from, scope) {
            (_, RevertScope::Files) => None,
            (Some(existing), _) => Some(existing.clone()),
            (None, _) => match snapshots {
                Some(snapshots) => snapshots.track().await,
                None => None,
            },
        };

        // What really came back, which is not always what the patches named —
        // see [`snapshot::Snapshots::revert`]. A conversation-only rewind moves
        // no file and so reports none, which is the empty list a frontend
        // already reads as "the conversation and not the checkout"
        // (`RevertInfo::files`).
        let restored = match scope.touches_files() {
            false => Vec::new(),
            true => {
                let snapshots = snapshots
                    .expect("a scope that touches files went through `snapshotting` above");
                // Back to the un-reverted tree first, so this revert applies to
                // the whole conversation rather than to what an earlier one
                // left behind — including when the picker steps *forward*, to a
                // checkpoint newer than where the session already stands.
                if let Some(existing) = &taken_from {
                    snapshots.restore(existing).await;
                }

                snapshots.revert(&patches).await
            }
        };

        if scope.touches_conversation() {
            let state = snapshot::RevertState {
                message_id: anchor,
                snapshot: redo,
                files: restored,
            };
            let info = state.info();
            self.remember_revert(Some(state));
            let _ = self
                .fanout
                .send(Event::RevertChanged {
                    session_id: self.session_id(),
                    revert: Some(info),
                    prompt,
                })
                .await;
        } else {
            // The one genuinely new state: the checkout moved and the
            // transcript did not. Nothing is hidden, so nothing is remembered
            // and a `/redo` after this finds nothing to step through — and any
            // revert that was already standing is still standing, because this
            // rewind said nothing about the conversation. The event still names
            // the checkpoint, so a frontend can say which one the files came
            // back to, and carries no prompt: nothing was taken back to retype.
            let _ = self
                .fanout
                .send(Event::RevertChanged {
                    session_id: self.session_id(),
                    revert: Some(RevertInfo {
                        message_id: anchor,
                        files: restored,
                    }),
                    prompt: None,
                })
                .await;
        }
        drop(turn);

        Ok(())
    }

    /// Reverts the working tree to the state `anchor`'s turn started from, and
    /// records that the session is now reverted that far.
    ///
    /// The files this reports are the ones the patches **named**, where
    /// [`Engine::revert_to_message`] reports the ones that really came back.
    /// The difference only shows when a checkout fails, and undo's wording is
    /// left exactly as it was on purpose: this path is `Command::Undo`'s and
    /// `Command::Redo`'s, and moving it was out of the rewind wave's scope.
    /// Aligning the two is a recorded follow-up.
    async fn revert_to(
        &self,
        snapshots: &snapshot::Snapshots,
        current: Option<&snapshot::RevertState>,
        anchor: crate::protocol::MessageId,
        prompt: Option<String>,
        patches: &[snapshot::Patch],
    ) {
        // Captured once per chain of undos and reused by every one after it. A
        // second capture would be taken from a tree the first undo had already
        // rewritten, and the redo would then restore a state that never
        // existed.
        let redo = match current.and_then(|state| state.snapshot.clone()) {
            Some(existing) => {
                // Back to the un-reverted tree first, so the deeper revert is
                // applied to the whole conversation rather than to what the
                // shallower one left behind.
                snapshots.restore(&existing).await;
                Some(existing)
            }
            None => snapshots.track().await,
        };
        let _ = snapshots.revert(patches).await;

        // In the order the patches named them, which is the order the turn
        // touched them in — what a marker row reads best in.
        let mut files: Vec<String> = Vec::new();
        for file in patches.iter().flat_map(|patch| &patch.files) {
            if !files.contains(file) {
                files.push(file.clone());
            }
        }

        let state = snapshot::RevertState {
            message_id: anchor,
            snapshot: redo,
            files,
        };
        let info = state.info();
        self.remember_revert(Some(state));
        let _ = self
            .fanout
            .send(Event::RevertChanged {
                session_id: self.session_id(),
                revert: Some(info),
                prompt,
            })
            .await;
    }

    /// Deletes the messages a revert hid, because a prompt has just made the
    /// choice permanent.
    ///
    /// **The anchor goes with them.** Upstream deletes it too when no part was
    /// named, and ganja never names one: what the user took back is the prompt
    /// itself, and a prompt left behind would ride into the very next request
    /// as though it had been asked twice.
    async fn truncate_reverted(&self) {
        let Some(state) = self.reverted() else {
            return;
        };
        let anchor = state.message_id;

        self.history
            .lock()
            .await
            .retain(|message| message.id < anchor);

        if let Some(persistence) = &self.persistence {
            let session = persistence
                .live
                .lock()
                .expect("the live session is never poisoned")
                .info
                .as_ref()
                .map(|info| info.id.clone());
            if let Some(session) = session {
                // Read back from the store rather than from the window that
                // was just truncated: an assistant turn that died before its
                // first fragment is kept on disk and left out of the window,
                // and one inside the undone range has to go with the rest.
                let stored = persistence
                    .storage
                    .load_transcript(&session)
                    .unwrap_or_default();
                for message in stored.iter().filter(|message| message.id >= anchor) {
                    if let Err(error) = persistence.storage.delete_message(&session, &message.id) {
                        tracing::warn!(
                            session = session.as_str(),
                            message = message.id.as_str(),
                            %error,
                            "a message the undo took back could not be deleted; \
                             it will be back when the session is resumed"
                        );
                    }
                }
            }
        }

        self.remember_revert(None);
        let _ = self
            .fanout
            .send(Event::RevertChanged {
                session_id: self.session_id(),
                revert: None,
                prompt: None,
            })
            .await;
    }

    /// The snapshots this session takes, or the refusal that says why it
    /// cannot undo.
    ///
    /// A session with none is refused rather than reverted: moving the
    /// transcript while leaving the files where they are would be an undo that
    /// only half happened, and nothing afterwards could tell.
    fn snapshotting(&self) -> Result<&snapshot::Snapshots, EngineError> {
        self.snapshots
            .as_deref()
            .filter(|snapshots| snapshots.enabled())
            .ok_or(EngineError::NoSnapshots)
    }

    /// How far back this session is currently reverted.
    fn reverted(&self) -> Option<snapshot::RevertState> {
        self.revert
            .lock()
            .expect("the revert state is never poisoned")
            .clone()
    }

    /// Records where the revert stands, and stores it when the engine
    /// persists: the messages a revert hides are still on disk, so a session
    /// reopened tomorrow has to be told it is looking at a hidden tail.
    fn remember_revert(&self, state: Option<snapshot::RevertState>) {
        *self
            .revert
            .lock()
            .expect("the revert state is never poisoned") = state.clone();

        let Some(persistence) = &self.persistence else {
            return;
        };
        let mut live = persistence
            .live
            .lock()
            .expect("the live session is never poisoned");
        let Some(info) = live.info.as_mut() else {
            return;
        };
        info.revert = state;
        info.updated = now();

        if let Err(error) = persistence.storage.save_info(info) {
            tracing::warn!(
                session = info.id.as_str(),
                %error,
                "the revert could not be stored; it holds for this process only"
            );
        }
    }

    /// The active selection, which is never held across an await.
    fn active(&self) -> std::sync::MutexGuard<'_, Active> {
        self.active
            .lock()
            .expect("the active selection is never poisoned")
    }

    /// The permission baseline a turn running as `agent` is judged by: the
    /// agent's own ruleset, with the standing rules after it.
    ///
    /// The one place a baseline is composed, and deliberately so. Losing the
    /// standing rules is not one bug but one per site that writes a baseline,
    /// and there are five: [`Engine::with_agents`], a resume's
    /// `restore_selection`, a `/agent` switch, the tool-set rebuild an MCP
    /// server's dial completes — all four through [`Engine::install`] — and
    /// the ruleset a `/command` naming its own agent derives for its turn.
    /// Composing here is what makes them one answer.
    ///
    /// Order is the point: last-match-wins reads the concatenation backwards,
    /// so standing rules *after* the agent's outrank the agent's own and the
    /// config's. The tier above them — the answers a person gave — is
    /// untouched, because that tier lives in the permission set rather than in
    /// any baseline.
    fn baseline_for(&self, agent: &Agent) -> Vec<Rule> {
        let mut rules = agent.rules.clone();
        rules.extend(self.standing());

        rules
    }

    /// The rules imposed above every agent's, cloned out from under the lock.
    fn standing(&self) -> Vec<Rule> {
        self.standing
            .lock()
            .expect("the standing rules are never poisoned")
            .clone()
    }

    /// Installs `agent`'s ruleset as the permission baseline, and rebuilds the
    /// tool set the model is offered so the task tool lists what *this* agent
    /// may delegate to.
    fn install(&self, agent: &Agent) {
        self.permissions
            .lock()
            .expect("the permission rules are never poisoned")
            .set_baseline(self.baseline_for(agent));

        let Some(agents) = &self.agents else {
            return;
        };
        let mut rebuilt = self
            .lent()
            .with(Arc::new(task::TaskTool::new(&subagent::roster(
                agents, agent,
            ))));
        // Both plan doors ride the same rebuild the task tool does, and for
        // the same doctrine: presence is ability, and the registry holding
        // the agent a door leads to is what makes "switch to that agent" a
        // promise the engine can keep. Registered here and nowhere else
        // because `install` runs from five sites including the MCP-dial
        // rebuild at every turn start — a tool added anywhere else is dropped
        // on the first rebuild — and never in `with_builtins`, whose surface
        // the golden differential pins. Every agent's model sees both (denied
        // tools are not hidden); the rules refuse everyone but plan for the
        // exit and everyone but build for the enter (**D477**).
        if agents.get(agent::BUILD).is_some() {
            rebuilt = rebuilt.with(Arc::new(plan::PlanExitTool));
        }
        if agents.get(agent::PLAN).is_some() {
            rebuilt = rebuilt.with(Arc::new(plan::PlanEnterTool));
        }
        let rebuilt = self.compose(Arc::new(rebuilt));
        *self
            .tools
            .lock()
            .expect("the tool registry is never poisoned") = rebuilt;
    }

    /// The base set plus whatever the MCP servers are currently lending.
    fn lent(&self) -> Arc<Registry> {
        Arc::clone(
            &self
                .lent_tools
                .lock()
                .expect("the tool registry is never poisoned"),
        )
    }

    /// Rebuilds the tool sets if the MCP servers' tool surface has moved since
    /// the last one.
    ///
    /// Called at the start of a turn and nowhere else: a turn already holding
    /// a snapshot keeps the tools it started with, so a server that connected
    /// halfway through is offered to the model at the *next* turn rather than
    /// changing the set under a request that has already been sent.
    fn refresh_mcp(&self) {
        let Some(servers) = &self.mcp else {
            return;
        };
        // A connection that went away is one whose tools stop being offered;
        // this is where that is noticed.
        servers.reap();
        // A server whose very first dial never succeeded gets its one
        // automatic re-dial here — spawned and never awaited, so this call
        // returns immediately and the once-per-turn contract below is
        // unaffected by however long the retry takes (**D463**).
        servers.retry_once();

        let generation = servers.generation();
        let mut installed = self
            .mcp_installed
            .lock()
            .expect("the MCP generation is never poisoned");
        if *installed == generation {
            return;
        }
        *installed = generation;
        drop(installed);

        let lent = Arc::new(self.base_tools().with_all(servers.tools()));
        *self
            .lent_tools
            .lock()
            .expect("the tool registry is never poisoned") = Arc::clone(&lent);
        self.rebuild_offered(lent);
    }

    /// Rebuilds the offered set from a freshly composed lent set.
    ///
    /// The task tool's roster is per agent, so the offered set is rebuilt
    /// through `install`, which is the one place that knows how. Shared by
    /// the MCP-generation rebuild above and [`Engine::replace_base_tools`],
    /// so the two cannot disagree about what riding the rebuild means.
    fn rebuild_offered(&self, lent: Arc<Registry>) {
        let name = self.active().agent.clone();
        let agent = self
            .agents
            .as_ref()
            .zip(name.as_deref())
            .and_then(|(registry, name)| registry.get(name));
        match agent {
            Some(agent) => self.install(agent),
            // No agents means no task tool, so the offered set *is* the lent
            // set — still through the deferral half, which is what keeps the
            // two arms one composition path.
            None => {
                let composed = self.compose(lent);
                *self
                    .tools
                    .lock()
                    .expect("the tool registry is never poisoned") = composed;
            }
        }
    }

    /// Replaces the base tool registry — the set the caller hands over at
    /// construction, before MCP lends and the task tool ride it — and
    /// rebuilds the lent and offered sets from it now. The skills half of
    /// the `/plugin` dialog's Reload seam (**D474**, declared at that
    /// action): the frontends install the skill tool over this registry, so
    /// a reload that moves the skill roots swaps the whole base set the way
    /// the startup path composed it.
    ///
    /// A turn already holding a snapshot keeps the tools it started with —
    /// the same contract the MCP rebuild keeps — and the next turn is
    /// offered the new set.
    pub fn replace_base_tools(&self, tools: Arc<Registry>) {
        *self
            .base_tools
            .lock()
            .expect("the base tools are never poisoned") = Arc::clone(&tools);
        let lent = match &self.mcp {
            Some(servers) => Arc::new(tools.with_all(servers.tools())),
            None => tools,
        };
        *self
            .lent_tools
            .lock()
            .expect("the tool registry is never poisoned") = Arc::clone(&lent);
        self.rebuild_offered(lent);
    }

    /// Rebuilds the offered set if the team's roster has moved since the last
    /// composition.
    ///
    /// [`Engine::refresh_mcp`]'s shape, at the same seam and for the same
    /// reason: `send_message`'s description **is** the roster, so a teammate
    /// spawned while a turn was streaming is addressable at the *next* turn
    /// rather than changing the tool set under a request already sent. And
    /// like that one it is a memo rather than an unconditional rebuild — a
    /// team whose membership has not moved costs a lock and a comparison.
    fn refresh_team(&self) {
        let Some(roster) = self.postbox_roster() else {
            return;
        };
        let mut installed = self
            .team_roster
            .lock()
            .expect("the team roster is never poisoned");
        if *installed == roster {
            return;
        }
        *installed = roster;
        drop(installed);

        self.rebuild_offered(self.lent());
    }

    /// The tools the next turn offers the model.
    fn tools(&self) -> Arc<Registry> {
        Arc::clone(
            &self
                .tools
                .lock()
                .expect("the tool registry is never poisoned"),
        )
    }

    /// The deferral the next turn carries, as last composed.
    fn deferral(&self) -> crate::tool::deferral::Deferral {
        self.deferral
            .lock()
            .expect("the deferral is never poisoned")
            .clone()
    }

    /// Everything a composed registry gains beyond the base set and what the
    /// MCP servers lend: the team's messaging tool, then the deferral.
    ///
    /// The shared composition path, and the only one — [`Engine::install`],
    /// [`Engine::rebuild_offered`] and the builders all arrive here, so a
    /// `/plugin` Reload, an agent switch and a server's dial cannot disagree
    /// about what the model is offered. Order matters in one direction only:
    /// the team tool joins before the arithmetic that reads the composed set's
    /// names, so it is in the definitions snapshot `tool_search` answers from.
    fn compose(&self, registry: Arc<Registry>) -> Arc<Registry> {
        self.compose_deferral(self.team_messaging(registry))
    }

    /// Adds `send_message` when this session has a postbox — a team's, or a
    /// teamless interactive session's solo one (**D498**, **D530**) — and
    /// nothing at all when it has none.
    ///
    /// Registered here rather than in `Registry::with_builtins` for `task`'s
    /// reason: presence is ability. A session with no postbox has nobody to
    /// address, so the tool is not offered — which is also what keeps the
    /// golden differential comparing two agents rather than two teams — and a
    /// session that has one is offered it again on every rebuild, so a
    /// `/plugin` Reload cannot quietly drop it.
    ///
    /// A teamless session's description carries no roster claim at all
    /// ([`send_message::SendMessageTool::teamless`]) rather than the empty
    /// roster a team-of-one renders — the two must not read alike (D530's own
    /// distinction) — decided by [`Engine::teamless`], which
    /// [`Engine::install_postbox`] and [`Engine::install_solo_postbox`]
    /// keep in lockstep with whichever postbox is actually installed. Both
    /// non-teamless halves come off the one postbox this engine posts
    /// through, which is what keeps the roster the model *reads* and the
    /// roster its call is *judged against* the same answer: the description
    /// lists everybody the sender may address, and the last rung of the
    /// tool's ladder asks that same value which of them leads.
    ///
    /// A subagent is offered the *lent* set rather than the composed one, so
    /// it does not get this — deliberately, and for the reason it does not get
    /// `task` either: a delegated turn runs inside the lead's own turn, and
    /// the identity it would send under is the lead's.
    fn team_messaging(&self, registry: Arc<Registry>) -> Arc<Registry> {
        let postbox = self
            .postbox
            .lock()
            .expect("the postbox is never poisoned")
            .clone();
        let Some(postbox) = postbox else {
            return registry;
        };

        let tool: Arc<dyn Tool> = if self.teamless.load(Ordering::Relaxed) {
            Arc::new(send_message::SendMessageTool::teamless())
        } else {
            Arc::new(send_message::SendMessageTool::new(&postbox.roster()))
        };

        Arc::new(registry.with(tool))
    }

    /// Everybody this engine's own `send_message` may address, as its postbox
    /// answers it. [`None`] when there is no team, which is not the same as a
    /// team of nobody.
    fn postbox_roster(&self) -> Option<Vec<Peer>> {
        self.postbox
            .lock()
            .expect("the postbox is never poisoned")
            .as_ref()
            .map(|postbox| postbox.roster())
    }

    /// The deferral half of every registry composition (**D492**), reached
    /// through [`Engine::compose`] so no path can disagree: candidates are
    /// grouped from the composed set's own `mcp__*` names with the activated
    /// set exempt before the arithmetic, `tool_search` joins the roster only
    /// while something defers — beside every builtin, `task`, `skill` and the
    /// plan doors, never itself deferred — and the definitions snapshot it
    /// answers from is rewritten last, so a reconnect's recomposition is what
    /// a later search reads.
    fn compose_deferral(&self, registry: Arc<Registry>) -> Arc<Registry> {
        let activated = self
            .activated_tools
            .lock()
            .expect("the activated set is never poisoned")
            .clone();
        let definitions = registry.definitions();
        let candidates = crate::tool::deferral::candidates(
            definitions
                .iter()
                .map(|definition| definition.name.as_str()),
            self.defer_threshold,
            &activated,
        );
        let deferral =
            crate::tool::deferral::Deferral::over(candidates, Arc::clone(&self.activated_tools));

        let registry = if deferral.any() {
            Arc::new(
                registry.with(Arc::new(crate::tool::deferral::ToolSearchTool::over(
                    Arc::clone(&self.tool_definitions),
                    deferral.clone(),
                ))),
            )
        } else {
            registry
        };

        *self
            .tool_definitions
            .lock()
            .expect("the definitions snapshot is never poisoned") = registry.definitions();
        *self
            .deferral
            .lock()
            .expect("the deferral is never poisoned") = deferral;

        registry
    }

    /// Runs the shared composition path over the currently offered set, in
    /// place — the builder-time half of it, for the engine that never rebuilds
    /// (base-tool `mcp__*` names, a threshold or a team installed after
    /// construction) and for `NewSession`, whose cleared activated set puts
    /// never-touched names back under the arithmetic.
    fn recompose_tools(&self) {
        let composed = self.compose(self.tools());
        *self
            .tools
            .lock()
            .expect("the tool registry is never poisoned") = composed;
    }

    /// What a `task` call needs to run a child loop, or [`None`] when this
    /// engine has no agents to spawn.
    fn spawn_host(&self, model: String) -> Option<Arc<subagent::Host>> {
        let deferral = self.deferral();
        // A subagent is offered this build's tools minus the one that
        // spawns subagents, which is the whole of the depth limit (D9).
        // MCP tools are in that set: a subagent works on the same project
        // with the same servers. Their asks refuse unattended, because
        // nobody is watching a subagent's turn. While something defers, the
        // resident `tool_search` rides along (**D492**) — a child reads the
        // same advertised subset, so it is owed the same door back in —
        // while `task` and the plan doors stay absent exactly as before.
        let tools = if deferral.any() {
            Arc::new(
                self.lent()
                    .with(Arc::new(crate::tool::deferral::ToolSearchTool::over(
                        Arc::clone(&self.tool_definitions),
                        deferral.clone(),
                    ))),
            )
        } else {
            self.lent()
        };

        Some(Arc::new(subagent::Host {
            provider: Arc::clone(&self.provider),
            model,
            small_model: self.small_model.clone(),
            agents: Arc::clone(self.agents.as_ref()?),
            tools,
            deferral,
            permissions: Arc::clone(&self.permissions),
            base_prompt: self.base_half(),
            prompt_suffix: self.environment_half(),
            cwd: self.cwd.clone(),
            root: self.root.clone(),
            credentials: self.credentials.clone(),
            lsp: self.lsp.clone(),
            persistence: self.persistence.clone(),
            jobs: Some(Arc::clone(&self.jobs) as Arc<dyn crate::tool::job::Jobs>),
            hooks: self.hooks(),
            concurrency: self.concurrency,
            // The root turn's Host, so the team crosses whole: `task {name}`
            // is the model-side spawn door (D504), and a `None` here is what
            // once left it answering NO_TEAM while the schema advertised the
            // argument. The depth guard lives elsewhere — a *delegated* child
            // runs with `spawn: None` (no `task` tool at all), and a teammate
            // engine is built without a team of its own.
            teammates: self.teammates.clone(),
            identity: Arc::clone(&self.identity),
        }))
    }

    /// Whether this engine's provider serves `model`, which must already be
    /// a bare catalog id.
    fn serves(&self, model: &str) -> bool {
        crate::provider::serves(self.provider.id(), model)
    }

    /// The model a config spelling names, when this provider serves it.
    ///
    /// Every model that reaches the engine from a config file — an agent's
    /// own, a command's — arrives spelled `"provider/model"` and has to be
    /// split before it means anything to the catalog. See
    /// [`crate::provider::adopt`].
    fn adopt(&self, spelled: &str) -> Option<String> {
        crate::provider::adopt(self.provider.id(), spelled)
    }

    /// The in-memory half of adopting `agent`: the tool rebuild, the active
    /// selection, both prompt halves, and the durable row. Emits **nothing**
    /// — the announced half belongs to [`Engine::adopt_agent`] on the manual
    /// path and to the turn boundary on the approval path, and a shared
    /// function that emitted would make one of them say it twice. Returns
    /// whether adopting the agent's preferred model cleared the effort, for
    /// the announcing caller to say so.
    fn apply_agent(&self, agent: &Agent) -> bool {
        self.install(agent);
        {
            let mut active = self.active();
            active.agent = Some(agent.name.clone());
            // Upstream's pickers key the model off the agent, so switching to
            // one that prefers a model switches the model with it. A model the
            // provider does not serve is not a reason to refuse the agent —
            // the session simply keeps asking the model it was already asking.
            if let Some(model) = agent.model.as_deref().and_then(|model| self.adopt(model)) {
                active.model = model;
            }
        }
        let cleared = self.reconcile_effort();
        self.recompose_environment();
        self.recompose_base();
        self.remember_selection();

        cleared
    }

    /// [`Engine::apply_agent`], announced: exactly one `agent_changed` frame
    /// per adoption, which is what makes a manual `/agents` switch visible to
    /// every subscriber rather than only to the frontend that issued it.
    ///
    /// Emitting under a held slot guard is established practice here, not a
    /// new hazard: `resume`, `revert_to` and `truncate_reverted` all publish
    /// the same way.
    async fn adopt_agent(&self, agent: &Agent) {
        let cleared = self.apply_agent(agent);

        let (name, model) = {
            let active = self.active();
            (agent.name.clone(), active.model.clone())
        };
        let _ = self
            .fanout
            .send(Event::AgentChanged {
                session_id: self.session_id(),
                agent: name,
                model,
            })
            .await;
        // After the agent frame, so a subscriber reads the model that made
        // the effort impossible before the clear that answers it.
        if cleared {
            let _ = self
                .fanout
                .send(Event::EffortChanged {
                    session_id: self.session_id(),
                    effort: None,
                })
                .await;
        }
    }

    /// Runs the rest of the session as `name`.
    async fn switch_agent(&self, name: String) -> Result<(), EngineError> {
        // Held across the whole switch, exactly as `resume` holds it: a prompt
        // that arrives mid-switch waits and then runs as the agent that was
        // asked for, rather than racing it. A pending approval is discarded
        // whole: the person's later explicit choice outranks the earlier Yes,
        // which is what keeps an approval sentence from ever landing beside
        // `PLAN_REMINDER` in a plan turn (deviation:
        // a-later-switch-outranks-a-yes).
        let turn = self.lock_entry(PendingPolicy::Discard).await?;

        let Some(registry) = &self.agents else {
            return Err(EngineError::NoAgents);
        };
        let Some(agent) = registry.get(&name) else {
            return Err(EngineError::UnknownAgent { name });
        };
        if agent.mode == AgentMode::Subagent {
            return Err(EngineError::SubagentNotSelectable { name });
        }

        self.adopt_agent(agent).await;
        drop(turn);

        Ok(())
    }

    /// Asks the rest of the session's requests of `model`.
    async fn switch_model(&self, model: String) -> Result<(), EngineError> {
        // An announced approval is applied *first*: this entry's own
        // `remember_selection` below would otherwise write the stale plan
        // agent over the row's build. The sentence keeps riding — a model
        // switch is not the prompt that delivers it.
        let turn = self.lock_entry(PendingPolicy::Apply).await?;

        if !self.serves(&model) {
            return Err(EngineError::UnknownModel {
                model,
                provider: self.provider.id().to_owned(),
            });
        }

        self.active().model = model;
        let cleared = self.reconcile_effort();
        self.recompose_environment();
        self.recompose_base();
        self.remember_selection();
        if cleared {
            let _ = self
                .fanout
                .send(Event::EffortChanged {
                    session_id: self.session_id(),
                    effort: None,
                })
                .await;
        }
        drop(turn);

        Ok(())
    }

    /// Runs the rest of the session under the named effort of the active
    /// model, or back under none.
    ///
    /// Validation is the catalog's, in two tiers matching how efforts exist
    /// at all: a provider with no rows has no names to select from, and a
    /// cataloged model refuses a name its row does not carry — with the row's
    /// real names in the refusal. Clearing (`None`) is never refused: it asks
    /// for the state every session starts in.
    async fn switch_effort(&self, effort: Option<String>) -> Result<(), EngineError> {
        // An announced approval is applied first, exactly as `switch_model`
        // applies it: this entry's `remember_selection` writes the row.
        let turn = self.lock_entry(PendingPolicy::Apply).await?;

        if let Some(name) = &effort {
            if !catalog::carries(self.provider.id()) {
                return Err(EngineError::UncatalogedEffort {
                    provider: self.provider.id().to_owned(),
                });
            }
            let model = self.active().model.clone();
            // A cataloged provider may still be asked for an uncataloged model
            // (`GANJA_MODEL` takes any spelling), and a row it does not have
            // carries no efforts — the empty list, not a panic. The lookup is
            // provider-scoped because two providers publish the same id with
            // rosters spliced for different wires (`catalog::model_for`).
            let available: Vec<String> = catalog::model_for(self.provider.id(), &model)
                .map(|info| info.variants.keys().cloned().collect())
                .unwrap_or_default();
            if !available.iter().any(|carried| carried == name) {
                return Err(EngineError::UnknownEffort {
                    effort: name.clone(),
                    model,
                    available,
                });
            }
        }

        self.active().effort = effort.clone();
        self.remember_selection();
        let _ = self
            .fanout
            .send(Event::EffortChanged {
                session_id: self.session_id(),
                effort,
            })
            .await;
        drop(turn);

        Ok(())
    }

    /// Takes the posture the next turn runs under (**D-15**, **D496**).
    ///
    /// **No [`Engine::lock_entry`], deliberately**, where the three switches
    /// above it all take one. It is D474's discipline rather than
    /// [`Command::SwitchAgent`]'s refusal, and the difference is who is
    /// sending: an agent, a model or an effort is picked by the person
    /// watching the turn, who can wait for it to end, while this may be a
    /// team's lead answering a teammate's `mode_set_request` mid-stream — and
    /// a refusal there would drop a decision nobody would think to re-send.
    ///
    /// So the change is taken at once, announced at once, and **bites at the
    /// next turn's start**, where [`Bypass::begin`] reads it. The event is
    /// therefore earlier than the effect, which is what its own documentation
    /// says.
    ///
    /// # Errors
    ///
    /// None today: the two postures are always reachable, and a mode name this
    /// build has no posture for was already refused where the name was read
    /// ([`PermissionMode::from_claude_name`]). The signature matches its
    /// neighbours' so the command table stays one shape.
    async fn set_permission_mode(&self, mode: PermissionMode) -> Result<(), EngineError> {
        *self
            .permission_mode
            .lock()
            .expect("the permission mode is never poisoned") = mode;
        let _ = self
            .fanout
            .send(Event::PermissionModeChanged {
                session_id: self.session_id(),
                mode,
            })
            .await;
        // A mode change re-decides every held inbound message under its own
        // recorded origin (**D524**, v2 §"Reevaluation and manual decision",
        // evidence 620778-620845): the receiver class just moved, and today
        // it is the only thing that moves mid-session — config does not
        // change under a running engine.
        for teammate::inbound::Reevaluated { id, settlement } in
            self.inbound.reevaluate(self.receiver_class())
        {
            settle_side_effects(&self.inbound, self.teammates.as_ref(), &id, settlement).await;
        }

        Ok(())
    }

    /// The posture the next turn will run under.
    ///
    /// The *next* one, always: a turn already streaming keeps what it began
    /// with, so this answers what a running turn is doing only when nothing
    /// has been set since it started.
    #[must_use]
    pub fn permission_mode(&self) -> PermissionMode {
        *self
            .permission_mode
            .lock()
            .expect("the permission mode is never poisoned")
    }

    /// Adopts a configured effort for a session that has not chosen one.
    ///
    /// A **default, not an override**, which is the whole difference from
    /// `Self::switch_effort`: a resume has already restored whatever the
    /// stored row carried by the time a frontend calls this, and a session
    /// that arrived holding an effort keeps it. Only a session still on the
    /// state every session starts in takes the config's.
    ///
    /// The other difference is that a name this model does not serve is not an
    /// error. A wrong `--effort` flag or `/effort` pick is a person asking for
    /// something now, and the refusal is the answer; a config key is a
    /// standing wish read before anybody knows which model a session will
    /// settle on, and refusing to start over it would make one line in a
    /// global file break every project whose model happens to be cataloged
    /// differently. So it clears, through the same `Self::reconcile_effort`
    /// a model switch clears through, and says why in the log.
    ///
    /// Announced like every other path that moves the selection, so a
    /// frontend's indicator shows a seeded effort without having asked for it.
    pub async fn seed_effort(&self, effort: Option<String>) {
        let Some(name) = effort else {
            return;
        };
        if self.active().effort.is_some() {
            return;
        }

        self.active().effort = Some(name.clone());
        if self.reconcile_effort() {
            tracing::warn!(
                effort = name.as_str(),
                model = self.active().model.as_str(),
                "the configured effort is not one this model carries; \
                 starting without one"
            );
            return;
        }

        self.remember_selection();
        let _ = self
            .fanout
            .send(Event::EffortChanged {
                session_id: self.session_id(),
                effort: Some(name),
            })
            .await;
    }

    /// Clears the selected effort when the active model's catalog row no
    /// longer carries its name, returning whether it did — upstream clears at
    /// the same boundary (`prompt.ts:654`). The caller owns announcing a
    /// clear, because two of the three model-moving paths announce from an
    /// async context this helper does not have.
    fn reconcile_effort(&self) -> bool {
        let mut active = self.active();
        let Some(name) = active.effort.as_ref() else {
            return false;
        };
        if catalog::model_for(self.provider.id(), &active.model)
            .is_some_and(|info| info.variants.contains_key(name))
        {
            return false;
        }

        active.effort = None;
        true
    }

    /// Writes the current selection onto the live session record, so that
    /// reopening the session reopens the same one.
    ///
    /// Nothing to do before the first prompt has minted a session — the record
    /// that does not exist yet is created carrying whatever is active then.
    fn remember_selection(&self) {
        let Some(state) = &self.persistence else {
            return;
        };
        let (model, effort, agent) = {
            let active = self.active();
            (
                active.model.clone(),
                active.effort.clone(),
                active.agent.clone(),
            )
        };

        let mut live = state
            .live
            .lock()
            .expect("the live session is never poisoned");
        let Some(info) = live.info.as_mut() else {
            return;
        };
        info.model = Some(model);
        info.effort = effort;
        info.agent = agent;
        info.updated = now();

        if let Err(error) = state.storage.save_info(info) {
            tracing::warn!(
                session = info.id.as_str(),
                %error,
                "the session's agent and model could not be stored; \
                 the switch holds for this process only"
            );
        }
    }

    /// The one door to the turn slot for a state-changing entry: Busy-check,
    /// then the named [`PendingPolicy`] applied while the guard is held —
    /// [`TurnSlot::entry`] moves the cell, and whatever engine-side work that
    /// transition owes is settled here before the guard is handed back.
    ///
    /// All seven entries route through this — `start_turn`, `switch_agent`,
    /// `switch_model`, `new_session`, `resume`, `undo`, `redo` — so a future
    /// entry cannot Busy-check without naming what it does to a pending
    /// approval, which is the structural fix for the drift a prose table
    /// already suffered once.
    async fn lock_entry(
        &self,
        policy: PendingPolicy,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<TurnHandle>>, EngineError> {
        let (guard, owed) = self.turn.entry(policy).await?;
        match owed {
            Owed::Nothing => {}
            Owed::Apply(target) => {
                // Presence is ability: the cell only reaches `Announced` for a
                // direction whose door was registered, and a door is
                // registered only when the registry holds the agent it leads
                // to.
                let adopted = self
                    .agents
                    .as_ref()
                    .and_then(|registry| registry.get(target.agent()))
                    .expect(
                        "a plan switch is only ever pending where the registry holds its target",
                    );
                // The AgentChanged half of this boundary was already announced
                // by the turn that moved the cell; an effort the adopted
                // model cannot carry still owes its own frame, or a frontend
                // keeps showing a selection the row no longer holds.
                if self.apply_agent(adopted) {
                    let _ = self
                        .fanout
                        .send(Event::EffortChanged {
                            session_id: self.session_id(),
                            effort: None,
                        })
                        .await;
                }
            }
            Owed::ReassertRow => self.remember_selection(),
        }

        Ok(guard)
    }

    /// The pending-switch cell, cloned for a root turn that could write or
    /// announce it — which is a turn of an engine whose registry holds an
    /// agent one of the two plan doors leads to, and nothing else. [`None`]
    /// here is what makes `ToolCtx::switch` [`None`] and phase one inert on
    /// every other engine, the same presence-is-ability gate that decides
    /// whether either door is registered at all. Either agent suffices, since
    /// the seam is one: which direction can actually be *recorded* is settled
    /// by which door [`Engine::install`] put in the registry (**D477**).
    fn pending_for_turn(&self) -> Option<Arc<std::sync::Mutex<PendingSwitch>>> {
        let agents = self.agents.as_ref()?;
        if agents.get(agent::BUILD).is_none() && agents.get(agent::PLAN).is_none() {
            return None;
        }

        Some(Arc::clone(&self.turn.pending))
    }

    async fn start_turn(
        &self,
        prompt: String,
        kind: TurnKind,
        overrides: Option<Overrides>,
    ) -> Result<(), EngineError> {
        // An announced approval is applied inside `lock_entry` — before the
        // `refresh_mcp` below, so the task-roster rebuild sees build, and
        // before the `previous_agent` bookkeeping, so the
        // `previous == plan && agent == build` reminder gate does too.
        let mut turn = self.lock_entry(PendingPolicy::Apply).await?;

        // Before the model hears a word of it, and inside the slot guard so
        // that nothing starts a turn while the hook is deciding. Only a turn
        // that *asks* the model: a `!` passthrough runs a command the person
        // typed themselves and a compaction says nothing new, so neither is a
        // prompt anybody submitted. A refusal returns here with the guard
        // dropped and the slot empty — the engine is idle, the prompt never
        // happened, and the frontend keeps the text.
        let mut hook_context = Vec::new();
        if matches!(kind, TurnKind::Prompt { .. })
            && let Some(hooks) = self.hooks()
        {
            let outcome = hooks
                .fire(
                    self.session_id().as_str(),
                    &hook::Payload::UserPromptSubmit {
                        prompt: prompt.clone(),
                    },
                )
                .await;
            outcome.report(hook::HookEvent::UserPromptSubmit);
            if let Some(reason) = outcome.blocked {
                return Err(EngineError::HookRefused {
                    event: hook::HookEvent::UserPromptSubmit.name(),
                    reason,
                });
            }
            hook_context = outcome.context;
        }

        // Between turns and never during one: a server that connected while
        // the last turn was streaming is offered to the model here, and a
        // connection that died is withdrawn here.
        self.refresh_mcp();
        // The same seam, for the same reason, for the team: a teammate spawned
        // mid-turn is somebody the next turn may write to.
        self.refresh_team();

        // A prompt or a shell command after an `/undo` is the user keeping
        // what the undo did. The messages it hid leave the transcript here,
        // before this turn appends anything, which is what stops a prompt that
        // was taken back from reaching the request that replaces it. A
        // compaction is not that kind of turn — it says nothing new, so it
        // decides nothing.
        if matches!(kind, TurnKind::Prompt { .. } | TurnKind::Shell { .. }) {
            self.truncate_reverted().await;
        }

        // Read once, and recorded as the previous turn's agent in the same
        // breath, so that the plan-to-build reminder fires for exactly one
        // turn however many follow it. Only a prompt is that kind of turn: a
        // `!` passthrough and a compaction never put the reminder in front of
        // the model, so letting one stand in as "the previous turn" would
        // spend a notice that was never delivered (deviation:
        // build-switch-counts-only-turns-that-ask).
        let asks_the_model = matches!(kind, TurnKind::Prompt { .. });
        let (mut model, effort, name, previous) = {
            let mut active = self.active();
            let name = active.agent.clone();
            let previous = if asks_the_model {
                std::mem::replace(&mut active.previous_agent, name.clone())
            } else {
                active.previous_agent.clone()
            };

            (active.model.clone(), active.effort.clone(), name, previous)
        };
        let session_agent = self
            .agents
            .as_ref()
            .zip(name.as_deref())
            .and_then(|(registry, name)| registry.get(name));

        // A command running as another agent gets that agent's prompt, model
        // and rules for this turn alone. The rules travel as a ruleset of the
        // turn's own rather than by installing a baseline that would have to be
        // put back afterwards; both sets answer for the same project and share
        // the same store, so an "always" given here still outlives the process —
        // it just does not reach the session's own set until the store is read
        // again (deviation: command-agent-derives-its-rules).
        //
        // Derived through `baseline_for`, so the standing rules cross with it.
        // What the session refused for every turn it runs is not the session
        // agent's opinion for a command to leave behind by naming another
        // agent — a headless run refuses `question` because nobody is there to
        // answer one, and that is no less true of the turn a `/command` takes.
        let (agent, permissions) = match overrides.as_ref().and_then(|it| it.agent.as_ref()) {
            None => (session_agent, Arc::clone(&self.permissions)),
            Some(agent) => {
                let derived = self
                    .permissions
                    .lock()
                    .expect("the permission rules are never poisoned")
                    .derive(self.baseline_for(agent));

                (Some(agent), Arc::new(std::sync::Mutex::new(derived)))
            }
        };
        if let Some(asked) = overrides.as_ref().and_then(|it| it.model.clone()) {
            model = asked;
        }
        // Resolved per turn against the model this turn will actually ask —
        // upstream re-resolves from each user message (`prompt.ts:649-666`) —
        // so a `/command`'s one-turn model override that lacks the session's
        // effort runs without it for that turn, and the session's selection
        // stands untouched. An empty map is "no effort", the shape every
        // request had before efforts existed.
        let effort_options = effort
            .as_ref()
            .and_then(|name| {
                catalog::model_for(self.provider.id(), &model)?
                    .variants
                    .get(name)
                    .cloned()
            })
            .unwrap_or_default();

        let system = self.system_for(agent);
        // A command that runs as another agent is not the session switching to
        // it, so the plan-to-build notice — which is about what the *user*
        // switched to — is left to the session's own agent. The approval cell
        // travels only when this turn asks the model: a shell or compact turn
        // never delivers a reminder, so letting it consume the sentence would
        // spend a notice that was never shown — the misfire `SentencePending`
        // exists to prevent.
        let mut reminders = reminders(
            name.as_deref(),
            previous.as_deref(),
            asks_the_model.then_some(self.turn.pending.as_ref()),
        );
        // Files that went stale while nobody was asking are named here, at the
        // top of the first turn that could act on what it read of them. Only a
        // turn that asks the model can deliver one — a `!` passthrough asks
        // nothing and a compaction asks a question of its own — so the queue is
        // drained by those turns alone and the notice waits for the prompt that
        // follows (deviation: stale-notice-only-on-turns-that-ask).
        if asks_the_model && let Some(notice) = stale_notice(&self.files.take_stale(), &self.root) {
            reminders.push(notice);
        }
        // What the session's own hooks asked to say, in the order they said it:
        // whatever a `SessionStart` queued (drained here, once — D460), then
        // what this prompt's own `UserPromptSubmit` hooks just added.
        if asks_the_model {
            reminders.extend(
                self.hook_context
                    .lock()
                    .expect("the hook context is never poisoned")
                    .drain(..),
            );
        }
        reminders.extend(hook_context);

        // The first prompt on a persistent engine creates the session record,
        // and it reaches the disk before the first byte streams: a crash
        // mid-turn must still leave something to resume. A store that
        // refuses is a warning, not a dead prompt.
        //
        // The record adopts the id the engine minted at construction (or at
        // the last `NewSession`) rather than minting one of its own: the id
        // predates the row on purpose, so the events of this very turn and
        // the stored session agree on which conversation they are.
        let persist = self.persistence.as_ref().map(|state| {
            let session = {
                let mut live = state
                    .live
                    .lock()
                    .expect("the live session is never poisoned");
                if live.info.is_none() {
                    live.warned_uncataloged = false;
                }
                live.info
                    .get_or_insert_with(|| {
                        fresh_session(
                            &state.storage,
                            self.session_id(),
                            name.clone(),
                            model.clone(),
                            effort.clone(),
                        )
                    })
                    .id
                    .clone()
            };

            Persist::new(Arc::clone(state), session)
        });

        let cancel = CancellationToken::new();
        let pending: Arc<std::sync::Mutex<PendingReplies>> = Arc::default();
        // Born with the slot, so a `Steer` racing the very first byte of the
        // turn finds a mailbox rather than a window in which the slot is
        // occupied and there is nowhere to post.
        let steer: Arc<std::sync::Mutex<Steering>> = Arc::default();
        *turn = Some(TurnHandle {
            cancel: cancel.clone(),
            permission: Arc::clone(&pending),
            steer: Arc::clone(&steer),
        });
        // Where a permission mode bites (**D496**): read once, here, so this
        // turn keeps the posture it began with however many arrive while it
        // streams — the same discipline the hooks and the tool set a turn
        // clones at its start keep.
        self.fanout.bypass.begin(self.permission_mode(), &pending);
        drop(turn);

        // The task is deliberately not joined. `cancel` is what stops a turn,
        // and it reaches the provider and every running tool, so an aborted
        // HTTP stream is the provider's business rather than something the
        // engine has to kill from outside. Aborting the task instead would
        // skip the cleanup that releases the busy slot and guarantees a
        // terminal event.
        let turn = Turn {
            provider: Arc::clone(&self.provider),
            spawn: self.spawn_host(model.clone()),
            concurrency: self.concurrency,
            session_id: self.session_id(),
            model,
            small_model: self.small_model.clone(),
            effort_options,
            system,
            reminders,
            kind,
            tools: self.tools(),
            skill_roots: self.skill_roots(),
            identity: Arc::clone(&self.identity),
            teamless: self.teamless.load(Ordering::Relaxed),
            teamless_send: self.teamless_send,
            deferral: self.deferral(),
            permissions,
            cwd: self.cwd.clone(),
            root: self.root.clone(),
            files: Arc::clone(&self.files),
            credentials: self.credentials.clone(),
            lsp: self.lsp.clone(),
            snapshots: self.snapshots.clone(),
            prompt,
            cancel,
            pending,
            steer,
            events: Arc::clone(&self.fanout),
            // The turn's release handle for its own boundary — not an
            // acquisition path; those stay behind `entry` and `observe`.
            slot: Arc::clone(&self.turn.slot),
            history: Arc::clone(&self.history),
            pending_switch: self.pending_for_turn(),
            jobs: Some(Arc::clone(&self.jobs) as Arc<dyn crate::tool::job::Jobs>),
            hooks: self.hooks(),
            postbox: self
                .postbox
                .lock()
                .expect("the postbox is never poisoned")
                .clone(),
            delegated: false,
            persist,
        };
        tokio::spawn(run_turn(turn));

        Ok(())
    }

    async fn cancel_turn(&self) {
        self.turn
            .observe(|turn| {
                if let Some(turn) = turn {
                    turn.cancel.cancel();
                }
            })
            .await;
    }

    /// Hands a message to the turn in flight, which takes it on at its next
    /// step boundary.
    ///
    /// **No [`Engine::lock_entry`], deliberately.** Every other state-changing
    /// entry Busy-checks an empty slot and names what it does to a pending
    /// plan approval; this one's precondition is the opposite — it needs an
    /// *occupied* slot — and it changes no engine state at all, only a cell
    /// the running turn owns. So it goes through the same read-only
    /// [`TurnSlot::observe`] the cancel and the reply routes do, and an empty
    /// slot is [`EngineError::NotStreaming`] rather than a policy question.
    ///
    /// Whether the turn actually drains it is the turn's business: a cancel
    /// reaching the loop first leaves the message unconsumed and unannounced,
    /// which is the frontend's cue to keep it (see
    /// [`Event::SteerConsumed`]).
    async fn steer(
        &self,
        id: String,
        text: String,
        mentions: Vec<crate::protocol::Mention>,
        skills: Vec<String>,
        peers: Vec<crate::protocol::team::PeerPayload>,
        session_mentions: Vec<String>,
    ) -> Result<(), EngineError> {
        let queued = self
            .turn
            .observe(move |turn| {
                let Some(turn) = turn else {
                    return false;
                };
                turn.steer
                    .lock()
                    .expect("the steer mailbox is never poisoned")
                    .push(SteerInput {
                        id,
                        text,
                        mentions,
                        skills,
                        peers,
                        session_mentions,
                    });

                true
            })
            .await;

        if queued {
            Ok(())
        } else {
            Err(EngineError::NotStreaming)
        }
    }

    /// Routes a reply to the permission wait that asked for it.
    ///
    /// A reply nothing is waiting for — the id is stale, the turn already
    /// ended, or a cancel raced it — is defined to be ignored: the turn task
    /// owns answering every request exactly once, so there is nothing here to
    /// repair.
    async fn reply_permission(
        &self,
        id: &crate::protocol::PermissionId,
        reply: crate::protocol::PermissionReply,
    ) {
        let delivered = self
            .turn
            .observe(|turn| {
                turn.is_some_and(|turn| {
                    // By the exact id, out of the map of permission waits — so
                    // a reply naming a question's id finds nothing rather than
                    // reaching a permission wait expecting a decision, and a
                    // reply naming one of several open dialogs reaches that
                    // one.
                    turn.permission
                        .lock()
                        .expect("the pending replies are never poisoned")
                        .answer_permission(id, reply)
                })
            })
            .await;

        if !delivered {
            tracing::debug!(id = id.as_str(), "no permission is waiting for this reply");
        }
    }

    /// Routes an answer — or a dismissal — to the question wait that asked for
    /// it.
    ///
    /// The same rule the permission route keeps: an answer nothing is waiting
    /// for is ignored, because the turn task owns answering every request
    /// exactly once. What differs is only that two commands land here, since
    /// upstream makes a rejection its own thing rather than a refusing value.
    async fn answer_question(&self, id: &crate::protocol::QuestionId, answered: Answered) {
        let delivered = self
            .turn
            .observe(|turn| {
                turn.is_some_and(|turn| {
                    turn.permission
                        .lock()
                        .expect("the pending replies are never poisoned")
                        .answer_question(id, answered)
                })
            })
            .await;

        if !delivered {
            tracing::debug!(id = id.as_str(), "no question is waiting for this reply");
        }
    }
}

/// What a tool part that was still open when its process died says on resume.
const INTERRUPTED: &str = "the session was interrupted before this call finished";

/// The synthetic user parts one turn's request carries, ported from upstream's
/// `session/reminders.ts`.
///
/// Two of them, and both are about the agent rather than about anything the
/// user said: the planning agent is told on every turn that it may not act,
/// and the turn that stops planning is told once that it may.
///
/// Upstream's second condition is "any assistant message in the window ran as
/// `plan`", which re-injects the notice on every build turn for the rest of
/// the session. This build compares against the previous turn alone, so it is
/// said once, where it means something (deviation: build-switch-once). The
/// cost is that neither survives a restart, since a stored message does not
/// record the agent that produced it.
///
/// `pending` is the plan-approval cell, handed over only by a turn that asks
/// the model — the one kind that can deliver a reminder. A riding
/// [`APPROVAL_SENTENCE`] is consumed here, exactly once, on the first build
/// prompt that assembles: injection is what moves the cell to
/// [`PendingSwitch::None`], so the sentence rides one request and never
/// returns (deviation: approval-rides-the-request — upstream stores a
/// synthetic user message instead).
/// Characters of `message` the next request would carry, split into what its
/// author produced — text, the tool calls' names and argument JSON, a
/// reasoning part's sealed state, a mention's path — and what came back from
/// tools: the outputs and errors, which are the one share of an assistant
/// message a provider's reported `output_tokens` never covered, so
/// [`Engine::context_breakdown`] estimates them even where it has actuals.
///
/// Bookkeeping parts (the step markers, a patch record) count nothing: no
/// wire carries them.
fn message_chars(message: &Message) -> (usize, usize) {
    let (mut generated, mut results) = (0_usize, 0_usize);

    for part in &message.parts {
        match &part.body {
            PartBody::Text { text } => generated += text.chars().count(),
            PartBody::Tool {
                call_id,
                tool,
                state,
            } => {
                generated += call_id.chars().count() + tool.chars().count();
                match state {
                    ToolState::Pending { .. } => {}
                    ToolState::Running { input, .. } => {
                        generated += input.to_string().chars().count();
                    }
                    ToolState::Completed { input, output, .. } => {
                        generated += input.to_string().chars().count();
                        results += output.chars().count();
                    }
                    ToolState::Error { input, error, .. } => {
                        generated += input.to_string().chars().count();
                        results += error.chars().count();
                    }
                }
            }
            // The stored part is a reference; its content is read at send
            // time, so the path is the only length this side holds.
            PartBody::File { path, .. } => generated += path.chars().count(),
            PartBody::Reasoning { encrypted, .. } => {
                generated += encrypted.as_deref().map_or(0, |blob| blob.chars().count());
            }
            // A peer's words count, and they are the one part here that is
            // drawn *and* sent (D495): the request assembly renders them into
            // the user turn, so a meter that skipped them would report a
            // window emptier than the one the next request fills. They count
            // as generated rather than as a result — a teammate wrote them,
            // and no tool of this session's answered with them.
            PartBody::Peer {
                from,
                summary,
                body,
                ..
            } => {
                generated += from.chars().count()
                    + summary.as_deref().map_or(0, |line| line.chars().count())
                    + body.chars().count();
            }
            // Readable thinking counts nothing for the bookkeeping parts'
            // reason: no wire carries it. It is on the screen, not in the
            // request, and a meter that counted it would report a window
            // filling with words the model is never sent.
            //
            // A provider-run tool counts nothing for the same reason, and
            // it is the sharper case: its arguments and its result were the
            // *gateway's* tokens, spent inside a request this side never
            // composed, and counting them against this window would report a
            // context filling with what somebody else sent.
            PartBody::ReasoningText { .. }
            | PartBody::ServerTool { .. }
            | PartBody::StepStart
            | PartBody::StepFinish { .. }
            | PartBody::Patch { .. } => {}
        }
    }

    (generated, results)
}

/// The in-process backend of a session that has nowhere to write a
/// conversation.
///
/// A teammate's transcript is a **root session** somebody may resume tomorrow
/// (D-8), so an engine built without a store — every scripted, golden and PTY
/// run — has no honest way to run one. It refuses by name rather than starting
/// a teammate whose whole transcript evaporates with the process, and the rest
/// of the team still works: the roster, the mailboxes and the two pane
/// surfaces are all somebody else's to provide.
#[derive(Debug)]
struct Storeless;

#[async_trait::async_trait]
impl teammate::TeammateBackend for Storeless {
    fn backend(&self) -> crate::protocol::team::MemberBackend {
        crate::protocol::team::MemberBackend::InProcess
    }

    async fn spawn(
        &self,
        _spec: &teammate::SpawnSpec,
    ) -> Result<teammate::Handle, teammate::Unsupported> {
        Err(teammate::Unsupported {
            backend: crate::protocol::team::MemberBackend::InProcess,
            reason: STORELESS.to_owned(),
        })
    }

    async fn kill(&self, handle: &teammate::Handle) {
        // Nothing this backend made can be here to end. Named rather than
        // ignored, because a handle arriving here would mean a registry had
        // crossed two backends.
        tracing::warn!(
            ?handle,
            "a storeless backend was asked to end something it did not start"
        );
    }

    fn delivery(&self) -> teammate::Delivery {
        teammate::Delivery::Acknowledged
    }

    // Never seeded, since every spawn is refused above; the native words, for
    // the surface this stands in for.
    fn preamble(&self, spec: &teammate::SpawnSpec) -> String {
        teammate::preamble::native(teammate::preamble::Names::of(spec), &spec.prompt)
    }
}

fn reminders(
    agent: Option<&str>,
    previous: Option<&str>,
    pending: Option<&std::sync::Mutex<PendingSwitch>>,
) -> Vec<String> {
    let mut found = Vec::new();

    if agent == Some(agent::PLAN) {
        found.push(agent::PLAN_REMINDER.to_owned());
    }
    if agent == Some(agent::BUILD) && previous == Some(agent::PLAN) {
        found.push(agent::BUILD_SWITCH_REMINDER.to_owned());
    }
    if agent == Some(agent::BUILD)
        && let Some(cell) = pending
    {
        let mut pending = cell.lock().expect("the pending switch is never poisoned");
        if *pending == PendingSwitch::SentencePending {
            *pending = PendingSwitch::None;
            found.push(APPROVAL_SENTENCE.to_owned());
        }
    }

    found
}

/// What the model is told about files that changed underneath it, before the
/// list of them.
const STALE_FILES: &str = "The following files changed on disk after they were read in this \
                           session; re-read them before relying on their contents:";

/// The one synthetic user part naming `stale`, or [`None`] when nothing went
/// stale.
///
/// A reminder like the two above and carried the same way: it belongs to the
/// request and not to the transcript, because it is about the state the
/// filesystem is in right now and a stored copy would be telling some later
/// turn about a file that has long since been re-read.
///
/// Paths are project-relative, as every other path the model is shown is: what
/// it does with the answer is call `read` with it.
fn stale_notice(stale: &[PathBuf], root: &Path) -> Option<String> {
    if stale.is_empty() {
        return None;
    }

    let mut notice = String::from(STALE_FILES);
    for path in stale {
        notice.push_str("\n- ");
        notice.push_str(
            &path
                .strip_prefix(root)
                .unwrap_or(path)
                .display()
                .to_string(),
        );
    }

    Some(notice)
}

/// A brand-new session record, already on disk by the time it is adopted,
/// carrying whatever the engine is set to run as.
///
/// The id arrives from the caller — the engine's current one — rather than
/// being minted here: the engine named its session at construction so that
/// even turn 1's events could carry the name, and the row created here has to
/// be the same session those events already named.
fn fresh_session(
    storage: &Storage,
    id: SessionId,
    agent: Option<String>,
    model: String,
    effort: Option<String>,
) -> SessionInfo {
    let created = now();
    let info = SessionInfo {
        id,
        version: storage::VERSION,
        title: None,
        created,
        updated: created,
        usage: Usage::default(),
        context_tokens: 0,
        summary: None,
        agent,
        model: Some(model),
        effort,
        activated_tools: std::collections::BTreeSet::new(),
        // A session a person started, not one a tool call delegated.
        parent: None,
        // Nothing has been undone in a session that has not run a turn.
        revert: None,
    };

    if let Err(error) = storage.save_info(&info) {
        tracing::warn!(
            session = info.id.as_str(),
            %error,
            "could not create the session on disk; the conversation continues in memory"
        );
    }

    info
}

/// Closes every tool part a crash left `Pending` or `Running`, in the loaded
/// transcript and on disk. The stored input is kept — it is what the call was
/// going to run with — but both timestamps are the load's, per the P4
/// contract: nothing here pretends to know when the old process died.
///
/// An assistant envelope whose `time.completed` is absent is left exactly as
/// found: that absence is the abort marker a frontend renders, and inventing
/// parts for it would put words in a dead process's mouth.
fn close_interrupted(storage: &Storage, session: &SessionId, transcript: &mut [Message]) {
    for message in transcript.iter_mut() {
        let message_id = message.id.clone();
        for part in &mut message.parts {
            let PartBody::Tool { state, .. } = &mut part.body else {
                continue;
            };
            let input = match state {
                ToolState::Completed { .. } | ToolState::Error { .. } => continue,
                ToolState::Running { input, .. } => input.clone(),
                ToolState::Pending { input } => {
                    input.clone().unwrap_or_else(|| serde_json::json!({}))
                }
            };

            let stamp = now();
            *state = ToolState::Error {
                input,
                error: INTERRUPTED.to_owned(),
                started: stamp,
                completed: stamp,
            };

            // The closure must outlive this process too — the next request,
            // whenever it happens, has to answer this call. A store that
            // refuses re-closes on the next resume.
            if let Err(error) = storage.save_part(session, &message_id, part) {
                tracing::warn!(
                    session = session.as_str(),
                    part = part.id.as_str(),
                    %error,
                    "could not persist an interrupted call's closure"
                );
            }
        }
    }
}

/// One parity hold's deadline (**D524**): sleeps to it, then expires the hold
/// — and loses gracefully to any settlement that got there first, because
/// [`teammate::inbound::Inbound::expire`] answers [`None`] for a claimed or
/// gone id.
///
/// Weak on both handles so a pending timer keeps neither the gate nor the
/// team alive past the engine; a deadline that fires after either is gone has
/// nothing left to expire.
async fn expire_hold(
    wait_ms: u64,
    id: crate::protocol::HeldId,
    inbound: std::sync::Weak<teammate::inbound::Inbound>,
    teammates: Option<std::sync::Weak<subagent::Teammates>>,
) {
    tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
    let Some(inbound) = inbound.upgrade() else {
        return;
    };
    let Some(settlement) = inbound.expire(&id) else {
        // Settled, or claimed by a drop whose prune is in flight: the race's
        // loser no-ops, first-settler-wins.
        return;
    };
    let teammates = teammates.as_ref().and_then(std::sync::Weak::upgrade);

    settle_side_effects(&inbound, teammates.as_ref(), &id, settlement).await;
}

/// What one settlement decision requires of the engine, performed after the
/// gate's lock is long gone (**D524**): nothing for a settled drop, the
/// release write for a socket-door delivery (M6's identity recorded after
/// the write that minted it), and H2's prune-before-unindex for a
/// mailbox-door drop — a failed prune re-holds, fail-closed and retryable,
/// never delivers.
async fn settle_side_effects(
    inbound: &Arc<teammate::inbound::Inbound>,
    teammates: Option<&Arc<subagent::Teammates>>,
    id: &crate::protocol::HeldId,
    settlement: teammate::inbound::Settlement,
) {
    match settlement {
        teammate::inbound::Settlement::Done(outcome) => {
            tracing::info!(id = id.as_str(), ?outcome, "a held peer message settled");
        }
        teammate::inbound::Settlement::Deliver(released) => {
            let Some(team) = teammates else {
                // The team is gone, so there is no inbox to release into; the
                // record is already settled, and the message goes with it —
                // the same end shutdown gives a socket-door hold.
                tracing::warn!(
                    id = id.as_str(),
                    "a released peer message had no team left to deliver into"
                );

                return;
            };
            let registry = team.registry();
            let message = subagent::PeerMessage {
                from: released.from,
                text: released.text,
                summary: released.summary,
                lead: registry.lead().as_str().to_owned(),
            };
            match subagent::deliver_to_lead(registry, message).await {
                Ok((_sent, identity)) => {
                    tracing::info!(id = id.as_str(), "a held peer message was released");
                    inbound.admit_identity(identity);
                }
                // The write's failure is the delivery path's own failure
                // channel, as it is for any peer write: the record stands
                // settled `delivered`, and the bytes are lost with the write.
                Err(error) => tracing::warn!(
                    id = id.as_str(),
                    %error,
                    "a released peer message could not be written"
                ),
            }
        }
        teammate::inbound::Settlement::PruneFirst { identity } => {
            let Some(team) = teammates else {
                // No team means no inbox to prune; the claim clears so the
                // record re-holds rather than wedging half-settled (H2).
                inbound.prune_failed(id);

                return;
            };
            if prune_lead_inboxes(team.registry(), &identity).await {
                if let Some(outcome) = inbound.pruned(id) {
                    tracing::info!(id = id.as_str(), ?outcome, "a held peer message settled");
                }
            } else {
                inbound.prune_failed(id);
                tracing::warn!(
                    id = id.as_str(),
                    "a held entry's prune failed; the hold stands, retryable (H2)"
                );
            }
        }
    }
}

/// Prunes one identity from every lead inbox this session reads — its own
/// root always, and a real `claude`'s root exactly when the roster holds a
/// claude-backed member, the same two-roots rule the §6.2 pass reads under
/// ([`teammate::lead_inbox::LeadInbox`]'s own doc owns why the second root
/// is conditional).
///
/// `true` only when every prune landed: pruning an identity from an inbox
/// that never held it is a rewrite that changes nothing, so the conjunction
/// is safe, and H2 needs the failure reported rather than shrugged at.
async fn prune_lead_inboxes(
    registry: &Arc<teammate::TeammateRegistry>,
    identity: &ganja_team::mailbox::Identity,
) -> bool {
    let own = registry.root().clone();
    let mut roots = vec![own.clone()];
    if registry.holds_backend(ganja_protocol::team::MemberBackend::Claude)
        && let Some(claude) = teammate::claude::teams_root()
        && claude != own
    {
        roots.push(claude);
    }

    let mut pruned = true;
    for root in roots {
        let path = root.inbox_path(registry.team(), registry.lead());
        let identities = vec![identity.clone()];
        if let Err(reason) =
            teammate::blocking_io(move || ganja_team::mailbox::prune_delivered(&path, &identities))
                .await
        {
            tracing::warn!(?identity, reason, "a held entry's inbox prune failed");
            pruned = false;
        }
    }

    pruned
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use futures::{
        StreamExt as _,
        stream::{self, BoxStream},
    };
    use tokio_util::sync::CancellationToken;

    use super::{
        Engine, EngineError, STALE_FILES, message_chars, send_message, stale_notice, teammate,
    };
    use crate::{
        config::TeamlessSend,
        permission::Permissions,
        protocol::{
            Command, Event, FinishReason, Message, Part, PermissionReply, RevertScope, Role, Usage,
        },
        provider::{
            ChatRequest, FakeProvider, Provider, ProviderError, ProviderEvent, fake::MODEL,
        },
        storage::{self, SessionId, SessionInfo, Storage},
        tool::{FileTimes, Registry},
    };

    /// How long a drain that should complete promptly is given before the
    /// test calls it wedged. Generous against a loaded machine, and reached
    /// only when delivery is broken — a green run never waits on it.
    const DRAIN_PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);

    /// An engine over `provider` with no tools and default rules, which is
    /// all these tests need: they prove the turn lifecycle, not the loop.
    fn bare(provider: Arc<dyn Provider>, model: &str) -> Engine {
        Engine::new(
            provider,
            model,
            Arc::new(Registry::new(Vec::new())),
            Permissions::default(),
        )
    }

    fn engine() -> Engine {
        bare(
            Arc::new(FakeProvider::new(
                "one two",
                std::time::Duration::from_millis(1),
            )),
            MODEL,
        )
    }

    /// Records what it was asked and answers with a scripted stream.
    struct ScriptedProvider {
        events: Vec<ProviderEvent>,
        failure: Option<ProviderError>,
        seen: Arc<Mutex<Vec<ChatRequest>>>,
    }

    impl ScriptedProvider {
        fn new(events: Vec<ProviderEvent>) -> Self {
            Self {
                events,
                failure: None,
                seen: Arc::default(),
            }
        }

        fn failing(failure: ProviderError) -> Self {
            Self {
                events: Vec::new(),
                failure: Some(failure),
                seen: Arc::default(),
            }
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        fn id(&self) -> &str {
            "scripted"
        }

        async fn stream(
            &self,
            request: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            self.seen
                .lock()
                .expect("the request log is never poisoned")
                .push(request);

            match &self.failure {
                Some(failure) => Err(failure.clone()),
                None => Ok(stream::iter(self.events.clone()).boxed()),
            }
        }
    }

    /// Drains events until the turn finishes, returning everything seen.
    async fn drain(events: &mut BoxStream<'static, Event>) -> Vec<Event> {
        let mut seen = Vec::new();

        loop {
            let Some(event) = events.next().await else {
                return seen;
            };
            let finished = matches!(event, Event::MessageFinished { .. });
            seen.push(event);

            if finished {
                return seen;
            }
        }
    }

    /// The text a transcript rebuilt from `events` alone would show.
    fn replay(events: &[Event]) -> String {
        let mut messages: Vec<Message> = Vec::new();

        for event in events {
            match event {
                Event::MessageStarted {
                    session_id: _,
                    message,
                } => messages.push(message.clone()),
                Event::PartStarted {
                    session_id: _,
                    message_id,
                    part,
                } => {
                    if let Some(message) = messages.iter_mut().find(|it| it.id == *message_id) {
                        message.parts.push(part.clone());
                    }
                }
                Event::PartDelta {
                    session_id: _,
                    message_id,
                    part_id,
                    delta,
                } => {
                    if let Some(text) = messages
                        .iter_mut()
                        .find(|it| it.id == *message_id)
                        .and_then(|message| message.parts.iter_mut().find(|it| it.id == *part_id))
                        .and_then(crate::protocol::Part::as_text_mut)
                    {
                        text.push_str(delta);
                    }
                }
                Event::MessageFinished { .. }
                | Event::PartUpdated { .. }
                | Event::PermissionRequested { .. }
                | Event::SteerConsumed { .. }
                | Event::PermissionReplied { .. }
                | Event::QuestionAsked { .. }
                | Event::QuestionReplied { .. }
                | Event::QuestionRejected { .. }
                | Event::RevertChanged { .. }
                | Event::AgentChanged { .. }
                | Event::PermissionModeChanged { .. }
                | Event::CompactionProgress { .. }
                // No replayed text, permanently: a hold's whole point is
                // that nothing reached the transcript, and even a released
                // message arrives as a peer part on the steer lane — which
                // `Part::as_text` excludes — never as replayed text (D524).
                | Event::PeerHeld { .. }
                | Event::PeerHoldSettled { .. }
                | Event::EffortChanged { .. } => {}
            }
        }

        messages
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(crate::protocol::Part::as_text)
            .collect()
    }

    #[tokio::test]
    async fn a_turn_reports_both_messages_and_streams_the_reply_into_one_part() {
        let engine = engine();
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        engine
            .send(Command::SendPrompt {
                text: "hi".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("an idle engine accepts a prompt");

        let seen = drain(&mut events).await;

        let Some(Event::MessageStarted {
            session_id: _,
            message: user,
        }) = seen.first()
        else {
            panic!("a turn should open with the user's message, got {seen:?}");
        };
        assert_eq!(user.role, Role::User);
        assert_eq!(
            user.parts.first().and_then(|part| part.as_text()),
            Some("hi")
        );

        let Some(Event::MessageStarted {
            session_id: _,
            message: assistant,
        }) = seen.get(1)
        else {
            panic!("the reply's envelope should follow, got {seen:?}");
        };
        assert_eq!(assistant.role, Role::Assistant);
        assert_eq!(assistant.model.as_deref(), Some(MODEL));
        assert!(assistant.parts.is_empty(), "the reply starts empty");

        assert_eq!(
            seen.iter()
                .filter(|event| matches!(
                    event,
                    Event::PartStarted { part, .. } if part.as_text().is_some()
                ))
                .count(),
            1,
            "streamed text belongs to one part, got {seen:?}"
        );
        assert_eq!(replay(&seen), "hione two");

        let Some(Event::MessageFinished {
            session_id: _,
            message_id,
            reason,
            usage,
            error,
            completed,
        }) = seen.last()
        else {
            panic!("a turn always ends with a finish, got {seen:?}");
        };
        assert_eq!(*message_id, assistant.id);
        assert_eq!(*reason, FinishReason::Completed);
        assert_eq!(
            *usage,
            Some(Usage {
                input_tokens: 1,
                output_tokens: 2,
                ..Usage::default()
            })
        );
        assert!(error.is_none());
        assert!(*completed >= assistant.time.created);
    }

    #[tokio::test]
    async fn a_second_turn_carries_the_first_one_in_its_request() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            ProviderEvent::TextDelta("sure".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ]));
        let seen = Arc::clone(&provider.seen);
        let engine = bare(provider, "scripted-model");
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        for prompt in ["first", "second"] {
            engine
                .send(Command::SendPrompt {
                    text: prompt.to_owned(),
                    mentions: Vec::new(),
                    skills: Vec::new(),
                    session_mentions: Vec::new(),
                    peers: Vec::new(),
                })
                .await
                .expect("an idle engine accepts a prompt");
            drain(&mut events).await;
        }

        let requests = seen.lock().expect("the request log is never poisoned");
        let first = requests.first().expect("the first turn asked the provider");
        assert_eq!(first.model, "scripted-model");
        assert!(
            first.system.is_none(),
            "an engine nobody configured asks without a system prompt"
        );
        assert_eq!(first.messages.len(), 1, "the first turn has no history");

        let second = requests.get(1).expect("the second turn asked too");
        let transcript: Vec<(&str, Option<&str>)> = second
            .messages
            .iter()
            .map(|message| {
                (
                    message.model.as_deref().unwrap_or("user"),
                    // The first text part: an assistant message now opens
                    // with a step marker before anything it said.
                    message
                        .parts
                        .iter()
                        .find_map(crate::protocol::Part::as_text),
                )
            })
            .collect();
        assert_eq!(
            transcript,
            vec![
                ("user", Some("first")),
                ("scripted-model", Some("sure")),
                ("user", Some("second")),
            ],
            "the second turn should carry the first one"
        );
    }

    #[tokio::test]
    async fn a_provider_that_cannot_answer_still_finishes_the_turn() {
        let engine = bare(
            Arc::new(ScriptedProvider::failing(ProviderError::Auth(
                "ANTHROPIC_API_KEY is unset".to_owned(),
            ))),
            "scripted-model",
        );
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        engine
            .send(Command::SendPrompt {
                text: "hi".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("an idle engine accepts a prompt");

        let seen = drain(&mut events).await;
        let Some(Event::MessageFinished { reason, error, .. }) = seen.last() else {
            panic!("a failed turn still finishes, got {seen:?}");
        };

        assert_eq!(*reason, FinishReason::Failed);
        assert!(
            error
                .as_deref()
                .is_some_and(|error| error.contains("ANTHROPIC_API_KEY")),
            "the refusal should explain itself, got {error:?}"
        );

        engine
            .send(Command::SendPrompt {
                text: "again".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("a failed turn leaves the engine idle");
    }

    #[tokio::test]
    async fn a_failed_turn_is_not_kept_as_context() {
        let provider = Arc::new(ScriptedProvider::failing(ProviderError::Transport(
            "connection reset".to_owned(),
        )));
        let seen = Arc::clone(&provider.seen);
        let engine = bare(provider, "scripted-model");
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        for prompt in ["first", "second"] {
            engine
                .send(Command::SendPrompt {
                    text: prompt.to_owned(),
                    mentions: Vec::new(),
                    skills: Vec::new(),
                    session_mentions: Vec::new(),
                    peers: Vec::new(),
                })
                .await
                .expect("an idle engine accepts a prompt");
            drain(&mut events).await;
        }

        let requests = seen.lock().expect("the request log is never poisoned");
        let second = requests.get(1).expect("the second turn asked too");
        assert_eq!(
            second.messages.len(),
            2,
            "an empty reply should not enter the history, got {:?}",
            second.messages
        );
    }

    /// Every request a turn makes carries the configured prompt — including
    /// the one that summarizes the conversation for compaction, which is what
    /// keeps a compacted session from being summarized under instructions the
    /// rest of it was never held under.
    #[tokio::test]
    async fn a_configured_system_prompt_reaches_the_agent_and_the_summarize_requests() {
        const SYSTEM: &str = "you are a canary";

        let provider = Arc::new(ScriptedProvider::new(vec![
            ProviderEvent::TextDelta("sure".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ]));
        let seen = Arc::clone(&provider.seen);

        // A model the catalog knows, and a session already at its ceiling, so
        // the next turn compacts before it asks anything.
        let model = crate::catalog::default_model("anthropic")
            .expect("the catalog has a default for a provider this build ships");
        let window = crate::catalog::model(model)
            .expect("the default model is in the catalog")
            .context_window;

        let directory = tempfile::tempdir().expect("a temporary directory");
        let storage = Storage::open(directory.path().join("storage"));
        let session = SessionId::ascending();
        let info = SessionInfo {
            effort: None,
            id: session.clone(),
            version: storage::VERSION,
            // Pre-titled, so the title machinery stays out of a test that is
            // not about it and cannot spend a request of its own.
            title: Some("seeded".to_owned()),
            created: 1,
            updated: 2,
            usage: Usage::default(),
            context_tokens: window,
            summary: None,
            agent: None,
            model: None,
            activated_tools: std::collections::BTreeSet::new(),
            parent: None,
            revert: None,
        };
        storage.save_info(&info).expect("the seeded record writes");
        let earlier = Message::user("the objective");
        storage
            .save_message(&session, &earlier)
            .expect("the seeded envelope writes");
        for part in &earlier.parts {
            storage
                .save_part(&session, &earlier.id, part)
                .expect("the seeded part writes");
        }

        let engine = Engine::persistent(
            provider,
            model,
            Arc::new(Registry::new(Vec::new())),
            Permissions::default(),
            storage,
        )
        .with_system_parts(Some(SYSTEM.to_owned()), None);
        let mut events = engine.subscribe().await.expect("the first subscriber wins");
        engine.resume(&session).await.expect("the session loads");

        engine
            .send(Command::SendPrompt {
                text: "next".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("an idle engine accepts a prompt");
        drain(&mut events).await;

        let requests = seen.lock().expect("the request log is never poisoned");
        assert_eq!(
            requests.len(),
            2,
            "a compacting turn asks twice: summarize, then the model itself"
        );
        assert!(
            requests[0].tools.is_empty(),
            "the summarize request is the toolless one, got {:?}",
            requests[0]
        );
        for request in requests.iter() {
            assert_eq!(request.system.as_deref(), Some(SYSTEM));
        }
    }

    /// The status bar's context meter polls this the way it polls `jobs()`:
    /// the estimate is the stored measure compaction reads, and the window is
    /// the catalog's — both visible without a turn in flight (**D469**).
    #[tokio::test]
    async fn the_context_estimate_reports_the_stored_measure_against_the_catalog_window() {
        let model = crate::catalog::default_model("anthropic")
            .expect("the catalog has a default for a provider this build ships");
        let window = crate::catalog::model(model)
            .expect("the default model is in the catalog")
            .context_window;

        let directory = tempfile::tempdir().expect("a temporary directory");
        let storage = Storage::open(directory.path().join("storage"));
        let session = SessionId::ascending();
        let info = SessionInfo {
            id: session.clone(),
            version: storage::VERSION,
            title: Some("seeded".to_owned()),
            created: 1,
            updated: 2,
            usage: Usage::default(),
            context_tokens: 1_234,
            summary: None,
            agent: None,
            model: None,
            effort: None,
            activated_tools: std::collections::BTreeSet::new(),
            parent: None,
            revert: None,
        };
        storage.save_info(&info).expect("the seeded record writes");

        let engine = Engine::persistent(
            Arc::new(FakeProvider::new("ok", std::time::Duration::from_millis(1))),
            model,
            Arc::new(Registry::new(Vec::new())),
            Permissions::default(),
            storage,
        );

        let before = engine.context_estimate();
        assert_eq!(
            before.tokens, 0,
            "before a resume there is no session to have measured anything"
        );
        assert_eq!(before.window, Some(window));

        engine.resume(&session).await.expect("the session loads");

        let after = engine.context_estimate();
        assert_eq!(
            after.tokens, 1_234,
            "the stored measure is what the bar shows"
        );
        assert_eq!(after.window, Some(window));
    }

    /// A model the catalog does not know has no window to report — the same
    /// honest absence that keeps such a session from ever auto-compacting.
    #[tokio::test]
    async fn the_context_estimate_has_no_window_for_an_uncataloged_model() {
        let estimate = engine().context_estimate();

        assert_eq!(estimate.tokens, 0, "an ephemeral engine stores no measure");
        assert_eq!(
            estimate.window, None,
            "only the catalog can size a window, and it does not know the fake model"
        );
    }

    /// An engine with something in every fixed category, for the breakdown
    /// tests: a base prompt, a suffix carrying an instruction file and a
    /// skills block spelled with the composer's own markers, and the builtin
    /// tools.
    fn furnished(model: &str) -> Engine {
        let suffix = "You are powered by the model named fake.\n<env>\n  Working directory: /\n</env>\
                      \nInstructions from: /project/AGENTS.md\nalways run the tests\
                      \nSkills provide specialized instructions and workflows for specific tasks.\n<available_skills>\n</available_skills>";

        Engine::new(
            Arc::new(FakeProvider::new(
                "one two",
                std::time::Duration::from_millis(1),
            )),
            model,
            Arc::new(Registry::with_builtins()),
            Permissions::default(),
        )
        .with_system_parts(Some("obey the tests".to_owned()), Some(suffix.to_owned()))
    }

    /// The grid's contract: the legend can only add up to the panel's total
    /// because the accessor's categories add up to its own.
    #[tokio::test]
    async fn the_breakdown_categories_sum_to_the_total() {
        let breakdown = furnished(MODEL).context_breakdown().await;

        let summed = breakdown.system_prompt
            + breakdown.instructions
            + breakdown.tools_builtin
            + breakdown.tools_mcp
            + breakdown.skills
            + breakdown.conversation_user
            + breakdown.conversation_assistant;
        assert!(summed > 0, "the furnished engine fills categories");
        assert_eq!(summed, breakdown.total());
    }

    /// The counts ride the same walk that priced the tools, and the model id
    /// is the engine's own: with only the builtins registered, the builtin
    /// count is exactly the registry's roster and the MCP count is zero.
    #[tokio::test]
    async fn the_breakdown_counts_the_tools_the_same_walk_priced() {
        let breakdown = furnished(MODEL).context_breakdown().await;

        assert_eq!(
            breakdown.tools_builtin_count,
            Registry::with_builtins().definitions().len(),
            "every builtin the registry serves is counted once"
        );
        assert_eq!(breakdown.tools_mcp_count, 0, "no server is connected");
        assert_eq!(breakdown.model, MODEL, "the id the engine runs under");
    }

    /// AC4 undisturbed: the counts are metadata for the panel's detail
    /// sections, so two breakdowns that differ only in them agree on every
    /// token figure — the total and the free space sum nothing from a count.
    #[test]
    fn the_counts_are_metadata_and_move_no_token_figure() {
        use super::ContextBreakdown;

        let bare = ContextBreakdown {
            system_prompt: 1_000,
            tools_builtin: 2_000,
            tools_mcp: 500,
            window: Some(10_000),
            reserve: Some(1_000),
            ..ContextBreakdown::default()
        };
        let counted = ContextBreakdown {
            tools_builtin_count: 12,
            tools_mcp_count: 193,
            ..bare.clone()
        };

        assert_eq!(bare.total(), counted.total());
        assert_eq!(bare.free(), counted.free());
    }

    /// The free-space row is window − used − reserve, read off the exposed
    /// reserve rather than re-derived from the compaction trigger.
    #[tokio::test]
    async fn free_space_is_the_window_minus_the_total_minus_the_reserve() {
        let model = crate::catalog::default_model("anthropic")
            .expect("the catalog has a default for a provider this build ships");
        let window = crate::catalog::model(model)
            .expect("the default model is in the catalog")
            .context_window;

        let engine = furnished(model);
        let mut events = engine.subscribe().await.expect("the first subscriber wins");
        engine
            .send(Command::SendPrompt {
                text: "fill the conversation a little".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("an idle engine accepts a prompt");
        drain(&mut events).await;

        let breakdown = engine.context_breakdown().await;
        assert_eq!(breakdown.window, Some(window));
        let reserve = breakdown.reserve.expect("a sized window has a reserve");
        assert!(reserve > 0, "the trigger holds a tenth back");
        assert_eq!(
            breakdown.free(),
            Some(window - breakdown.total() - reserve),
            "free space is what the window has left after the load and the reserve"
        );
    }

    /// Review changelog MAJOR 3's whole point: `/context` on a session that
    /// has said nothing must still show what the first request would carry —
    /// the fixed shares are computed on demand, not stashed by a turn that
    /// never ran.
    #[tokio::test]
    async fn a_fresh_session_reports_system_and_tool_shares_and_no_conversation() {
        let breakdown = furnished(MODEL).context_breakdown().await;

        assert_eq!(breakdown.conversation_user, 0);
        assert_eq!(breakdown.conversation_assistant, 0);
        assert!(breakdown.system_prompt > 0, "{breakdown:?}");
        assert!(breakdown.instructions > 0, "{breakdown:?}");
        assert!(breakdown.skills > 0, "{breakdown:?}");
        assert!(breakdown.tools_builtin > 0, "{breakdown:?}");
        assert_eq!(breakdown.tools_mcp, 0, "no server is connected");
    }

    /// A standing conversation revert hides the anchor and everything after
    /// it from the *next* request — `truncate_reverted` runs at the next
    /// prompt — so a breakdown read in between must already leave those
    /// messages out.
    #[tokio::test]
    async fn a_breakdown_right_after_a_revert_reflects_the_truncated_conversation() {
        let engine = engine();
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        engine
            .send(Command::SendPrompt {
                text: "the first prompt".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("an idle engine accepts a prompt");
        drain(&mut events).await;
        let after_one_turn = engine.context_breakdown().await;

        engine
            .send(Command::SendPrompt {
                text: "the second prompt, which the revert takes back".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("an idle engine accepts a prompt");
        let second_turn = drain(&mut events).await;
        let anchor = second_turn
            .iter()
            .find_map(|event| match event {
                Event::MessageStarted { message, .. } if message.role == Role::User => {
                    Some(message.id.clone())
                }
                _ => None,
            })
            .expect("the second turn opened with its user message");

        let after_two_turns = engine.context_breakdown().await;
        assert!(
            after_two_turns.conversation_user > after_one_turn.conversation_user,
            "the second turn grew the conversation"
        );

        engine
            .send(Command::RevertTo {
                message_id: anchor,
                scope: RevertScope::Conversation,
            })
            .await
            .expect("a checkpoint that exists is revertable");

        let after_revert = engine.context_breakdown().await;
        assert_eq!(
            (
                after_revert.conversation_user,
                after_revert.conversation_assistant
            ),
            (
                after_one_turn.conversation_user,
                after_one_turn.conversation_assistant
            ),
            "what the revert hid is already left out"
        );
    }

    /// The same honest absence `context_estimate` reports: no catalog row, no
    /// window, no reserve, no free-space figure — the dialog's degraded panel.
    #[tokio::test]
    async fn the_breakdown_has_no_window_for_an_uncataloged_model() {
        let breakdown = furnished(MODEL).context_breakdown().await;

        assert_eq!(breakdown.window, None);
        assert_eq!(breakdown.reserve, None);
        assert_eq!(breakdown.free(), None);
    }

    /// AC4's one-estimator claim, spelled honestly: `context_estimate` reads
    /// the stored measure a finished request stamped — an *actual*, which no
    /// on-demand estimate can be asserted equal to — so what "one estimator"
    /// means, and what this pins, is the **convention**: the breakdown prices
    /// characters exactly as the compaction fit guard does, four to a token,
    /// and never through a second tokenizer.
    #[tokio::test]
    async fn the_breakdown_prices_by_the_compaction_estimators_own_convention() {
        let engine = engine();
        let mut events = engine.subscribe().await.expect("the first subscriber wins");
        engine
            .send(Command::SendPrompt {
                text: "x".repeat(400),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("an idle engine accepts a prompt");
        drain(&mut events).await;

        let breakdown = engine.context_breakdown().await;
        assert_eq!(
            breakdown.conversation_user,
            crate::session::estimate_tokens(400),
            "four hundred characters are a hundred tokens under the shared convention"
        );
    }

    /// The successor to `a_second_subscriber_is_refused`, asserting the
    /// contract that replaced the refusal: every subscriber has a queue of
    /// its own, so a second one registered before the turn holds the same
    /// transcript the first does, frame for frame.
    #[tokio::test]
    async fn a_second_subscriber_sees_the_same_events_the_first_does() {
        let engine = engine();
        let mut first = engine
            .subscribe()
            .await
            .expect("the first subscriber claims the birth queue");
        let mut second = engine
            .subscribe()
            .await
            .expect("a later subscriber registers a queue of its own");

        engine
            .send(Command::SendPrompt {
                text: "hi".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("an idle engine accepts a prompt");

        // Bounded so a delivery that forgot one of the queues fails loudly
        // instead of waiting forever on a stream nothing feeds.
        let heard_first = tokio::time::timeout(DRAIN_PATIENCE, drain(&mut first))
            .await
            .expect("the first subscriber hears the whole turn");
        let heard_second = tokio::time::timeout(DRAIN_PATIENCE, drain(&mut second))
            .await
            .expect("the second subscriber hears the whole turn");

        assert!(
            matches!(heard_first.last(), Some(Event::MessageFinished { .. })),
            "a drained turn ends with its finish: {heard_first:?}"
        );
        assert_eq!(
            heard_first, heard_second,
            "two lossless subscribers of one turn hold the same transcript"
        );
    }

    /// Every event is addressed: it names the engine's current session, which
    /// has a name even on an engine that stores nothing.
    #[tokio::test]
    async fn every_event_of_a_turn_carries_the_engines_session_id() {
        let engine = engine();
        let mut events = engine
            .subscribe()
            .await
            .expect("the first subscriber claims the birth queue");

        let session = engine.session_id();
        assert!(
            crate::protocol::is_uuidv7(session.as_str()),
            "an ephemeral engine's session id is a bare UUIDv7 now that the \
             `ses_` prefix is gone: {session:?}"
        );

        engine
            .send(Command::SendPrompt {
                text: "hi".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("an idle engine accepts a prompt");
        let seen = drain(&mut events).await;

        assert!(!seen.is_empty(), "a turn reports something");
        for event in &seen {
            assert_eq!(
                event.session_id(),
                &session,
                "every event of the turn names the engine's session: {event:?}"
            );
        }
    }

    /// `Command::NewSession` renames the engine before anything can be said
    /// in the next conversation. Left stale, the second conversation's lazy
    /// create would adopt the first one's id and `save_info` would upsert
    /// over its row — so the pin is that two conversations on one persistent
    /// engine store two distinct sessions, each addressed as itself.
    #[tokio::test]
    async fn two_conversations_on_one_engine_store_two_distinct_sessions() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let engine = Engine::persistent(
            Arc::new(FakeProvider::new(
                "one two",
                std::time::Duration::from_millis(1),
            )),
            MODEL,
            Arc::new(Registry::new(Vec::new())),
            Permissions::default(),
            Storage::open(directory.path().join("storage")),
        );
        let mut events = engine.subscribe().await.expect("the first subscriber");

        engine
            .send(Command::SendPrompt {
                text: "first".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("an idle engine accepts a prompt");
        let first_turn = drain(&mut events).await;
        let first = engine
            .current_session()
            .expect("the first prompt created a session");
        assert_eq!(
            engine.session_id(),
            first.id,
            "the stored row adopted the id the engine was already using"
        );
        assert!(
            first_turn
                .iter()
                .all(|event| event.session_id() == &first.id),
            "the first conversation's events name its session: {first_turn:?}"
        );

        engine
            .send(Command::NewSession)
            .await
            .expect("an idle engine forgets its session");
        engine
            .send(Command::SendPrompt {
                text: "second".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("a fresh conversation accepts a prompt");
        let second_turn = drain(&mut events).await;
        let second = engine
            .current_session()
            .expect("the second prompt created a session");

        assert_ne!(first.id, second.id, "a new conversation is a new session");
        assert!(
            second_turn
                .iter()
                .all(|event| event.session_id() == &second.id),
            "the second conversation's events name its own session: {second_turn:?}"
        );

        let stored = engine.sessions().await.expect("the store lists");
        let ids: Vec<&SessionId> = stored.iter().map(|info| &info.id).collect();
        assert_eq!(stored.len(), 2, "two conversations, two rows: {ids:?}");
        assert!(
            ids.contains(&&first.id) && ids.contains(&&second.id),
            "and they are exactly the two the engine was on: {ids:?}"
        );
    }

    #[tokio::test]
    async fn a_prompt_sent_mid_turn_is_refused() {
        let engine = engine();
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        engine
            .send(Command::SendPrompt {
                text: "first".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("an idle engine accepts a prompt");
        assert!(matches!(
            events.next().await,
            Some(Event::MessageStarted { .. })
        ));

        assert!(matches!(
            engine
                .send(Command::SendPrompt {
                    text: "second".to_owned(),
                    mentions: Vec::new(),
                    skills: Vec::new(),
                    session_mentions: Vec::new(),
                    peers: Vec::new(),
                })
                .await,
            Err(EngineError::Busy)
        ));
    }

    /// **D119.** Upstream aborts the running session and then reverts; here
    /// the person at the terminal cancels first, so an undo is never something
    /// that stopped work they were watching. Refused before anything else is
    /// even looked at, which is why an engine with no snapshots still answers
    /// `Busy` here rather than `NoSnapshots`.
    #[tokio::test]
    async fn an_undo_during_a_turn_is_refused_rather_than_stopping_it() {
        let engine = engine();
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        engine
            .send(Command::SendPrompt {
                text: "first".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("an idle engine accepts a prompt");
        assert!(matches!(
            events.next().await,
            Some(Event::MessageStarted { .. })
        ));

        assert!(matches!(
            engine.send(Command::Undo).await,
            Err(EngineError::Busy)
        ));
        assert!(matches!(
            engine.send(Command::Redo).await,
            Err(EngineError::Busy)
        ));
    }

    /// An engine that takes no snapshots says so rather than moving the
    /// transcript: an undo that hid the messages and left every file where it
    /// was would be an undo that only half happened, and nothing afterwards
    /// could tell.
    #[tokio::test]
    async fn an_undo_without_snapshots_refuses_instead_of_half_happening() {
        let engine = engine();
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        engine
            .send(Command::SendPrompt {
                text: "first".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("an idle engine accepts a prompt");
        drain(&mut events).await;

        assert!(matches!(
            engine.send(Command::Undo).await,
            Err(EngineError::NoSnapshots)
        ));
        assert_eq!(
            engine.history.lock().await.len(),
            2,
            "a refused undo leaves the conversation exactly as it was"
        );
    }

    #[tokio::test]
    async fn the_engine_accepts_a_prompt_again_once_the_turn_finished() {
        let engine = engine();
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        engine
            .send(Command::SendPrompt {
                text: "first".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("an idle engine accepts a prompt");

        drain(&mut events).await;

        engine
            .send(Command::SendPrompt {
                text: "second".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("a finished turn leaves the engine idle");
    }

    #[tokio::test]
    async fn cancelling_while_idle_does_nothing() {
        let engine = engine();
        let _events = engine.subscribe().await.expect("the first subscriber wins");

        engine
            .send(Command::CancelTurn)
            .await
            .expect("an idle cancel is a no-op");
    }

    /// The context meter's half of the display-only invariant (bead `pwe`).
    ///
    /// This one is not about what is *sent* but about what is *claimed to be
    /// sent*: the meter reports how full the window is, and counting thinking
    /// nothing carries would have it fill with words the model never receives
    /// — a session told to compact by a measure of its own scratch paper. The
    /// sealed half beside it *is* counted, because that one really does ride
    /// the next request.
    #[test]
    fn readable_thinking_counts_nothing_toward_a_window_it_never_reaches() {
        let mut assistant = Message::assistant("claude-test");
        assistant.parts.push(Part::text("Hello!"));
        let (said, results) = message_chars(&assistant);

        assistant
            .parts
            .push(Part::reasoning_text("a".repeat(10_000)));
        assert_eq!(
            message_chars(&assistant),
            (said, results),
            "ten thousand characters of thinking moved the meter; nothing \
             sends them"
        );

        // And the contrast that keeps this from passing by measuring nothing:
        // sealed state is handed back, so it counts.
        assistant
            .parts
            .push(Part::reasoning("openai", "rs_1", Some("b".repeat(64))));
        assert_eq!(
            message_chars(&assistant),
            (said + 64, results),
            "the sealed half rides the next request and has to be measured"
        );
    }

    #[test]
    fn the_stale_notice_names_its_files_the_way_the_model_would_ask_for_them() {
        let root = std::path::Path::new("/project");

        assert_eq!(stale_notice(&[], root), None, "nothing stale, nothing said");
        assert_eq!(
            stale_notice(
                &[
                    PathBuf::from("/project/src/main.rs"),
                    PathBuf::from("/project/README.md"),
                    // A file the session read outside the project has no
                    // relative form; naming it absolutely is what `read`
                    // would take back.
                    PathBuf::from("/etc/hosts"),
                ],
                root,
            )
            .as_deref(),
            Some(
                "The following files changed on disk after they were read in this session; \
                 re-read them before relying on their contents:\n\
                 - src/main.rs\n\
                 - README.md\n\
                 - /etc/hosts"
            )
        );
    }

    /// Marks `path` stale in `files` the way the watcher would: read, moved by
    /// somebody else, noticed.
    fn condemn(files: &FileTimes, path: &std::path::Path) {
        files.record(path);
        // Opened for writing because a stamp is metadata a handle must be
        // allowed to write: unix grants that with the file's own permissions,
        // Windows only through a handle that asked for write access.
        std::fs::File::options()
            .write(true)
            .open(path)
            .and_then(|file| file.set_modified(std::time::SystemTime::UNIX_EPOCH))
            .expect("the fixture can move the stamp");
        files.note_change(path);
    }

    /// The text parts of the last user message in `request` — where a
    /// reminder lands.
    fn last_user_text(request: &ChatRequest) -> Vec<&str> {
        request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .expect("a request carries the user's message")
            .parts
            .iter()
            .filter_map(crate::protocol::Part::as_text)
            .collect()
    }

    /// **AC-23**, end to end: a command carrying a teammate's message becomes
    /// a [`PartBody::Peer`] part on the user's own message, and reaches the
    /// wire as §5.3's envelope inside that turn's text (**D495**).
    ///
    /// The two halves are asserted in one test because each is worthless
    /// without the other: a part nothing renders is a message the model was
    /// never told, and an envelope nothing builds a part for is dead code.
    ///
    /// The prompt's text is empty on purpose — a delivery turn is a turn whose
    /// content *is* what the teammate said — which is also the case that pins
    /// the empty text part being dropped rather than sent as a blank block.
    #[tokio::test]
    async fn a_teammates_message_reaches_the_wire_as_the_envelope() {
        let provider = Arc::new(ScriptedProvider::new(vec![
            ProviderEvent::TextDelta("thanks".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ]));
        let seen = Arc::clone(&provider.seen);
        let engine = bare(provider, "scripted-model");
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        engine
            .send(Command::SendPrompt {
                text: String::new(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: vec![crate::protocol::team::PeerPayload::new(
                    "w1",
                    Some("picked up W2".to_owned()),
                    None,
                    "on the protocol",
                )],
            })
            .await
            .expect("an idle engine accepts a prompt");
        let announced = drain(&mut events).await;

        let Some(Event::MessageStarted { message: user, .. }) = announced.first() else {
            panic!("a turn opens with the user's message, got {announced:?}");
        };
        assert_eq!(
            user.parts.len(),
            1,
            "a delivery turn carries the teammate's words and no blank text part: {:?}",
            user.parts
        );
        assert!(
            matches!(
                &user.parts[0].body,
                crate::protocol::PartBody::Peer { from, body, .. }
                    if from == "w1" && body == "on the protocol"
            ),
            "the payload became the part that says whose words these are: {:?}",
            user.parts
        );

        let requests = seen.lock().expect("the request log is never poisoned");
        let carried: Vec<&str> = requests
            .first()
            .expect("the turn asked the provider")
            .messages
            .last()
            .expect("a request carries the user's message")
            .parts
            .iter()
            .filter_map(crate::protocol::Part::as_text)
            .collect();
        assert_eq!(
            carried,
            vec![
                "<teammate-message teammate_id=\"w1\" summary=\"picked up W2\">\n\
                 on the protocol\n\
                 </teammate-message>"
            ],
            "the wire carries the envelope and nothing else"
        );
    }

    #[tokio::test]
    async fn files_that_went_stale_are_named_to_the_model_once() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join("notes.md");
        std::fs::write(&path, "one").expect("the fixture writes");

        let provider = Arc::new(ScriptedProvider::new(vec![
            ProviderEvent::TextDelta("sure".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ]));
        let seen = Arc::clone(&provider.seen);
        let engine = bare(provider, "scripted-model");
        let mut events = engine.subscribe().await.expect("the first subscriber wins");
        condemn(&engine.files, &path);

        for prompt in ["first", "second"] {
            engine
                .send(Command::SendPrompt {
                    text: prompt.to_owned(),
                    mentions: Vec::new(),
                    skills: Vec::new(),
                    session_mentions: Vec::new(),
                    peers: Vec::new(),
                })
                .await
                .expect("an idle engine accepts a prompt");
            drain(&mut events).await;
        }

        let requests = seen.lock().expect("the request log is never poisoned");
        let first = last_user_text(requests.first().expect("the first turn asked"));
        assert_eq!(
            first.first(),
            Some(&"first"),
            "the user's own text comes first: {first:?}"
        );
        let notice = first
            .get(1)
            .expect("the turn after the change carries the notice");
        assert!(
            notice.starts_with(STALE_FILES) && notice.contains("notes.md"),
            "got {notice:?}"
        );

        assert_eq!(
            last_user_text(requests.get(1).expect("the second turn asked too")),
            vec!["second"],
            "one episode is told once; a later turn is not reminded again"
        );
    }

    /// A `!` passthrough asks the model nothing, so it is not a turn that can
    /// carry a notice — and must not consume one on the way past.
    #[tokio::test]
    async fn a_passthrough_between_the_change_and_the_prompt_does_not_spend_the_notice() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join("notes.md");
        std::fs::write(&path, "one").expect("the fixture writes");

        let provider = Arc::new(ScriptedProvider::new(vec![
            ProviderEvent::TextDelta("sure".to_owned()),
            ProviderEvent::Finish(FinishReason::Completed),
        ]));
        let seen = Arc::clone(&provider.seen);
        let engine = bare(provider, "scripted-model");
        let mut events = engine.subscribe().await.expect("the first subscriber wins");
        condemn(&engine.files, &path);

        engine
            .send(Command::RunShell {
                command: "true".to_owned(),
            })
            .await
            .expect("an idle engine accepts a passthrough");
        drain(&mut events).await;

        engine
            .send(Command::SendPrompt {
                text: "now what".to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("a finished passthrough leaves the engine idle");
        drain(&mut events).await;

        let requests = seen.lock().expect("the request log is never poisoned");
        assert_eq!(
            requests.len(),
            1,
            "a passthrough asks the provider nothing, got {requests:?}"
        );
        let carried = last_user_text(&requests[0]);
        assert!(
            carried
                .iter()
                .any(|text| text.starts_with(STALE_FILES) && text.contains("notes.md")),
            "the notice waited for the turn that could deliver it: {carried:?}"
        );
    }

    /// The effort rule's outer tier: the fake provider has no catalog rows,
    /// so *any* name is refused with the no-catalog sentence — the same
    /// posture that already denies such a session sizing and pricing — while
    /// clearing asks for the state the session is already in and is accepted
    /// and announced like any adoption.
    #[tokio::test]
    async fn an_effort_on_an_uncataloged_provider_is_refused_naming_the_catalog() {
        let engine = engine();
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        let refusal = engine
            .send(Command::SwitchEffort {
                effort: Some("max".to_owned()),
            })
            .await
            .expect_err("the fake provider has no catalog rows");
        assert!(
            matches!(refusal, EngineError::UncatalogedEffort { .. }),
            "got {refusal:?}"
        );
        assert!(
            refusal.to_string().contains("not in the catalog"),
            "the refusal names the reason: {refusal}"
        );
        assert_eq!(engine.effort(), None, "a refused switch adopts nothing");

        engine
            .send(Command::SwitchEffort { effort: None })
            .await
            .expect("clearing needs no catalog: it asks for the state every session starts in");
        let event = events.next().await.expect("the adoption is announced");
        assert!(
            matches!(event, Event::EffortChanged { effort: None, .. }),
            "got {event:?}"
        );
    }

    // ---- D474: the `/plugin` Reload seam ----

    /// The skills half of the reload: swapping the base registry is what the
    /// next turn is offered, task-tool riding and all — asserted through the
    /// same private accessor the turn assembly reads.
    #[test]
    fn replacing_the_base_tools_is_what_the_next_turn_is_offered() {
        let engine = engine();
        assert!(
            engine.tools().get("read").is_none(),
            "the fixture starts with an empty registry, or the swap proves nothing"
        );

        engine.replace_base_tools(Arc::new(Registry::with_builtins()));

        assert!(
            engine.tools().get("read").is_some(),
            "the offered set is rebuilt from the replaced base"
        );
        assert!(
            engine.lent().get("read").is_some(),
            "the lent set a subagent is offered moves with it"
        );
    }

    /// The team's messaging tool is offered where a team exists, nowhere else,
    /// and a reload of the base set does not drop it.
    ///
    /// Three moves in one test because the claim is the difference between
    /// them: an engine with no team must not offer `send_message` at all, or
    /// the second move proves nothing; and the third is the reload seam
    /// (**D474**), whose whole hazard is a tool that lives outside the shared
    /// composition path and is quietly lost the first time the set is rebuilt.
    #[test]
    fn a_reload_of_the_base_tools_keeps_the_teams_messaging_tool() {
        assert!(
            engine().tools().get(send_message::ID).is_none(),
            "a session with no team has nobody to address"
        );

        let engine = engine().with_teammates(Arc::new(teammate::TeammateRegistry::new(
            ganja_team::TeamsRoot::new(std::path::PathBuf::from("/nonexistent/teams")),
            ganja_team::TeamName::parse("session-abcd1234").expect("a team name"),
            "session-abcd1234",
            std::path::PathBuf::from("/nonexistent/project"),
        )));
        assert!(
            engine.tools().get(send_message::ID).is_some(),
            "a session with a team is offered the tool that addresses it"
        );

        engine.replace_base_tools(Arc::new(Registry::with_builtins()));

        assert!(
            engine.tools().get(send_message::ID).is_some(),
            "a reload rebuilds through the shared composition path, which offers it again"
        );
    }

    /// A process that *is* a member is offered the messaging tool off its own
    /// postbox, leads no team, and keeps the tool across a reload — the same
    /// composition path as the lead's, entered through the other door.
    ///
    /// The negative half is the same one the lead's test opens with, and it
    /// is asserted again here rather than assumed: the whole claim is that
    /// presence of the tool tracks presence of a postbox, and only that.
    #[test]
    fn a_member_engine_with_a_postbox_is_offered_send_message_and_leads_no_team() {
        assert!(
            engine().tools().get(send_message::ID).is_none(),
            "a session with no postbox has nobody to address"
        );

        let postbox = Arc::new(teammate::member::MemberPostbox::new(
            ganja_team::MemberName::parse("worker").expect("a member name"),
            ganja_team::TeamName::parse("session-abcd1234").expect("a team name"),
            ganja_team::TeamsRoot::new(std::path::PathBuf::from("/nonexistent/teams")),
        ));
        let engine = engine().with_postbox(postbox);
        assert!(
            engine.tools().get(send_message::ID).is_some(),
            "a member is offered the tool that addresses its team"
        );
        assert!(engine.teammates().is_none(), "and leads no team of its own");
        assert!(
            engine.teammate_dialogs().is_none(),
            "so no dialog channel is opened for it"
        );

        engine.replace_base_tools(Arc::new(Registry::with_builtins()));

        assert!(
            engine.tools().get(send_message::ID).is_some(),
            "a reload rebuilds through the shared composition path, which offers it again"
        );
    }

    /// **D530**: a session with no team gets the solo postbox and the
    /// teamless-described tool — not the empty-roster ("team of one")
    /// variant, which must not read alike.
    #[test]
    fn a_solo_postbox_is_offered_send_message_with_the_teamless_description() {
        let engine = engine().with_solo_postbox();
        let tools = engine.tools();
        let tool = tools
            .get(send_message::ID)
            .expect("a solo session is offered it");
        assert!(
            !tool
                .description()
                .contains("Teammates this session can address"),
            "no roster is claimed, unlike a team of one: {}",
            tool.description()
        );
        assert!(engine.teammates().is_none(), "and leads no team of its own");
    }

    /// **AC-40's engine-cell half (ADJ-2)**: `/rename` sets the self-name
    /// cell the solo postbox reads at send time, moving the *next* send's
    /// `from` without this cell holding a stale copy — [`Engine::self_name`]
    /// answers it back exactly as set.
    #[test]
    fn set_self_name_moves_the_cell_the_solo_postbox_reads() {
        let engine = engine().with_solo_postbox();
        assert_eq!(
            engine.self_name(),
            crate::tool::registry::FALLBACK_NAME,
            "unseeded, the cell holds the same fallback D527's own sanitizer falls back to"
        );

        engine.set_self_name("fresh");

        assert_eq!(engine.self_name(), "fresh");
    }

    /// A solo-postbox engine over one script per step, ready for a
    /// `send_message` call — D530/D531's fixture. Distinct from this
    /// module's own single-script [`ScriptedProvider`] because a posture test
    /// needs a different script for each of several turns.
    fn teamless(
        scripts: Vec<Vec<ProviderEvent>>,
        rules: Vec<crate::permission::Rule>,
        posture: TeamlessSend,
    ) -> Engine {
        let (provider, _requests) = ganja_testkit::ScriptedProvider::new(scripts);
        let mut permissions = Permissions::default();
        permissions.set_baseline(rules);

        Engine::new(
            provider,
            MODEL,
            Arc::new(Registry::new(Vec::new())),
            permissions,
        )
        .with_solo_postbox()
        .with_teamless_send(posture)
    }

    /// A `send_message` call to a name nobody answers to, one turn's worth.
    fn send_call(to: &str, message: &str) -> Vec<ProviderEvent> {
        ganja_testkit::tool_call(
            send_message::ID,
            serde_json::json!({ "to": to, "message": message }),
        )
    }

    async fn prompt(engine: &Engine, text: &str) {
        engine
            .send(Command::SendPrompt {
                text: text.to_owned(),
                mentions: Vec::new(),
                skills: Vec::new(),
                session_mentions: Vec::new(),
                peers: Vec::new(),
            })
            .await
            .expect("an idle engine accepts a prompt");
    }

    /// Reads until a `PermissionRequested` arrives, and answers its id —
    /// this module's own `drain` cannot be used first, since the turn sits
    /// in the dialog rather than finishing.
    async fn until_requested(
        events: &mut BoxStream<'static, Event>,
    ) -> crate::protocol::PermissionId {
        loop {
            if let Event::PermissionRequested { id, .. } =
                events.next().await.expect("the stream outlives the turn")
            {
                return id;
            }
        }
    }

    /// **D531**: unasked by default, for a session that never named a
    /// posture at all.
    #[tokio::test]
    async fn a_teamless_send_is_unasked_by_default() {
        let engine = teamless(
            vec![send_call("nobody", "hi"), ganja_testkit::says("done")],
            Vec::new(),
            TeamlessSend::Unasked,
        );
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        prompt(&engine, "send it").await;
        let seen = drain(&mut events).await;

        assert!(
            !seen
                .iter()
                .any(|event| matches!(event, Event::PermissionRequested { .. })),
            "a teamless session's default send is unasked: {seen:?}"
        );
    }

    /// **D531**: `teamless_send: "ask"` raises the ordinary permission
    /// dialog, and a stored "always allow" answer silences the next one —
    /// the computed default sits *beneath* every rule, never above it.
    #[tokio::test]
    async fn teamless_ask_raises_a_dialog_and_a_stored_always_answer_silences_the_next() {
        let engine = teamless(
            vec![
                send_call("nobody", "first"),
                ganja_testkit::says("first done"),
                send_call("nobody", "second"),
                ganja_testkit::says("second done"),
            ],
            Vec::new(),
            TeamlessSend::Ask,
        );
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        prompt(&engine, "one").await;
        let waiting = until_requested(&mut events).await;
        engine
            .send(Command::ReplyPermission {
                id: waiting,
                reply: PermissionReply::Always,
            })
            .await
            .expect("the dialog this turn raised is answerable");
        drain(&mut events).await;

        prompt(&engine, "two").await;
        let second = drain(&mut events).await;
        assert!(
            !second
                .iter()
                .any(|event| matches!(event, Event::PermissionRequested { .. })),
            "a stored always answer outranks the handed-in default: {second:?}"
        );
    }

    /// **D531**: a deny rule still denies — the computed default sits
    /// beneath every rule, never above one.
    #[tokio::test]
    async fn a_deny_rule_outranks_the_teamless_ask_default() {
        let engine = teamless(
            vec![send_call("nobody", "hi"), ganja_testkit::says("done")],
            vec![crate::permission::Rule {
                permission: send_message::ID.to_owned(),
                pattern: "*".to_owned(),
                action: crate::permission::Action::Deny,
            }],
            TeamlessSend::Ask,
        );
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        prompt(&engine, "send it").await;
        let seen = drain(&mut events).await;

        assert!(
            !seen
                .iter()
                .any(|event| matches!(event, Event::PermissionRequested { .. })),
            "a rule already answered this; nobody is asked: {seen:?}"
        );
    }

    /// **AC-35's mid-session clause, and AC-42**: a team installed mid-session
    /// flips the computed default back to `allow` with the key still `ask` —
    /// in-team D498 stays byte-untouched — and retiring the team swaps the
    /// solo postbox back, so asking resumes.
    #[tokio::test]
    async fn a_team_installed_mid_session_reverts_to_unasked_and_retiring_it_reverts_to_teamless() {
        let engine = teamless(
            vec![
                send_call("nobody", "one"),
                ganja_testkit::says("one done"),
                send_call("nobody", "two"),
                ganja_testkit::says("two done"),
                send_call("nobody", "three"),
                ganja_testkit::says("three done"),
            ],
            Vec::new(),
            TeamlessSend::Ask,
        );
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        // Teamless, ask key set: the first send is asked about.
        prompt(&engine, "one").await;
        let waiting = until_requested(&mut events).await;
        engine
            .send(Command::ReplyPermission {
                id: waiting,
                reply: PermissionReply::Once,
            })
            .await
            .expect("the dialog this turn raised is answerable");
        drain(&mut events).await;

        // A team is installed mid-session (D530/F10's seam): the key still
        // says `ask`, but a session that holds a team is D498's static
        // ladder again, key regardless.
        let home = tempfile::tempdir().expect("a temp teams root");
        let registry = Arc::new(teammate::TeammateRegistry::new(
            ganja_team::TeamsRoot::new(home.path().join("teams")),
            ganja_team::TeamName::parse("session-abcd1234").expect("a team name"),
            "session-abcd1234",
            home.path(),
        ));
        engine.install_team(&registry);

        prompt(&engine, "two").await;
        let second = drain(&mut events).await;
        assert!(
            !second
                .iter()
                .any(|event| matches!(event, Event::PermissionRequested { .. })),
            "in-team D498 stays unasked regardless of the key: {second:?}"
        );

        // The team ends: the solo postbox swaps back, and asking resumes —
        // `TEAM_GONE` never answers this, because the reinstalled postbox
        // holds no registry to fail upgrading at all.
        engine.retire_team();

        prompt(&engine, "three").await;
        let waiting = until_requested(&mut events).await;
        engine
            .send(Command::ReplyPermission {
                id: waiting,
                reply: PermissionReply::Once,
            })
            .await
            .expect("the dialog this turn raised is answerable");
        let third = drain(&mut events).await;
        assert!(
            !third.iter().any(|event| matches!(
                event,
                Event::PermissionReplied {
                    reply: PermissionReply::Reject,
                    ..
                }
            )),
            "the send was not rejected: {third:?}"
        );
    }

    /// **D528**'s `NewSession` door: a pin this conversation made does not
    /// survive a new one.
    #[tokio::test]
    async fn new_session_clears_the_identity_pin_map() {
        let engine = engine();
        engine.identity.pin("backend", "ses-far", "0198c1a2");
        assert!(engine.identity.pinned("backend").is_some());

        engine
            .new_session()
            .await
            .expect("an idle engine accepts a new session");

        assert_eq!(
            engine.identity.pinned("backend"),
            None,
            "a new conversation has addressed nobody"
        );
    }

    /// An engine with no store has no honest way to run an in-process
    /// teammate — its transcript would be a root session nobody could resume —
    /// so [`Storeless`] refuses the spawn by name while the pane surfaces stay
    /// somebody else's to provide.
    #[tokio::test]
    async fn a_storeless_engine_refuses_an_in_process_teammate_by_name() {
        struct AllowSpawn;

        #[async_trait::async_trait]
        impl crate::subagent::SpawnAsker for AllowSpawn {
            async fn ask(
                &self,
                _request: crate::subagent::SpawnAsk,
            ) -> crate::protocol::PermissionReply {
                crate::protocol::PermissionReply::Once
            }
        }

        let home = tempfile::tempdir().expect("a temp teams root");
        let engine = engine().with_teammates(Arc::new(teammate::TeammateRegistry::new(
            ganja_team::TeamsRoot::new(home.path().join("teams")),
            ganja_team::TeamName::parse("session-abcd1234").expect("a team name"),
            "session-abcd1234",
            home.path().join("project"),
        )));

        let refused = engine
            .teammates()
            .expect("the engine leads a team")
            .start(
                crate::tool::task::TeammateSpawn {
                    name: "w1".to_owned(),
                    backend: Some("in-process".to_owned()),
                    agent_type: "general".to_owned(),
                    prompt: "hello".to_owned(),
                },
                &crate::subagent::Caller {
                    model: MODEL.to_owned(),
                    cwd: home.path().join("project"),
                    permissions: Arc::new(std::sync::Mutex::new(
                        crate::permission::Permissions::default(),
                    )),
                    project_root: home.path().join("project"),
                },
                &AllowSpawn,
            )
            .await
            .expect_err("a storeless engine cannot keep a teammate's transcript");
        assert!(
            refused.reason.contains(super::STORELESS),
            "the refusal names the missing store: {}",
            refused.reason
        );
    }

    /// One limit, written twice, pinned equal.
    ///
    /// `ganja-tool` may not name `ganja-protocol` — its internal dependency
    /// list is exactly the permission crate — so §5.3's cap on a summary is
    /// declared on both sides of that boundary. This crate is the only one
    /// that sees both, which is what makes the pin its debt.
    #[test]
    fn the_summary_cap_the_tool_enforces_is_the_one_the_wire_declares() {
        assert_eq!(
            send_message::SUMMARY_CAP,
            crate::protocol::team::DISPLAY_FIELD_CAP,
            "a summary capped at one number and rendered against another is a summary cut twice"
        );
    }

    /// The hooks half of the reload: an install lands for the next fire, and
    /// [`None`] uninstalls rather than leaving the old table standing.
    #[test]
    fn replacing_the_hooks_installs_for_the_next_fire_and_none_uninstalls() {
        let engine = engine();
        assert!(engine.hooks().is_none(), "the fixture starts hookless");

        let table = std::collections::BTreeMap::from([(
            "Stop".to_owned(),
            vec![crate::config::HookMatcher {
                matcher: None,
                hooks: vec![crate::config::HookHandler::Command(
                    crate::config::HookCommand {
                        command: "true".to_owned(),
                        timeout: None,
                    },
                )],
            }],
        )]);
        let hooks = crate::hook::Hooks::new(&table, &PathBuf::from("."))
            .expect("one Stop handler is a hooks table");
        engine.replace_hooks(Some(hooks));
        assert!(
            engine
                .hooks()
                .is_some_and(|hooks| hooks.fires(crate::hook::HookEvent::Stop)),
            "the swapped-in table is the one the next fire reads"
        );

        engine.replace_hooks(None);
        assert!(
            engine.hooks().is_none(),
            "a reload that found no hooks leaves an engine that does no hook work"
        );
    }

    /// The prompt half of the reload: the replaced closure is recomposed on
    /// the spot, so the suffix the next request carries already reflects it.
    #[test]
    fn replacing_the_environment_recomposes_the_suffix_immediately() {
        let engine = engine();
        assert_eq!(engine.environment_half(), None);

        engine.replace_environment(|model| Some(format!("environment for {model}")));

        assert_eq!(
            engine.environment_half().as_deref(),
            Some(format!("environment for {MODEL}").as_str()),
            "the swap recomposes now rather than waiting for a model switch"
        );
    }
}
