//! The engine frontends drive: commands in, an ordered event stream out.
//!
//! Delivery is lossless. Events travel a bounded channel, so a producer that
//! outruns its consumer waits instead of dropping fragments; backpressure lands
//! on the turn task and never on the render loop. A single subscriber is
//! supported through P6, after which fanout gets per-subscriber queues.
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
    sync::Arc,
};

use futures::{StreamExt as _, stream::BoxStream};
use tokio::sync::{Mutex, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::{
    agent::{self, Agent},
    command,
    config::AgentMode,
    lsp, mcp,
    permission::Permissions,
    protocol::{Command, Event, Message, PartBody, Role, ToolState, Usage, now},
    provider::Provider,
    session::{
        LiveSession, Persist, RootParts, SessionState, Turn, TurnHandle, TurnKind, run_turn,
    },
    snapshot,
    storage::{self, SessionId, SessionInfo, Storage, StorageError},
    subagent,
    tool::{Credentials, FileTimes, Registry, task},
    watch,
};

/// Events the engine queues before a producer has to wait for the subscriber.
pub const EVENT_CAPACITY: usize = 1024;

/// A command the engine refused.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// A turn is streaming — or waiting on a permission — and the engine runs
    /// one turn at a time. Session switches wait for the same reason: the
    /// turn in flight is writing into the session it started on.
    #[error("a turn is already streaming; cancel it before sending another prompt")]
    Busy,
    /// [`Engine::subscribe`] was called more than once.
    #[error("the engine already has a subscriber")]
    AlreadySubscribed,
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
    /// Agent whose prompt and rules the next turn runs under. [`None`] on an
    /// engine built without a registry, where there is nothing to run as.
    agent: Option<String>,
    /// Agent the *previous* turn ran under, which is the whole of what the
    /// plan-to-build reminder needs to know. In memory only: a message does
    /// not record the agent that produced it, so a resumed session starts
    /// with no opinion about what came before.
    previous_agent: Option<String>,
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
    active: std::sync::Mutex<Active>,
    /// The half of the system prompt an agent replaces: the base prompt for
    /// the model's family, composed by [`crate::instruction::system_prompt`].
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
    environment: Option<Arc<Environment>>,
    /// Agents this session may run as. [`None`] leaves every turn on the base
    /// prompt with no agent rules, which is what an engine built for a golden
    /// run wants.
    agents: Option<Arc<agent::Registry>>,
    /// Tools as the caller handed them over, without the task tool and
    /// without anything an MCP server lent. What every rebuild below starts
    /// from.
    base_tools: Arc<Registry>,
    /// [`Engine::base_tools`] plus whatever the connected MCP servers are
    /// currently lending. What a subagent is offered — the same set the parent
    /// has, minus the task tool it never gets.
    ///
    /// Behind its own lock because a connect finishing has to change it
    /// without disturbing a turn that is already holding a snapshot.
    lent_tools: std::sync::Mutex<Arc<Registry>>,
    /// MCP servers this session was configured with, once somebody installed
    /// them. [`None`] is every engine that was never given any, which is every
    /// scripted and golden run.
    mcp: Option<Arc<mcp::Servers>>,
    /// Which [`mcp::Servers::generation`] the registries above were built
    /// from, so a rebuild happens exactly when the tool surface moved.
    mcp_installed: std::sync::Mutex<u64>,
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
    events: mpsc::Sender<Event>,
    unclaimed: Mutex<Option<mpsc::Receiver<Event>>>,
    /// Holds the handle of the turn in flight, and doubles as the idle/busy
    /// flag. The handle carries the turn's cancellation token and the
    /// permission wait a [`Command::ReplyPermission`] routes into.
    turn: Arc<Mutex<Option<TurnHandle>>>,
    /// The conversation the next request carries. On a persistent engine this
    /// is the live window — everything from the compaction summary onward —
    /// rather than the whole stored transcript.
    history: Arc<Mutex<Vec<Message>>>,
    /// The store and the live session, when this engine persists. [`None`]
    /// is [`Engine::new`]'s in-memory engine, and with it every P4 behaviour
    /// is absent: no write-through, no auto-title, no compaction.
    persistence: Option<Arc<SessionState>>,
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

        Self {
            provider,
            active: std::sync::Mutex::new(Active {
                model,
                agent: None,
                previous_agent: None,
            }),
            base_prompt: std::sync::Mutex::new(None),
            base_follows_model: false,
            prompt_suffix: std::sync::Mutex::new(None),
            environment: None,
            agents: None,
            // The task tool is never one of these: it exists only once the
            // engine knows which agents it may spawn, which is
            // `with_agents`'s business.
            base_tools: Arc::clone(&tools),
            lent_tools: std::sync::Mutex::new(Arc::clone(&tools)),
            mcp: None,
            mcp_installed: std::sync::Mutex::new(0),
            lsp: None,
            snapshots: None,
            revert: std::sync::Mutex::new(None),
            tools: std::sync::Mutex::new(tools),
            commands: Arc::new(command::Registry::builtin(&root)),
            permissions: Arc::new(std::sync::Mutex::new(permissions)),
            cwd,
            root,
            files: Arc::new(FileTimes::default()),
            // An engine whose store cannot be resolved has nothing to guard,
            // and says so in a form a construction site cannot leave blank.
            credentials: crate::auth::store_path()
                .ok()
                .map_or(Credentials::Unguarded, Credentials::Guarded),
            watcher: std::sync::Mutex::new(None),
            events,
            unclaimed: Mutex::new(Some(receiver)),
            turn: Arc::default(),
            history: Arc::default(),
            persistence,
        }
    }

    /// Sets what the model is told before it is told anything else.
    ///
    /// Consuming rather than a setter, so it composes with either constructor
    /// and cannot be called on an engine that is already streaming a turn. The
    /// prompt is captured once and carried by every request a turn makes —
    /// including the one that summarizes a conversation for compaction, which
    /// is what stops a compacted session from losing the instructions the rest
    /// of it was written under.
    ///
    /// [`None`] leaves the requests without one, which is what
    /// [`Engine::new`]'s scripted and golden runs depend on.
    #[must_use]
    pub fn with_system(self, system: Option<String>) -> Self {
        self.with_system_parts(system, None)
    }

    /// Sets the system prompt as its two halves.
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
        mut self,
        compose: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.environment = Some(Arc::new(compose));
        self.recompose_environment();

        self
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
        let Some(compose) = self.environment.as_ref() else {
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

    /// Closes every MCP connection and ends every local server's process
    /// group.
    pub async fn shutdown_mcp(&self) {
        if let Some(servers) = &self.mcp {
            servers.shutdown().await;
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
    /// Consuming for the same reason [`Engine::with_system`] is: the roster is
    /// resolved once, before anything can be streaming.
    #[must_use]
    pub fn with_commands(mut self, commands: Arc<command::Registry>) -> Self {
        self.commands = commands;

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
        let slot = self.turn.lock().await;
        if slot.is_some() {
            return Err(EngineError::Busy);
        }

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
                .events
                .send(Event::RevertChanged {
                    revert: Some(revert.info()),
                    prompt: None,
                })
                .await;
        }
        drop(slot);

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

    /// Claims the event stream.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::AlreadySubscribed`] on every call after the
    /// first: splitting one lossless queue between two readers would hand each
    /// of them an arbitrary half of the transcript.
    pub async fn subscribe(&self) -> Result<BoxStream<'static, Event>, EngineError> {
        let receiver = self
            .unclaimed
            .lock()
            .await
            .take()
            .ok_or(EngineError::AlreadySubscribed)?;

        Ok(ReceiverStream::new(receiver).boxed())
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
            Command::SendPrompt { text, mentions } => {
                self.start_turn(text, TurnKind::Prompt { mentions }, None)
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
            Command::SwitchAgent { name } => self.switch_agent(name).await,
            Command::SwitchModel { model } => self.switch_model(model).await,
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

        self.start_turn(
            definition.expand(args),
            TurnKind::Prompt {
                mentions: Vec::new(),
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
        // mid-clear must land on the new session, not race the old one.
        let turn = self.turn.lock().await;
        if turn.is_some() {
            return Err(EngineError::Busy);
        }

        self.history.lock().await.clear();
        if let Some(state) = &self.persistence {
            let mut live = state
                .live
                .lock()
                .expect("the live session is never poisoned");
            live.info = None;
            live.warned_uncataloged = false;
        }
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
        let turn = self.turn.lock().await;
        if turn.is_some() {
            return Err(EngineError::Busy);
        }
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
        let turn = self.turn.lock().await;
        if turn.is_some() {
            return Err(EngineError::Busy);
        }
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
                    .events
                    .send(Event::RevertChanged {
                        revert: None,
                        prompt: None,
                    })
                    .await;
            }
        }
        drop(turn);

        Ok(())
    }

    /// Reverts the working tree to the state `anchor`'s turn started from, and
    /// records that the session is now reverted that far.
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
        snapshots.revert(patches).await;

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
            .events
            .send(Event::RevertChanged {
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
            .events
            .send(Event::RevertChanged {
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

    /// Installs `agent`'s ruleset as the permission baseline, and rebuilds the
    /// tool set the model is offered so the task tool lists what *this* agent
    /// may delegate to.
    fn install(&self, agent: &Agent) {
        self.permissions
            .lock()
            .expect("the permission rules are never poisoned")
            .set_baseline(agent.rules.clone());

        let Some(agents) = &self.agents else {
            return;
        };
        let rebuilt = self
            .lent()
            .with(Arc::new(task::TaskTool::new(&subagent::roster(
                agents, agent,
            ))));
        *self
            .tools
            .lock()
            .expect("the tool registry is never poisoned") = Arc::new(rebuilt);
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
        // this is where that is noticed, because there is no reconnect to
        // notice it anywhere else.
        servers.reap();

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

        let lent = Arc::new(self.base_tools.with_all(servers.tools()));
        *self
            .lent_tools
            .lock()
            .expect("the tool registry is never poisoned") = Arc::clone(&lent);

        // The task tool's roster is per agent, so the offered set is rebuilt
        // through `install`, which is the one place that knows how.
        let name = self.active().agent.clone();
        let agent = self
            .agents
            .as_ref()
            .zip(name.as_deref())
            .and_then(|(registry, name)| registry.get(name));
        match agent {
            Some(agent) => self.install(agent),
            // No agents means no task tool, so the offered set *is* the lent
            // set.
            None => {
                *self
                    .tools
                    .lock()
                    .expect("the tool registry is never poisoned") = lent;
            }
        }
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

    /// What a `task` call needs to run a child loop, or [`None`] when this
    /// engine has no agents to spawn.
    fn spawn_host(&self, model: String) -> Option<Arc<subagent::Host>> {
        Some(Arc::new(subagent::Host {
            provider: Arc::clone(&self.provider),
            model,
            agents: Arc::clone(self.agents.as_ref()?),
            // A subagent is offered this build's tools minus the one that
            // spawns subagents, which is the whole of the depth limit (D9).
            // MCP tools are in that set: a subagent works on the same project
            // with the same servers. Their asks refuse unattended, because
            // nobody is watching a subagent's turn.
            tools: self.lent(),
            permissions: Arc::clone(&self.permissions),
            base_prompt: self.base_half(),
            prompt_suffix: self.environment_half(),
            cwd: self.cwd.clone(),
            root: self.root.clone(),
            credentials: self.credentials.clone(),
            lsp: self.lsp.clone(),
            persistence: self.persistence.clone(),
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

    /// Runs the rest of the session as `name`.
    async fn switch_agent(&self, name: String) -> Result<(), EngineError> {
        // Held across the whole switch, exactly as `resume` holds it: a prompt
        // that arrives mid-switch waits and then runs as the agent that was
        // asked for, rather than racing it.
        let turn = self.turn.lock().await;
        if turn.is_some() {
            return Err(EngineError::Busy);
        }

        let Some(registry) = &self.agents else {
            return Err(EngineError::NoAgents);
        };
        let Some(agent) = registry.get(&name) else {
            return Err(EngineError::UnknownAgent { name });
        };
        if agent.mode == AgentMode::Subagent {
            return Err(EngineError::SubagentNotSelectable { name });
        }

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
        self.recompose_environment();
        self.recompose_base();
        self.remember_selection();
        drop(turn);

        Ok(())
    }

    /// Asks the rest of the session's requests of `model`.
    async fn switch_model(&self, model: String) -> Result<(), EngineError> {
        let turn = self.turn.lock().await;
        if turn.is_some() {
            return Err(EngineError::Busy);
        }

        if !self.serves(&model) {
            return Err(EngineError::UnknownModel {
                model,
                provider: self.provider.id().to_owned(),
            });
        }

        self.active().model = model;
        self.recompose_environment();
        self.recompose_base();
        self.remember_selection();
        drop(turn);

        Ok(())
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
        let (model, agent) = {
            let active = self.active();
            (active.model.clone(), active.agent.clone())
        };

        let mut live = state
            .live
            .lock()
            .expect("the live session is never poisoned");
        let Some(info) = live.info.as_mut() else {
            return;
        };
        info.model = Some(model);
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

    async fn start_turn(
        &self,
        prompt: String,
        kind: TurnKind,
        overrides: Option<Overrides>,
    ) -> Result<(), EngineError> {
        let mut turn = self.turn.lock().await;
        if turn.is_some() {
            return Err(EngineError::Busy);
        }

        // Between turns and never during one: a server that connected while
        // the last turn was streaming is offered to the model here, and a
        // connection that died is withdrawn here.
        self.refresh_mcp();

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
        let (mut model, name, previous) = {
            let mut active = self.active();
            let name = active.agent.clone();
            let previous = if asks_the_model {
                std::mem::replace(&mut active.previous_agent, name.clone())
            } else {
                active.previous_agent.clone()
            };

            (active.model.clone(), name, previous)
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
        let (agent, permissions) = match overrides.as_ref().and_then(|it| it.agent.as_ref()) {
            None => (session_agent, Arc::clone(&self.permissions)),
            Some(agent) => {
                let derived = self
                    .permissions
                    .lock()
                    .expect("the permission rules are never poisoned")
                    .derive(agent.rules.clone());

                (Some(agent), Arc::new(std::sync::Mutex::new(derived)))
            }
        };
        if let Some(asked) = overrides.as_ref().and_then(|it| it.model.clone()) {
            model = asked;
        }

        let system = self.system_for(agent);
        // A command that runs as another agent is not the session switching to
        // it, so the plan-to-build notice — which is about what the *user*
        // switched to — is left to the session's own agent.
        let mut reminders = reminders(name.as_deref(), previous.as_deref());
        // Files that went stale while nobody was asking are named here, at the
        // top of the first turn that could act on what it read of them. Only a
        // turn that asks the model can deliver one — a `!` passthrough asks
        // nothing and a compaction asks a question of its own — so the queue is
        // drained by those turns alone and the notice waits for the prompt that
        // follows (deviation: stale-notice-only-on-turns-that-ask).
        if asks_the_model && let Some(notice) = stale_notice(&self.files.take_stale(), &self.root) {
            reminders.push(notice);
        }

        // The first prompt on a persistent engine mints the session, and its
        // record reaches the disk before the first byte streams: a crash
        // mid-turn must still leave something to resume. A store that
        // refuses is a warning, not a dead prompt.
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
                        fresh_session(&state.storage, name.clone(), model.clone())
                    })
                    .id
                    .clone()
            };

            Persist::new(Arc::clone(state), session)
        });

        let cancel = CancellationToken::new();
        let pending = Arc::new(std::sync::Mutex::new(None));
        *turn = Some(TurnHandle {
            cancel: cancel.clone(),
            permission: Arc::clone(&pending),
        });
        drop(turn);

        // The task is deliberately not joined. `cancel` is what stops a turn,
        // and it reaches the provider and every running tool, so an aborted
        // HTTP stream is the provider's business rather than something the
        // engine has to kill from outside. Aborting the task instead would
        // skip the cleanup that releases the busy slot and guarantees a
        // terminal event.
        let turn = Turn::root(RootParts {
            provider: Arc::clone(&self.provider),
            spawn: self.spawn_host(model.clone()),
            model,
            system,
            reminders,
            kind,
            tools: self.tools(),
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
            events: self.events.clone(),
            slot: Arc::clone(&self.turn),
            history: Arc::clone(&self.history),
            persist,
        });
        tokio::spawn(run_turn(turn));

        Ok(())
    }

    async fn cancel_turn(&self) {
        if let Some(turn) = self.turn.lock().await.as_ref() {
            turn.cancel.cancel();
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
        let delivered = self.turn.lock().await.as_ref().is_some_and(|turn| {
            let mut pending = turn
                .permission
                .lock()
                .expect("the pending permission is never poisoned");

            match pending.take_if(|waiting| waiting.id == *id) {
                // A closed receiver means the turn is already tearing down,
                // which is the same race as replying after the turn ended.
                Some(waiting) => waiting.sender.send(reply).is_ok(),
                None => false,
            }
        });

        if !delivered {
            tracing::debug!(id = id.as_str(), "no permission is waiting for this reply");
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
fn reminders(agent: Option<&str>, previous: Option<&str>) -> Vec<String> {
    let mut found = Vec::new();

    if agent == Some(agent::PLAN) {
        found.push(agent::PLAN_REMINDER.to_owned());
    }
    if agent == Some(agent::BUILD) && previous == Some(agent::PLAN) {
        found.push(agent::BUILD_SWITCH_REMINDER.to_owned());
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
fn fresh_session(storage: &Storage, agent: Option<String>, model: String) -> SessionInfo {
    let created = now();
    let info = SessionInfo {
        id: SessionId::ascending(),
        version: storage::VERSION,
        title: None,
        created,
        updated: created,
        usage: Usage::default(),
        context_tokens: 0,
        summary: None,
        agent,
        model: Some(model),
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
                ToolState::Pending => serde_json::json!({}),
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

    use super::{Engine, EngineError, STALE_FILES, stale_notice};
    use crate::{
        permission::Permissions,
        protocol::{Command, Event, FinishReason, Message, Role, Usage},
        provider::{
            ChatRequest, FakeProvider, Provider, ProviderError, ProviderEvent, fake::MODEL,
        },
        storage::{self, SessionId, SessionInfo, Storage},
        tool::{FileTimes, Registry},
    };

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
                Event::MessageStarted { message } => messages.push(message.clone()),
                Event::PartStarted { message_id, part } => {
                    if let Some(message) = messages.iter_mut().find(|it| it.id == *message_id) {
                        message.parts.push(part.clone());
                    }
                }
                Event::PartDelta {
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
                | Event::PermissionReplied { .. }
                | Event::RevertChanged { .. } => {}
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
            })
            .await
            .expect("an idle engine accepts a prompt");

        let seen = drain(&mut events).await;

        let Some(Event::MessageStarted { message: user }) = seen.first() else {
            panic!("a turn should open with the user's message, got {seen:?}");
        };
        assert_eq!(user.role, Role::User);
        assert_eq!(
            user.parts.first().and_then(|part| part.as_text()),
            Some("hi")
        );

        let Some(Event::MessageStarted { message: assistant }) = seen.get(1) else {
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
        .with_system(Some(SYSTEM.to_owned()));
        let mut events = engine.subscribe().await.expect("the first subscriber wins");
        engine.resume(&session).await.expect("the session loads");

        engine
            .send(Command::SendPrompt {
                text: "next".to_owned(),
                mentions: Vec::new(),
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

    #[tokio::test]
    async fn a_second_subscriber_is_refused() {
        let engine = engine();
        let _first = engine.subscribe().await.expect("the first subscriber wins");

        assert!(matches!(
            engine.subscribe().await,
            Err(EngineError::AlreadySubscribed)
        ));
    }

    #[tokio::test]
    async fn a_prompt_sent_mid_turn_is_refused() {
        let engine = engine();
        let mut events = engine.subscribe().await.expect("the first subscriber wins");

        engine
            .send(Command::SendPrompt {
                text: "first".to_owned(),
                mentions: Vec::new(),
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
            })
            .await
            .expect("an idle engine accepts a prompt");

        drain(&mut events).await;

        engine
            .send(Command::SendPrompt {
                text: "second".to_owned(),
                mentions: Vec::new(),
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
        std::fs::File::open(path)
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
}
