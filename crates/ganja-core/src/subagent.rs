//! Runs a subagent: the second agent loop a `task` call delegates to.
//!
//! Spec: upstream `packages/opencode/src/tool/task.ts` and
//! `packages/opencode/src/agent/subagent-permissions.ts`. The tool
//! ([`crate::tool::task`]) knows what the model asked for and what it will read
//! back; everything between those two — which agent may run, what rules it runs
//! under, whose provider answers it, and where its events go — is here, because
//! all of it is the engine's vocabulary rather than a tool's.
//!
//! [`Spawn`] is the implementation of [`Subagents`] the engine hands to a call.
//!
//! # Why it does not go through the engine
//!
//! [`Engine::send`](crate::engine::Engine::send) runs one turn at a time, and
//! the parent's turn is still holding that slot while the call runs — a child
//! asking the engine for a turn would wait for a turn that is waiting for it.
//! So the child drives [`run_turn`] directly, with a [`Turn`] of its own.
//!
//! # What the parent sees
//!
//! The child's events go to a **private channel**, not the stream frontends
//! subscribe to. Every event now names its session, so the wire *could* say
//! whose they are — but the child session is one no frontend can see: its
//! transcript is never seeded, no picker lists it, and a stream that suddenly
//! interleaved a second conversation's messages would have every consumer
//! rendering them into the one it is showing. So the child stays off the
//! stream, and what crosses over is exactly two things:
//!
//! - the child's permission requests and their replies — re-addressed to the
//!   **parent's** session as they cross, because a dialog naming an invisible
//!   session is one a filtering client could not attribute — since a subagent
//!   that asks a question nobody can see is a subagent that hangs, and the
//!   reply routes back through the *parent's* pending slot, which is free
//!   precisely because the parent is blocked in the call;
//! - progress on the parent's own tool part: `{current_tool, toolcalls,
//!   calls}` in [`ToolState::Running::metadata`], which is what lets a
//!   frontend render
//!   upstream's single inline row without a single new event variant.
//!
//! # Depth
//!
//! One level, fixed (**D9**). A child's registry is this registry without the
//! task tool, so a subagent is not refused the call — it is never offered it.
//!
//! # Fan-out (**D462**, `parallel-subagents-are-a-claude-port`)
//!
//! *Width*, on the other hand, is not one. A run of consecutive `task` calls in
//! one assistant step is a **batch**: the children run at the same time, capped
//! by `agents.concurrency`, and each result is applied to the parent's message
//! as it comes home rather than in the order the model asked. Upstream opencode
//! runs subagents one at a time and has no counterpart to any of it, so nothing
//! in this family ports a TypeScript file; Claude Code's observed contract —
//! fan out within a turn, fan the summaries back in — is the spec.
//!
//! Three pieces carry it, and each is documented where it lives rather than
//! here: the batch executor and the reply registry in [`crate::session`]
//! (`resolve_batch` and `PendingReplies`), and the delivery lock that keeps two
//! children's watchers from publishing into one subscriber's queue while
//! another is half-served (`Fanout::publish`). What did **not** change is
//! anything above the batch: root turns are still one at a time, a child's
//! events still stay off the subscribed stream, the depth limit above still
//! holds, and a subagent still inherits refusals and never allows.
//!
//! # The teammate half (**D501**, the `task` tool's second door)
//!
//! Spec: Claude Code's teammates — §4.1's spawn sequence and §5's messaging.
//! Upstream opencode has no teammates and no counterpart to any of it.
//!
//! Two more things live here, and both are here because this module is what
//! the `task` tool's seam has always been implemented in:
//!
//! - [`Teammates`], which is what a `task` call carrying a `name` reaches. It
//!   is **not** a delegation: nothing is awaited, no child loop runs on this
//!   turn's thread, and what comes back is a member of the team rather than an
//!   answer. [`Spawn`] holds it only through [`Host`], because a team outlives
//!   every call in it while a [`Spawn`] is built per call.
//! - [`Postbox`], the engine's side of what `send_message` sends through. It
//!   is here rather than beside the team because [`crate::teammate`] owns a
//!   lifetime and a runner, and this owns the seams a tool is offered — the
//!   two doors of `task`, and the one door of `send_message`.
//!
//! **The sender is bound at construction and is never an argument.** A
//! [`Postbox`] is built for one engine, carrying that engine's name, so a
//! teammate's postbox can only ever write that teammate's name on a message.
//! There is one per engine and nothing is shared: the lead's carries the
//! lead's name, and [`Postbox::of`] takes the [`Teammate`] itself rather than
//! a string precisely so the name cannot be chosen by whoever builds it. The
//! third implementation — for a process that *is* a member, a pane launched
//! by some other session's lead — is [`crate::teammate::member::MemberPostbox`],
//! which binds its sender from the launch line the same way and reads its
//! roster off the team file rather than a registry it has no share of.
//!
//! # The cross-session tail (**D505**)
//!
//! Spec: §5.6's `uds:` scheme, whose wire the reference never traced (D-12:
//! ganja's is `ganja-serve`'s own HTTP over a Unix socket, one per session).
//! Both ends of it are here, beside the postbox they are two arms of.
//! Outbound, a validated `uds:<path>` address goes through
//! [`Postbox::deliver_over_socket`]: `GET /team` to learn who leads the
//! session at that socket, then `POST /team/{lead}/message` with plain text
//! stamped `<sender>@<team>`. Inbound, `ganja-serve`'s socket-only route
//! hands what arrived to [`receive`], which climbs the tool's rungs on the
//! side that has no tool in front of it and delivers through
//! [`Postbox::peer`] — a postbox bound to the peer's derived identity, which
//! the `@` in it keeps from ever spelling a member of this team. Nothing
//! structured crosses in either direction (§5.2-6): the tool refuses it at
//! rung 6, the outbound arm refuses it again, and the inbound arm classifies
//! the text before anything is written.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use ganja_protocol::team::{
    DISPLAY_FIELD_CAP, Frame, MemberBackend, ShutdownRequest, TeamView, cap_chars, cap_for_display,
};
use ganja_team::{MemberName, record};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agent::{self, Agent};
use crate::engine::{EVENT_CAPACITY, Fanout};
use crate::permission::{Action, Decision, Permissions, Rule, TASK};
use crate::protocol::{
    Event, FinishReason, MessageId, Part, PartBody, PartId, PeerMessageId, PermissionId,
    PermissionReply, Role, ToolState, Usage,
};
use crate::provider::Provider;
use crate::session::{ChildParts, Persist, SessionState, Turn, TurnKind, run_turn};
use crate::storage::{self, SessionId, SessionInfo};
use crate::teammate::postbox::LEADS;
use crate::teammate::{
    DEFAULT_BACKEND, SpawnRequest, Teammate, TeammateBackend, TeammateRegistry, backend_name,
    identity, parse_backend, posture, posture_line,
};
use crate::tool::send_message::NOT_A_SESSION_SOCKET;
use crate::tool::task::{
    Delegated, Delegation, NO_TEAM, NotSpawned, Offered, Subagents, TeammateSpawn, Teammated,
    Unanswered,
};
use crate::tool::team::{self, Address, Body, Peer, Reserved, Sent, Undelivered};
use crate::tool::{Credentials, Registry};

/// The second permission a subagent's ruleset gets denied unless it says
/// otherwise: an unattended child keeps no todo list, because nobody is
/// reading one.
///
/// [`subagent_rules`] owns why both denials are appended and how an agent
/// takes one back; the delegation half is [`TASK`], imported rather than
/// spelled here.
const TODOWRITE: &str = "todowrite";

/// The pattern that covers every call to a permission.
const ANY: &str = "*";

/// Everything a child agent loop needs that does not change between calls.
///
/// Held by the turn and cloned into each [`Spawn`]. Its own [`std::fmt::Debug`]
/// because [`Provider`] has none, and because a derived one would be a wall of
/// prompt text in every tool-call log line.
pub(crate) struct Host {
    /// Who answers the child's requests. The same provider the parent uses:
    /// the instance is fixed when the engine is built.
    pub(crate) provider: Arc<dyn Provider>,
    /// What the parent is asking, which a subagent naming no model of its own
    /// inherits.
    pub(crate) model: String,
    /// The config's `small_model`, carried so a child's own stored session is
    /// titled by the key that titles the parent's.
    pub(crate) small_model: Option<String>,
    /// Agents the parent may spawn.
    pub(crate) agents: Arc<agent::Registry>,
    /// Tools the **child** is offered: this build's registry without the task
    /// tool, which is the whole of the depth guard.
    pub(crate) tools: Arc<Registry>,
    /// The parent's deferral, whole (**D492**): the child reads the same
    /// advertised subset, and its activations join the same session set —
    /// permission gating untouched, the only effect parent-visible roster
    /// growth.
    pub(crate) deferral: crate::tool::deferral::Deferral,
    /// The parent's rules, which the child derives its own from.
    pub(crate) permissions: Arc<std::sync::Mutex<Permissions>>,
    /// The half of the system prompt an agent replaces, for a subagent that
    /// brings no prompt of its own.
    pub(crate) base_prompt: Option<String>,
    /// The half no agent replaces.
    pub(crate) prompt_suffix: Option<String>,
    /// Where the child's tool calls resolve relative paths.
    pub(crate) cwd: PathBuf,
    /// Where the project starts, for the same two uses the parent has.
    pub(crate) root: PathBuf,
    /// The credential store the child's `read` and `grep` refuse, which is the
    /// parent's: a subagent runs unattended, so it is the last conversation
    /// that should be able to read a key out of the disk.
    pub(crate) credentials: Credentials,
    /// The session's language servers, shared rather than started again: a
    /// client is identified by `(root, server)`, so a child working in the
    /// same project reuses the server the parent already has warm.
    pub(crate) lsp: Option<Arc<crate::lsp::Lsp>>,
    /// The store, when the engine persists. A child session is an ordinary
    /// stored session that names its parent.
    pub(crate) persistence: Option<Arc<SessionState>>,
    /// The parent engine's own background-job registry, shared rather than
    /// withheld: a job outlives whichever turn started it, and the depth
    /// guard `tools` already draws (no `task` tool in the child's set) is
    /// about delegating *more* work, not about a subagent's own `bash` calls
    /// losing a capability its parent has.
    pub(crate) jobs: Option<Arc<dyn crate::tool::job::Jobs>>,
    /// The session's hooks, shared with every child: a `PreToolUse` hook that
    /// stopped applying inside a delegated turn would be a gate with a hole in
    /// it. Which of the two stop hooks a child's end fires is decided by
    /// `Turn::delegated`, not here.
    pub(crate) hooks: Option<Arc<crate::hook::Hooks>>,
    /// The session's fan-out cap, carried so a child turn can be built from
    /// this alone. Never read there — a child has no `task` tool to batch.
    pub(crate) concurrency: usize,
    /// The team this session leads, when it leads one (**D501**).
    ///
    /// Here rather than on [`Spawn`] because the two have different lifetimes:
    /// a [`Spawn`] is built per call and names the part that call reports on,
    /// while a team is the session's and outlives every call in it. [`None`]
    /// on a session that leads no team — and on every child turn, since a
    /// subagent gets no `task` tool at all, which is the depth guard applying
    /// to this door as it already does to the other.
    pub(crate) teammates: Option<Arc<Teammates>>,
    /// The parent engine's own identity resolver (**D528**), carried so a
    /// child [`Turn`] has a value to hold even though it is never consulted:
    /// a subagent's prompt carries no `@`-mentions of its own (`session.rs`'s
    /// `Turn::child` always seeds an empty `session_mentions`), and cloning
    /// the parent's `Arc` — rather than building a fresh resolver over the
    /// default socket directory — keeps a test's `--socket-dir` override
    /// reaching a child the same way it reaches the parent.
    pub(crate) identity: Arc<identity::Identity>,
}

impl std::fmt::Debug for Host {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Host")
            .field("provider", &self.provider.id())
            .field("model", &self.model)
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

/// What one task call delegates through: the session-wide [`Host`], plus where
/// in the parent's transcript this call is so its progress can be reported.
#[derive(Clone)]
pub(crate) struct Spawn {
    pub(crate) host: Arc<Host>,
    /// The parent turn's fanout — used for the parent's own tool part and
    /// for forwarding the child's permission dialogs, and for nothing else.
    /// What crosses here reaches every subscriber the parent has.
    pub(crate) events: Arc<Fanout>,
    /// The parent's session, which everything sent on that fanout names:
    /// the progress part is the parent's own, and a crossing dialog is
    /// re-addressed to the conversation whose turn is waiting on it.
    pub(crate) session_id: SessionId,
    /// Where open permission requests wait, shared with the parent turn: the
    /// parent is blocked inside this call, so the registry is the child's to use
    /// and a reply routed to the parent reaches the child.
    ///
    /// A registry rather than a slot since **D462**: several `task` calls from
    /// one step share this, so two children can hold two dialogs at once and
    /// each reply is routed by the id it names.
    pub(crate) pending: Arc<std::sync::Mutex<crate::session::PendingReplies>>,
    /// The parent message holding this call's part.
    pub(crate) message_id: MessageId,
    /// The part this call's progress is reported on.
    pub(crate) part_id: PartId,
    /// This call's own token, so a dialog raised here is retired when the turn
    /// that raised it is cancelled.
    ///
    /// A **delegated turn** takes its cancel token as an argument, because the
    /// call waits for the whole child loop; a **spawn** does not — it hands a
    /// teammate its task and returns — so the only thing here that can outlive
    /// a cancel is the permission dialog, and this is what closes it. The
    /// teammate itself is deliberately outside this token's reach: its
    /// lifetime is the registry's and not a turn's, which is the whole of
    /// **D500**.
    pub(crate) cancel: CancellationToken,
}

impl std::fmt::Debug for Spawn {
    /// Hand-written because the pending-reply slot is a channel end with no
    /// [`Debug`] of its own, and because what is worth reading here is where
    /// the call sits, not the machinery behind it.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Spawn")
            .field("host", &self.host)
            .field("message_id", &self.message_id)
            .field("part_id", &self.part_id)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Subagents for Spawn {
    async fn delegate(
        &self,
        request: Delegation,
        cancel: CancellationToken,
    ) -> Result<Delegated, Unanswered> {
        // Upstream does not check the mode here — only the permission dialog
        // stands between the model and `subagent_type: "build"`. This build
        // refuses it: an agent the roster never offered is one the model has no
        // business naming, and running a *primary* agent unattended is the one
        // thing subagent mode exists to prevent
        // (deviation: task-spawns-subagents-only).
        let Some(agent) =
            self.host.agents.get(&request.subagent_type).filter(|agent| agent.spawnable())
        else {
            return Err(Unanswered::Unknown);
        };

        let child = Child::open(self, agent, request.task_id.as_deref());
        let outcome = child.run(self, agent, &request, cancel).await;
        let task_id = child.session.as_str().to_owned();

        match outcome.stop {
            ChildStop::Cancelled => Err(Unanswered::Cancelled),
            ChildStop::Failed(message) => Err(Unanswered::Failed { task_id, message }),
            ChildStop::Completed => Ok(Delegated {
                task_id,
                agent: agent.name.clone(),
                model: child.model,
                text: outcome.text,
                toolcalls: outcome.toolcalls,
                calls: outcome.calls,
            }),
        }
    }

    async fn spawn_teammate(&self, request: TeammateSpawn) -> Result<Teammated, NotSpawned> {
        let Some(teammates) = self.host.teammates.as_ref() else {
            return Err(NotSpawned { reason: NO_TEAM.to_owned() });
        };

        teammates.start(request, &self.caller(), self).await
    }
}

impl Spawn {
    /// What this turn brings to a spawn.
    ///
    /// Everything in it is the **caller's** rather than the team's: a teammate
    /// started from a turn asks the model that turn is asking, works where that
    /// turn works, and is judged by the rules that turn is under. None of the
    /// four is a `task` argument, so none is the model's to choose.
    fn caller(&self) -> Caller {
        Caller {
            model: self.host.model.clone(),
            cwd: self.host.cwd.clone(),
            permissions: Arc::clone(&self.host.permissions),
            project_root: self.host.root.clone(),
        }
    }
}

#[async_trait]
impl SpawnAsker for Spawn {
    /// Raises the dialog on the **calling turn's** own fanout and waits.
    ///
    /// The same two moves `session.rs`'s per-call wait makes, and for the same
    /// reason they work: the request is registered in the pending-reply
    /// registry before it is published, so a frontend answering the instant it
    /// renders finds somebody waiting; and the registry is the *parent's*,
    /// which is free to take a reply precisely because the parent is blocked
    /// inside this call.
    ///
    /// A dropped sender is a refusal, and so is a cancel. Refusing is the
    /// honest reading of both: a spawn nobody could be asked about is one
    /// nobody approved.
    ///
    /// The cancel arm is not redundant with the dropped sender, and that is
    /// worth saying because it looks like it should be. Nothing closes this
    /// call's entry in the pending-reply registry when a turn is cancelled —
    /// the registry is an `Arc` this value holds, so it outlives the turn —
    /// which means the sender is never dropped and this wait never ends. What
    /// is left behind without the arm is a spawn dialog still on somebody's
    /// screen with no `PermissionReplied` ever coming, and a registry entry
    /// nothing will ever take.
    async fn ask(&self, request: SpawnAsk) -> PermissionReply {
        let (sender, receiver) = oneshot::channel();
        let id = PermissionId::ascending();
        self.pending
            .lock()
            .expect("the pending replies are never poisoned")
            .open_permission(id.clone(), sender);

        let published = self
            .events
            .send(Event::PermissionRequested {
                session_id: self.session_id.clone(),
                id: id.clone(),
                // The `task` part this spawn is being asked about. A dialog is
                // answered by its own `id`; what `call_id` buys is a frontend
                // being able to say *which* call is waiting, and in the
                // transcript that call is this part.
                call_id: self.part_id.as_str().to_owned(),
                tool: crate::tool::task::ID.to_owned(),
                title: request.title,
                args: request.args,
                directories: request
                    .directories
                    .iter()
                    .map(|directory| directory.to_string_lossy().into_owned())
                    .collect(),
            })
            .await;
        if published.is_err() {
            self.pending
                .lock()
                .expect("the pending replies are never poisoned")
                .close_permission(&id);

            return PermissionReply::Reject;
        }

        let received = tokio::select! {
            biased;
            () = self.cancel.cancelled() => None,
            reply = receiver => reply.ok(),
        };
        let reply = match received {
            Some(reply) => reply,
            None => {
                // Retired **by its own id**, never by clearing the registry: a
                // step's batched calls each hold an entry, and taking them all
                // would abandon a sibling's open dialog (**D462**).
                self.pending
                    .lock()
                    .expect("the pending replies are never poisoned")
                    .close_permission(&id);

                PermissionReply::Reject
            }
        };
        // Terminal either way, so a frontend may retire its dialog
        // unconditionally — the contract every other permission wait here
        // keeps.
        let _ = self
            .events
            .send(Event::PermissionReplied { session_id: self.session_id.clone(), id, reply })
            .await;

        reply
    }
}

/// The implementations this session has, one per surface a teammate can run
/// on (**D501**, **D508**, **D538**).
///
/// A map rather than a field per surface since D538, and assembled **outside**
/// the engine: what a `ganja` pane or a `codex` TUI needs is a tmux server, a
/// resolved shell and a column width, none of which an engine holds or should.
/// The engine inserts its own in-process entry and nothing else.
///
/// **The trade this makes, stated.** A field per surface meant [`Backends::of`]
/// was an exhaustive `match` and a seventh [`MemberBackend`] variant nobody
/// wired was a build failure. With a map it is a spawn-time refusal naming the
/// backend instead. That is the price of the whole ruling — a seventh surface
/// now edits its own adapter and the frontend that assembles one, rather than
/// six places in the engine — and what buys the check back is a test over
/// [`crate::teammate::BACKENDS`], which is where the roster is spelled.
#[derive(Debug, Default)]
pub struct Backends(BTreeMap<MemberBackend, Arc<dyn TeammateBackend>>);

impl Backends {
    /// No backends at all: a session that assembles nothing spawns nothing but
    /// what [`crate::Engine::with_teammates`] puts in.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `backend` under the surface it says it is.
    ///
    /// # Panics
    ///
    /// When handed a [`MemberBackend::InProcess`] implementation. That entry is
    /// the engine's own — it holds the lead's provider, tool set and store, and
    /// only the engine has those honestly — so an assembler offering one is a
    /// mistake to name at the offending call rather than a collision to resolve
    /// silently.
    #[must_use]
    pub fn with(mut self, backend: Arc<dyn TeammateBackend>) -> Self {
        let named = backend.backend();
        assert!(
            named != MemberBackend::InProcess,
            "the in-process backend is the engine's own and is inserted by \
             `Engine::with_teammates`; an assembled `Backends` must not carry one"
        );
        self.0.insert(named, backend);

        self
    }

    /// The implementation of `backend`, or [`None`] where this session
    /// assembled none — which every caller refuses by name rather than falling
    /// back to another surface.
    #[must_use]
    pub fn of(&self, backend: MemberBackend) -> Option<Arc<dyn TeammateBackend>> {
        self.0.get(&backend).map(Arc::clone)
    }

    /// Puts the in-process implementation in — the one entry
    /// [`Backends::with`] refuses.
    ///
    /// Its own door rather than part of the builder, because supplying this
    /// entry is a different act: it is built out of a session's own provider,
    /// tool set and store, so its caller is [`crate::Engine::with_teammates`],
    /// or a harness standing in for one where a suite drives
    /// [`Teammates::new`] directly.
    #[must_use]
    pub fn with_in_process(mut self, backend: Arc<dyn TeammateBackend>) -> Self {
        self.0.insert(MemberBackend::InProcess, backend);

        self
    }
}

/// What a spawn is refused with when this session assembled no backend for the
/// surface it named.
///
/// Refused by name, never fallen back to: a person who asked for a teammate
/// and got a different *kind* of teammate has been told something untrue about
/// their own session (**D501**'s rule, which the map does not relax).
#[must_use]
fn no_backend(backend: MemberBackend) -> String {
    format!("this session has no {} backend", backend_name(backend))
}

/// What the calling turn brings to a spawn.
///
/// Handed in rather than held on [`Teammates`], because none of the four is the
/// *team's*: three of them change when the session switches agent or model, and
/// a door that had copied them would answer with what was true when it was
/// built. A team outlives all of that.
#[derive(Clone, Debug)]
pub struct Caller {
    /// The model this turn is asking, which the teammate inherits.
    pub model: String,
    /// Where this turn works, which the teammate works in.
    pub cwd: PathBuf,
    /// The **lead's** own ruleset, which is what decides whether this spawn may
    /// happen at all. Shared rather than cloned: it is the live one, and a
    /// snapshot would answer with rules a stored "always" had since changed.
    pub permissions: Arc<std::sync::Mutex<Permissions>>,
    /// The lead's project root — what its rules were loaded for, and
    /// emphatically not whatever project the teammate's directory sits in
    /// (`posture::spawn_gate` owns why that distinction is the anti-laundering
    /// rule rather than a detail).
    pub project_root: PathBuf,
}

/// One spawn, as a person is asked about it.
///
/// Carries no prompt, and the omission is deliberate: a spawn prompt is
/// documented as a place a credential lands in cleartext, and a dialog is read
/// by a person and rendered by whatever frontend is attached. What a person
/// needs to answer "may this run" is who, where and on what.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnAsk {
    /// One line saying what would be started.
    pub title: String,
    /// The spawn's own facts, for a dialog to render.
    pub args: serde_json::Value,
    /// Directories outside the project this teammate would work in, disclosed
    /// with the request because a person cannot answer "may this teammate run"
    /// without "where" — the same disclosure a call's own dialog makes.
    pub directories: Vec<PathBuf>,
}

/// Who a spawn's own permission dialog is put in front of.
///
/// A seam rather than a call into the turn, because [`Teammates`] is reached by
/// two doors and only one of them is a tool call inside a turn: `/team spawn`
/// asks the person who typed it, and a test asks nobody. What every one of them
/// has in common is that a spawn the rules do not already settle is a question,
/// and this is the shape of asking it.
#[async_trait]
pub trait SpawnAsker: Send + Sync {
    /// Asks about one spawn and reports the answer.
    ///
    /// [`PermissionReply::Reject`] is the answer for anything that is not a
    /// yes, a dismissal and a vanished dialog included: a spawn nobody could be
    /// asked about is one nobody approved.
    async fn ask(&self, request: SpawnAsk) -> PermissionReply;
}

/// The `task` tool's second door: this session's team, and the surfaces a
/// teammate may run on (**D501**).
///
/// Session-wide, where the delegation seam is built per call — which is why a
/// turn reaches this through what it already holds for the session rather than
/// building one beside each call. What a single call decides is only a name, a
/// surface and a task; everything else a spawn needs is the team's, and
/// the registry's own `spawn` fills it in.
#[derive(Debug)]
pub struct Teammates {
    registry: Arc<TeammateRegistry>,
    backends: Backends,
}

impl Teammates {
    /// The door onto `registry`, served by `backends`.
    #[must_use]
    pub fn new(registry: Arc<TeammateRegistry>, backends: Backends) -> Self {
        Self { registry, backends }
    }

    /// The team this door leads onto, which is also what a [`Postbox`] is
    /// built against.
    #[must_use]
    pub fn registry(&self) -> &Arc<TeammateRegistry> {
        &self.registry
    }

    /// Starts a teammate and answers without waiting for its work (§4.1).
    ///
    /// # The gate comes first, and a deny raises nothing
    ///
    /// [`crate::teammate::posture::spawn_gate`] reads the **lead's** rules for
    /// the two things a spawn decides — whether it may work outside the
    /// project, and, on a shim backend, whether that vendor's CLI may run at
    /// all (**D508(c)**) — before anything is written.
    /// A [`Decision::Deny`] refuses here and asks nobody: a deny is not a
    /// question, and putting one in front of a person would be inventing an
    /// answer they already gave. A [`Decision::Ask`] goes to `asker`, and only
    /// a [`Decision::Allow`] — or a yes — reaches the registry.
    ///
    /// # Errors
    ///
    /// [`NotSpawned`], carrying one sentence: a `backend` value nothing
    /// answers to, a surface this build has not got, a name the team refused,
    /// a rule or a person that refused the spawn, or a team file or mailbox
    /// that would not be written. Every one of them is text the model reads and
    /// may retry on, which is why they collapse to a sentence rather than
    /// staying a kind — the tool has no branch that would act on the
    /// difference.
    pub async fn start(
        &self,
        request: TeammateSpawn,
        caller: &Caller,
        asker: &dyn SpawnAsker,
    ) -> Result<Teammated, NotSpawned> {
        // One door for both callers, and one request value. Until 2026-08-22
        // a second entry stood beside this one for the door a person typed
        // at, carrying a `bypass` the `task` door could not — **D-5**'s
        // asymmetry, Resolution 4 — and **D513** retired it: a spawn asks for
        // nothing about its dialogs whoever asked for the spawn, so there is
        // nothing left for the two doors to differ on.
        let backend = match request.backend.as_deref() {
            Some(named) => parse_backend(named).map_err(refused)?,
            // Absence is the default and never an inference: what a session
            // does or does not have — `$TMUX`, a `claude` on the path — decides
            // whether a *named* surface can run, never which one is chosen.
            //
            // Which since **Dv-1** is a rule with teeth rather than a nicety.
            // The default is `ganja`, a pane, so an unnamed backend in a
            // session that cannot reach tmux is **refused by name at spawn**
            // — the same refusal naming it explicitly would have earned. The
            // refusal happens there and not here: this is where a *name* is
            // read, and "no tmux" is not a thing a name can be wrong about.
            None => DEFAULT_BACKEND,
        };
        // Parsed here, so a name the grammar refuses is refused before a
        // person is asked about it. The words are `resolve_unique`'s own —
        // this is the same [`MemberName::parse`] it runs first — so refusing
        // early costs no second sentence.
        let name = MemberName::parse(&request.name).map_err(refused)?;
        let gate = posture::spawn_gate(
            &caller.permissions.lock().expect("the lead's rules are never poisoned"),
            &caller.project_root,
            &caller.cwd,
            backend,
        );
        // Refused by name here, at the one place a surface becomes an
        // implementation, and reported as the ordinary spawn refusal a model
        // reads and may retry on. The warn line beside it names the session,
        // because an assembled map missing a surface is a wiring fault in
        // whoever assembled it rather than something the model did.
        //
        // **Ahead of the dialog**, because a teammate this session cannot
        // start is not a thing to ask a person to approve: the answer changes
        // nothing and the question describes a surface that does not exist.
        let Some(implementation) = self.backends.of(backend) else {
            tracing::warn!(
                backend = backend_name(backend),
                session = self.registry.lead_session_id(),
                "a spawn named a backend this session did not assemble"
            );

            return Err(NotSpawned { reason: no_backend(backend) });
        };
        match gate.action() {
            Decision::Deny => {
                return Err(NotSpawned {
                    // Present whenever something was denied, which is what this
                    // arm *is*; the fallback is the arm's own name rather than
                    // an empty sentence, because a refusal a model cannot read
                    // is a refusal it cannot act on.
                    reason: gate.refusal().unwrap_or_else(|| REFUSED_BY_RULE.to_owned()),
                });
            }
            Decision::Ask => {
                let mut args = serde_json::json!({
                    "name": name.as_str(),
                    "backend": backend_name(backend),
                    "agent_type": request.agent_type,
                    "cwd": caller.cwd.to_string_lossy(),
                });
                // What the grant actually bounds, for the spawns where this
                // dialog is the last thing anybody is asked (**D508(c)**).
                //
                // Inserted rather than written into the literal above, because
                // the key is **absent** for P25's three surfaces rather than
                // `null`: their bounds are the lead's own rules and they go on
                // asking afterwards, so a null here would invite a reader to
                // look for a posture that is not a thing those spawns have.
                if let Some(posture) = posture_line(backend) {
                    args["posture"] = serde_json::Value::from(posture);
                }
                // And what the *surface* adds to it, where it adds anything:
                // a shim in its CLI's native TUI (**D512**) says whose prompts
                // now render in the pane and that the lead hears nothing
                // back. The backend's own answer rather than a second table
                // keyed on `backend`, because which door a shim spawns on is
                // the wired backend's fact — the headless one answers `None`
                // — and the same answer closes the registry's ring lines, so
                // the dialog and `/team` cannot describe one pane differently.
                if let Some(surface) = implementation.surface_line() {
                    args["surface"] = serde_json::Value::from(surface);
                }

                let reply = asker
                    .ask(SpawnAsk {
                        // The name **asked for**, which is not always the name
                        // that answers: the registry resolves a collision by
                        // appending a counter, and it does so after this
                        // dialog because resolving first would mean holding a
                        // name across a wait for a person who may say no. So
                        // the sentence says what is certain and admits what is
                        // not, rather than naming a teammate that may never
                        // exist under that name.
                        title: format!(
                            "start teammate {name} on the {} backend (a name already taken gets a counter)",
                            backend_name(backend)
                        ),
                        args,
                        directories: gate.directories(),
                    })
                    .await;
                if reply == PermissionReply::Reject {
                    return Err(NotSpawned { reason: REFUSED_BY_HAND.to_owned() });
                }
                // `Always` is taken as the yes it is and remembered nowhere.
                // `Permissions::remember` takes a `CallDecision`, and this gate
                // is not one — its two questions are not a call's patterns — so
                // the only way to store an answer here would be to invent a
                // decision about a call that never happened. A person who wants
                // to stop being asked writes the rule, or answers "always" on a
                // call that really does work in that directory, which leaves
                // behind the very `external_directory` rule this gate reads.
            }
            Decision::Allow => {}
        }

        let spawned = self
            .registry
            .spawn(
                implementation,
                SpawnRequest {
                    name: request.name,
                    backend,
                    agent_type: request.agent_type,
                    model: caller.model.clone(),
                    // §4.3's palette assigns one; a `task` call has no colour
                    // in mind and no argument to say so with.
                    color: None,
                    prompt: request.prompt,
                    cwd: caller.cwd.clone(),
                    // Not this door's to ask for: **D501** gives the door
                    // `name` and `backend`, so a teammate that must start in
                    // plan mode is asked for by a person — and that door does
                    // not exist yet.
                    plan_mode_required: false,
                },
            )
            .await
            .map_err(refused)?;

        Ok(Teammated {
            name: spawned.name.as_str().to_owned(),
            agent_id: spawned.agent_id,
            // Echoed from what was resolved rather than from the argument, so
            // a defaulted surface is visible in the transcript.
            backend: backend_name(spawned.backend).to_owned(),
            note: spawned.note.to_owned(),
        })
    }

    /// Asks one teammate to shut down (§6.1).
    ///
    /// A **frame through the mailbox** rather than a call into the registry,
    /// and deliberately: the teammate's own runner is what tears it down, in
    /// the order §6.1 fixes — a `shutdown_request` jumps ahead of everything
    /// else in its inbox — and it answers with the `shutdown_approved` that the
    /// lead's own inbox pass retires it on
    /// ([`crate::teammate::lead_inbox::LeadInbox::poll`]). A registry call would
    /// skip that loop and leave the answer nobody wrote.
    ///
    /// It lives here rather than in the frontend that asks, for the reading
    /// half's own argument: **encoding a §6.1 frame is engine knowledge.**
    /// Which frames exist, which of them a lead may send, and what order the
    /// far side reads them in are facts this crate already owns, so a frontend
    /// that built the document would be a second place for one wire to drift.
    ///
    /// The sender is not an argument here either — [`Postbox::lead`] binds it
    /// from the team — so nothing above can stamp another member's name on a
    /// shutdown.
    ///
    /// # Errors
    ///
    /// [`Undelivered`], exactly as an ordinary message would be: a name nobody
    /// on this team answers to, a team that has been shut down, or an inbox
    /// that would not be written.
    pub async fn ask_shutdown(&self, member: &str) -> Result<Sent, Undelivered> {
        use team::Postbox as _;

        let request = Frame::ShutdownRequest(ShutdownRequest {
            request_id: crate::protocol::uuidv7(),
            // The team's own lead name rather than a constant spelled here: it
            // is what the postbox is about to stamp on the envelope, and two
            // spellings of one identity is one more than a reader can check.
            from: self.registry.lead().as_str().to_owned(),
            reason: Some(SHUTDOWN_ASKED.to_owned()),
            timestamp: record::now_iso8601(),
        });
        let document = serde_json::to_value(&request)
            .map_err(|error| Undelivered::Failed { reason: format!("{UNENCODABLE} {error}") })?;

        Postbox::lead(&self.registry, None)
            .deliver(Address::Local(member.to_owned()), Body::Frame(document))
            .await
    }

    /// Asks every teammate to stop, and says what each one answered.
    ///
    /// The lead is skipped, and not as a special case worth a guard: it is the
    /// one member of the roster that is not a teammate, and a lead writing a
    /// shutdown request into its own inbox would read it back as a stranger's.
    ///
    /// One outcome per teammate, in roster order, so the caller says **one**
    /// sentence about the whole fan-out rather than a notice per member that
    /// the next one overwrites. An empty answer is a team that is only its
    /// lead — worded by the caller, because "nobody to stop" is a sentence
    /// about the question somebody asked rather than about the team.
    ///
    /// Sequential rather than concurrent: every write takes the same team
    /// directory's lock, so asking at once would only queue in a different
    /// place.
    pub async fn ask_whole_team_to_stop(&self) -> Vec<(String, Result<Sent, Undelivered>)> {
        let members: Vec<String> = self
            .registry
            .view()
            .members
            .into_iter()
            .filter(|member| !member.is_lead)
            .map(|member| member.name)
            .collect();

        let mut outcomes = Vec::with_capacity(members.len());
        for member in members {
            let outcome = self.ask_shutdown(&member).await;
            outcomes.push((member, outcome));
        }

        outcomes
    }
}

/// Where one engine's `send_message` calls are posted.
///
/// The sender is a field, set when this is built for a particular engine, and
/// no method takes a `from`. That is the anti-forgery half the tool cannot
/// hold: a `from` argument would be a fact about what a model *typed*, and a
/// teammate whose arguments said `"from": "team-lead"` would stamp the lead's
/// name on its own message for every sibling to believe. Bound here, the
/// identity is a fact about who is calling.
pub struct Postbox {
    /// The name every message written through this carries.
    sender: String,
    /// The team, for the roster this caller may address and the inbox each of
    /// those names resolves to.
    ///
    /// **Weak, and it has to be.** The registry holds every teammate, a
    /// teammate holds its [`Engine`], and that engine holds the postbox
    /// installed into it — so a strong handle here closes a cycle, and no
    /// teammate's engine would ever be dropped, shutdown or not. The team
    /// outliving its postboxes is the honest direction of that lifetime
    /// anyway: a postbox is a *view* onto a team, and a view of a team that
    /// has gone is nothing rather than a team kept alive to be looked at.
    ///
    /// [`Engine`]: crate::Engine
    registry: Weak<TeammateRegistry>,
    /// The identity resolver and this engine's live session id, present on
    /// **the lead's own postbox only** (**D528**): the plan's own scoping
    /// puts the `to` ladder's extension on "the lead postbox's" `Address::Local`
    /// arm, and a teammate's own postbox ([`Postbox::of`]) has no cross-team
    /// addressing to extend — its roster miss stays [`Undelivered::Unknown`]
    /// exactly as it always has.
    resolver: Option<(Arc<identity::Identity>, Arc<std::sync::Mutex<SessionId>>)>,
    /// The three facts a `uds:` send stamps onto its [`SocketMessage`]
    /// beyond `from` (**D532**): this session's own asserted permission
    /// class, its reply address and its outgoing hop chain. [`Unbound`]
    /// until the engine installs its own through
    /// [`Postbox::with_peer_facts`] — every constructor below keeps
    /// building against that default and never has to change for it.
    peer_facts: Arc<dyn PeerFacts>,
    /// Where a held answer registers an outstanding send (**D534**): the
    /// engine's own receipt state, shared with every other reader of it.
    /// [`None`] until the engine installs one, which is every fixture and
    /// every teammate's own postbox — a send from one of those simply
    /// registers nothing and expects no settlement.
    receipts: Option<Arc<crate::teammate::receipts::Receipts>>,
}

/// Renders which team and which sender this speaks for, and nothing of the
/// machinery behind them — the rule [`crate::teammate::SpawnSpec`] states, for
/// the reason it states it: this lands in a `{:?}` of somebody's `ToolCtx`.
impl fmt::Debug for Postbox {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A team that has gone renders as `None` rather than as a remembered
        // name: this is a debug view of a live handle, and printing the team
        // it *used* to speak for would read as one it still reaches.
        let registry = self.registry.upgrade();
        formatter
            .debug_struct("Postbox")
            .field("team", &registry.as_ref().map(|registry| registry.team()))
            .field("sender", &self.sender)
            .finish_non_exhaustive()
    }
}

impl Postbox {
    /// The lead's own postbox.
    ///
    /// Takes the [`Arc`] rather than a [`Weak`] because the sender's name is
    /// read off the team here and now; what is *kept* is the downgrade, so a
    /// postbox never keeps alive the team that holds the engine that holds it.
    ///
    /// `resolver` is the D528 identity index and this engine's live session
    /// id, threaded so the roster-miss arm below can consult it — the lead's
    /// alone, per [`Postbox`]'s own doc — and [`None`] for the one internal
    /// caller that never reaches that arm at all
    /// ([`Teammates::ask_shutdown`], whose recipient is always a known
    /// roster name). The session id is the engine's own live cell rather
    /// than a snapshot, so a rebind (a resume, a `NewSession`) is reflected
    /// without this postbox holding a copy that could go stale.
    #[must_use]
    pub fn lead(
        registry: &Arc<TeammateRegistry>,
        resolver: Option<(&Arc<identity::Identity>, Arc<std::sync::Mutex<SessionId>>)>,
    ) -> Self {
        Self {
            sender: registry.lead().as_str().to_owned(),
            registry: Arc::downgrade(registry),
            resolver: resolver.map(|(identity, own_session)| (Arc::clone(identity), own_session)),
            peer_facts: Arc::new(Unbound),
            receipts: None,
        }
    }

    /// A teammate's, stamped with the name the team gave it.
    ///
    /// Takes the [`Teammate`] rather than a name because that is what makes
    /// the binding a mechanism: the only string that answers
    /// [`Teammate::name`] is the one
    /// the registry's own `spawn` resolved, so a caller
    /// building this cannot choose a different one either.
    #[must_use]
    pub fn of(registry: &Arc<TeammateRegistry>, teammate: &Teammate) -> Self {
        Self {
            sender: teammate.name().to_owned(),
            registry: Arc::downgrade(registry),
            resolver: None,
            peer_facts: Arc::new(Unbound),
            receipts: None,
        }
    }

    /// Installs the engine's own `PeerFacts` (**D532**), the one seam the
    /// engine calls once it exists to implement anything — see
    /// `SoloPostbox::with_peer_facts`, this postbox's twin.
    #[must_use]
    pub fn with_peer_facts(mut self, peer_facts: Arc<dyn PeerFacts>) -> Self {
        self.peer_facts = peer_facts;
        self
    }

    /// Installs the engine's own receipt state (**D534**), the seam
    /// [`Postbox::with_peer_facts`] is the twin of and for the same reason:
    /// the state lives on the engine and this postbox is built before the
    /// engine exists to hand it over, so every constructor above keeps
    /// building against [`None`] and never has to change for it.
    #[must_use]
    pub fn with_receipts(mut self, receipts: Arc<crate::teammate::receipts::Receipts>) -> Self {
        self.receipts = Some(receipts);
        self
    }

    /// What a socket-crossing send reads about this session — the two
    /// installed values, borrowed together, so no arm can pass one and
    /// forget the other.
    fn sender_side(&self) -> SenderSide<'_> {
        SenderSide { facts: self.peer_facts.as_ref(), receipts: self.receipts.as_deref() }
    }

    /// The team as this caller may address it, once somebody holding the team
    /// has proved it is still there.
    ///
    /// Takes the registry rather than reaching for it, so a call that has to
    /// both resolve a recipient and write to it upgrades exactly once and
    /// cannot get two different answers about whether the team exists.
    fn peers(&self, registry: &TeammateRegistry) -> Vec<Peer> {
        registry
            .view()
            .members
            .into_iter()
            // A caller is not in its own roster. There is nothing to say to
            // yourself that a turn cannot say directly, and a message written
            // into your own inbox is one you read back as if somebody else had
            // sent it.
            .filter(|member| !member.name.eq_ignore_ascii_case(&self.sender))
            .map(|member| Peer {
                description: Some(if member.is_lead {
                    LEADS.to_owned()
                } else {
                    crate::teammate::postbox::peer_description(backend_name(member.backend))
                }),
                lead: member.is_lead,
                name: member.name,
            })
            .collect()
    }

    /// The member `name` names, as the team's own names are matched.
    ///
    /// Case-insensitively, because that is how a name was made unique in the
    /// first place (`resolve_unique` lowercases before it compares) and how a
    /// case-insensitive filesystem reads the inbox file it resolves to. So the
    /// canonical spelling comes back from the roster rather than from the
    /// arguments, and [`Sent::to`] reports what was really written to.
    fn recipient(&self, registry: &TeammateRegistry, name: &str) -> Option<Peer> {
        self.peers(registry).into_iter().find(|peer| peer.name.eq_ignore_ascii_case(name))
    }

    /// A postbox speaking for **another session**, stamped with the identity
    /// that session presented over this one's socket (**D505**).
    ///
    /// The third way a sender gets bound, and the one that has to be argued:
    /// [`Postbox::lead`] and [`Postbox::of`] read the name off a value the
    /// caller could not have forged, where this one takes a string that
    /// arrived in a request body. What makes it safe to bind is the shape the
    /// string is held to — a peer names itself by §2.2's derived identity,
    /// `<name>@<team>`, and `@` is a character the member-name grammar refuses
    /// — so whatever a peer claims, the stamp cannot spell a member of *this*
    /// team. Two leads both called `team-lead` (the default, so the common
    /// case) therefore never read as one another, and a claim of a local
    /// name is refused at the door rather than written into an inbox as if a
    /// sibling had said it. That is the whole of the guarantee: the transport
    /// is same-uid cleartext (D-12), so a peer's word is a peer's word, and
    /// this makes it *visibly* a peer's.
    ///
    /// Not being a member, a peer is filtered out of nobody's roster, which is
    /// what lets it reach the lead: the socket's own reason to exist is that a
    /// message addressed `uds:<path>` lands in **that session's** next turn.
    ///
    /// The identity is also *bounded* here, because it lands in the lead's
    /// prompt as the `teammate_id` of a `<teammate-message>` envelope: it
    /// may carry no control character (the renderer escapes the attribute,
    /// and a bounded, printable name is still the right thing to hand it)
    /// and no more than [`DISPLAY_FIELD_CAP`] characters — the cap every
    /// display-only field of a peer message already wears. Refused rather
    /// than truncated: cutting an identity could make two peers read as one.
    ///
    /// # Errors
    ///
    /// [`NotReceived::NotAPeerIdentity`] when `identity` is not
    /// `<name>@<team>` with both halves present, plain, and within the cap.
    fn peer(registry: &Arc<TeammateRegistry>, identity: &str) -> Result<Self, NotReceived> {
        let derived = identity
            .split_once('@')
            .is_some_and(|(name, team)| !name.is_empty() && !team.is_empty());
        let plain = !identity.chars().any(char::is_control);
        let bounded = identity.chars().count() <= DISPLAY_FIELD_CAP;
        if !derived || !plain || !bounded {
            return Err(NotReceived::NotAPeerIdentity { identity: reflected(identity) });
        }

        Ok(Self {
            sender: identity.to_owned(),
            registry: Arc::downgrade(registry),
            // A shape check only — this value is dropped without ever
            // calling `deliver`, so it needs no resolver and no facts beyond
            // the default.
            resolver: None,
            peer_facts: Arc::new(Unbound),
            receipts: None,
        })
    }

    /// The cross-session arm of `deliver` (**D505**): the message crosses to
    /// the session listening at `path`, over that session's own
    /// `ganja-serve` socket, and lands in **its lead's** inbox — a `uds:`
    /// address names a session, and a session's next turn is its lead's.
    ///
    /// Two requests, and the first is not overhead: `GET /team` is how this
    /// side learns the peer's lead's name and team without assuming either,
    /// and it is the one probe that tells a dead socket from a live one
    /// before anything is written. What then crosses is **plain text only**
    /// (§5.2-6): a frame is refused here as well as at the tool's rung 6,
    /// because [`team::Postbox`] is a public trait and this arm is what every
    /// caller of it gets. The sender is stamped `<name>@<team>` — the same
    /// derived identity the far side's [`Postbox::peer`] holds it to, so both
    /// ends agree on what a peer's name looks like without a second rule.
    ///
    /// One `reqwest::Client` per socket path, **built per send** rather than
    /// cached: a cross-session message is a rare thing (one `send_message`
    /// call), a client bound to a socket is a small object with nothing to
    /// warm up, and a cache would need an eviction story for the sockets that
    /// die — which is every one of them, eventually — that nothing here is
    /// placed to know. Bound to exactly one path and never switched, which is
    /// never switched; every failure is a typed [`Undelivered`] naming the
    /// socket, under a deadline, never a hang.
    async fn deliver_over_socket(&self, path: &Path, body: Body) -> Result<Sent, Undelivered> {
        let Some(registry) = self.registry.upgrade() else {
            return Err(Undelivered::Failed { reason: TEAM_GONE.to_owned() });
        };
        let from = format!("{}@{}", self.sender, registry.team());
        drop(registry);

        deliver_over_socket(path, body, from, FRAME_OVER_SOCKET, self.sender_side()).await
    }
}

/// The vet-connect-answer sequence every socket-crossing send shares
/// (**D530**, **F1**): [`Postbox::deliver_over_socket`]'s own body until this
/// plan, factored out and **parameterized by `from`** because that method's
/// `from` is composed off the upgraded team registry
/// (`<sender>@<team>`) — a composition a registry-less caller cannot repeat.
/// The lead's own arm above feeds it exactly that; [`SoloPostbox`] feeds the
/// self-name cell's `<self-name>@solo` instead, and both is why `frame_refusal`
/// is a parameter too — the sentence a frame body earns names "a member of
/// this team" for the lead and has no team to name for a teamless sender.
///
/// Two requests, and the first is not overhead: `GET /team` is how this side
/// learns the peer's lead's name and team without assuming either, and it is
/// the one probe that tells a dead socket from a live one before anything is
/// written. What then crosses is **plain text only** (§5.2-6): a frame is
/// refused here as well as at the tool's rung 6, because [`team::Postbox`] is
/// a public trait and this is what every caller of it gets. `from` is
/// whatever the caller stamped — the same derived identity the far side's
/// [`Postbox::peer`] holds it to, so both ends agree on what a peer's name
/// looks like without a second rule.
///
/// **Sender-side composition** (**D532**): `peer_facts` is read for exactly
/// three of the four new fields — `from_mode`, `hop_chain` and `reply_to`,
/// each carried onto the wire exactly as [`PeerFacts`] answers it, with no
/// further processing here. `message_id` is not one of them: it names *this*
/// message rather than a fact about the sender, so it is minted fresh for
/// every send, bound or not — whether it is ever spent by registering an
/// outstanding receipt is a later, held-and-reply-capable question this
/// function does not answer.
async fn deliver_over_socket(
    path: &Path,
    body: Body,
    from: String,
    frame_refusal: &'static str,
    sender: SenderSide<'_>,
) -> Result<Sent, Undelivered> {
    let Body::Text { text, summary } = body else {
        return Err(Undelivered::Failed { reason: frame_refusal.to_owned() });
    };

    // The tool's rung 3 has already judged the address; it is judged
    // once more here because [`team::Postbox`] is a public trait and this
    // arm is what every caller of it gets — the last gate before a
    // connection is the one that has to hold by construction, whoever
    // called. Same predicate, one spelling (`ganja_tool::socket`), and
    // the refusal in the rung's own sentence.
    crate::tool::socket::vet_address(path).map_err(|why| Undelivered::Failed {
        reason: format!("{NOT_A_SESSION_SOCKET} {}: {why}.", path.display()),
    })?;

    let socket = Socket::open(path)?;
    let view: TeamView = socket.get(TEAM_ROUTE).await?;
    // The lead's name is the far end's word, and it goes into a URL: it
    // is held to the member-name grammar before it does — which refuses
    // `/`, `.`, `?`, `#` and everything else that could steer the POST
    // to some other route on that server — so a listener in a session
    // socket's shape cannot choose where this side posts.
    // The grammar's own error is not repeated: it spells the name whole,
    // and the name is the peer's word — `reflected` is the one place it
    // is allowed to appear, cut.
    let lead = MemberName::parse(&view.lead).map_err(|_| Undelivered::Failed {
        reason: format!("{SOCKET_LEAD_UNNAMED} {}: {:?}.", path.display(), reflected(&view.lead)),
    })?;
    let message_id = PeerMessageId::ascending();
    let reply_to = sender.facts.reply_to();
    let delivered: SocketDelivered = socket
        .post(
            &format!("{TEAM_ROUTE}/{lead}{MESSAGE_ROUTE}"),
            &SocketMessage {
                from,
                text,
                summary,
                message_id: Some(message_id.clone()),
                from_mode: sender.facts.sender_mode(),
                hop_chain: sender.facts.hop_chain(),
                reply_to: reply_to.as_ref().map(|path| format!("uds:{}", path.display())),
            },
        )
        .await?;

    let sent = Sent {
        // The far side answers with the bare name it wrote to; what this
        // side reports back is that name *in that session*, so a transcript
        // never reads a peer's `team-lead` as this team's. Both are the
        // peer's words, and are cut to a line before the model reads them.
        to: reflected(&format!("{}@{}", delivered.to, view.team)),
        note: reflected(&delivered.note),
    };

    // **D534**: an outstanding id is kept **only** when the synchronous
    // answer said held *and* this session named somewhere to answer. An
    // accept and a refuse were both fully resolved by the very bytes above,
    // so neither leaves anything to wait on — which is what makes silence on
    // the receipt route mean exactly one thing, still held. The fact is read
    // off the typed `held` field rather than off `note`'s prose (**N2**):
    // that note is a display cut of the peer's own sentence, and a state
    // machine driven by string-sniffing it would be a state machine driven
    // by the peer.
    if let Some(receipts) = sender.receipts {
        receipts.register(
            message_id,
            sent.to.clone(),
            delivered.held.is_some(),
            reply_to.as_deref(),
        );
    }

    Ok(sent)
}

/// One name's resolution — a registry read (`Identity`'s own module doc says
/// it is a fresh one every call) — off this call's own thread rather than
/// the runtime's, the same hop `ganja-team`'s synchronous mailbox writes
/// take (`crate::teammate::blocking_io`). One spelling for the two postboxes
/// whose name-resolved sends share it.
async fn resolve_blocking(
    identity: &Arc<identity::Identity>,
    name: &str,
    own_session: String,
) -> identity::Resolution {
    let identity = Arc::clone(identity);
    let name = name.to_owned();

    tokio::task::spawn_blocking(move || identity.resolve(&name, &own_session))
        .await
        .expect("resolving a send_message recipient never panics")
}

/// The D528 table both postboxes' name-resolved sends apply identically:
/// ambiguity, a moved pin and a partial listing all refuse; a unique session
/// pins (text bodies only, and before the connect — the pin protects the
/// *choice* of recipient, not the delivery's success) and crosses
/// [`deliver_over_socket`]; nothing answers is `Unknown`. `Sent.to` composes
/// all three identities a transcript needs to audit a name-resolved delivery
/// (**N6**): the name as asked, the resolved socket, and the far side's own
/// reflected answer.
async fn deliver_resolved(
    resolver: &identity::Identity,
    name: &str,
    resolution: identity::Resolution,
    body: Body,
    from: String,
    frame_refusal: &'static str,
    sender: SenderSide<'_>,
) -> Result<Sent, Undelivered> {
    match resolution {
        identity::Resolution::Session { id, stem, socket, .. } => {
            // A frame is refused by `deliver_over_socket`'s own guard before
            // anything is pinned — checked here, ahead of the move, because
            // pinning is the choice this arm accepted, not the connect's own
            // later failure.
            if matches!(body, Body::Text { .. }) {
                resolver.pin(name, &id, &stem);
            }

            let inner = deliver_over_socket(&socket, body, from, frame_refusal, sender).await?;
            Ok(Sent {
                to: format!("{name} (uds:{} \u{2192} {})", socket.display(), inner.to),
                note: inner.note,
            })
        }
        identity::Resolution::Ambiguous { candidates, .. } => {
            Err(Undelivered::Ambiguous { reason: identity::ambiguous_refusal(name, &candidates) })
        }
        identity::Resolution::Moved { pinned_stem, candidates, .. } => {
            Err(Undelivered::NameMoved {
                reason: identity::moved_refusal(name, &pinned_stem, &candidates),
            })
        }
        identity::Resolution::NoneSuch { .. } => Err(Undelivered::Unknown),
        identity::Resolution::ListingFailed { error } => {
            Err(Undelivered::Failed { reason: identity::listing_refusal(name, &error) })
        }
    }
}

/// This session's own permission class, as it is written onto the wire
/// (**D532**) — an assertion, never a proof: a same-uid writer of this field
/// is trusted for nothing beyond what it says (v2 §"Attribute semantics and
/// trust").
///
/// [`SenderMode::from`] is the **one** function both directions of the
/// parity matrix read through (**AC-3**): the receiver's own
/// [`inbound::ReceiverClass`](crate::teammate::inbound::ReceiverClass) and
/// what this session emits here can never name two different classes,
/// because both are one enum apart from the same
/// [`Engine::receiver_class`](crate::engine::Engine::receiver_class) read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SenderMode {
    /// This session prompts: dialogs ask, and the rules decide for it.
    Prompting,
    /// This session bypasses: its own dialogs answer themselves.
    Bypass,
}

impl From<crate::teammate::inbound::ReceiverClass> for SenderMode {
    fn from(class: crate::teammate::inbound::ReceiverClass) -> Self {
        match class {
            crate::teammate::inbound::ReceiverClass::Prompting => Self::Prompting,
            crate::teammate::inbound::ReceiverClass::Bypass => Self::Bypass,
        }
    }
}

/// The most hop markers a [`SocketMessage`] may carry (**D532**, **AC-48**):
/// v2's own sender cap (v2 §"Hop chain: two different caps", evidence
/// 153301-153329), enforced here — at deserialization, where the visitor
/// refuses the first entry past the cap so the `Vec` never grows beyond it —
/// rather than left to the guard's own, smaller thresholds (10 own-marker /
/// 28 chain entries), which fire only after a full chain is in memory. No
/// conforming sender ever exceeds it; axum's request-body cap bounds the
/// request itself and is not the bound anything here relies on.
pub(crate) const MAX_HOP_CHAIN_ENTRIES: usize = 32;

/// Refuses a `hop_chain` past [`MAX_HOP_CHAIN_ENTRIES`] readably, at parse
/// time, instead of silently truncating or accepting an unbounded body. A
/// visitor rather than materialize-then-measure, so the refusal happens at
/// the first entry past the cap and the bound above is a fact about the
/// allocation, not only about what a caller sees.
fn deserialize_hop_chain<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct BoundedChain;

    impl<'de> serde::de::Visitor<'de> for BoundedChain {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                formatter,
                "at most the {MAX_HOP_CHAIN_ENTRIES} hop markers a conforming sender may send"
            )
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            // The size hint is a sender's claim: capped, so a hostile
            // length cannot pre-allocate what the cap exists to refuse.
            let mut chain =
                Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX_HOP_CHAIN_ENTRIES));
            while let Some(marker) = seq.next_element::<String>()? {
                if chain.len() == MAX_HOP_CHAIN_ENTRIES {
                    return Err(serde::de::Error::custom(format!(
                        "hop_chain carries more than the {MAX_HOP_CHAIN_ENTRIES} entries a \
                         conforming sender may send"
                    )));
                }
                chain.push(marker);
            }
            Ok(chain)
        }
    }

    deserializer.deserialize_seq(BoundedChain)
}

/// The socket route's request body, as both ends of the wire spell it: what
/// the outbound `uds:` arm of `Postbox::deliver` sends and what the engine's
/// receiving door takes in.
///
/// One struct rather than two so the two ends cannot drift — the sender is
/// this crate, and the receiver is `ganja-serve`'s handler feeding
/// [`Incoming`], which shapes `from`/`text`/`summary` into a validated peer
/// message; the four fields below cross the same struct and are the
/// admission gate's own to read (**D532**).
///
/// # What does not port, and why (**P1**)
///
/// v2's envelope is a text grammar (v2 §"Grammar", evidence 153204-153209)
/// guarded by a byte-exact canonical re-serialization check, because a
/// crafted body could otherwise terminate the grammar early inside prompt
/// text it is embedded in (v2 §"Canonical parsing (`ndd`)", evidence
/// 153210-153247). This struct is a JSON body behind `deny_unknown_fields`,
/// never embedded in anything a model reads as markup, so the grammar, its
/// escaper, its attribute-order contract and the re-serialization check that
/// defends them all have nothing here to defend and are not ported.
///
/// Also absent, and named rather than silently missing: `from-session`,
/// which v2's own reference records both call sites passing as `undefined`;
/// and the model-visible/control-plane split v2 draws around hop metadata
/// (v2 §"Hop metadata is retained separately", evidence 153248-153285,
/// 415199-415235). Ganja's chain never enters prompt text at all — the model
/// reads [`PartBody::Peer`] composed from `text` alone — so the property v2
/// achieves there by stripping a field out of what it renders, this build
/// has structurally, by never putting one in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SocketMessage {
    /// The sender's derived identity, `<name>@<team>`.
    pub from: String,
    /// What the recipient reads.
    pub text: String,
    /// The sender's one line about it, when it wrote one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// This message's own id, minted per send (D493's v7 family, so it sorts
    /// in creation order) — sender-minted and trusted for nothing beyond
    /// naming the message a later receipt might settle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<PeerMessageId>,
    /// This sender's own asserted permission class — an attestation, never a
    /// proof (v2 §"Attribute semantics and trust").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_mode: Option<SenderMode>,
    /// The route this message has already crossed, oldest first: loop
    /// metadata carrying no signature, sender-capped at
    /// `MAX_HOP_CHAIN_ENTRIES` and refused past it at parse (**AC-48**).
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_hop_chain"
    )]
    pub hop_chain: Vec<String>,
    /// This sender's own `uds:` address, present only when it has one bound
    /// — the same present-only-if-applicable shape v2's own `rOn` derivation
    /// uses for its reply address ("only if that env is set; otherwise
    /// omitted", v2 §"Reply addresses and one-way sends"; §"Same-machine
    /// send (`rOn` / `oFd`)", evidence 220949-220975). A routing hint,
    /// vetted before it is ever opened and never a principal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

/// The typed fact a hold answer carries back to the sender (**N2**,
/// **D532**/**D534**): present on [`SocketDelivered`] only when the gate
/// held the message, carrying the cause as a typed value rather than
/// leaving a lane to string-sniff `note`'s free prose.
///
/// The absence, not just the presence, is load-bearing: an accept and a
/// refuse both omit this field and so stay byte-identical to each other and
/// to a hold answered before this field existed — an outcome enum naming
/// accept was considered and rejected for exactly the reason this shape
/// exists, because it would have reopened the enumeration channel `ganja-serve`'s
/// routes close (D523).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldWire {
    /// Why the gate held it, in [`Event::PeerHeld`]'s own vocabulary — a
    /// review surface and a sender's wire answer are both honest about the
    /// same fact rather than each inventing its own spelling of it.
    pub cause: ganja_protocol::HoldCause,
}

/// What the socket route answers when the message landed — [`Sent`] as it
/// crosses the wire.
///
/// **Standing note for whoever adds the next field here**: this struct is
/// `deny_unknown_fields`, so every additive answer field, this one included,
/// makes a new receiver's answer unparseable to an **old** sender — reported
/// to that sender's model as a failed send, not as the version skew it
/// actually is. `held`'s own skew is priced at the field below; the same
/// price is owed by whatever is added after it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SocketDelivered {
    /// The bare member name the far side wrote to.
    pub to: String,
    /// What became of it, in the far side's words.
    pub note: String,
    /// Present **only** when this answer is a hold — absent for an accept
    /// **and** absent for a refuse, so those two stay byte-identical to each
    /// other (**N2**, **AC-52**).
    ///
    /// Skew, both directions: a **new sender** reading an **old receiver's**
    /// answer never sees this field and degrades to [`SocketMessage`]'s own
    /// AC-2 posture — no registration, no receipt, nothing to wait on. A
    /// **new receiver** answering a hold to an **old sender** fails that
    /// sender's parse under `deny_unknown_fields` — and unlike an ordinary
    /// version-skew refusal, the message in that case **was** accepted into
    /// review and will most likely deliver the moment somebody approves it,
    /// so the sending model is told "failed" about a message that is, in
    /// fact, alive on the far side. Bounded by circumstance rather than by
    /// design: both binaries are one user's, in one `/tmp/ganja-<uid>/`
    /// directory, so a mixed-version pair is a transient state during that
    /// user's own upgrade. Dropping `deny_unknown_fields` would close it and
    /// is refused — the strict answer-parse is this build's analogue of v2's
    /// canonical-parsing posture, and trading it away to soften an upgrade
    /// window would be the larger loss.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<HeldWire>,
}

/// A refusal as `ganja-serve` puts one on the wire: its tag and its sentence.
/// Read here so a peer's refusal reaches the model in the peer's own words.
#[derive(Debug, Deserialize)]
struct SocketRefusal {
    #[serde(default)]
    message: String,
}

/// One sender's settlement of an outstanding held entry, as `POST
/// /peer/receipt` on **that sender's own socket** carries it (**D534**).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SocketReceipt {
    /// The id this sender minted for the message this receipt settles.
    pub message_id: PeerMessageId,
    /// How it settled.
    pub status: ReceiptStatus,
}

/// How a held entry this session sent ultimately settled.
///
/// Exactly the reference's four settlement statuses minus `held`, which
/// ganja answers **synchronously**, in the very [`SocketDelivered`] that was
/// held, rather than over this route (v2 §"Receipts and sender UX", evidence
/// 886033-886075, 886636-886697; v2 §"Explicit outcomes (`P8a`)", evidence
/// 620644-620683: accept merely lets the message continue and only a hold
/// ever sends a receipt at all). An unknown status — the string `"held"`
/// included — refuses readably at deserialization by the ordinary derived
/// behavior of an externally-tagged enum, rather than being guessed at or
/// silently accepted as a fourth state this route does not carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptStatus {
    /// A person approved the held message and it reached its ordinary
    /// delivery path.
    Delivered,
    /// A person denied it. It was never delivered and never will be.
    Denied,
    /// The review window ran out with nobody having decided.
    Expired,
}

/// What a postbox's own session can state about itself when it sends a
/// `uds:` message, beyond `from` (**D532**): its own asserted permission
/// class, where a settlement should be posted back, and the chain of
/// sessions this message has already crossed.
///
/// A trait rather than a plain field, because the facts live on the
/// [`Engine`](crate::engine::Engine) — the receiver class, the inbound-chain
/// cell, the bound socket's own path — while both postbox constructors are
/// called from `engine.rs` before the engine exists to implement anything
/// (moving the composition into a later wave was considered and rejected:
/// it would have left this wave's own new wire fields untested until the
/// wave after they land). [`Postbox::with_peer_facts`] and
/// [`SoloPostbox::with_peer_facts`] are the one seam the engine installs its
/// own implementation through; every constructor above keeps building
/// against [`Unbound`] and never has to change to make room for it.
///
/// Every method returns an owned value with its lock already dropped:
/// nothing implementing this may hold a `std::sync::Mutex` guard across
/// [`deliver_over_socket`]'s socket `await` — `await_holding_lock` runs at
/// `-D warnings` and would catch it at the clippy gate rather than in
/// review.
pub trait PeerFacts: fmt::Debug + Send + Sync {
    /// This session's own permission class, translated through
    /// [`SenderMode`]'s one conversion (**AC-3**) — [`None`] for a sender
    /// with no class to assert.
    fn sender_mode(&self) -> Option<SenderMode>;

    /// This session's own bound socket, as a `uds:` address, when one is
    /// bound — [`None`] otherwise.
    fn reply_to(&self) -> Option<PathBuf>;

    /// The chain this send carries, **already composed**: whatever this
    /// session inherited from the peer message it is answering, with this
    /// session's own marker appended and the whole truncated oldest-first to
    /// [`MAX_HOP_CHAIN_ENTRIES`] (v2 §"Hop chain: two different caps",
    /// evidence 153301-153329) — a caller here does nothing to it but read
    /// it. Empty for a sender with no bound socket and nothing inherited
    /// (Axis 3's unbound sub-case).
    fn hop_chain(&self) -> Vec<String>;
}

/// What one socket-crossing send reads about the session making it
/// (**D532**, **D534**): the facts it stamps onto the envelope, and the
/// receipt state a held answer registers into.
///
/// One borrowed value rather than two parameters because the two travel
/// together through every arm of the crossing and always come from the same
/// postbox — and because splitting them apart is how a future arm comes to
/// pass one and forget the other.
#[derive(Clone, Copy)]
struct SenderSide<'a> {
    /// This session's own asserted class, reply address and hop chain.
    facts: &'a dyn PeerFacts,
    /// Where a held answer registers an outstanding send. [`None`] for every
    /// postbox the engine has not installed its own state into — a fixture,
    /// a teammate's own — which registers nothing and expects no settlement.
    receipts: Option<&'a crate::teammate::receipts::Receipts>,
}

/// The [`PeerFacts`] every postbox is built with until the engine installs
/// its own: a sender with no bound socket asserts no permission class,
/// gives no reply address, and forwards no chain — Axis 3's unbound
/// sub-case, and **AC-5**'s unbound case, both name this arm.
#[derive(Clone, Copy, Debug)]
struct Unbound;

impl PeerFacts for Unbound {
    fn sender_mode(&self) -> Option<SenderMode> {
        None
    }

    fn reply_to(&self) -> Option<PathBuf> {
        None
    }

    fn hop_chain(&self) -> Vec<String> {
        Vec::new()
    }
}

/// The two routes this side drives, spelled once. `ganja-serve` registers the
/// second only on its socket router (D-13), which is what makes a TCP `ganja
/// serve` unreachable through this arm by construction rather than by
/// credential.
const TEAM_ROUTE: &str = "/team";
const MESSAGE_ROUTE: &str = "/message";

/// The scheme and host every socket request is spelled under. `reqwest`
/// resolves nothing when a client is bound to a socket, so the host is a
/// label rather than an address; it is the URL's, and the peer's router does
/// not read it.
const SOCKET_URL: &str = "http://ganja";

/// How long one request to a peer may take, end to end. A local socket
/// answers in milliseconds or not at all — a connect that fails is instant,
/// and a peer that accepted and then went silent is what this bounds — so
/// this is a ceiling on a hang, not a budget a healthy exchange approaches.
const SOCKET_DEADLINE: Duration = Duration::from_secs(10);

/// The most of a peer's answer this side will read, in bytes. What a peer
/// answers is a `TeamView` — a roster of some names and each member's capped
/// ring of one-line calls, kilobytes at the outside — or a two-field
/// receipt, or a refusal envelope; a body past this is not one of those,
/// whatever it says it is, and is refused rather than buffered. The rule
/// exists because the far end is another process's word: a peer that is not
/// a `ganja-serve` at all must not be able to hand this one an unbounded
/// body to hold, let alone to read back to a model.
const SOCKET_BODY_CAP: usize = 1 << 20;

/// The most of a peer's *words* — the sentence of a refusal, the note of a
/// receipt, the name it wrote to — that reaches this side's model. Every one
/// of those is a string the peer composed and this side reads next, so each
/// is cut here to a line's worth of characters; the body cap above bounds
/// what is held, and this bounds what is repeated.
const REFLECTED_CAP: usize = 512;

/// `text`, cut to [`REFLECTED_CAP`] characters on a character boundary —
/// [`cap_chars`], at this side's wider bound.
fn reflected(text: &str) -> String {
    cap_chars(text, REFLECTED_CAP).to_owned()
}

/// One session's socket, as this side speaks to it: a `reqwest::Client`
/// bound to that path and nothing else, and the path itself for every
/// sentence a failure is read in.
///
/// This intentionally duplicates `ganja-client/src/lib.rs`'s
/// `Client::on_socket` transport. The module doc there names this twin and the
/// three routes the socket serves; CI's internal-dependency allowlist forbids
/// `ganja-core → ganja-client`, because the latter must remain a pure consumer
/// of the served engine rather than becoming another layer beneath it.
struct Socket {
    http: reqwest::Client,
    path: PathBuf,
}

impl Socket {
    /// Binds a client to `path`. Nothing is connected yet — a socket that is
    /// not there fails the first request, in that request's words.
    fn open(path: &Path) -> Result<Self, Undelivered> {
        let http =
            reqwest::Client::builder().unix_socket(path).timeout(SOCKET_DEADLINE).build().map_err(
                |error| Undelivered::Failed {
                    reason: format!("{SOCKET_CLIENT_FAILED} {}: {error}", path.display()),
                },
            )?;

        Ok(Self { http, path: path.to_path_buf() })
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, route: &str) -> Result<T, Undelivered> {
        let response = self
            .http
            .get(format!("{SOCKET_URL}{route}"))
            .send()
            .await
            .map_err(|error| self.unreachable(error))?;

        self.read(response).await
    }

    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        route: &str,
        body: &impl Serialize,
    ) -> Result<T, Undelivered> {
        let response = self
            .http
            .post(format!("{SOCKET_URL}{route}"))
            .json(body)
            .send()
            .await
            .map_err(|error| self.unreachable(error))?;

        self.read(response).await
    }

    /// The answer, or the peer's refusal in the peer's own sentence — read
    /// under [`SOCKET_BODY_CAP`], and refused past it, whatever the status.
    async fn read<T: serde::de::DeserializeOwned>(
        &self,
        mut response: reqwest::Response,
    ) -> Result<T, Undelivered> {
        let status = response.status();
        let oversized = || Undelivered::Failed {
            reason: format!(
                "{SOCKET_OVERSIZED} {}: more than {SOCKET_BODY_CAP} bytes.",
                self.path.display()
            ),
        };
        // A declared length past the cap is refused before a byte is read;
        // an undeclared or lying one is refused the moment the cap is passed.
        if response.content_length().is_some_and(|length| length > SOCKET_BODY_CAP as u64) {
            return Err(oversized());
        }
        let mut body: Vec<u8> = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| self.unreachable(error))? {
            if body.len() + chunk.len() > SOCKET_BODY_CAP {
                return Err(oversized());
            }
            body.extend_from_slice(&chunk);
        }
        let text = String::from_utf8_lossy(&body);
        if !status.is_success() {
            let message = serde_json::from_str::<SocketRefusal>(&text)
                .map(|refusal| refusal.message)
                .ok()
                .filter(|message| !message.is_empty())
                .unwrap_or_else(|| text.to_string());

            return Err(Undelivered::Failed {
                reason: format!(
                    "{SOCKET_REFUSED} {} ({}): {}",
                    self.path.display(),
                    status.as_u16(),
                    reflected(&message)
                ),
            });
        }

        serde_json::from_str(&text).map_err(|error| Undelivered::Failed {
            reason: format!(
                "{SOCKET_UNREADABLE} {}: {error}. The two sessions are different \
                 versions of ganja.",
                self.path.display()
            ),
        })
    }

    /// Nothing answered, or the connection died under the request.
    fn unreachable(&self, error: reqwest::Error) -> Undelivered {
        // `reqwest::Error`'s Display nests its causes only one level deep, and
        // the level that says *why* — no such file, connection refused — is
        // the innermost. Walked so the model reads the reason and not "error
        // sending request".
        let mut cause: &dyn std::error::Error = &error;
        while let Some(source) = cause.source() {
            cause = source;
        }

        Undelivered::Failed {
            reason: format!("{SOCKET_UNREACHABLE} {}: {cause}", self.path.display()),
        }
    }
}

/// A plain message another session sent over **this** session's socket, as
/// the socket route hands it in (**D505**, the receiving end of the outbound
/// `uds:` arm of `Postbox::deliver`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Incoming {
    /// The peer's derived identity, `<name>@<team>` — what the message is
    /// stamped with, once the receiving side's shape rule (`Postbox::peer`)
    /// has accepted it.
    pub from: String,
    /// The bare name of the member of this team it is for. The lead's own
    /// name is the usual answer, and a valid one: a peer is nobody's sibling,
    /// so no roster filters it away from the lead.
    pub to: String,
    /// What the recipient reads.
    pub text: String,
    /// The peer's one line about it, when it wrote one.
    pub summary: Option<String>,
}

/// Why a message that arrived over the socket went no further.
///
/// The receiving side's rungs, in the order they are climbed. Each carries
/// its own sentence because that sentence is what crosses back to the peer —
/// through `ganja-serve`'s refusal envelope and into the sender's
/// [`Undelivered::Failed`] — and the peer's model reads it next.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NotReceived {
    /// The message is empty once its whitespace is gone.
    #[error("A message needs some text; this one is whitespace.")]
    Blank,
    /// The text parses as a protocol frame. Nothing structured crosses a
    /// session, whichever of §5.1's two sets it belongs to (§5.2-6).
    #[error(
        "A protocol frame does not cross a socket, and this text is a {kind} frame; a peer \
         session sends plain text."
    )]
    Frame {
        /// The frame's own `type`.
        kind: &'static str,
    },
    /// The sender did not name itself the way a peer does.
    #[error(
        "A peer session names itself by its derived identity, <name>@<team> — plain text, no \
         longer than a summary — and {identity:?} is not one."
    )]
    NotAPeerIdentity {
        /// What it wrote instead, cut to a line.
        identity: String,
    },
    /// This session leads no team, so there is no inbox here to deliver into.
    #[error("This session leads no team; there is nobody here to deliver to.")]
    NoTeam,
    /// The route named a member other than the lead. The socket delivers to
    /// the session — its lead — and to nobody else: the outbound arm never
    /// addresses anyone else, no caller does, and a door left wider than its
    /// one use is a door to state later.
    #[error(
        "A message over a session's socket is for that session's lead, {lead:?}, and this one \
         was addressed to {name:?}; a member of the team is reached through its lead."
    )]
    NotTheLead {
        /// The name the route carried.
        name: String,
        /// The one name it may carry.
        lead: String,
    },
    /// Nobody in this team goes by the name the route carried. Unreachable
    /// while [`NotTheLead`](Self::NotTheLead) stands in front of the
    /// delivery — the lead is always in its own roster — and kept as the
    /// arm the deliverer's own `Unknown` maps onto, answered rather than
    /// unwrapped. Sentenced here rather than borrowed from `send_message`,
    /// because the reader is a *peer's* model, which has no roster of this
    /// team to retry against.
    #[error("Nobody in this team answers to {name:?}; GET /team lists who does.")]
    Unknown {
        /// The name that matched no member.
        name: String,
    },
    /// The recipient exists and the message did not land, in the deliverer's
    /// own words — an inbox that would not open, most of all.
    #[error("{reason}")]
    Failed {
        /// What went wrong.
        reason: String,
    },
}

/// One peer message past the ladder: everything the rungs validated, waiting
/// on the admission gate's verdict before anything is written (**D523**).
///
/// Plain owned strings rather than a live handle, so the gate between the
/// two halves holds nothing of the team while it decides.
pub(crate) struct PeerMessage {
    /// The sender's derived identity, shape-checked by [`Postbox::peer`].
    pub(crate) from: String,
    /// The body, non-blank and frame-free.
    pub(crate) text: String,
    /// The peer's one line about it, blank dropped and capped for display.
    pub(crate) summary: Option<String>,
    /// The lead's canonical name — the one recipient this door delivers to,
    /// and the `to` every answer reports whatever the verdict was, so a
    /// refuse cannot be told from an accept by the name it echoes.
    pub(crate) lead: String,
}

/// The receiving end of the socket route, first half (**D505**, **D523**):
/// the rungs judged, nothing written. The one production caller is
/// [`crate::Engine::receive_peer_message`], which runs the admission gate's
/// verdict between this and [`deliver_to_lead`] — the ladder judges shape,
/// the gate decides admission, and only a `Deliver` reaches the tail.
///
/// The rungs are the tool's own, applied on the side that has no tool in
/// front of it: blank text (rung 5), a frame in the text (rung 7, and rung 6
/// with it — nothing structured crosses), the identity's shape
/// ([`Postbox::peer`]), and the recipient — **the lead, and the lead
/// alone**: a `uds:` address names a session and a session's next turn is
/// its lead's. The summary is capped here as it is at every other seam it
/// crosses: the type says it arrives capped, and a peer's word for that is
/// not enough. These refusals are *shape* errors that predate policy, which
/// is why they keep their own statuses and sentences while a policy refuse
/// answers byte-identically to an accept.
///
/// # Errors
///
/// A [`NotReceived`], one per rung.
pub(crate) fn receive_ladder(
    registry: &Arc<TeammateRegistry>,
    incoming: Incoming,
) -> Result<PeerMessage, NotReceived> {
    if incoming.text.trim().is_empty() {
        return Err(NotReceived::Blank);
    }
    if let Some(kind) = Frame::reserved_kind(&incoming.text) {
        return Err(NotReceived::Frame { kind });
    }
    // The shape check alone: the postbox it builds binds the peer's identity,
    // and the delivery tail rebuilds that binding from the same validated
    // string, so nothing live has to cross the gate.
    let _ = Postbox::peer(registry, &incoming.from)?;
    let lead = registry.lead().as_str();
    if !incoming.to.eq_ignore_ascii_case(lead) {
        return Err(NotReceived::NotTheLead {
            name: reflected(&incoming.to),
            lead: lead.to_owned(),
        });
    }
    let summary = incoming
        .summary
        .filter(|summary| !summary.trim().is_empty())
        .map(|summary| cap_for_display(&summary).to_owned());

    Ok(PeerMessage { from: incoming.from, text: incoming.text, summary, lead: lead.to_owned() })
}

/// The receiving end of the socket route, second half: the write every local
/// delivery ends in, aimed at the lead, answering with the [`RECEIVED`] note
/// and the §2.3 identity the write minted (M6) — what the engine's socket
/// door hands to the gate's admitted set, and what a released hold's write
/// hands back the same way. Reached only past an admission the gate decided;
/// a peer's message to the lead is still written by the same code a
/// teammate's is.
///
/// # Errors
///
/// [`NotReceived::Failed`] for a write that did not land — the delivery
/// path's own failure channel, as it is for any peer write.
pub(crate) async fn deliver_to_lead(
    registry: &Arc<TeammateRegistry>,
    message: PeerMessage,
) -> Result<(Sent, ganja_team::mailbox::Identity), NotReceived> {
    // The lead resolved directly rather than through the roster walk: the
    // ladder already held `to` to this one name, and the lead is the member
    // that cannot leave its own team.
    let recipient = Peer {
        name: message.lead.clone(),
        description: Some(crate::teammate::postbox::LEADS.to_owned()),
        lead: true,
    };
    let (sent, identity) = crate::teammate::postbox::write_to_peer(
        &message.from,
        registry.root(),
        registry.team(),
        &recipient,
        Body::Text { text: message.text, summary: message.summary },
    )
    .await
    .map_err(|undelivered| match undelivered {
        // Unreachable for the lead's own name; the arm is the enum's,
        // answered rather than unwrapped.
        Undelivered::Unknown => NotReceived::Unknown { name: message.lead },
        Undelivered::NoTransport { reason }
        | Undelivered::Ambiguous { reason }
        | Undelivered::NameMoved { reason }
        | Undelivered::Failed { reason } => NotReceived::Failed { reason },
    })?;

    Ok((
        Sent {
            to: sent.to,
            // The socket door's own note, never the in-team `WRITTEN`: the
            // gate can now falsify "it will be read", and the uniform wording
            // is what keeps a refuse byte-identical to this accept (D523).
            note: RECEIVED.to_owned(),
        },
        identity,
    ))
}

/// What the socket door answers for a message that arrived — under accept
/// **and** under an explicit refuse or a guard drop, byte-identically,
/// because refused messages do not notify the sender (v2 §"Explicit outcomes
/// (`P8a`)", evidence 620644-620683). True either way: it claims arrival
/// alone and defers admission to the receiving session's own policy.
pub(crate) const RECEIVED: &str =
    "It reached that session; what its inbound policy admits is that session's own.";

/// The socket door's answer for a held message: named, as the reference's
/// held receipt names its `reason` (v2 §"Receipts and sender UX", evidence
/// 220977-221015), riding the free-text `note` so an older sender's
/// `deny_unknown_fields` never sees a new field.
pub(crate) fn held_note(cause: ganja_protocol::HoldCause) -> String {
    let why = match cause {
        ganja_protocol::HoldCause::Explicit { source } => {
            let tier = match source {
                ganja_protocol::PolicySource::Global => "its global config",
                ganja_protocol::PolicySource::ExplicitFile => "its explicit config file",
                ganja_protocol::PolicySource::Project => "a project config file",
            };
            format!("an explicit hold policy from {tier}")
        }
        ganja_protocol::HoldCause::ModeMismatch => {
            "a permission-mode mismatch with the sender".to_owned()
        }
        ganja_protocol::HoldCause::NoModeAsserted => {
            "no sender mode asserted at a bypassed receiver".to_owned()
        }
        ganja_protocol::HoldCause::ModeUnknown => {
            "a receiver mode that could not be read".to_owned()
        }
    };

    format!(
        "It reached that session and is held for a person's review ({why}); it has not been delivered."
    )
}

#[async_trait]
impl team::Postbox for Postbox {
    fn classify(&self, text: &str) -> Reserved {
        crate::teammate::postbox::classify_reserved(text)
    }

    async fn deliver(&self, to: Address, body: Body) -> Result<Sent, Undelivered> {
        let name = match to {
            Address::Local(name) => name,
            // Validated by the tool, and delivered over the other session's own
            // socket (**D505**).
            Address::Uds { path } => return self.deliver_over_socket(&path, body).await,
        };
        // Answered before the roster is consulted, and answered as a failure
        // rather than as `Unknown`: nothing is wrong with the *name* — the
        // team it belonged to has been shut down — and a model told that
        // nobody answers to it would go looking for a name that does.
        let Some(registry) = self.registry.upgrade() else {
            return Err(Undelivered::Failed { reason: TEAM_GONE.to_owned() });
        };
        if let Some(recipient) = self.recipient(&registry, &name) {
            return crate::teammate::postbox::write_to_peer(
                &self.sender,
                registry.root(),
                registry.team(),
                &recipient,
                body,
            )
            .await
            // The minted identity is the admission gate's key (M6), recorded
            // only where the engine's socket door writes; an in-team
            // delivery is ungated and has no set to feed.
            .map(|(sent, _)| sent);
        }

        // Roster miss: **D528**'s extension, the lead postbox's alone
        // (`self.resolver`'s own doc says why a teammate's postbox has
        // none). Answered `Unknown` unchanged where there is nothing to
        // consult.
        let Some((identity, own_session)) = self.resolver.clone() else {
            return Err(Undelivered::Unknown);
        };
        let own = own_session.lock().expect("the session id is never poisoned").as_str().to_owned();
        let from = format!("{}@{}", self.sender, registry.team());
        drop(registry);

        let resolution = resolve_blocking(&identity, &name, own).await;

        deliver_resolved(
            &identity,
            &name,
            resolution,
            body,
            from,
            FRAME_OVER_SOCKET,
            self.sender_side(),
        )
        .await
    }

    fn roster(&self) -> Vec<Peer> {
        // A team that has gone is an empty roster rather than a refusal: this
        // answers "who may I address", and the honest answer is nobody. The
        // sentence explaining why belongs to the call that tried to send —
        // `deliver` — where there is something to explain.
        self.registry.upgrade().map_or_else(Vec::new, |registry| self.peers(&registry))
    }
}

/// Every reason a spawn did not happen, in the one shape the tool reads.
///
/// The kinds behind it — a `backend` value nothing answers to, a surface this
/// build has not got, a name a team refused, a mailbox that would not open —
/// are the engine's own types and stay so on this side of the seam; what
/// crosses is what the model reads and may retry on.
fn refused(error: impl fmt::Display) -> NotSpawned {
    NotSpawned { reason: error.to_string() }
}

/// Why a spawn a rule denied was refused, when the gate names no clause of its
/// own. Unreachable through [`crate::teammate::posture::SpawnGate`], which
/// always has a sentence for a deny — a fallback rather than a message anybody
/// is expected to read.
const REFUSED_BY_RULE: &str = "a rule refuses this spawn";

/// Why a spawn somebody was asked about was refused.
const REFUSED_BY_HAND: &str =
    "the spawn was refused at the permission dialog; nothing was started and no team was joined";

/// A structured message offered to the socket arm — refused at the tool's
/// rung 6 already, and refused here again for whoever reaches the trait
/// without the tool in front of it (§5.2-6).
const FRAME_OVER_SOCKET: &str = "A protocol frame does not cross a socket: a session reached at a uds: address takes plain text. Send prose, or address a member of this team by name.";

/// [`FRAME_OVER_SOCKET`]'s solo-postbox variant (**D530**, the D528 table's
/// frame-body row): there is no team to point a teamless caller at, so the
/// sentence names a live session instead.
const FRAME_OVER_SOCKET_SOLO: &str = "A protocol frame does not cross a socket: a session reached at a uds: address takes plain text. Send prose, or address a live session by name.";

/// A client that would not build for a socket path — ahead of what reqwest
/// said, and unreachable for any path the tool's rung 3 let through.
const SOCKET_CLIENT_FAILED: &str = "The socket could not be opened at";

/// Nothing answered at the socket, ahead of the OS's own reason.
const SOCKET_UNREACHABLE: &str = "The session at that socket did not answer; it may have ended, and `ganja sessions --live` lists the ones still there. Socket";

/// The peer answered with a refusal, ahead of its status and its sentence.
const SOCKET_REFUSED: &str = "The session at that socket refused the message. Socket";

/// The peer answered something this build has no type for.
const SOCKET_UNREADABLE: &str =
    "The session at that socket answered a body this build cannot read. Socket";

/// The peer named a lead the member-name grammar refuses — nothing this
/// build's binder would ever answer, and a name that cannot go into a URL.
const SOCKET_LEAD_UNNAMED: &str = "The session at that socket named a lead that is not a member name, so no message was posted to it. Socket";

/// The peer answered more than this side reads — refused, not buffered.
const SOCKET_OVERSIZED: &str = "The session at that socket answered more than a session ever does, and the answer was refused unread. Socket";

/// Why a message written after the team itself has gone reaches nobody.
///
/// Read as a failure rather than as an unknown recipient on purpose: the name
/// may well have been right, and there is nothing to retry with. Distinct
/// from the tool's own `NO_TEAM`, which is the answer for a session that never
/// had a team at all — this one is a team that has ended.
const TEAM_GONE: &str =
    "The team this session led has been shut down; there is nobody left to deliver to.";

/// The reserved team-shape word marking the absence of a team in a teamless
/// sender's derived identity (**D530**): a real team named `solo` collides
/// only in display — `from` is unauthenticated routing data on the
/// receiving side regardless (the admission gate's own axiom), so the
/// collision adds no confusion the gate does not already price in.
///
/// Reachable by no shipped binary since **D542**; bead `ganja-code-3tng`.
pub const SOLO_TEAM: &str = "solo";

/// Appended to a teamless send's success note (**D530**'s asymmetry rule): a
/// session with no registered record and no bound socket cannot be answered
/// back, and no text this build ships may imply otherwise.
///
/// No shipped binary reaches this sentence since **D542** — nothing in one
/// installs a [`SoloPostbox`] any more — which also retires the coupling
/// `ganja-code-e99` flagged, that its truth was a property of the assembly
/// rather than of the postbox. Bead `ganja-code-3tng`.
const ONE_WAY_NOTE: &str = " This session is not addressable back — it binds no socket.";

/// A session that leads no team, addressing other live sessions by name or by
/// `uds:` address (**D530**).
///
/// **Built by no shipped binary since D542** (2026-08-29): the one production
/// installer was `ganja-tui`'s no-config-home assembly arm, deleted because
/// the condition it selected on is one `run` has already exited on. What
/// remains is a seam this crate's own tests and `ganja-testkit` drive; bead
/// `ganja-code-3tng` decides whether it becomes live or goes.
///
/// No roster, and — the structural half of **AC-42** — no
/// `Weak<TeammateRegistry>` to fail upgrading: [`TEAM_GONE`] can answer
/// [`Postbox`]'s send because a lead's postbox holds a registry that can go
/// away underneath it, and this one holds no registry at all, so that arm has
/// nothing here to be unreachable *from* rather than merely never taken.
#[derive(Debug)]
pub struct SoloPostbox {
    /// Where this session's self-name lives, read at send time so a
    /// `/rename` moves the next send's `from` without this postbox holding a
    /// stale copy of its own (**ADJ-2**; `Engine::set_self_name`'s cell).
    self_name: Arc<std::sync::Mutex<String>>,
    /// The D528 identity index this session's sends and mentions share.
    identity: Arc<identity::Identity>,
    /// This engine's live session id — the same cell [`Engine::session_id`]
    /// reads, shared rather than snapshotted so a resume or a `NewSession`
    /// moves this postbox's own-session exclusion with it.
    own_session: Arc<std::sync::Mutex<SessionId>>,
    /// See [`Postbox`]'s own field of the same name — [`Unbound`] until
    /// [`SoloPostbox::with_peer_facts`] installs the engine's.
    peer_facts: Arc<dyn PeerFacts>,
    /// Where a held answer registers an outstanding send (**D534**): the
    /// engine's own receipt state, shared with every other reader of it.
    /// [`None`] until the engine installs one, which is every fixture and
    /// every teammate's own postbox — a send from one of those simply
    /// registers nothing and expects no settlement.
    receipts: Option<Arc<crate::teammate::receipts::Receipts>>,
}

impl SoloPostbox {
    /// A postbox for a session that leads no team, bound to the engine's own
    /// self-name cell, identity resolver and live session id.
    #[must_use]
    pub fn new(
        self_name: Arc<std::sync::Mutex<String>>,
        identity: Arc<identity::Identity>,
        own_session: Arc<std::sync::Mutex<SessionId>>,
    ) -> Self {
        Self { self_name, identity, own_session, peer_facts: Arc::new(Unbound), receipts: None }
    }

    /// Installs the engine's own [`PeerFacts`] (**D532**); see
    /// [`Postbox::with_peer_facts`], this postbox's twin.
    #[must_use]
    pub fn with_peer_facts(mut self, peer_facts: Arc<dyn PeerFacts>) -> Self {
        self.peer_facts = peer_facts;
        self
    }

    /// Installs the engine's own receipt state (**D534**); see
    /// [`Postbox::with_receipts`], this postbox's twin.
    #[must_use]
    pub fn with_receipts(mut self, receipts: Arc<crate::teammate::receipts::Receipts>) -> Self {
        self.receipts = Some(receipts);
        self
    }

    /// See [`Postbox::sender_side`], this postbox's twin.
    fn sender_side(&self) -> SenderSide<'_> {
        SenderSide { facts: self.peer_facts.as_ref(), receipts: self.receipts.as_deref() }
    }

    /// The derived identity every send through this stamps `from` with:
    /// `<self-name>@solo`, read fresh so a `/rename` since construction is
    /// honoured.
    fn from(&self) -> String {
        format!(
            "{}@{SOLO_TEAM}",
            self.self_name.lock().expect("the self-name cell is never poisoned")
        )
    }

    fn own_session(&self) -> String {
        self.own_session.lock().expect("the session id is never poisoned").as_str().to_owned()
    }
}

#[async_trait]
impl team::Postbox for SoloPostbox {
    fn classify(&self, text: &str) -> Reserved {
        crate::teammate::postbox::classify_reserved(text)
    }

    async fn deliver(&self, to: Address, body: Body) -> Result<Sent, Undelivered> {
        let sent = match to {
            Address::Local(name) => {
                let resolution = resolve_blocking(&self.identity, &name, self.own_session()).await;

                deliver_resolved(
                    &self.identity,
                    &name,
                    resolution,
                    body,
                    self.from(),
                    FRAME_OVER_SOCKET_SOLO,
                    self.sender_side(),
                )
                .await?
            }
            Address::Uds { path } => {
                deliver_over_socket(
                    &path,
                    body,
                    self.from(),
                    FRAME_OVER_SOCKET_SOLO,
                    self.sender_side(),
                )
                .await?
            }
        };

        Ok(Sent { note: format!("{}{ONE_WAY_NOTE}", sent.note), ..sent })
    }

    fn roster(&self) -> Vec<Peer> {
        // There is no team, so there is no roster to consult before the
        // resolver: every `Address::Local` name goes straight to `identity`
        // (Axis 11 / D530).
        Vec::new()
    }
}

/// The reason a lead gives a teammate it is asking to stop.
///
/// Worded without naming a door, because both of them — `/team shutdown` and
/// whatever asks next — write the same frame, and the teammate reading it is
/// told why rather than where from.
const SHUTDOWN_ASKED: &str = "the lead asked this teammate to stop";

/// A frame that would not encode, ahead of what serde said about it.
///
/// Unreachable through [`ShutdownRequest`], whose every field is a string —
/// answered rather than unwrapped because the cost of being wrong is a panic
/// in somebody's event loop.
const UNENCODABLE: &str = "The shutdown request could not be written:";

/// The agents `caller` may delegate to, as the task tool lists them.
///
/// The filter is upstream's `describeTask` (`tool/registry.ts`) — `mode !==
/// "primary"`, then the caller's own rules consulted for `task`/`<name>` and
/// dropped when they say deny. `hidden` deliberately does not filter here: it
/// hides an agent from the pickers a *person* uses, not from the model.
pub(crate) fn roster(agents: &agent::Registry, caller: &Agent) -> Vec<Offered> {
    agents
        .agents()
        .iter()
        .filter(|agent| agent.spawnable() && !denies_task(&caller.rules, &agent.name))
        .map(|agent| Offered { name: agent.name.clone(), description: agent.description.clone() })
        .collect()
}

/// Whether `rules` refuse a task call naming `subagent`, by the same
/// last-match-wins walk the gate does.
fn denies_task(rules: &[Rule], subagent: &str) -> bool {
    rules
        .iter()
        .rev()
        .find(|rule| {
            crate::permission::matches(TASK, &rule.permission)
                && crate::permission::matches(subagent, &rule.pattern)
        })
        .is_some_and(|rule| rule.action == Action::Deny)
}

/// One child session, resolved: where it writes and what it runs as.
struct Child {
    session: SessionId,
    model: String,
    /// The transcript a resumed child continues from; empty for a fresh one.
    history: Vec<crate::protocol::Message>,
    /// Whether the session record still has to be created.
    fresh: bool,
}

impl Child {
    /// Resolves the session a call runs in: the one `task_id` names when the
    /// store still holds it, and a new one otherwise.
    fn open(spawn: &Spawn, agent: &Agent, task_id: Option<&str>) -> Self {
        let model = agent
            .model
            .as_deref()
            .and_then(|model| crate::provider::adopt(spawn.host.provider.id(), model))
            .unwrap_or_else(|| spawn.host.model.clone());

        let resumed = task_id.zip(spawn.host.persistence.as_ref()).and_then(
            |(id, state)| -> Option<(SessionId, Vec<crate::protocol::Message>)> {
                let id = SessionId::from(id.to_owned());
                // Only a session some earlier call left behind. A root id —
                // the live conversation's own, most of all — names a
                // transcript somebody is having, and appending a child's turns
                // into it would interleave two conversations in one record;
                // the unanswerable id starts a fresh child instead.
                state.storage.load_info(&id).ok().flatten().filter(|info| info.parent.is_some())?;
                let transcript = state.storage.load_transcript(&id).unwrap_or_default();

                Some((id, transcript))
            },
        );

        match resumed {
            Some((session, history)) => Self { session, model, history, fresh: false },
            // A `task_id` the store cannot answer for starts a fresh session
            // rather than failing the call: the model asked for work to happen,
            // and the id was a hint about where to continue it
            // (deviation: task-id-miss-starts-fresh).
            None => {
                Self { session: SessionId::ascending(), model, history: Vec::new(), fresh: true }
            }
        }
    }

    /// Runs the child loop to its finish and reports what it said.
    async fn run(
        &self,
        spawn: &Spawn,
        agent: &Agent,
        request: &Delegation,
        cancel: CancellationToken,
    ) -> Outcome {
        let host = &spawn.host;
        let persist = host.persistence.as_ref().map(|state| {
            if self.fresh {
                create(state, &self.session, agent, &request.description, &self.model);
            }
            Persist::new(Arc::clone(state), self.session.clone())
        });

        let permissions = {
            let parent = host.permissions.lock().expect("the permission rules are never poisoned");
            parent.derive_subagent(subagent_rules(agent, &parent))
        };

        // The child's own channel. Its events never reach the frontend — see
        // the module docs — so the watcher below is the only reader, and it is
        // what turns them back into the two things the parent is entitled to.
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let watcher = tokio::spawn(watch(
            receiver,
            Watched {
                events: Arc::clone(&spawn.events),
                session_id: spawn.session_id.clone(),
                tools: Arc::clone(&host.tools),
                message_id: spawn.message_id.clone(),
                part_id: spawn.part_id.clone(),
                command: request.description.clone(),
            },
        ));

        let turn = Turn::child(
            spawn,
            ChildParts {
                session_id: self.session.clone(),
                model: self.model.clone(),
                system: crate::instruction::joined(
                    agent.prompt.as_deref().or(host.base_prompt.as_deref()),
                    host.prompt_suffix.as_deref(),
                ),
                kind: TurnKind::Prompt {
                    mentions: Vec::new(),
                    skills: Vec::new(),
                    peers: Vec::new(),
                    // A `task` call's own prompt carries no composer
                    // `@`-mentions: that grammar belongs to a person's
                    // prompt, and a subagent is offered no `send_message` to
                    // point one at anyway (`postbox: None`, below).
                    session_mentions: Vec::new(),
                },
                prompt: request.prompt.clone(),
                permissions,
                events,
                history: self.history.clone(),
                cancel,
                persist,
            },
        );
        run_turn(turn).await;

        let outcome = watcher.await.unwrap_or_else(|_| Outcome {
            stop: ChildStop::Failed("the subagent task did not finish".to_owned()),
            ..Outcome::default()
        });

        // **D461**, both halves. The plan's seam was between `run_turn` and the
        // watcher above; it is after instead, because how the child ended is
        // what the watcher resolves to and a hook told only that *a* subagent
        // stopped learns less than one told which and how. The wait is the
        // child's own channel closing, which happens as `run_turn` returns, so
        // nothing is delayed by moving it. `agent` and `outcome` are ganja's
        // own additions to Claude's envelope for the same reason: this build
        // has named agents, and a hook that cannot tell which one ran cannot
        // act on it.
        if let Some(hooks) = &host.hooks {
            let stopped = hooks
                .fire(
                    // The **parent's** session, which is the conversation a
                    // person is having and the only one a hook has any other
                    // way of hearing about; the child's private session id
                    // would name a row nothing else in the envelope refers to.
                    spawn.session_id.as_str(),
                    &crate::hook::Payload::SubagentStop {
                        stop_hook_active: false,
                        agent: agent.name.clone(),
                        outcome: match &outcome.stop {
                            ChildStop::Completed => "completed".to_owned(),
                            ChildStop::Cancelled => "cancelled".to_owned(),
                            ChildStop::Failed(_) => "failed".to_owned(),
                        },
                    },
                )
                .await;
            stopped.report(crate::hook::HookEvent::SubagentStop);
        }

        if let Some(state) = &host.persistence {
            settle(state, &self.session, outcome.usage);
        }

        outcome
    }
}

/// The rules a subagent runs under: its own, then what the parent session
/// insists on, then the two denials that keep it from delegating further.
///
/// Upstream's `deriveSubagentSessionPermission` (`agent/subagent-permissions.ts`)
/// appends `task`/`todowrite` denials unless the subagent's own set already
/// mentions them, which is how `general`'s explicit `todowrite: deny` and an
/// agent that deliberately re-enables it both survive.
///
/// This is the child's whole ruleset. It reaches the child through
/// [`Permissions::derive_subagent`], which leaves the parent's stored answers
/// behind — a set assembled this carefully would mean nothing under a tier
/// that outranks all of it.
fn subagent_rules(agent: &Agent, parent: &Permissions) -> Vec<Rule> {
    let mut rules = agent.rules.clone();
    rules.extend(parent.inherited_by_subagent());

    for permission in [TASK, TODOWRITE] {
        if !Permissions::baseline_mentions(&agent.rules, permission) {
            rules.push(Rule {
                permission: permission.to_owned(),
                pattern: ANY.to_owned(),
                action: Action::Deny,
            });
        }
    }

    rules
}

/// Writes the child's session record before its first byte streams, naming the
/// parent so a listing can tell a delegated conversation from a real one.
fn create(state: &SessionState, session: &SessionId, agent: &Agent, what: &str, model: &str) {
    let created = crate::protocol::now();
    let parent = state
        .live
        .lock()
        .expect("the live session is never poisoned")
        .info
        .as_ref()
        .map(|info| info.id.clone());

    let info = SessionInfo {
        id: session.clone(),
        version: storage::VERSION,
        // Upstream's default child title, so a stored child says what it was
        // for without spending a title request on finding out.
        title: Some(format!("{what} (@{} subagent)", agent.name)),
        created,
        updated: created,
        usage: Usage::default(),
        context_tokens: 0,
        summary: None,
        agent: Some(agent.name.clone()),
        model: Some(model.to_owned()),
        // A child runs no effort — see `Turn::child` — so its record claims
        // none either.
        effort: None,
        // A child's activations live in the shared in-memory set and reach
        // the *root* row at the parent's fan-in flush; its own row never
        // carries any (**D492**).
        activated_tools: std::collections::BTreeSet::new(),
        parent,
        revert: None,
    };

    if let Err(error) = state.storage.save_info(&info) {
        tracing::warn!(
            session = session.as_str(),
            %error,
            "could not create the subagent's session on disk; it runs in memory"
        );
    }
}

/// Adds what the child spent to its own record.
///
/// The turn's own closing write cannot: [`Persist::finish`] only touches the
/// record of the session the engine is *live* on, which is the parent's — that
/// guard is what keeps a child from overwriting its parent's bookkeeping.
fn settle(state: &SessionState, session: &SessionId, usage: Usage) {
    let Ok(Some(mut info)) = state.storage.load_info(session) else {
        return;
    };
    info.usage = usage;
    info.updated = crate::protocol::now();

    if let Err(error) = state.storage.save_info(&info) {
        tracing::warn!(
            session = session.as_str(),
            %error,
            "could not record what the subagent spent"
        );
    }
}

/// What a finished child loop reports back.
#[derive(Default)]
struct Outcome {
    /// The last text part the child produced, which is upstream's
    /// `parts.findLast(p => p.type === "text")`.
    text: String,
    /// How the loop ended.
    stop: ChildStop,
    /// How many tools it called, which the parent's inline row shows.
    toolcalls: usize,
    /// The calls it made, in order, each named the way the running row named
    /// it — capped at [`CALL_LOG`], with `toolcalls` the true total.
    calls: Vec<String>,
    /// What it spent.
    usage: Usage,
}

/// Calls the log keeps: the newest, the oldest dropped.
///
/// The log rides every Running republish and the settled part both, so it is
/// capped. The row shows the last few and the transcript expansion the kept
/// hundred, while `toolcalls` stays the true total — both surfaces admit
/// exactly what the cap cost (2026-08-15).
const CALL_LOG: usize = 100;

/// Why a child loop stopped.
#[derive(Default)]
enum ChildStop {
    /// It ran out of things to say, which is the only success.
    #[default]
    Completed,
    /// The parent's cancel reached it.
    Cancelled,
    /// The provider could not answer. The message is what the parent model
    /// reads.
    Failed(String),
}

/// What the watcher needs to translate a child's events.
struct Watched {
    events: Arc<Fanout>,
    /// The parent's session — what everything the watcher publishes names.
    session_id: SessionId,
    /// The child's registry, so a running call can be named the way a dialog
    /// would name it — `read src/main.rs`, not `read`.
    tools: Arc<Registry>,
    message_id: MessageId,
    part_id: PartId,
    /// What the model called the task, which is the input the parent's part
    /// keeps showing while the child works.
    command: String,
}

/// Reads the child's event stream and does the two things the parent is owed:
/// forwards permission dialogs, re-addressed to the parent's session, and
/// keeps `{current_tool, toolcalls}` on the parent's tool part current.
///
/// Everything else is dropped on purpose. Events name their session now, so a
/// frontend *could* tell a child's messages apart — but the child session is
/// invisible to every frontend (never seeded, never listed), and today's
/// consumers apply the whole stream into the conversation they are showing, so
/// publishing a second session's messages would tear every one of them.
/// Upstream publishes and lets its frontend filter by session id; this build
/// keeps the child off the stream until a consumer exists that asked to filter
/// (deviation: subagent-events-stay-off-the-stream).
async fn watch(mut receiver: mpsc::Receiver<Event>, watched: Watched) -> Outcome {
    let mut outcome = Outcome::default();
    let mut current: Option<String> = None;
    // The text part being streamed right now. The last one to be opened is the
    // last one there is, which is what upstream's `findLast` selects.
    let mut open: Option<PartId> = None;

    while let Some(event) = receiver.recv().await {
        match event {
            // Anything a subagent needs the user for is the user's to answer,
            // and the reply routes back through the parent's pending slot —
            // permission dialogs, and the questions the `question` tool asks.
            // Both are re-addressed to the **parent's** session as they cross:
            // the child session is invisible to every frontend — its events
            // never reach the stream and no picker lists it — so a dialog
            // naming it would hand a session-filtering client a request it
            // cannot attribute, about a conversation it cannot see. The
            // parent's is the conversation whose turn is actually waiting on
            // the answer.
            //
            // The question terminals cross too, and must: a frontend that
            // opened a dialog on the child's `QuestionAsked` would never
            // retire it if the reply or the rejection stayed behind.
            Event::PermissionRequested { .. }
            | Event::PermissionReplied { .. }
            | Event::QuestionAsked { .. }
            | Event::QuestionReplied { .. }
            | Event::QuestionRejected { .. } => {
                let mut crossing = event;
                if let Event::PermissionRequested { session_id, .. }
                | Event::PermissionReplied { session_id, .. }
                | Event::QuestionAsked { session_id, .. }
                | Event::QuestionReplied { session_id, .. }
                | Event::QuestionRejected { session_id, .. } = &mut crossing
                {
                    *session_id = watched.session_id.clone();
                }
                let _ = watched.events.send(crossing).await;
            }
            Event::PartStarted { part, .. } => match &part.body {
                PartBody::Text { .. } => {
                    open = Some(part.id.clone());
                    outcome.text.clear();
                }
                PartBody::Tool { .. } => {
                    outcome.toolcalls += 1;
                    report(&watched, current.as_deref(), &outcome).await;
                }
                // A child's thinking is emphatically not a child's answer:
                // leaving `open` where it is keeps the deltas below
                // accumulating the reply, which is the whole of what the
                // parent's tool result carries.
                // A gateway's own tool run is not a call this child made
                // either — the parent's tool result reports what the child
                // *did*, and this is something a vendor did for it. Neither
                // is a teammate's message: it is something a child was told,
                // and the accumulator below is what the child answered.
                PartBody::File { .. }
                | PartBody::StepStart
                | PartBody::StepFinish { .. }
                | PartBody::Patch { .. }
                | PartBody::ReasoningText { .. }
                | PartBody::ServerTool { .. }
                | PartBody::Peer { .. }
                | PartBody::Reasoning { .. } => {}
            },
            Event::PartDelta { part_id, delta, .. } => {
                if open.as_ref() == Some(&part_id) {
                    outcome.text.push_str(&delta);
                }
            }
            Event::PartUpdated { part, .. } => {
                if let PartBody::Tool { tool, state, .. } = &part.body
                    && let ToolState::Running { input, .. } = state
                {
                    let name = describe_call(&watched.tools, tool, input);
                    // A call republishes its Running part as it streams, so
                    // only a *new* name joins the log; two genuinely identical
                    // calls back to back collapse into one row, which the true
                    // count still admits.
                    if outcome.calls.last() != Some(&name) {
                        if outcome.calls.len() == CALL_LOG {
                            outcome.calls.remove(0);
                        }
                        outcome.calls.push(name.clone());
                    }
                    current = Some(name);
                    report(&watched, current.as_deref(), &outcome).await;
                }
            }
            Event::MessageStarted { session_id: _, message } => {
                // A compaction summary arrives as a complete assistant message
                // rather than a streamed one; it is not the child's answer.
                if message.role == Role::User {
                    outcome.text.clear();
                    open = None;
                }
            }
            Event::MessageFinished { reason, usage, error, .. } => {
                if let Some(usage) = usage {
                    outcome.usage = usage;
                }
                outcome.stop = match reason {
                    FinishReason::Completed => ChildStop::Completed,
                    FinishReason::Cancelled => ChildStop::Cancelled,
                    FinishReason::Failed => ChildStop::Failed(
                        error.unwrap_or_else(|| "the subagent could not answer".to_owned()),
                    ),
                };
            }
            // A child takes no snapshots of its own, so nothing here ever
            // reverts; the arm exists because the parent's watcher reads the
            // whole event stream and must not be surprised by one of them.
            // The same holds for an agent change — a child is never handed the
            // approval cell — and for an effort change and a permission-mode
            // change, which only the engine's command paths announce. A steer
            // cannot reach a child either: no handle of a child's ever enters
            // the engine's slot, so its mailbox has no route in.
            // And a child never compacts: the fill-level guard reads the
            // parent's live record and walks away when the ids differ, so a
            // progress gauge is the root turn's alone.
            Event::RevertChanged { .. }
            | Event::AgentChanged { .. }
            | Event::SteerConsumed { .. }
            | Event::PermissionModeChanged { .. }
            | Event::CompactionProgress { .. }
            | Event::EffortChanged { .. } => {}
            // A hold, its settlement and a sender's own settlement receipt
            // are a lead's or a sender's surfaces (**D524**, **D534**), and
            // no child session leads a team, binds a socket a peer could
            // reach, or sends a `uds:` message of its own to hold a receipt
            // for — a subagent installs no teammates and posts through
            // none of this. Permanent, not a bridge: the gate landed and
            // this stayed true by construction.
            Event::PeerHeld { .. } | Event::PeerHoldSettled { .. } | Event::PeerReceipt { .. } => {}
        }
    }

    outcome
}

/// How a running call is named on a row a person reads.
///
/// Upstream shows the tool's own `state.title`, which its running parts carry
/// and ganja's do not. What stands in is the line a permission dialog would
/// have used for the same call — `read src/main.rs`, not `read` — which is the
/// same sentence by a different route
/// (deviation: task-progress-names-the-call). The watcher's row and the
/// D503 ring ([`crate::teammate`]'s `fold_calls`) both name calls through
/// this.
pub(crate) fn describe_call(tools: &Registry, tool: &str, input: &serde_json::Value) -> String {
    tools.get(tool).map_or_else(|| tool.to_owned(), |found| found.describe(input))
}

/// Rewrites the parent's tool part with what the child is doing now — and the
/// log of what it has already done, which is what the task row expands
/// (2026-08-15).
///
/// The part travels whole, as every [`Event::PartUpdated`] does. The parent's
/// own copy is deliberately not touched: this is progress, not transcript, and
/// what reaches the disk is the completed call.
async fn report(watched: &Watched, current: Option<&str>, outcome: &Outcome) {
    let mut metadata = serde_json::json!({
        "toolcalls": outcome.toolcalls,
        "calls": outcome.calls,
    });
    if let Some(current) = current {
        metadata["current_tool"] = serde_json::json!(current);
    }

    let _ = watched
        .events
        .send(Event::PartUpdated {
            // The part being rewritten is the parent's own, so the event is
            // the parent session's however a child's work fills it.
            session_id: watched.session_id.clone(),
            message_id: watched.message_id.clone(),
            part: Part {
                id: watched.part_id.clone(),
                body: PartBody::Tool {
                    call_id: watched.part_id.as_str().to_owned(),
                    tool: crate::tool::task::ID.to_owned(),
                    state: ToolState::Running {
                        input: serde_json::json!({ "description": watched.command }),
                        metadata,
                        started: 0,
                    },
                },
            },
        })
        .await;
}

#[cfg(test)]
#[path = "subagent_tests.rs"]
mod tests;
