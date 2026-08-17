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
//! a string precisely so the name cannot be chosen by whoever builds it.

use std::{fmt, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use ganja_protocol::team::{Frame, MemberBackend};
use ganja_team::{MailboxMessage, MemberName, mailbox, record};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    agent::{self, Agent},
    engine::{EVENT_CAPACITY, Fanout},
    permission::{Action, Permissions, Rule, TASK},
    protocol::{Event, FinishReason, MessageId, Part, PartBody, PartId, Role, ToolState, Usage},
    provider::Provider,
    session::{ChildParts, Persist, SessionState, Turn, TurnKind, run_turn},
    storage::{self, SessionId, SessionInfo},
    teammate::{
        DEFAULT_BACKEND, SpawnRequest, Teammate, TeammateBackend, TeammateRegistry, backend_name,
        parse_backend,
    },
    tool::{
        Credentials, Registry,
        task::{
            Delegated, Delegation, NO_TEAM, NotSpawned, Offered, Subagents, TeammateSpawn,
            Teammated, Unanswered,
        },
        team::{self, Address, Body, Peer, Reserved, Sent, Undelivered},
    },
};

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
        let Some(agent) = self
            .host
            .agents
            .get(&request.subagent_type)
            .filter(|agent| agent.spawnable())
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
            return Err(NotSpawned {
                reason: NO_TEAM.to_owned(),
            });
        };

        // The model and the directory are the **caller's** rather than the
        // team's: a teammate started from a turn asks the model that turn is
        // asking and works where that turn works, exactly as a delegated child
        // does. Neither is a `task` argument, so neither is the model's to
        // choose.
        teammates
            .start(request, self.host.model.clone(), self.host.cwd.clone())
            .await
    }
}

/// One implementation per surface a teammate can run on (**D501**).
///
/// Three fields rather than a lookup, so [`Teammates`] picks one by an
/// exhaustive match: a fourth surface is then a build failure here instead of a
/// `backend` value that resolves to nothing at run time. Which implementation
/// sits in each slot is the engine's to decide — this build's are
/// [`crate::teammate::InProcess`], [`crate::teammate::pane::GanjaPane`] and
/// [`crate::teammate::claude::ClaudePane`], and only the first of the three
/// holds anything of the host's.
#[derive(Debug)]
pub struct Backends {
    /// The teammate that runs in the lead's own process.
    pub in_process: Arc<dyn TeammateBackend>,
    /// The teammate with a `ganja` pane of its own.
    pub pane: Arc<dyn TeammateBackend>,
    /// The teammate that is a real `claude`.
    pub claude: Arc<dyn TeammateBackend>,
}

impl Backends {
    /// The implementation of `backend`.
    fn of(&self, backend: MemberBackend) -> Arc<dyn TeammateBackend> {
        match backend {
            MemberBackend::InProcess => Arc::clone(&self.in_process),
            MemberBackend::Pane => Arc::clone(&self.pane),
            MemberBackend::Claude => Arc::clone(&self.claude),
        }
    }
}

/// The `task` tool's second door: this session's team, and the surfaces a
/// teammate may run on (**D501**).
///
/// Session-wide, where the delegation seam is built per call — which is why a
/// turn reaches this through what it already holds for the session rather than
/// building one beside each call. What a single call decides is only a name, a
/// surface and a task; everything else a spawn needs is the team's, and
/// [`crate::teammate::TeammateRegistry::spawn`] fills it in.
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
    /// `model` and `cwd` are the calling turn's own, handed in rather than
    /// held: a door is not the place a session's current model lives, and a
    /// value copied here would be one that goes stale the next time the
    /// session switches agent.
    ///
    /// # Errors
    ///
    /// [`NotSpawned`], carrying one sentence: a `backend` value nothing
    /// answers to, a surface this build has not got, a name the team refused,
    /// or a team file or mailbox that would not be written. Every one of them
    /// is text the model reads and may retry on, which is why they collapse to
    /// a sentence rather than staying a kind — the tool has no branch that
    /// would act on the difference.
    pub async fn start(
        &self,
        request: TeammateSpawn,
        model: String,
        cwd: PathBuf,
    ) -> Result<Teammated, NotSpawned> {
        let backend = match request.backend.as_deref() {
            Some(named) => parse_backend(named).map_err(refused)?,
            // Absence is the default and never an inference: what a session
            // does or does not have — `$TMUX`, a `claude` on the path — decides
            // whether a *named* surface can run, never which one is chosen.
            None => DEFAULT_BACKEND,
        };

        let spawned = self
            .registry
            .spawn(
                self.backends.of(backend),
                SpawnRequest {
                    name: request.name,
                    backend,
                    agent_type: request.agent_type,
                    model,
                    // §4.3's palette assigns one; a `task` call has no colour
                    // in mind and no argument to say so with.
                    color: None,
                    prompt: request.prompt,
                    cwd,
                    // Neither is this door's to ask for: **D501** gives both
                    // doors exactly `name` and `backend`, so a teammate that
                    // must start in plan mode or that wants dialogs bypassed
                    // is asked for by a person at `/team spawn`, not by a
                    // model in a tool call.
                    plan_mode_required: false,
                    bypass: false,
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
    registry: Arc<TeammateRegistry>,
}

/// Renders which team and which sender this speaks for, and nothing of the
/// machinery behind them — the rule [`crate::teammate::SpawnSpec`] states, for
/// the reason it states it: this lands in a `{:?}` of somebody's `ToolCtx`.
impl fmt::Debug for Postbox {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Postbox")
            .field("team", self.registry.team())
            .field("sender", &self.sender)
            .finish_non_exhaustive()
    }
}

impl Postbox {
    /// The lead's own postbox.
    #[must_use]
    pub fn lead(registry: Arc<TeammateRegistry>) -> Self {
        Self {
            sender: registry.lead().as_str().to_owned(),
            registry,
        }
    }

    /// A teammate's, stamped with the name the team gave it.
    ///
    /// Takes the [`Teammate`] rather than a name because that is what makes
    /// the binding a mechanism: the only string that answers
    /// [`Teammate::name`] is the one
    /// [`crate::teammate::TeammateRegistry::spawn`] resolved, so a caller
    /// building this cannot choose a different one either.
    #[must_use]
    pub fn of(registry: Arc<TeammateRegistry>, teammate: &Teammate) -> Self {
        Self {
            sender: teammate.name().to_owned(),
            registry,
        }
    }

    /// The member `name` names, as the team's own names are matched.
    ///
    /// Case-insensitively, because that is how a name was made unique in the
    /// first place (`resolve_unique` lowercases before it compares) and how a
    /// case-insensitive filesystem reads the inbox file it resolves to. So the
    /// canonical spelling comes back from the roster rather than from the
    /// arguments, and [`Sent::to`] reports what was really written to.
    fn recipient(&self, name: &str) -> Option<Peer> {
        team::Postbox::roster(self)
            .into_iter()
            .find(|peer| peer.name.eq_ignore_ascii_case(name))
    }
}

#[async_trait]
impl team::Postbox for Postbox {
    fn classify(&self, text: &str) -> Reserved {
        // One parse and one lookup, both `ganja-protocol`'s: the tool may not
        // name that crate, so this is the only place the fifteen are known,
        // and there is no list of frame names anywhere on the tool's side to
        // fall out of step with it.
        match Frame::reserved_kind(text) {
            None => Reserved::No,
            Some(kind) if Frame::is_agent_sendable_kind(kind) => Reserved::AgentSendable { kind },
            Some(kind) => Reserved::HarnessOnly { kind },
        }
    }

    async fn deliver(&self, to: Address, body: Body) -> Result<Sent, Undelivered> {
        let name = match to {
            Address::Local(name) => name,
            // Validated by the tool, deliverable by nobody yet: the ladder is
            // complete and only the transport waits.
            Address::Uds { .. } => {
                return Err(Undelivered::NoTransport {
                    reason: NO_SOCKET.to_owned(),
                });
            }
        };
        let Some(recipient) = self.recipient(&name) else {
            return Err(Undelivered::Unknown);
        };
        // A roster name was resolved through the name grammar before it was
        // ever a member, so this cannot fail — and it is answered rather than
        // unwrapped because the cost of being wrong is a panic in somebody's
        // turn, where the cost of being right is one arm.
        let member = MemberName::parse(&recipient.name).map_err(|error| Undelivered::Failed {
            reason: format!("{UNADDRESSABLE} {:?}: {error}", recipient.name),
        })?;

        let (text, summary) = match body {
            Body::Text { text, summary } => (text, summary),
            // A frame crosses as the document its sender wrote. The far side
            // reads it back with the same one parse `classify` uses, so
            // re-encoding it through a typed value here would only be a second
            // spelling of one document.
            Body::Frame(document) => (document.to_string(), None),
        };
        let mut message = MailboxMessage::new(&self.sender, text, record::now_iso8601());
        message.summary = summary;

        let path = self
            .registry
            .root()
            .inbox_path(self.registry.team(), &member);
        let written = tokio::task::spawn_blocking(move || mailbox::write(&path, message))
            .await
            .map_err(|error| error.to_string())
            .and_then(|written| written.map_err(|error| error.to_string()));

        match written {
            Ok(_) => Ok(Sent {
                to: member.into_inner(),
                note: WRITTEN.to_owned(),
            }),
            Err(reason) => Err(Undelivered::Failed {
                reason: format!("{UNWRITTEN} {reason}"),
            }),
        }
    }

    fn roster(&self) -> Vec<Peer> {
        self.registry
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
                    format!("{RUNS_ON} {} backend", backend_name(member.backend))
                }),
                lead: member.is_lead,
                name: member.name,
            })
            .collect()
    }
}

/// Every reason a spawn did not happen, in the one shape the tool reads.
///
/// The kinds behind it — a `backend` value nothing answers to, a surface this
/// build has not got, a name a team refused, a mailbox that would not open —
/// are the engine's own types and stay so on this side of the seam; what
/// crosses is what the model reads and may retry on.
fn refused(error: impl fmt::Display) -> NotSpawned {
    NotSpawned {
        reason: error.to_string(),
    }
}

/// Why a `uds:` address is validated and then not delivered.
const NO_SOCKET: &str = "A message to another session travels over that session's socket, and this build has no such transport yet. A member of this team is reached by its bare name.";

/// A member of the team whose name the name grammar refuses — impossible
/// through this build's own registration, and answered rather than trusted.
const UNADDRESSABLE: &str = "This team is holding a member under a name that cannot be addressed:";

/// A write that did not land, ahead of what the mailbox said about it.
const UNWRITTEN: &str = "The message could not be written to that teammate's inbox:";

/// What became of a message that did land.
const WRITTEN: &str = "It is in that inbox and will be read on the next pass.";

/// What the roster says about the one member that is not a teammate.
const LEADS: &str = "the session that leads this team";

/// What it says about the ones that are, ahead of the surface each runs on.
const RUNS_ON: &str = "a teammate on the";

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
        .map(|agent| Offered {
            name: agent.name.clone(),
            description: agent.description.clone(),
        })
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
                state
                    .storage
                    .load_info(&id)
                    .ok()
                    .flatten()
                    .filter(|info| info.parent.is_some())?;
                let transcript = state.storage.load_transcript(&id).unwrap_or_default();

                Some((id, transcript))
            },
        );

        match resumed {
            Some((session, history)) => Self {
                session,
                model,
                history,
                fresh: false,
            },
            // A `task_id` the store cannot answer for starts a fresh session
            // rather than failing the call: the model asked for work to happen,
            // and the id was a hint about where to continue it
            // (deviation: task-id-miss-starts-fresh).
            None => Self {
                session: SessionId::ascending(),
                model,
                history: Vec::new(),
                fresh: true,
            },
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
                create(
                    state,
                    &self.session,
                    agent,
                    &request.description,
                    &self.model,
                );
            }
            Persist::new(Arc::clone(state), self.session.clone())
        });

        let permissions = {
            let parent = host
                .permissions
                .lock()
                .expect("the permission rules are never poisoned");
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
                    let name = name_of(&watched, tool, input);
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
            Event::MessageStarted {
                session_id: _,
                message,
            } => {
                // A compaction summary arrives as a complete assistant message
                // rather than a streamed one; it is not the child's answer.
                if message.role == Role::User {
                    outcome.text.clear();
                    open = None;
                }
            }
            Event::MessageFinished {
                reason,
                usage,
                error,
                ..
            } => {
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
            Event::RevertChanged { .. }
            | Event::AgentChanged { .. }
            | Event::SteerConsumed { .. }
            | Event::PermissionModeChanged { .. }
            | Event::EffortChanged { .. } => {}
        }
    }

    outcome
}

/// How a running child call is named on the parent's row.
///
/// Upstream shows the tool's own `state.title`, which its running parts carry
/// and ganja's do not. What stands in is the line a permission dialog would
/// have used for the same call — `read src/main.rs`, not `read` — which is the
/// same sentence by a different route
/// (deviation: task-progress-names-the-call).
fn name_of(watched: &Watched, tool: &str, input: &serde_json::Value) -> String {
    watched
        .tools
        .get(tool)
        .map_or_else(|| tool.to_owned(), |found| found.describe(input))
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
mod tests {
    use std::sync::Arc;

    use ganja_team::{MailboxMessage, TeamName, TeamsRoot};
    use tokio::sync::mpsc;

    use super::{
        Address, Body, DEFAULT_BACKEND, MemberName, NO_SOCKET, Postbox, Reserved, SpawnRequest,
        Teammate, TeammateRegistry, Undelivered, Watched, denies_task, mailbox, roster,
        subagent_rules, team, watch,
    };
    use crate::{
        agent::{self, Registry},
        config::Config,
        engine::Fanout,
        permission::{Action, Permissions, Rule},
        protocol::{Event, MessageId, Part, PartBody, PartId, SessionId, ToolState},
        tool::{
            Tool as _,
            task::{DESCRIPTION, ROSTER_HEADER, TaskTool},
        },
    };

    fn registry() -> Registry {
        Registry::from_config(&Config::default()).expect("the default config resolves agents")
    }

    /// A child's thinking is not a child's answer (bead `pwe`), and the
    /// accumulator is where that could go wrong without anybody seeing it.
    ///
    /// What the watcher collects becomes the **parent's tool result** — text
    /// the parent model reads as the report it asked for, and which every
    /// later request in the parent's conversation then carries. So a thought
    /// leaking here is a thought that reaches a model, by a route no wire
    /// encoder guards: the arm that keeps `open` where it is on a
    /// `ReasoningText` part is the whole of the barrier, and moving it into
    /// the text arm would have the child's scratch paper delivered as its
    /// conclusion.
    #[tokio::test]
    async fn a_childs_thinking_is_not_the_answer_its_parent_is_handed() {
        const THOUGHT: &str = "the-user-is-probably-testing-me";

        let (events, received) = mpsc::channel(64);
        let (parent, _parent_reader) = mpsc::channel(64);
        let watched = Watched {
            events: Arc::new(Fanout::new(parent)),
            session_id: SessionId::from("ses_parent".to_owned()),
            tools: Arc::new(crate::tool::Registry::new(Vec::new())),
            message_id: MessageId::ascending(),
            part_id: PartId::ascending(),
            command: "look something up".to_owned(),
        };

        let message_id = MessageId::ascending();
        let session_id = SessionId::from("ses_child".to_owned());
        // Thinking on *both* sides of the reply, which is the shape that makes
        // this an assertion rather than an accident: what the parent is handed
        // is the last text part the child opened (upstream's `findLast`), so a
        // trailing thought is the one that would actually be delivered as the
        // answer. A thought that only ever preceded the reply would be
        // overwritten by it and prove nothing.
        let mut stream = Vec::new();
        for (part, delta) in [
            (Part::reasoning_text(""), THOUGHT),
            (Part::text(""), "the answer"),
            (Part::reasoning_text(""), "and-a-trailing-second-thought"),
        ] {
            let part_id = part.id.clone();
            stream.push(Event::PartStarted {
                session_id: session_id.clone(),
                message_id: message_id.clone(),
                part,
            });
            stream.push(Event::PartDelta {
                session_id: session_id.clone(),
                message_id: message_id.clone(),
                part_id,
                delta: delta.to_owned(),
            });
        }
        for event in stream {
            events.send(event).await.expect("the watcher is listening");
        }
        drop(events);

        let outcome = watch(received, watched).await;

        assert_eq!(
            outcome.text, "the answer",
            "the parent is handed the child's conclusion and nothing else"
        );
        assert!(
            !outcome.text.contains(THOUGHT) && !outcome.text.contains("second-thought"),
            "the child's thinking reached the parent's tool result: {}",
            outcome.text
        );
    }

    /// The log under the row (2026-08-15): every distinct call the child
    /// makes joins `calls` in order, a call republishing its running part as
    /// it streams joins once, the cap keeps the newest, and the finished
    /// outcome carries the log out to the completed part's metadata.
    #[tokio::test]
    async fn the_watcher_logs_the_childs_calls_in_order_and_keeps_the_newest() {
        let (events, received) = mpsc::channel(2048);
        let (parent, parent_reader) = mpsc::channel(64);
        // The reports the watcher publishes are nobody's to read here, and an
        // undrained lossless subscriber would park the watcher at the
        // channel's cap — dropping it makes every report a cheap refusal.
        drop(parent_reader);
        let watched = Watched {
            events: Arc::new(Fanout::new(parent)),
            session_id: SessionId::from("ses_parent".to_owned()),
            tools: Arc::new(crate::tool::Registry::new(Vec::new())),
            message_id: MessageId::ascending(),
            part_id: PartId::ascending(),
            command: "map it".to_owned(),
        };

        let message_id = MessageId::ascending();
        let session_id = SessionId::from("ses_child".to_owned());
        let call = |index: usize, state: ToolState| Part {
            id: PartId::from(format!("prt_{index}")),
            body: PartBody::Tool {
                call_id: format!("call_{index}"),
                tool: format!("tool-{index}"),
                state,
            },
        };
        for index in 0..105 {
            events
                .send(Event::PartStarted {
                    session_id: session_id.clone(),
                    message_id: message_id.clone(),
                    part: call(index, ToolState::Pending { input: None }),
                })
                .await
                .expect("the watcher is listening");
            // The same running part twice, the way a streaming call
            // republishes: one row in the log, not two.
            for _ in 0..2 {
                events
                    .send(Event::PartUpdated {
                        session_id: session_id.clone(),
                        message_id: message_id.clone(),
                        part: call(
                            index,
                            ToolState::Running {
                                input: serde_json::Value::Null,
                                metadata: serde_json::Value::Null,
                                started: 0,
                            },
                        ),
                    })
                    .await
                    .expect("the watcher is listening");
            }
        }
        drop(events);

        let outcome = watch(received, watched).await;

        assert_eq!(outcome.toolcalls, 105, "the count is the true total");
        assert_eq!(
            outcome.calls.len(),
            super::CALL_LOG,
            "the log holds exactly the cap"
        );
        assert_eq!(
            outcome.calls.first().map(String::as_str),
            Some("tool-5"),
            "the oldest five fell off the cap"
        );
        assert_eq!(
            outcome.calls.last().map(String::as_str),
            Some("tool-104"),
            "the newest call ends the log"
        );
    }

    #[test]
    fn the_description_is_upstreams_text_followed_by_the_callers_roster() {
        let agents = registry();
        let build = agents.get(agent::BUILD).expect("build is builtin");
        let tool = TaskTool::new(&roster(&agents, build));
        let described = tool.description();

        assert!(
            described.starts_with(DESCRIPTION),
            "upstream's text comes first, unedited"
        );
        // Only the tail past the header is the roster: upstream's own text
        // carries `- ` bullets of its own.
        let (_, listed) = described
            .split_once(ROSTER_HEADER)
            .expect("the roster header is appended");
        let roster: Vec<&str> = listed
            .lines()
            .filter(|line| line.starts_with("- "))
            .collect();
        assert_eq!(roster.len(), 2, "two subagents ship: {roster:?}");
        assert!(roster[0].starts_with("- explore: "), "sorted by name");
        assert!(roster[1].starts_with("- general: "));
    }

    /// The planning agent denies `task: general`, so what it may delegate to
    /// is what is left.
    #[test]
    fn an_agent_that_denies_a_subagent_is_not_offered_it() {
        let agents = registry();
        let plan = agents.get(agent::PLAN).expect("plan is builtin");
        let tool = TaskTool::new(&roster(&agents, plan));
        let described = tool.description();

        assert!(described.contains("- explore: "));
        assert!(
            !described.contains("- general: "),
            "plan denies task:general: {described}"
        );
    }

    #[test]
    fn a_subagent_may_not_delegate_and_may_not_keep_a_todo_list() {
        let agents = registry();
        let explore = agents.get(agent::EXPLORE).expect("explore is builtin");
        let rules = subagent_rules(explore, &Permissions::default());

        assert!(denies_task(&rules, "general"));
        assert_eq!(
            rules
                .iter()
                .rev()
                .find(|rule| rule.permission == "todowrite")
                .map(|rule| rule.action.clone()),
            Some(Action::Deny)
        );
    }

    /// `general` already says something about `todowrite`, so upstream leaves
    /// that decision alone rather than appending a second one.
    #[test]
    fn a_subagent_that_already_rules_on_todowrite_keeps_its_own_rule() {
        let agents = registry();
        let general = agents.get(agent::GENERAL).expect("general is builtin");
        let rules = subagent_rules(general, &Permissions::default());

        assert_eq!(
            rules
                .iter()
                .filter(|rule| rule.permission == "todowrite")
                .count(),
            1,
            "the appended denial would be a second one: {rules:?}"
        );
    }

    #[test]
    fn a_parents_denial_reaches_the_child_and_a_parents_allowance_does_not() {
        let mut parent = Permissions::default();
        parent.set_baseline(vec![
            Rule {
                permission: "webfetch".to_owned(),
                pattern: "*".to_owned(),
                action: Action::Deny,
            },
            Rule {
                permission: "bash".to_owned(),
                pattern: "cargo *".to_owned(),
                action: Action::Allow,
            },
        ]);

        let agents = registry();
        let general = agents.get(agent::GENERAL).expect("general is builtin");
        let rules = subagent_rules(general, &parent);

        assert!(
            rules
                .iter()
                .any(|rule| rule.permission == "webfetch" && rule.action == Action::Deny),
            "a denial travels down: {rules:?}"
        );
        assert!(
            !rules.iter().any(|rule| rule.pattern == "cargo *"),
            "an allowance does not: {rules:?}"
        );
    }

    /// One teammate, its postbox, and the team both are members of.
    ///
    /// Every root is handed in — the store, the teams directory — so nothing
    /// here touches process-wide state and the module keeps holding one tests
    /// module rather than earning a binary.
    struct Team {
        /// Dropping this deletes the tree both roots are under.
        _home: tempfile::TempDir,
        root: TeamsRoot,
        team: TeamName,
        registry: Arc<TeammateRegistry>,
        /// The postbox of a teammate called `worker`.
        worker: Postbox,
    }

    impl Team {
        async fn new() -> Self {
            let home = ganja_testkit::temp_dir();
            let root = TeamsRoot::new(home.path().join("teams"));
            let team = TeamName::parse("session-abcd1234").expect("a team name");
            let registry = Arc::new(TeammateRegistry::new(
                root.clone(),
                team.clone(),
                "01998ad0-0000-7000-8000-000000000000",
                home.path(),
            ));
            registry
                .spawn(
                    Arc::new(crate::teammate::InProcess::new(
                        Arc::new(crate::provider::FakeProvider::new(
                            "on it",
                            std::time::Duration::ZERO,
                        )),
                        Arc::new(crate::tool::Registry::new(Vec::new())),
                        crate::Storage::open(home.path().join("storage")),
                        |_| Permissions::default(),
                    )),
                    SpawnRequest {
                        name: "worker".to_owned(),
                        backend: DEFAULT_BACKEND,
                        agent_type: "general".to_owned(),
                        model: "recorder-model".to_owned(),
                        color: None,
                        prompt: "hold the fort".to_owned(),
                        cwd: home.path().to_path_buf(),
                        plan_mode_required: false,
                        bypass: false,
                    },
                )
                .await
                .expect("a teammate joins");

            // A second `Teammate` under the name the registry just resolved,
            // because the one the registry made is behind its own handle and a
            // postbox only ever reads a name off the value it is given. What
            // this stands in for is the installation the registry itself does
            // when it starts a teammate's engine; what it proves is what
            // `Postbox::of` binds.
            let teammate = Teammate::new(
                "worker",
                Arc::new(crate::provider::FakeProvider::new(
                    "on it",
                    std::time::Duration::ZERO,
                )),
                "recorder-model",
                Arc::new(crate::tool::Registry::new(Vec::new())),
                Permissions::default(),
                crate::Storage::open(home.path().join("storage")),
            );
            let worker = Postbox::of(Arc::clone(&registry), &teammate);

            Self {
                _home: home,
                root,
                team,
                registry,
                worker,
            }
        }

        /// Every message in `name`'s inbox that checked out.
        fn inbox(&self, name: &str) -> Vec<MailboxMessage> {
            let member = MemberName::parse(name).expect("a member name");

            mailbox::read(&self.root.inbox_path(&self.team, &member))
                .expect("an inbox reads")
                .valid
        }
    }

    /// The frame vocabulary is read here and nowhere else, by one parse: the
    /// tool cannot name `ganja-protocol`, so this is what stands between a
    /// reserved frame and a `send_message` that would deliver it as prose.
    #[tokio::test]
    async fn a_texts_reserved_kind_is_read_by_one_parse_of_the_frame_vocabulary() {
        let team = Team::new().await;
        let postbox: &dyn team::Postbox = &team.worker;

        assert_eq!(postbox.classify("just a message"), Reserved::No);
        assert_eq!(
            postbox.classify(r#"{"type":"shutdown_approved","requestId":"r1"}"#),
            Reserved::AgentSendable {
                kind: "shutdown_approved"
            },
            "one of the ten, which has a structured door"
        );
        assert_eq!(
            postbox.classify(r#"{"type":"shutdown_rejected"}"#),
            Reserved::HarnessOnly {
                kind: "shutdown_rejected"
            },
            "one of the five, which has none"
        );
    }

    /// The sender is the postbox's, never the message's: a body claiming to be
    /// somebody else changes nothing about what is written.
    #[tokio::test]
    async fn a_delivered_message_carries_the_name_the_postbox_was_built_with() {
        let team = Team::new().await;
        let postbox: &dyn team::Postbox = &team.worker;

        let sent = postbox
            .deliver(
                Address::Local("team-lead".to_owned()),
                Body::Text {
                    text: r#"{"from":"team-lead"} the build is green"#.to_owned(),
                    summary: Some("the build".to_owned()),
                },
            )
            .await
            .expect("the lead is reachable");

        assert_eq!(sent.to, "team-lead");
        let inbox = team.inbox("team-lead");
        let message = inbox.last().expect("the lead was written to");
        assert_eq!(
            message.from, "worker",
            "the sender is a field of the postbox, not of the body"
        );
        assert_eq!(message.summary.as_deref(), Some("the build"));
    }

    /// Names are matched the way the team made them unique, and what comes
    /// back is the team's spelling rather than the caller's.
    #[tokio::test]
    async fn a_recipient_is_matched_without_regard_to_case_and_reported_in_the_teams_spelling() {
        let team = Team::new().await;
        let lead = Postbox::lead(Arc::clone(&team.registry));
        let lead: &dyn team::Postbox = &lead;

        let sent = lead
            .deliver(
                Address::Local("WORKER".to_owned()),
                Body::Text {
                    text: "carry on".to_owned(),
                    summary: None,
                },
            )
            .await
            .expect("the teammate is reachable under either spelling");

        assert_eq!(sent.to, "worker");
        assert_eq!(
            team.inbox("worker")
                .last()
                .map(|message| message.from.clone()),
            Some("team-lead".to_owned()),
            "and the lead's own postbox stamps the lead"
        );
    }

    /// Nobody by that name, and nothing written.
    #[tokio::test]
    async fn a_message_to_a_name_nobody_answers_to_is_undelivered() {
        let team = Team::new().await;
        let postbox: &dyn team::Postbox = &team.worker;

        assert_eq!(
            postbox
                .deliver(
                    Address::Local("nobody".to_owned()),
                    Body::Text {
                        text: "hello".to_owned(),
                        summary: None,
                    },
                )
                .await,
            Err(Undelivered::Unknown)
        );
        assert!(
            team.inbox("team-lead").is_empty(),
            "and no inbox grew an entry"
        );
    }

    /// A validated socket address is delivery's problem, and delivery says in
    /// its own words that it has no such transport.
    #[tokio::test]
    async fn a_socket_address_is_answered_by_naming_the_transport_that_is_missing() {
        let team = Team::new().await;
        let postbox: &dyn team::Postbox = &team.worker;

        assert_eq!(
            postbox
                .deliver(
                    Address::Uds {
                        path: "/tmp/ganja.sock".into(),
                    },
                    Body::Text {
                        text: "hello".to_owned(),
                        summary: None,
                    },
                )
                .await,
            Err(Undelivered::NoTransport {
                reason: NO_SOCKET.to_owned(),
            })
        );
    }

    /// A caller is not in its own roster, and exactly one row leads — the
    /// invariant `send_message`'s last rung reads the lead's name out of.
    #[tokio::test]
    async fn a_caller_is_not_in_its_own_roster_and_exactly_one_row_leads() {
        let team = Team::new().await;

        let seen = team::Postbox::roster(&team.worker);
        assert_eq!(
            seen.iter()
                .map(|peer| peer.name.as_str())
                .collect::<Vec<_>>(),
            vec!["team-lead"],
            "a teammate sees the lead and not itself: {seen:?}"
        );
        assert_eq!(seen.iter().filter(|peer| peer.lead).count(), 1);

        let seen = team::Postbox::roster(&Postbox::lead(Arc::clone(&team.registry)));
        assert_eq!(
            seen.iter()
                .map(|peer| peer.name.as_str())
                .collect::<Vec<_>>(),
            vec!["worker"],
            "and the lead sees the teammate and not itself: {seen:?}"
        );
        assert_eq!(
            seen.iter().filter(|peer| peer.lead).count(),
            0,
            "so a roster carries at most one lead, and this one carries none"
        );
    }
}
