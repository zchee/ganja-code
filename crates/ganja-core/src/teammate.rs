//! A teammate's own conversation, running in the lead's process.
//!
//! Upstream opencode has **no counterpart at all**: its `task` tool delegates a
//! turn and awaits it, so nothing there outlives the call that started it. What
//! is ported is Claude Code's teammate runtime — an agent registered in the
//! lead's team, holding a mailbox, taking turns of its own until somebody shuts
//! it down. The reference sections this module answers to are §4.2 (the
//! in-process member: same registration and mailbox path, no new process) and
//! §6.1 (the runner loop that will drive it), and the plan is
//! `.omc/plans/2026-08-17-teammates-first-landing.md`.
//!
//! # D500 — who owns a teammate's turns
//!
//! **A teammate is a second [`Engine`] over a clone of the lead's [`Storage`].**
//! The two rejected shapes and the reasons are in the plan; what the tree
//! settles is that this one is nearly free and that the other two fight the
//! engine's own invariants:
//!
//! - the engine has exactly one turn slot and exactly one session stamp, and
//!   every event it emits reads that stamp — so a teammate sharing the lead's
//!   engine would either be refused as `Busy` or stream into the lead's
//!   transcript;
//! - [`Storage`] is `Clone` over an `Arc` of shared inner state whose handles
//!   are opened once, under a mutex, by whichever operation is first
//!   (`storage.rs`'s `handles`). A second engine over a clone therefore starts
//!   **no** second writer thread, runs **no** second schema migration and takes
//!   **no** second quarantine probe: it joins the store the lead already has.
//!
//! # What the shape buys, and what it costs
//!
//! A teammate engine gets its own turn slot, its own session stamp, its own
//! event fanout and its own job registry for free — which is exactly the
//! isolation a separate conversation needs. Four consequences are invariants
//! rather than conveniences, and are recorded here because each one is the kind
//! of thing a later reader would "tidy" away:
//!
//! 1. **Its read log is its own.** `Engine`'s `files` field is built fresh at
//!    construction rather than passed in, so a teammate's `read` cannot satisfy
//!    the lead's read-before-write gate. Hoisting that field into a shared
//!    construction path would silently let one conversation unlock another's
//!    writes. Pinned by `a_teammates_read_does_not_unlock_the_leads_write`.
//! 2. **It takes no snapshots.** [`crate::teammate::Teammate::new`] never
//!    calls `Engine::with_snapshots`, and [`crate::teammate::Teammate`] hands
//!    out only a shared reference to its engine, so no caller can consume one
//!    into an engine that does. `Command::Undo` on a teammate therefore answers
//!    `EngineError::NoSnapshots`, and that refusal is the intended behaviour:
//!    two engines walking one worktree's snapshot store is a hazard with no
//!    upside, since a teammate reverting the lead's files is not a feature
//!    anybody asked for. Pinned by `a_teammate_engine_refuses_undo`.
//! 3. **Sharing the tool registry is safe.** [`Registry`] holds tool
//!    *definitions* — `Arc<dyn Tool>` values with no per-session state; every
//!    per-call value a tool reads arrives in its `ToolCtx`, which the turn
//!    builds from the engine that is running it. So the lead's `Arc<Registry>`
//!    is handed in rather than rebuilt.
//! 4. **Its outbound identity is its own.** The name is bound here, at
//!    construction, and never passed per send: a `from` argument on the send
//!    path would let a teammate stamp the lead's name on a message. W4's
//!    postbox is constructed against [`crate::teammate::Teammate::name`] for
//!    that reason.
//!
//! # The session row
//!
//! A teammate is a conversation somebody may resume tomorrow, not a delegated
//! turn, so its row carries `parent: None` and `ganja sessions` lists it (D-8).
//! That falls out of this shape rather than needing a new creation path:
//! `Engine`'s own lazy create writes `parent: None`, where `subagent.rs`'s
//! reads the live session's id. The teammate's transcript therefore lands in
//! the shared store under the teammate's own id, which is what
//! `a_teammate_session_runs_one_turn_against_the_fake_provider_and_settles`
//! reads back.
//!
//! # D501 — the backend trait, and how one is chosen
//!
//! Minted here, because this is where the decision is made rather than where
//! the panes are built. Three surfaces a teammate can run on, one trait
//! ([`crate::teammate::TeammateBackend`]) and one vocabulary — the protocol's
//! own [`ganja_protocol::team::MemberBackend`], whose three spellings are
//! exactly the argument both doors take:
//!
//! | Door | Argument | Default |
//! |---|---|---|
//! | the `task` tool | `name`, `backend: "in-process" \| "pane" \| "claude"` | `in-process` |
//! | `/team spawn <name>` | `--backend in-process\|pane\|claude` | `in-process` |
//!
//! **The backend is an explicit argument on both doors, never inferred.**
//! `$TMUX` governs whether a pane backend *can run*, not which backend is
//! chosen: a session without it refuses `pane` and `claude` readably rather
//! than falling back to `in-process`, because a person who asked for a window
//! and silently got none has been lied to. That rule is enforced against real
//! panes in P25b; in this phase both pane values refuse identically with
//! [`crate::teammate::Unsupported`], and an unknown value is refused by name
//! listing the three ([`crate::teammate::parse_backend`]).
//!
//! One vocabulary rather than a second three-valued enum here: the argument,
//! the member record's `backendType` and [`ganja_protocol::team::MemberView`]
//! would otherwise be three spellings of one fact, and every seam between them
//! a place to get the mapping wrong. What is *not* shared is Claude's own
//! `backendType` word list (`"tmux"`, `"in-process"`), which stays a string
//! where it appears on somebody else's document.
//!
//! # Scope
//!
//! The construction path, the backend trait, the registry that owns a
//! teammate's lifetime and the §6.1 runner ([`crate::teammate::runner`]). The
//! permission posture is W5a/L4's, and the two pane bodies are P25b's —
//! [`crate::teammate::pane`] and [`crate::teammate::claude`] are the compiling
//! skeletons they land in, declared here because this module owns every `mod`
//! line in it.
//!
//! [`Registry`]: crate::tool::Registry

use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use futures::StreamExt as _;
use ganja_protocol::team::{MemberBackend, MemberView, TeamView};
use ganja_team::{
    MailboxMessage, MemberName, MemberRecord, NameError, Spawn, Surface, TeamFile, TeamName,
    TeamsRoot, mailbox, record, team::resolve_unique,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{
    Engine, Storage,
    engine::Evicted,
    permission::Permissions,
    protocol::{Event, PartBody, ToolState},
    provider::Provider,
    tool::Registry,
};

/// A teammate that is a real `claude` pane (P25b).
pub mod claude;
/// A teammate in a `ganja` pane of its own (P25b).
pub mod pane;
/// Killing panes the lead left behind when it died (P25b).
pub mod reaper;
/// The §6.1 loop that drives one in-process teammate.
pub mod runner;
/// The tmux calls the two pane backends are built on (P25b).
pub mod tmux;

/// One teammate: the name it answers to, and the engine its turns run on.
///
/// The engine is reachable only by shared reference. That is the whole of what
/// keeps invariant 2 above true — every `Engine::with_*` builder consumes the
/// engine, so a value that never yields an owned one cannot be given snapshots
/// after the fact.
pub struct Teammate {
    /// What the team calls this teammate, and what its outbound messages will
    /// be stamped with.
    name: String,
    /// The second engine, over the lead's store.
    engine: Engine,
    /// The lead's own registry, kept so that whatever watches this teammate can
    /// name a running call the way a permission dialog would — `read
    /// src/main.rs`, not `read`. A clone of an `Arc` the constructor was handed
    /// anyway, and emphatically *not* a second door onto the engine: the
    /// registry is tool definitions, which invariant 3 above already says are
    /// safe to share.
    tools: Arc<Registry>,
}

impl Teammate {
    /// Builds a teammate over the lead's `storage`, `tools` and `provider`.
    ///
    /// Every argument is something the lead already holds: `storage` is a clone
    /// of the lead's handle (the store itself is shared), `tools` is the lead's
    /// own registry `Arc`, and `permissions` is the ruleset the teammate runs
    /// under — derived from the lead's rather than invented, so a rule that
    /// denies the lead denies the teammate too.
    ///
    /// What is deliberately *not* passed: snapshots, for the reason in the
    /// module doc. Language servers and MCP servers are not passed either,
    /// which is a scope line rather than a rule — when a later lane shares the
    /// lead's `Arc`s for those in, it must not shut them down on the teammate's
    /// way out, because they are the lead's to close.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        provider: Arc<dyn Provider>,
        model: impl Into<String>,
        tools: Arc<Registry>,
        permissions: Permissions,
        storage: Storage,
    ) -> Self {
        Self {
            name: name.into(),
            engine: Engine::persistent(provider, model, Arc::clone(&tools), permissions, storage),
            tools,
        }
    }

    /// What the team calls this teammate.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The registry this teammate's calls are named against.
    #[must_use]
    pub fn tools(&self) -> &Arc<Registry> {
        &self.tools
    }

    /// The engine its turns run on.
    ///
    /// A caller subscribes here **before** it prompts: the engine's birth queue
    /// goes to the first lossless subscriber, so a runner that prompted first
    /// would either lose the opening events or have to claim the buffer
    /// afterwards. Subscribing first is the cheaper of the two and is what the
    /// suite pins.
    #[must_use]
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Waits for the teammate's turn tail and ends what the teammate owns.
    ///
    /// `Engine::settle` is per-engine, so a teammate's tail — its `Stop` hook
    /// included — is settled on the teammate's engine and never on the lead's;
    /// a registry tearing several down settles each before the lead's own exit
    /// path runs. Only the teammate's own background jobs are ended here.
    /// `Engine::shutdown_mcp` and `Engine::shutdown_lsp` are deliberately not
    /// called: those handles belong to the lead when a teammate has any, and
    /// they die with it.
    ///
    /// Returns whether the engine really went idle within `limit`.
    pub async fn shutdown(&self, limit: Duration) -> bool {
        let settled = self.engine.settle(limit).await;
        self.engine.shutdown_jobs().await;

        settled
    }
}

/// The three spellings the `backend` argument takes, in the order a refusal
/// lists them.
///
/// Written out rather than derived from [`MemberBackend`]'s serde renaming,
/// and checked against it by `every_backend_value_is_spelled_the_way_it_is
/// _serialized`: the argument's vocabulary and the document's have to agree,
/// and a test saying so is cheaper than a reader assuming it.
pub const BACKENDS: [&str; 3] = ["in-process", "pane", "claude"];

/// What a door spawns when nobody named a backend (**D501**).
pub const DEFAULT_BACKEND: MemberBackend = MemberBackend::InProcess;

/// Why a pane backend refuses in P25a.
///
/// One sentence for both pane values, because AC-14's P25a leg is exactly that
/// they refuse *identically*: a door that spawned where the other refused would
/// be two behaviours wearing one argument.
pub const REFUSED_UNTIL_P25B: &str =
    "this build spawns only in-process teammates; a teammate with a pane of its own lands in P25b";

/// How long a teammate is given to reach the end of its turn before what it
/// owns is ended anyway.
///
/// A teammate's tail is its own — `Engine::settle` polls that engine's turn
/// slot — so this is spent per teammate at shutdown rather than once. Five
/// seconds is the same bound the isolation suite settles a single turn under.
pub const SETTLE: Duration = Duration::from_secs(5);

/// What the door reports back when a spawn succeeded.
///
/// §4.1's own result sentence in ganja's words: the point a caller has to come
/// away with is that the call did **not** wait for the work — the prompt
/// travelled through the mailbox, and so will everything after it.
pub const SPAWNED: &str = "the teammate is running; this instruction and \
     everything after it reach it through its mailbox";

/// How many recent calls one teammate's ring holds (**D503**).
///
/// Small on purpose: it is a live view of what a teammate is doing now, drawn
/// under a row in `/team`, not a log. The full account of a teammate's work is
/// its own transcript, which is a root session anybody can open.
pub const RECENT_CALLS: usize = 8;

/// §4.3's palette, assigned round-robin and memoized per name.
const PALETTE: [&str; 4] = ["blue", "green", "pink", "purple"];

/// A `backend` argument nothing answers to.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no backend named {value:?}; the backends are {}", spell(&BACKENDS))]
pub struct UnknownBackend {
    /// What was asked for.
    pub value: String,
}

/// Reads the `backend` argument both doors take (**D501**).
///
/// # Errors
///
/// [`UnknownBackend`], naming the value and listing the three — an unknown
/// backend is a typo somebody can fix, and the fix is the list.
pub fn parse_backend(value: &str) -> Result<MemberBackend, UnknownBackend> {
    match value {
        "in-process" => Ok(MemberBackend::InProcess),
        "pane" => Ok(MemberBackend::Pane),
        "claude" => Ok(MemberBackend::Claude),
        other => Err(UnknownBackend {
            value: other.to_owned(),
        }),
    }
}

/// How a backend is spelled as an argument.
///
/// An exhaustive match rather than a lookup, so a fourth surface is a build
/// failure here instead of a value that prints as nothing.
#[must_use]
pub const fn backend_name(backend: MemberBackend) -> &'static str {
    match backend {
        MemberBackend::InProcess => BACKENDS[0],
        MemberBackend::Pane => BACKENDS[1],
        MemberBackend::Claude => BACKENDS[2],
    }
}

/// `a`, `b` and `c` — the list a refusal ends with.
fn spell(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [only] => format!("{only:?}"),
        [rest @ .., last] => format!(
            "{} and {last:?}",
            rest.iter()
                .map(|name| format!("{name:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// A backend this build cannot spawn on, and why.
///
/// Carries the backend rather than only a sentence, so a caller may act on
/// *which* surface was refused — the `/team` dialog says one thing about a
/// session with no tmux and another about a build that has no panes yet — and
/// still has one sentence to show when it does not care.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the {} backend is unavailable: {reason}", backend_name(*backend))]
pub struct Unsupported {
    /// Which surface was asked for.
    pub backend: MemberBackend,
    /// Why it could not be had, in the terms whoever asked reads next.
    pub reason: String,
}

impl Unsupported {
    /// The refusal both pane backends answer with in P25a.
    #[must_use]
    pub fn until_p25b(backend: MemberBackend) -> Self {
        Self {
            backend,
            reason: REFUSED_UNTIL_P25B.to_owned(),
        }
    }
}

/// What a backend can tell the lead about a message it handed over
/// (**D501**, spent by **D503**).
///
/// The distinction is not cosmetic: it decides how long the lead's queue strip
/// shows an entry as pending. An [`Delivery::Acknowledged`] backend prunes the
/// inbox entry when it consumes the message, so the lead can watch it
/// disappear and retire the entry on a fact. A [`Delivery::FireAndForget`]
/// backend gives no such signal — a real `claude` pane marks a message read
/// when it *reads* it, not when a turn takes it on — so an entry addressed to
/// one is retired at write time. Without the split a claude peer's message
/// sits pending in the lead's UI forever.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Consumption is observable, so the lead may wait for it.
    Acknowledged,
    /// Handing the message over is all there is to see.
    FireAndForget,
}

/// What a spawn decided, as the backend that runs it needs it.
///
/// The split from what a *backend* holds is deliberate and load-bearing: this
/// is what one spawn decided, while the provider, the tool registry, the
/// permissions and the store are the host's own handles and live on the
/// backend value. So a `SpawnSpec` is cheap to build in a test and carries
/// nothing a caller would have to invent.
///
/// `name` is the **resolved** name: [`TeammateRegistry::spawn`] runs the
/// desired one through [`ganja_team::team::resolve_unique`] first, so no
/// backend ever sees a name that could collide with a member already in the
/// team.
#[derive(Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    /// The unique name this teammate answers to, and its mailbox's basename.
    pub name: MemberName,
    /// The team it joins.
    pub team: TeamName,
    /// The team's lead — who it addresses, and whose frames it obeys.
    pub lead: MemberName,
    /// Where the team's documents live.
    pub root: TeamsRoot,
    /// Which surface it runs on.
    pub backend: MemberBackend,
    /// The `task` tool's `subagent_type`, recorded as §2.2's `agentType`.
    pub agent_type: String,
    /// The model its turns ask.
    pub model: String,
    /// §4.3's assigned colour.
    pub color: String,
    /// The spawn prompt. Travels through the mailbox rather than the command
    /// line (§4.1), so this is what the registry writes into the inbox before
    /// the backend runs — and it is persisted verbatim in the member record,
    /// which is a place a credential written into a prompt lands in cleartext.
    pub prompt: String,
    /// The directory it works in.
    pub cwd: PathBuf,
    /// Whether it must start in plan mode.
    pub plan_mode_required: bool,
    /// Whether the spawn asked for permission dialogs to be bypassed. Answered
    /// through the permission engine rather than honoured on sight — W5a/L4's
    /// posture owns what it costs; what it is is a fact about this spawn.
    pub bypass: bool,
    /// The lead's session, which §4.1 passes a pane as `--parent-session-id`.
    pub parent_session_id: String,
}

/// Renders everything except the prompt, which is rendered as a size — the
/// same rule [`ganja_team::Spawn`] states, for the same reason: the field is
/// documented as a place credentials land, so it must not be the field a
/// `{:?}` in some caller's error path prints.
impl fmt::Debug for SpawnSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SpawnSpec")
            .field("name", &self.name)
            .field("team", &self.team)
            .field("lead", &self.lead)
            .field("root", &self.root)
            .field("backend", &self.backend)
            .field("agent_type", &self.agent_type)
            .field("model", &self.model)
            .field("color", &self.color)
            .field("prompt", &format_args!("<{} bytes>", self.prompt.len()))
            .field("cwd", &self.cwd)
            .field("plan_mode_required", &self.plan_mode_required)
            .field("bypass", &self.bypass)
            .field("parent_session_id", &self.parent_session_id)
            .finish()
    }
}

impl SpawnSpec {
    /// This teammate's own inbox.
    #[must_use]
    pub fn inbox(&self) -> PathBuf {
        self.root.inbox_path(&self.team, &self.name)
    }

    /// The lead's inbox — where this teammate's own frames go.
    #[must_use]
    pub fn lead_inbox(&self) -> PathBuf {
        self.root.inbox_path(&self.team, &self.lead)
    }

    /// §2.2's derived `<name>@<team>` identity.
    #[must_use]
    pub fn agent_id(&self) -> String {
        self.name.agent_id(&self.team)
    }
}

/// What a spawn produced: the thing that has to be torn down again.
///
/// Two shapes, and the pane's is a **pair** rather than an id: `%N` recycles,
/// so a lead that killed panes by id alone would eventually kill somebody
/// else's window. The birth time tmux reports beside the id is what makes the
/// identity stable, and [`crate::teammate::reaper`] is where that comparison
/// lives.
pub enum Handle {
    /// A teammate running in this process, holding its own engine.
    InProcess(Arc<Teammate>),
    /// A teammate with a pane of its own.
    Pane {
        /// The `%N` tmux gave it.
        pane_id: String,
        /// The pane's start time, as tmux reports it.
        birth: String,
    },
}

impl fmt::Debug for Handle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InProcess(teammate) => formatter
                .debug_tuple("InProcess")
                .field(&teammate.name())
                .finish(),
            Self::Pane { pane_id, birth } => formatter
                .debug_struct("Pane")
                .field("pane_id", pane_id)
                .field("birth", birth)
                .finish(),
        }
    }
}

impl Handle {
    /// The teammate behind an in-process handle.
    #[must_use]
    pub fn teammate(&self) -> Option<&Arc<Teammate>> {
        match self {
            Self::InProcess(teammate) => Some(teammate),
            Self::Pane { .. } => None,
        }
    }

    /// What §2.2's overloaded `tmuxPaneId` records for this handle.
    #[must_use]
    pub fn surface(&self) -> Surface {
        match self {
            Self::InProcess(_) => Surface::InProcess,
            Self::Pane { pane_id, .. } => Surface::Pane {
                id: pane_id.clone(),
            },
        }
    }
}

/// One way of running a teammate.
///
/// A backend holds the host's own handles — the provider, the tool registry,
/// the store — and turns a [`SpawnSpec`] into a [`Handle`]. It knows nothing
/// about the team file or the mailbox: registration is the registry's, so a
/// backend that refuses leaves nothing behind to unwind but what the registry
/// itself wrote.
///
/// Every method is `async` because one implementation of each genuinely is:
/// killing an in-process teammate settles its turn, and spawning a pane waits
/// on a `tmux` process. The three call sites this signature is fixed against
/// from the start are [`InProcess`], [`crate::teammate::pane`] and
/// [`crate::teammate::claude`].
#[async_trait]
pub trait TeammateBackend: fmt::Debug + Send + Sync {
    /// Which surface this backend is the implementation of.
    fn backend(&self) -> MemberBackend;

    /// Runs a teammate, or says why it cannot.
    ///
    /// # Errors
    ///
    /// [`Unsupported`] when this build, or this session, cannot have the
    /// surface asked for — a pane in a session with no tmux, or either pane
    /// value in P25a.
    async fn spawn(&self, spec: &SpawnSpec) -> Result<Handle, Unsupported>;

    /// Ends what [`TeammateBackend::spawn`] produced. Idempotent: a handle
    /// whose teammate has already gone is nothing to end.
    async fn kill(&self, handle: &Handle);

    /// What this backend can tell the lead about a message it handed over.
    fn delivery(&self) -> Delivery;
}

/// A teammate in the lead's own process: the D500 shape, as a backend.
///
/// Holds what the *host* lends — every argument [`Teammate::new`] takes that a
/// spawn does not decide. `permissions` is a factory rather than a value
/// because [`Permissions`] is not [`Clone`] and each teammate engine takes its
/// own ruleset; it is also the one seam W5a/L4's posture lands in, which is why
/// it takes the whole [`SpawnSpec`] rather than nothing.
pub struct InProcess {
    provider: Arc<dyn Provider>,
    tools: Arc<Registry>,
    storage: Storage,
    permissions: Box<dyn Fn(&SpawnSpec) -> Permissions + Send + Sync>,
}

impl fmt::Debug for InProcess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("InProcess").finish_non_exhaustive()
    }
}

impl InProcess {
    /// Builds the backend over the lead's own handles.
    ///
    /// `storage` must be a **clone** of the lead's handle rather than a second
    /// [`Storage::open`] of the same path: the store's connection, writer
    /// thread and migration live behind the `Arc` a clone shares, and a second
    /// open would start all three again. Nothing in the type system says so,
    /// which is why it is said here.
    #[must_use]
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Arc<Registry>,
        storage: Storage,
        permissions: impl Fn(&SpawnSpec) -> Permissions + Send + Sync + 'static,
    ) -> Self {
        Self {
            provider,
            tools,
            storage,
            permissions: Box::new(permissions),
        }
    }
}

#[async_trait]
impl TeammateBackend for InProcess {
    fn backend(&self) -> MemberBackend {
        MemberBackend::InProcess
    }

    async fn spawn(&self, spec: &SpawnSpec) -> Result<Handle, Unsupported> {
        Ok(Handle::InProcess(Arc::new(Teammate::new(
            spec.name.as_str(),
            Arc::clone(&self.provider),
            spec.model.clone(),
            Arc::clone(&self.tools),
            (self.permissions)(spec),
            self.storage.clone(),
        ))))
    }

    async fn kill(&self, handle: &Handle) {
        if let Some(teammate) = handle.teammate()
            && !teammate.shutdown(SETTLE).await
        {
            tracing::warn!(
                teammate = teammate.name(),
                "a teammate was still working when its lifetime ended"
            );
        }
    }

    fn delivery(&self) -> Delivery {
        // The runner prunes an inbox entry when it takes the message into a
        // turn, so the lead can retire its queue entry on having watched that
        // happen rather than on having written it.
        Delivery::Acknowledged
    }
}

/// What a door asks for. The team's own half — the root, the team, the lead
/// and a name nothing else answers to — is [`TeammateRegistry`]'s to fill in.
#[derive(Clone, PartialEq, Eq)]
pub struct SpawnRequest {
    /// The name asked for, before it is made unique.
    pub name: String,
    /// Which surface, explicitly. Both doors default it to
    /// [`DEFAULT_BACKEND`]; neither infers it.
    pub backend: MemberBackend,
    /// The `task` tool's `subagent_type`.
    pub agent_type: String,
    /// The model its turns ask.
    pub model: String,
    /// A colour, when the caller has one in mind; §4.3's palette otherwise.
    pub color: Option<String>,
    /// What it is being asked to do.
    pub prompt: String,
    /// Where it works.
    pub cwd: PathBuf,
    /// Whether it starts in plan mode.
    pub plan_mode_required: bool,
    /// Whether the spawn asked for permission dialogs to be bypassed.
    pub bypass: bool,
}

impl fmt::Debug for SpawnRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SpawnRequest")
            .field("name", &self.name)
            .field("backend", &self.backend)
            .field("agent_type", &self.agent_type)
            .field("model", &self.model)
            .field("color", &self.color)
            .field("prompt", &format_args!("<{} bytes>", self.prompt.len()))
            .field("cwd", &self.cwd)
            .field("plan_mode_required", &self.plan_mode_required)
            .field("bypass", &self.bypass)
            .finish()
    }
}

/// What a door tells the model after a spawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spawned {
    /// The name the teammate really answers to, which is not always the one
    /// that was asked for.
    pub name: MemberName,
    /// Its `<name>@<team>` identity.
    pub agent_id: String,
    /// The surface it runs on.
    pub backend: MemberBackend,
    /// What its backend can tell the lead about a delivery.
    pub delivery: Delivery,
    /// [`SPAWNED`], as the thing the caller reads.
    pub note: &'static str,
}

/// Why a teammate could not be started.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    /// The name was refused, or could not be made unique.
    #[error(transparent)]
    Name(#[from] NameError),
    /// The surface was refused.
    #[error(transparent)]
    Unsupported(#[from] Unsupported),
    /// The inbox could not be seeded or written.
    #[error(transparent)]
    Mailbox(#[from] ganja_team::MailboxError),
    /// The team file could not be read or written.
    #[error("the team file at {path} could not be {doing}: {source}")]
    TeamFile {
        /// What was being done to it.
        doing: &'static str,
        /// Where it is.
        path: String,
        /// What went wrong.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The blocking task the file work runs on did not come back — a panic in
    /// it, or a runtime shutting down under it. Its own variant rather than an
    /// I/O one, because nothing was read or written and saying so would be a
    /// guess about which.
    #[error("a teams-directory operation was lost: {0}")]
    Lost(#[from] tokio::task::JoinError),
}

/// Every teammate this session started, and the lifetime none of its turns
/// owns.
///
/// [`crate::job::JobRegistry`]'s shape, for [`crate::job::JobRegistry`]'s
/// reason: the root cancellation token is a child of nothing a turn holds, so
/// cancelling a turn never ends a teammate, and [`TeammateRegistry::shutdown`]
/// is on the engine's own exit path beside `shutdown_jobs`. What differs is
/// what a shutdown *waits* for — a teammate's turn is settled, not killed,
/// because its transcript is a session somebody may open tomorrow.
///
/// # What lives here and what lives on disk
///
/// The team file and the mailboxes are Claude's documents and are written
/// through `ganja-team`. The ring of recent calls (**D503**) is emphatically
/// **not**: it is live state, worthless once the process exits and misleading
/// if a resumed session showed a stale one, and writing it into
/// [`ganja_team::MemberRecord`] would be an unstated amendment to a format a
/// real `claude` also reads. It is surfaced through
/// [`ganja_protocol::team::MemberView`], which is ganja's own projection and
/// exists for exactly that.
pub struct TeammateRegistry {
    root: TeamsRoot,
    team: TeamName,
    lead: MemberName,
    lead_session_id: String,
    cwd: PathBuf,
    /// Every runner's token is a child of this one, so one cancel ends them
    /// all and no turn's cancel ends any of them.
    cancel: CancellationToken,
    members: Mutex<BTreeMap<String, Arc<Member>>>,
    colors: Mutex<Colors>,
    /// The tasks a spawn started, kept so a shutdown can wait for them to
    /// actually finish rather than only ask them to.
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl fmt::Debug for TeammateRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TeammateRegistry")
            .field("team", &self.team)
            .field("lead", &self.lead)
            .field("members", &self.members().len())
            .finish_non_exhaustive()
    }
}

/// One teammate, as the registry holds it.
struct Member {
    name: MemberName,
    agent_id: String,
    backend: MemberBackend,
    color: String,
    handle: Handle,
    /// The backend that made [`Member::handle`], so the same implementation
    /// that spawned it is the one that ends it.
    surface: Arc<dyn TeammateBackend>,
    /// **D503**'s ring: what this teammate most recently did, newest last.
    recent: Arc<Mutex<VecDeque<String>>>,
    /// Cleared when the runner's task ends, so a teammate that shut itself
    /// down stops being listed without the registry having to be told.
    alive: Arc<AtomicBool>,
}

/// §4.3's colour assignment: `palette[index % len]`, with a monotonically
/// increasing index, memoized per name.
#[derive(Debug, Default)]
struct Colors {
    assignments: BTreeMap<String, String>,
    index: usize,
}

impl TeammateRegistry {
    /// An empty registry over the team `lead_session_id` leads.
    ///
    /// The team's directory is not touched until something is spawned into it:
    /// a session that never spawns a teammate leaves no team on disk.
    #[must_use]
    pub fn new(
        root: TeamsRoot,
        team: TeamName,
        lead_session_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        Self {
            root,
            team,
            lead: MemberName::lead(),
            lead_session_id: lead_session_id.into(),
            cwd: cwd.into(),
            cancel: CancellationToken::new(),
            members: Mutex::new(BTreeMap::new()),
            colors: Mutex::new(Colors::default()),
            tasks: Mutex::new(Vec::new()),
        }
    }

    /// The team these teammates are members of.
    #[must_use]
    pub fn team(&self) -> &TeamName {
        &self.team
    }

    /// The lead's own name — what [`ganja_protocol::team::LeadFrame::parse`]
    /// is checked against, and the one member of this roster that is not a
    /// teammate.
    #[must_use]
    pub fn lead(&self) -> &MemberName {
        &self.lead
    }

    /// Where the team's documents live.
    #[must_use]
    pub fn root(&self) -> &TeamsRoot {
        &self.root
    }

    /// The lead's inbox — where a teammate's frames arrive.
    #[must_use]
    pub fn lead_inbox(&self) -> PathBuf {
        self.root.inbox_path(&self.team, &self.lead)
    }

    /// How many teammates are still running, which is what the status bar
    /// counts.
    #[must_use]
    pub fn running(&self) -> usize {
        self.members()
            .values()
            .filter(|member| member.alive.load(Ordering::Relaxed))
            .count()
    }

    /// The team as a frontend renders it (**D503**).
    ///
    /// The lead is always the first row and is always the only row with
    /// [`MemberView::is_lead`] set — the invariant `ganja-tool`'s roster
    /// depends on, held here rather than re-checked per call. A teammate that
    /// has shut itself down is not listed: its record on disk is the lead's to
    /// retire when it reads the `shutdown_approved`.
    #[must_use]
    pub fn view(&self) -> TeamView {
        let mut members = vec![MemberView {
            name: self.lead.as_str().to_owned(),
            agent_id: self.lead.agent_id(&self.team),
            backend: MemberBackend::InProcess,
            color: None,
            is_lead: true,
            recent_calls: Vec::new(),
        }];
        members.extend(
            self.members()
                .values()
                .filter(|member| member.alive.load(Ordering::Relaxed))
                .map(|member| MemberView {
                    name: member.name.as_str().to_owned(),
                    agent_id: member.agent_id.clone(),
                    backend: member.backend,
                    color: Some(member.color.clone()),
                    is_lead: false,
                    recent_calls: member
                        .recent
                        .lock()
                        .expect("the call ring is never poisoned")
                        .iter()
                        .cloned()
                        .collect(),
                }),
        );

        TeamView {
            team: self.team.as_str().to_owned(),
            lead: self.lead.as_str().to_owned(),
            members,
        }
    }

    /// Registers a teammate, seeds its mailbox with the task it was given, and
    /// starts it.
    ///
    /// §4.1's sequence, with one step moved and the move deliberate. The
    /// reference creates the surface, writes the member record, seeds the
    /// inbox, writes the spawn prompt and only then launches the command —
    /// four steps of which the first and the last are one call here, because
    /// [`TeammateBackend::spawn`] is what yields the surface a record has to
    /// name. So the record is written **after** the backend answers, and a
    /// refused spawn leaves no member behind rather than one somebody has to
    /// clean up. What a pane needs before it starts — its inbox and the task in
    /// it — is still written first; what it does *not* need is the record,
    /// because §4.1 hands a pane its own name on the command line.
    ///
    /// A refused spawn also takes the prompt back out of the inbox: leaving
    /// somebody's instructions in a mailbox nothing will ever read is the one
    /// half of a failed spawn that would still be visible tomorrow.
    ///
    /// # Errors
    ///
    /// [`SpawnError::Name`] for a name that is refused or cannot be made
    /// unique, [`SpawnError::Unsupported`] for a surface this build or this
    /// session cannot have, and the two I/O variants for a team file or a
    /// mailbox that would not be written.
    pub async fn spawn(
        &self,
        backend: Arc<dyn TeammateBackend>,
        request: SpawnRequest,
    ) -> Result<Spawned, SpawnError> {
        let taken = self.taken().await?;
        let name = resolve_unique(&request.name, taken.iter().map(String::as_str))?;
        let color = request
            .color
            .clone()
            .unwrap_or_else(|| self.color_for(name.as_str()));
        let spec = SpawnSpec {
            name,
            team: self.team.clone(),
            lead: self.lead.clone(),
            root: self.root.clone(),
            backend: request.backend,
            agent_type: request.agent_type,
            model: request.model,
            color,
            prompt: request.prompt,
            cwd: request.cwd,
            plan_mode_required: request.plan_mode_required,
            bypass: request.bypass,
            parent_session_id: self.lead_session_id.clone(),
        };

        let seeded = seed_inbox(&spec).await?;
        let handle = match backend.spawn(&spec).await {
            Ok(handle) => handle,
            Err(unsupported) => {
                unseed_inbox(&spec, seeded).await;
                return Err(SpawnError::Unsupported(unsupported));
            }
        };
        if let Err(error) = self.record(&spec, &handle).await {
            backend.kill(&handle).await;
            unseed_inbox(&spec, seeded).await;
            return Err(error);
        }

        let spawned = Spawned {
            name: spec.name.clone(),
            agent_id: spec.agent_id(),
            backend: request.backend,
            delivery: backend.delivery(),
            note: SPAWNED,
        };
        self.start(&spec, handle, backend);

        Ok(spawned)
    }

    /// Ends every teammate and waits for each one to really be gone.
    ///
    /// The order is the one the lead's own exit path already keeps: the runners
    /// are told to stop, each teammate's turn is settled and its background
    /// jobs ended, and only then are the tasks joined. What is emphatically not
    /// done is `shutdown_mcp` or `shutdown_lsp` on a teammate engine — those
    /// handles are the lead's, shut down one layer below the engine by the
    /// frontend that owns them, and a teammate closing them would be a second
    /// close of somebody else's servers.
    pub async fn shutdown(&self) {
        self.cancel.cancel();

        let members: Vec<Arc<Member>> = self.members().values().map(Arc::clone).collect();
        for member in members {
            member.surface.kill(&member.handle).await;
        }

        let tasks =
            std::mem::take(&mut *self.tasks.lock().expect("the task list is never poisoned"));
        for task in tasks {
            let _ = task.await;
        }
    }

    fn members(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Arc<Member>>> {
        self.members
            .lock()
            .expect("the member map is never poisoned")
    }

    /// §4.3's assignment, memoized: asking twice for one name answers twice
    /// with one colour, and the index only ever moves forward.
    fn color_for(&self, name: &str) -> String {
        let mut colors = self.colors.lock().expect("the palette is never poisoned");
        if let Some(color) = colors.assignments.get(name) {
            return color.clone();
        }

        let color = PALETTE[colors.index % PALETTE.len()].to_owned();
        colors.index += 1;
        colors.assignments.insert(name.to_owned(), color.clone());

        color
    }

    /// Every name a new teammate may not be given: the team file's members —
    /// the lead among them, which is what keeps a teammate from taking the
    /// lead's name — and everything this registry has started, running or not.
    async fn taken(&self) -> Result<Vec<String>, SpawnError> {
        let mut taken: Vec<String> = self
            .read_team()
            .await?
            .members
            .iter()
            .map(|member| member.name.clone())
            .collect();
        taken.push(self.lead.as_str().to_owned());
        taken.extend(self.members().keys().cloned());

        Ok(taken)
    }

    /// The team file, or the team it would be if nothing has written one yet.
    async fn read_team(&self) -> Result<TeamFile, SpawnError> {
        let path = self.root.config_path(&self.team);
        let team = self.team.clone();
        let session = self.lead_session_id.clone();
        let cwd = self.cwd.to_string_lossy().into_owned();

        blocking(move || match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|error| SpawnError::TeamFile {
                doing: "read",
                path: path.display().to_string(),
                source: Box::new(error),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(TeamFile::new(&team, session, cwd, record::now_millis()))
            }
            Err(error) => Err(SpawnError::TeamFile {
                doing: "read",
                path: path.display().to_string(),
                source: Box::new(error),
            }),
        })
        .await
    }

    /// Adds this teammate to the team file, re-reading it under the same
    /// blocking hop that writes it back.
    async fn record(&self, spec: &SpawnSpec, handle: &Handle) -> Result<(), SpawnError> {
        let record = MemberRecord::teammate(
            &spec.name,
            &spec.team,
            Spawn {
                agent_type: spec.agent_type.clone(),
                model: spec.model.clone(),
                color: spec.color.clone(),
                prompt: spec.prompt.clone(),
                plan_mode_required: spec.plan_mode_required,
                surface: handle.surface(),
                cwd: spec.cwd.to_string_lossy().into_owned(),
            },
            record::now_millis(),
        );
        let mut file = self.read_team().await?;
        file.members.retain(|member| member.name != record.name);
        file.members.push(record);
        self.write_team(file).await
    }

    /// Writes the team file whole, through a staged file and a rename: a reader
    /// sharing this directory — a real `claude` among them — sees the old
    /// document or the new one and never half of either.
    async fn write_team(&self, file: TeamFile) -> Result<(), SpawnError> {
        let path = self.root.config_path(&self.team);

        blocking(move || {
            let failed = |doing: &'static str, source: Box<dyn std::error::Error + Send + Sync>| {
                SpawnError::TeamFile {
                    doing,
                    path: path.display().to_string(),
                    source,
                }
            };
            let document =
                record::document(&file).map_err(|error| failed("encoded", Box::new(error)))?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| failed("written", Box::new(error)))?;
            }
            let staged = path.with_extension(format!("json.new-{}", std::process::id()));
            std::fs::write(&staged, document)
                .map_err(|error| failed("written", Box::new(error)))?;
            std::fs::rename(&staged, &path).map_err(|error| failed("written", Box::new(error)))
        })
        .await
    }

    /// Registers the teammate in memory and starts what watches it.
    ///
    /// Two subscribers, each with its own reason. The ring reads a
    /// **droppable** subscription (**D503**), so a reader that falls behind is
    /// evicted rather than allowed to backpressure the teammate's turn. The
    /// runner claims a **lossless** one, and must: the engine's birth queue is
    /// a lossless lane registered at construction, and an unclaimed one fills
    /// and then makes the teammate's own first turn wait on nobody. Both are
    /// registered before the runner's first pass can prompt.
    fn start(&self, spec: &SpawnSpec, handle: Handle, backend: Arc<dyn TeammateBackend>) {
        let recent = Arc::new(Mutex::new(VecDeque::with_capacity(RECENT_CALLS)));
        let alive = Arc::new(AtomicBool::new(true));
        let mut tasks = self.tasks.lock().expect("the task list is never poisoned");

        if let Some(teammate) = handle.teammate() {
            let events = teammate.engine().subscribe_droppable();
            tasks.push(tokio::spawn(fold_calls(
                events,
                Arc::clone(teammate.tools()),
                Arc::clone(&recent),
                spec.name.as_str().to_owned(),
                self.cancel.child_token(),
            )));

            let runner = runner::Runner::new(
                Arc::clone(teammate),
                self.lead.clone(),
                spec.inbox(),
                spec.lead_inbox(),
                handle.surface(),
                self.cancel.child_token(),
            );
            let alive = Arc::clone(&alive);
            tasks.push(tokio::spawn(async move {
                runner.run().await;
                alive.store(false, Ordering::Relaxed);
            }));
        }

        self.members().insert(
            spec.name.as_str().to_owned(),
            Arc::new(Member {
                name: spec.name.clone(),
                agent_id: spec.agent_id(),
                backend: spec.backend,
                color: spec.color.clone(),
                handle,
                surface: backend,
                recent,
                alive,
            }),
        );
    }
}

/// Runs one piece of `ganja-team`'s synchronous file I/O off the runtime's
/// worker threads.
///
/// That crate is synchronous on purpose — its lock waits out a peer with a
/// blocking ladder — so every call into it from here goes through this rather
/// than parking a runtime thread on a `mkdir` retry.
async fn blocking<T, F>(work: F) -> Result<T, SpawnError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, SpawnError> + Send + 'static,
{
    tokio::task::spawn_blocking(work).await?
}

/// §4.1's steps 4 and 5: the inbox exists, and the task is in it.
///
/// The prompt travels through the mailbox rather than the command line, which
/// is what makes "here is your task" and "here is a follow-up" one channel with
/// one ordering and one lock. Returns what identifies the entry, so a spawn
/// that fails afterwards can take it back out.
async fn seed_inbox(spec: &SpawnSpec) -> Result<mailbox::Identity, SpawnError> {
    let path = spec.inbox();
    let message = MailboxMessage::new(
        spec.lead.as_str(),
        spec.prompt.clone(),
        record::now_iso8601(),
    );
    let identity = mailbox::identity(&message);

    blocking(move || {
        mailbox::seed(&path).map_err(|error| SpawnError::TeamFile {
            doing: "seeded",
            path: path.display().to_string(),
            source: Box::new(error),
        })?;
        mailbox::write(&path, message)?;

        Ok(())
    })
    .await?;

    Ok(identity)
}

/// Takes a spawn prompt back out of an inbox a spawn never got to use.
///
/// Reported rather than returned: the spawn has already failed, and a cleanup
/// that failed too is a line in the log rather than a second error to explain.
async fn unseed_inbox(spec: &SpawnSpec, seeded: mailbox::Identity) {
    let path = spec.inbox();
    let outcome = tokio::task::spawn_blocking(move || mailbox::prune_delivered(&path, &[seeded]))
        .await
        .map_err(|error| error.to_string())
        .and_then(|pruned| pruned.map_err(|error| error.to_string()));

    if let Err(error) = outcome {
        tracing::warn!(
            teammate = spec.name.as_str(),
            %error,
            "a refused spawn left its prompt in an inbox"
        );
    }
}

/// Folds a teammate's own event stream into the ring `/team` draws (**D503**).
///
/// A running call is named the way a permission dialog would name it, which is
/// the same trick `task`'s watcher plays: the tool describes its own arguments,
/// so the row reads `read src/main.rs` rather than `read`. A call republishes
/// its running part as it streams, so only a *new* name joins the ring.
///
/// Ends on the registry's own token rather than on the stream: the teammate it
/// is watching is held by the registry, so its event stream would still be open
/// long after the teammate had stopped and a shutdown waiting for this task to
/// finish would wait forever.
async fn fold_calls(
    mut events: futures::stream::BoxStream<'static, Result<Event, Evicted>>,
    tools: Arc<Registry>,
    recent: Arc<Mutex<VecDeque<String>>>,
    teammate: String,
    cancel: CancellationToken,
) {
    loop {
        let next = tokio::select! {
            () = cancel.cancelled() => return,
            next = events.next() => next,
        };
        let event = match next {
            Some(Ok(event)) => event,
            Some(Err(Evicted)) => {
                tracing::warn!(
                    teammate,
                    "a teammate's recent calls fell behind and stopped being collected"
                );
                return;
            }
            None => return,
        };
        if let Event::PartUpdated { part, .. } = event
            && let PartBody::Tool { tool, state, .. } = &part.body
            && let ToolState::Running { input, .. } = state
        {
            let line = tools
                .get(tool)
                .map_or_else(|| tool.clone(), |found| found.describe(input));
            let mut ring = recent.lock().expect("the call ring is never poisoned");
            if ring.back() != Some(&line) {
                if ring.len() == RECENT_CALLS {
                    ring.pop_front();
                }
                ring.push_back(line);
            }
        }
    }
}
