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
//! and silently got none has been lied to. Both pane values refuse
//! **identically** — one [`crate::teammate::Unsupported`] carrying
//! [`crate::teammate::tmux::REFUSED_NO_TMUX`], since a door that spawned where
//! the other refused would be two behaviours wearing one argument — and an
//! unknown value is refused by name listing the three
//! ([`crate::teammate::parse_backend`]).
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
//! teammate's lifetime and the §6.1 runner ([`crate::teammate::runner`]). What
//! a teammate is allowed to do, and who answers when it asks, is
//! [`crate::teammate::posture`]'s; the two pane bodies are
//! [`crate::teammate::pane`]'s and [`crate::teammate::claude`]'s — both shipped,
//! both real, one splitting a `ganja` of this very build and one a `claude` off
//! `PATH` — declared here because this module owns every `mod` line in it.
//!
//! [`Registry`]: crate::tool::Registry

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
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
/// The §6.2 pass the lead makes over its own inbox.
pub mod lead_inbox;
/// What a process that *is* a member holds: its postbox, and its asks on their
/// way to the lead over §5's frames.
pub mod member;
/// A teammate in a `ganja` pane of its own (P25b).
pub mod pane;
/// What a teammate may do, and who answers when it asks (**D-5**).
pub mod posture;
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
        Self::deferring(
            name,
            provider,
            model,
            tools,
            permissions,
            storage,
            crate::config::DEFAULT_TOOL_DEFER_THRESHOLD,
        )
    }

    /// The same teammate under a named advertised-schema budget (**D492**).
    ///
    /// Crate-internal because the only caller with a budget to pass is
    /// [`InProcess`], which reads the **lead's** own: a teammate offered the
    /// lead's MCP tools under a different budget from the lead's would defer a
    /// different set of them, which is one session answering two ways about
    /// one config key.
    ///
    /// The builder is what does the work, and it does it for
    /// [`Teammate::new`] too: `Engine::persistent` installs the set it is
    /// handed **verbatim**, so without a composition pass a teammate holding
    /// `mcp__*` names would advertise every one of them and be offered no
    /// `tool_search` to fetch a deferred schema back with. Composing here is
    /// also why nothing recomposes later — a teammate engine dials no MCP
    /// servers of its own, so the set it starts with is the set it keeps.
    #[must_use]
    pub(crate) fn deferring(
        name: impl Into<String>,
        provider: Arc<dyn Provider>,
        model: impl Into<String>,
        tools: Arc<Registry>,
        permissions: Permissions,
        storage: Storage,
        defer_threshold: usize,
    ) -> Self {
        Self {
            name: name.into(),
            engine: Engine::persistent(provider, model, Arc::clone(&tools), permissions, storage)
                .with_defer_threshold(defer_threshold),
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
    /// A turn that outlasts `limit` is **cancelled** rather than left running,
    /// and then given [`CANCELLED`] to unwind. Waiting is the courtesy a
    /// teammate's transcript is owed; waiting forever is not one anybody asked
    /// for, and a turn still streaming into a store the process is about to
    /// drop is the outcome both sides lose by. A cancelled turn is stored the
    /// way any cancelled turn is, so nothing is lost that was not already
    /// going to be.
    ///
    /// Returns whether the turn ended **on its own**, before anything
    /// cancelled it — which is what the callers that warn are warning about.
    pub async fn shutdown(&self, limit: Duration) -> bool {
        let settled = self.engine.settle(limit).await;
        if !settled && let Err(error) = self.engine.send(crate::protocol::Command::CancelTurn).await
        {
            tracing::warn!(
                teammate = self.name,
                %error,
                "a teammate's turn could not be cancelled on the way out"
            );
        }
        if !settled && !self.engine.settle(CANCELLED).await {
            tracing::warn!(
                teammate = self.name,
                "a teammate's turn did not end even after it was cancelled"
            );
        }
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

/// How long a teammate is given to reach the end of its turn before what it
/// owns is ended anyway.
///
/// A teammate's tail is its own — `Engine::settle` polls that engine's turn
/// slot — so this is spent per teammate at shutdown rather than once. Five
/// seconds is the same bound the isolation suite settles a single turn under.
pub const SETTLE: Duration = Duration::from_secs(5);

/// How long a teammate whose turn outlasted [`SETTLE`] is given to unwind
/// after being cancelled ([`Teammate::shutdown`]).
///
/// Short, and short on purpose: what is being waited for here is the tail of a
/// step that has already been told to stop, not a model's answer.
pub const CANCELLED: Duration = Duration::from_secs(1);

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

/// Where the teams live under this build's own config home — Claude's
/// `$CLAUDE_CONFIG_DIR/teams` (§2.1), read and written under ganja's home
/// rather than under somebody else's.
const TEAMS_DIR: &str = "teams";

/// How much of a session id §2.1's implicit team name is built from.
const TEAM_HEX: usize = 8;

/// §2.1's implicit team for a session: `session-<first 8 hex of its id>`.
///
/// The id is a bare UUIDv7 since W1 (**D493**), so its first group is eight hex
/// digits and this is a pure function of it — which is what lets a resumed
/// session rejoin the team it left rather than orphaning one, and what lets a
/// pane derive its lead's team from the `--parent-session-id` it was handed.
///
/// A stored id from before that migration is not UUID-shaped, and rather than
/// mint a name [`TeamName::parse`] would refuse, such a session joins
/// [`TeamName::default_team`] — §2.1's own fallback for a session with no team
/// of its own, and the honest answer for an id there is no team name to derive.
#[must_use]
pub fn session_team(session_id: &str) -> TeamName {
    let hex: String = session_id
        .chars()
        .take_while(|character| *character != '-')
        .filter(char::is_ascii_hexdigit)
        .take(TEAM_HEX)
        .flat_map(char::to_lowercase)
        .collect();
    if hex.len() < TEAM_HEX {
        return TeamName::default_team();
    }

    TeamName::parse(&format!("session-{hex}")).unwrap_or_else(|_| TeamName::default_team())
}

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
/// `name` is the **resolved** name: [`TeammateRegistry`]'s own `spawn` runs the
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
/// else's window. What tmux reports beside the id is `#{pane_pid}` — there is no
/// `pane_start_time` format, as [`crate::teammate::tmux`]'s module doc records
/// against `man tmux` and against a live server — so **birth is that pid**, and
/// it is what makes the identity stable for as long as the machine keeps
/// running. [`crate::teammate::reaper`] is where the comparison lives, and where
/// the cold-start case that pid cannot answer for is dealt with.
pub enum Handle {
    /// A teammate running in this process, holding its own engine.
    InProcess(Arc<Teammate>),
    /// A teammate with a pane of its own.
    Pane {
        /// The `%N` tmux gave it.
        pane_id: String,
        /// `#{pane_pid}`: the process tmux forked into the pane, fixed for the
        /// pane's life. Not a creation time — tmux reports none.
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

    /// Starts what [`TeammateBackend::spawn`] produced, once the team file
    /// names it (§4.1's step 6, after its step 2).
    ///
    /// The default does nothing, and that is the right answer for the
    /// in-process backend: a teammate is running from the moment it is
    /// spawned. A pane backend splits its window in `spawn` — that is what
    /// yields the handle a record has to name — and types the launch line
    /// **here**, because the record is the first thing the pane's process
    /// reads, and a process launched before its record exists would read a
    /// team it is not yet a member of. The registry calls this right after
    /// its record write, and nowhere else.
    ///
    /// A hook rather than a watch for the record from inside `spawn`, because
    /// a call has an unwind path and a poll does not: a launch line that could
    /// not be typed after the record was written would otherwise be a
    /// registered member holding an idle shell that nothing cleans up. That is
    /// the intent, not yet a description of the shipped pane backend: this
    /// build's [`crate::teammate::pane::GanjaPane`] still launches off its own
    /// internal record-watch — typing the line only once the team file names
    /// the member — and this hook is the migration target it moves onto
    /// (bead `ganja-code-ipg`).
    ///
    /// # Errors
    ///
    /// [`Unsupported`] when the surface could not be started — the same
    /// vocabulary a refused `spawn` answers in, because to whoever asked it is
    /// the same fact: this surface cannot be had. The registry unwinds exactly
    /// as it does for a record that would not write — the handle is killed,
    /// the record and the seeded prompt are taken back out, the name is given
    /// back — and the caller reads one refusal.
    async fn launch(&self, _spec: &SpawnSpec, _handle: &Handle) -> Result<(), Unsupported> {
        Ok(())
    }

    /// Whether this backend seeds its teammate's inbox itself, and the registry
    /// must therefore not.
    ///
    /// [`false`] for every backend whose inbox is the one [`SpawnSpec::inbox`]
    /// names, which is all of them but one: the registry writes §4.1's steps 4
    /// and 5 there before it spawns, and unwinds that write on every failing
    /// path.
    ///
    /// [`crate::teammate::claude::ClaudePane`] answers [`true`], because its
    /// teammate reads a **different root** — `$CLAUDE_CONFIG_DIR/teams`, which
    /// nothing will persuade a real `claude` to look away from (§2.1) — and it
    /// writes a different message there, [`crate::teammate::claude::preamble`]'s
    /// rather than the bare prompt. Two writers over one spawn were a defect
    /// both ways round (verify-l3 F-1): with the two roots pointed at one
    /// directory (AC-13's own configuration) the teammate's inbox held the bare
    /// task *ahead* of the preamble, so the first thing a real `claude` read was
    /// the one message that does not tell it how to address its lead — §5.5.1's
    /// exact failure; with the roots apart (the ordinary case) a second copy of
    /// somebody's prompt sat under the ganja root where nothing would ever read
    /// it. So a backend that owns the inbox owns all of it: the seed, the
    /// message, and taking it back out when its own launch is refused.
    fn owns_inbox(&self) -> bool {
        false
    }

    /// Ends what [`TeammateBackend::spawn`] produced. Idempotent: a handle
    /// whose teammate has already gone is nothing to end.
    async fn kill(&self, handle: &Handle);

    /// What this backend can tell the lead about a message it handed over.
    fn delivery(&self) -> Delivery;
}

/// A teammate in the lead's own process: the D500 shape, as a backend.
///
/// Holds what the *host* lends — every argument [`Teammate`]'s own `deferring` takes
/// that a spawn does not decide. Two of them are **factories rather than
/// values**, for two different reasons that come to the same thing: what the
/// host lends is live, and a backend built once at install time must not hand
/// out what was true then.
///
/// - `permissions`, because [`Permissions`] is not [`Clone`] and each teammate
///   engine takes its own ruleset; it is also the seam the posture lands in,
///   which is why it takes the whole [`SpawnSpec`].
/// - `tools`, because the lead's MCP servers are dialled **in the background
///   at startup** and a team installed before they answer would otherwise lend
///   every teammate of that session the empty set those servers had not filled
///   yet. Read per spawn, a teammate started after a dial gets what the dial
///   brought, and one started before it does not — which is the same rule the
///   lead's own turns run under.
pub struct InProcess {
    provider: Arc<dyn Provider>,
    tools: Box<dyn Fn() -> Arc<Registry> + Send + Sync>,
    storage: Storage,
    permissions: Box<dyn Fn(&SpawnSpec) -> Permissions + Send + Sync>,
    /// The lead's own `tool_defer_threshold`, so a teammate offered the lead's
    /// MCP tools defers the same set of them (**D492**).
    defer_threshold: usize,
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
        Self::lending(
            provider,
            move || Arc::clone(&tools),
            storage,
            permissions,
            crate::config::DEFAULT_TOOL_DEFER_THRESHOLD,
        )
    }

    /// The same backend over a tool set that is **read at each spawn** and a
    /// budget somebody named.
    ///
    /// What [`Engine::with_teammates`] builds, and the only shape that can:
    /// the engine's lent set moves as its MCP servers answer, so the handle a
    /// teammate is built from has to be the live one rather than a snapshot of
    /// it. Its sibling above is the fixed-set shape — what a caller who is not
    /// an engine has, and what a test wants.
    ///
    /// `defer_threshold` is the lead's own; see [`Teammate`]'s `deferring` for
    /// why a teammate must not be given a different one.
    ///
    /// `pub(crate)` for the same reason `install_postbox` is: the live tool
    /// handle it closes over is the *engine's* own, so the only caller that
    /// can honestly build one is the engine.
    #[must_use]
    pub(crate) fn lending(
        provider: Arc<dyn Provider>,
        tools: impl Fn() -> Arc<Registry> + Send + Sync + 'static,
        storage: Storage,
        permissions: impl Fn(&SpawnSpec) -> Permissions + Send + Sync + 'static,
        defer_threshold: usize,
    ) -> Self {
        Self {
            provider,
            tools: Box::new(tools),
            storage,
            permissions: Box::new(permissions),
            defer_threshold,
        }
    }
}

#[async_trait]
impl TeammateBackend for InProcess {
    fn backend(&self) -> MemberBackend {
        MemberBackend::InProcess
    }

    async fn spawn(&self, spec: &SpawnSpec) -> Result<Handle, Unsupported> {
        Ok(Handle::InProcess(Arc::new(Teammate::deferring(
            spec.name.as_str(),
            Arc::clone(&self.provider),
            spec.model.clone(),
            (self.tools)(),
            (self.permissions)(spec),
            self.storage.clone(),
            self.defer_threshold,
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
    /// The teammate's own inbox could not be created.
    ///
    /// Its own variant rather than [`SpawnError::TeamFile`], which is what it
    /// used to be reported as: the team file and an inbox are two different
    /// documents in two different places, and a sentence naming the wrong one
    /// sends whoever reads it to look at a file that is fine. Its own variant
    /// rather than [`SpawnError::Mailbox`] too, because [`ganja_team`]'s I/O
    /// error carries no path and the path is the useful half here.
    #[error("the inbox at {path} could not be created: {source}")]
    Inbox {
        /// Where it would have been.
        path: String,
        /// What went wrong.
        source: std::io::Error,
    },
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
    /// Every name this registry has ever spent, plus the ones a spawn is
    /// still in the middle of spending.
    ///
    /// The whole of what keeps two concurrent spawns of one name apart. A
    /// spawn crosses four awaits between reading what is taken and inserting
    /// itself into [`TeammateRegistry::members`] — an inbox seed, a backend, a
    /// team-file write — and tool bodies really do run several at once
    /// (`agents.concurrency`, **D462**), so without this both would resolve to
    /// `worker`, seed one inbox between them, and the second registration
    /// would drop the first out of the map: a teammate still running that
    /// nothing can shut down, because nothing holds it any more.
    ///
    /// **The set only grows.** A name whose spawn *completed* is deliberately
    /// never given back, and a shutdown does not clear it either, because
    /// [`TeammateRegistry::claim`] resolves against a `taken()` snapshot read
    /// before its own await: a whole spawn that began and finished inside that
    /// window is in neither the snapshot nor — if the name were released — the
    /// reservation set, so the next claimer would resolve to the same name and
    /// its registration would evict a teammate that is already running.
    /// Monotonicity closes that window without a retry loop, and what it costs
    /// is one `String` per teammate this session ever started, beside a member
    /// map that already holds one entry each and never shrinks.
    ///
    /// Only a **failed** spawn gives its name back
    /// ([`TeammateRegistry::release`]): nothing was registered under it, so
    /// nothing would be evicted by handing it to somebody else.
    reserved: Mutex<BTreeSet<String>>,
    /// Held across the team file's whole read-modify-write; see
    /// [`TeammateRegistry::record`] for what is lost without it.
    team_file: tokio::sync::Mutex<()>,
    /// How many colours §4.3's palette has handed out, which is the whole of
    /// the assignment. A map from name to colour used to sit beside it, and it
    /// was dead weight with a leak in it: the name it was keyed on is unique
    /// for the life of the registry by construction, so it was never asked
    /// twice about one name, and nothing ever removes a member — so the map
    /// only grew.
    next_color: Mutex<usize>,
    /// The tasks a spawn started, kept so a shutdown can wait for them to
    /// actually finish rather than only ask them to.
    tasks: Mutex<Vec<JoinHandle<()>>>,
    /// Where a teammate's permission dialogs are handed to the lead (**D-5**).
    ///
    /// [`None`] until a frontend attaches one, and a registry that never gets
    /// one refuses every ask its teammates raise rather than leaving them
    /// hanging — see [`crate::teammate::posture::Forwarding`]. Set rather than
    /// constructed with, because the value a frontend has to build is a channel
    /// it also drains, and a registry is useful to a test that has neither.
    dialogs: Mutex<Option<tokio::sync::mpsc::Sender<posture::Forwarded>>>,
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
    /// The mailbox loop driving this teammate, for the one thing a caller has
    /// to be able to tell it: what it is waiting for
    /// ([`TeammateRegistry::awaiting_plan_approval`]). [`None`] for a pane,
    /// which runs its own loop in its own process.
    runner: Option<Arc<runner::Runner>>,
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
            reserved: Mutex::new(BTreeSet::new()),
            team_file: tokio::sync::Mutex::new(()),
            next_color: Mutex::new(0),
            tasks: Mutex::new(Vec::new()),
            dialogs: Mutex::new(None),
        }
    }

    /// The registry a frontend installs for the session it has just opened.
    ///
    /// §2.1's implicit session team, resolved from the two things only a
    /// frontend holds: where this build keeps its own directories, and which
    /// conversation this process leads. The teams root is `<config home>/teams`
    /// — Claude's `$CLAUDE_CONFIG_DIR/teams` under ganja's own home, because a
    /// port that wrote into somebody else's config directory would be
    /// discovering foreign state rather than interoperating with it.
    ///
    /// **The directory is not touched here.** A session that never spawns a
    /// teammate leaves no team on disk, which is what makes installing this
    /// unconditionally free: what a team costs is paid at the first spawn.
    #[must_use]
    pub fn for_session(
        config_home: &std::path::Path,
        session_id: &str,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        Self::new(
            TeamsRoot::new(config_home.join(TEAMS_DIR)),
            session_team(session_id),
            session_id,
            cwd,
        )
    }

    /// Sends every teammate's permission dialogs to `lead` (**D-5**).
    ///
    /// Called before anything is spawned: a teammate started without a surface
    /// keeps the one it was started with for its whole life, because the
    /// forwarding is a task of its own and the registry does not restart it.
    pub fn forward_dialogs_to(&self, lead: tokio::sync::mpsc::Sender<posture::Forwarded>) {
        *self
            .dialogs
            .lock()
            .expect("the dialog surface is never poisoned") = Some(lead);
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

    /// The session this team's lead is, as its `leadSessionId` records it —
    /// what a dialog raised on the lead's behalf is stamped with.
    #[must_use]
    pub fn lead_session_id(&self) -> &str {
        &self.lead_session_id
    }

    /// Where a teammate's dialogs go, when a frontend has said (**D-5**).
    ///
    /// A clone of the sender rather than the slot: [`crate::teammate::lead_inbox`]
    /// offers a pane's forwarded ask on it exactly as
    /// [`crate::teammate::posture::Forwarding`] offers an in-process one —
    /// `try_send`, never a wait — and a registry nobody attached a surface to
    /// answers [`None`], which both callers read as a refusal.
    pub(crate) fn dialog_surface(&self) -> Option<tokio::sync::mpsc::Sender<posture::Forwarded>> {
        self.dialogs
            .lock()
            .expect("the dialog surface is never poisoned")
            .clone()
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

    /// Whether any member of this team runs on `backend`.
    ///
    /// What [`crate::teammate::lead_inbox::LeadInbox`] asks before it reads a
    /// **second** teams root: a `claude` teammate answers under
    /// `$CLAUDE_CONFIG_DIR/teams` (§2.1) rather than under ganja's own home, so a
    /// lead with one in the roster has two inboxes to read and a lead without one
    /// has no business looking inside somebody else's config directory.
    ///
    /// Counts every member this registry holds rather than only the live ones: a
    /// pane's row is dropped by a retire, and until then whatever it wrote is
    /// still owed a read.
    #[must_use]
    pub fn holds_backend(&self, backend: MemberBackend) -> bool {
        self.members()
            .values()
            .any(|member| member.backend == backend)
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

    /// What `teammate`'s backend can tell the lead about a delivery
    /// (**D503**).
    ///
    /// Asked of the backend that spawned it rather than mapped from
    /// [`MemberBackend`], so the answer is the implementation's own and there
    /// is no second table to keep in step with the three [`Delivery`]
    /// implementations. [`None`] for a name this registry does not hold —
    /// including a peer some other process put in the team — and what a caller
    /// makes of that is [`crate::teammate::lead_inbox::Delivered`]'s business.
    #[must_use]
    pub fn delivery_of(&self, teammate: &str) -> Option<Delivery> {
        self.members()
            .get(teammate)
            .map(|member| member.surface.delivery())
    }

    /// Forgets `teammate`, ends what it ran on, and takes its record out of
    /// the team file (§6.2).
    ///
    /// What the lead does when it reads a `shutdown_approved`: the teammate has
    /// already torn itself down on its own side, and this is the half that only
    /// the lead can do — the surface it ran on is ended, and the roster the
    /// lead renders and the document a resumed session would read both stop
    /// naming a conversation that has ended.
    ///
    /// **The kill goes through the backend that spawned it, against the
    /// handle recorded at spawn**, and nothing a frame carried: a
    /// `shutdown_approved` names a `paneId`, and a member that could name
    /// somebody else's there could have the lead kill a stranger's window.
    /// The pane backend compares that recorded `(pane_id, birth)` pair against
    /// what is live before it sends `kill-pane` (the reaper's rule, AC-12: a
    /// mismatch never kills), and the in-process one settles a teammate that
    /// has already settled itself — idempotent by [`TeammateBackend::kill`]'s
    /// own contract, so a teammate that tore itself down costs a look and
    /// nothing else.
    ///
    /// **Kill first, record second — §6.2's own order** (`killPane`, then
    /// remove from the roster). The order is chosen for what survives a
    /// failure between the two: a kill that landed before a write that did not
    /// leaves a *dead* pane and a stale record, which the reaper drops at the
    /// next startup by finding no live pane behind it; the other way round, a
    /// record gone before a kill that did not land leaves a *live* pane that
    /// no record names — and the reaper walks the team file, so that pane is
    /// invisible to it, stranded until a person closes it. A kill this cannot
    /// observe failing (the trait answers nothing) is logged by the backend
    /// itself, loudly.
    ///
    /// The team file's read-modify-write is held under the same lock a spawn's
    /// is, for the same reason: a retire racing a spawn would otherwise write
    /// back a document missing whichever member the other had just added.
    ///
    /// Answers whether this registry was holding the name. A miss is ordinary
    /// rather than exceptional — a shutdown read twice, or a peer another
    /// process started — and the document is still looked at either way,
    /// because the record is the half that outlives this process. A member
    /// nothing here holds has nothing here to kill: what another process
    /// spawned is that process's to end.
    ///
    /// **The roster is forgotten first, and a failing write does not put it
    /// back.** Nothing is lost by that: the teammate has already torn itself
    /// down — that is why its `shutdown_approved` exists — so what a re-added
    /// member would buy is a row naming a conversation that has ended and a
    /// shutdown with nothing to shut down. What the failure really costs is a
    /// stale row in the *file*, which the caller says out loud.
    ///
    /// # Errors
    ///
    /// [`SpawnError::TeamFile`] when the document could not be read or written,
    /// and [`SpawnError::Lost`] when the blocking hop that does it did not come
    /// back.
    pub async fn retire(&self, teammate: &str) -> Result<bool, SpawnError> {
        let removed = self
            .members
            .lock()
            .expect("the member map is never poisoned")
            .remove(teammate);
        let held = removed.is_some();
        if let Some(member) = removed {
            tracing::info!(
                teammate,
                handle = ?member.handle,
                "ending a retired teammate's surface"
            );
            member.surface.kill(&member.handle).await;
        }

        self.unrecord(teammate).await?;

        Ok(held)
    }

    /// Takes `teammate`'s record out of the team file, under the same lock a
    /// spawn's write holds. Answers whether the document named it.
    ///
    /// The half of a retire that outlives this process, and the unwind a
    /// refused launch owes — the one failing spawn path that runs after the
    /// record was written.
    async fn unrecord(&self, teammate: &str) -> Result<bool, SpawnError> {
        let writing = self.team_file.lock().await;
        let mut file = self.read_team().await?;
        let before = file.members.len();
        file.members.retain(|member| member.name != teammate);
        if file.members.len() == before {
            // Nothing to rewrite, and rewriting anyway would stage and rename a
            // byte-identical document over a directory a real `claude` may be
            // reading.
            return Ok(false);
        }
        self.write_team(file, &writing).await?;

        Ok(true)
    }

    /// Tells `teammate` that it is waiting on the lead's answer to
    /// `request_id`, so that answer is applied rather than ignored as stale.
    ///
    /// The registry is where this belongs because the registry is what holds a
    /// teammate's loop: the answering half is that loop's own `plan_approval`,
    /// and what makes an answer *not* stale is a waiter recorded here first.
    ///
    /// Answers whether anybody was told — [`false`] for a name this team has
    /// never had, and for a pane, which keeps its own wait in its own process.
    pub fn awaiting_plan_approval(&self, teammate: &str, request_id: impl Into<String>) -> bool {
        let member = self.members().get(teammate).map(Arc::clone);
        let Some(runner) = member.as_ref().and_then(|member| member.runner.as_ref()) else {
            return false;
        };
        runner.awaiting_plan_approval(request_id);

        true
    }

    /// Registers a teammate, seeds its mailbox with the task it was given, and
    /// starts it.
    ///
    /// §4.1's sequence, in this order: **claim the name → seed the inbox →
    /// `spawn` → record → `launch` → start.** The reference creates the
    /// surface, writes the member record, seeds the inbox, writes the spawn
    /// prompt and only then launches the command; here [`TeammateBackend::spawn`]
    /// is what yields the surface a record has to name, so the record is
    /// written **after** the backend answers — a refused spawn leaves no
    /// member behind rather than one somebody has to clean up — and
    /// [`TeammateBackend::launch`] is what starts the surface **after** the
    /// record exists, since a pane's process reads its record first. What a
    /// pane needs before either — its inbox and the task in it — is still
    /// written first.
    ///
    /// A refused spawn also takes the prompt back out of the inbox: leaving
    /// somebody's instructions in a mailbox nothing will ever read is the one
    /// half of a failed spawn that would still be visible tomorrow. A refused
    /// **launch** takes the record back out too, and kills the handle: it is
    /// the one failing path that runs after the team file names the member,
    /// so it is the one that has a record to unwind.
    ///
    /// The inbox half of that is skipped for a backend that
    /// [`owns`](TeammateBackend::owns_inbox) its own — one writer per inbox, and
    /// therefore one unwinder: [`crate::teammate::claude::ClaudePane`] seeds in
    /// its `launch` and prunes there too. Which also moves those two steps to
    /// where §4.1 puts them for that backend (record, then inbox, then prompt,
    /// then the launch line), rather than ahead of the surface.
    ///
    /// # Two spawns at once
    ///
    /// The name is **claimed synchronously** before any of that begins
    /// ([`TeammateRegistry::claim`]), and given back on every failing path,
    /// because everything between resolving a name and registering it is
    /// `await`. Two `task` calls in one assistant step really do run at once.
    ///
    /// # Not a door
    ///
    /// `pub(crate)`, and deliberately: the gate a spawn passes —
    /// [`crate::teammate::posture::spawn_gate`], and whoever it asks — lives at
    /// [`crate::subagent::Teammates::start`], which is this method's one
    /// caller. A second public entry here would be a way to start a teammate
    /// that no rule was ever consulted about.
    ///
    /// # Errors
    ///
    /// [`SpawnError::Name`] for a name that is refused or cannot be made
    /// unique, [`SpawnError::Unsupported`] for a surface this build or this
    /// session cannot have, and the two I/O variants for a team file or a
    /// mailbox that would not be written.
    ///
    /// Takes `self` as an [`Arc`] because a teammate's own postbox is bound to
    /// the team it belongs to ([`crate::subagent::Postbox::of`]), and this is
    /// where the two exist together for the first time.
    pub(crate) async fn spawn(
        self: &Arc<Self>,
        backend: Arc<dyn TeammateBackend>,
        request: SpawnRequest,
    ) -> Result<Spawned, SpawnError> {
        let name = self.claim(&request.name).await?;
        let color = request.color.clone().unwrap_or_else(|| self.color_for());
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

        // Skipped outright for a backend that seeds its own — see
        // [`TeammateBackend::owns_inbox`] for what two writers over one spawn
        // cost. `None` therefore means "there is nothing of ours in any inbox",
        // and every unwind below reads it that way rather than as a failure.
        let seeded = if backend.owns_inbox() {
            None
        } else {
            match seed_inbox(&spec).await {
                Ok(seeded) => Some(seeded),
                Err(error) => {
                    self.release(&spec.name);
                    return Err(error);
                }
            }
        };
        let handle = match backend.spawn(&spec).await {
            Ok(handle) => handle,
            Err(unsupported) => {
                unseed_inbox(&spec, seeded).await;
                self.release(&spec.name);
                return Err(SpawnError::Unsupported(unsupported));
            }
        };
        if let Err(error) = self.record(&spec, &handle).await {
            backend.kill(&handle).await;
            unseed_inbox(&spec, seeded).await;
            self.release(&spec.name);
            return Err(error);
        }
        if let Err(unsupported) = backend.launch(&spec, &handle).await {
            backend.kill(&handle).await;
            match self.unrecord(spec.name.as_str()).await {
                Ok(_) => {}
                // Not fatal to the unwind and not retried: what is left is a
                // stale row naming a surface that has already been ended,
                // which the next lead's startup sweep is what drops.
                Err(error) => tracing::warn!(
                    teammate = spec.name.as_str(),
                    %error,
                    "a refused launch left its record in the team file"
                ),
            }
            unseed_inbox(&spec, seeded).await;
            self.release(&spec.name);
            return Err(SpawnError::Unsupported(unsupported));
        }

        let spawned = Spawned {
            name: spec.name.clone(),
            agent_id: spec.agent_id(),
            backend: request.backend,
            delivery: backend.delivery(),
            note: SPAWNED,
        };
        // Registered, and never given back: a spent name stays reserved for
        // the life of the registry. Why that is the fix rather than an
        // oversight is on [`TeammateRegistry::reserved`].
        self.start(&spec, handle, backend);

        Ok(spawned)
    }

    /// Resolves a free name and holds it until the spawn is registered.
    ///
    /// The reservation and the resolution that produced it are **one critical
    /// section**. `taken()` reads a file and can only ever be a snapshot, so
    /// the resolution is done again inside the lock over that snapshot plus
    /// whatever is reserved *now* — and everything ever claimed is reserved,
    /// since nothing writes a member record or a team file before claiming its
    /// name and nothing but a failure gives one back. A second spawn asking
    /// for the same name therefore resolves past it rather than colliding with
    /// it, and no retry loop is needed because nothing here can lose a race it
    /// then has to run again.
    ///
    /// The "ever" is load-bearing rather than sloppy bookkeeping: a spawn that
    /// runs to completion between this snapshot being read and this lock being
    /// taken is invisible to the snapshot, so a reservation set that forgot
    /// spent names would hand this claimer the running teammate's name. See
    /// [`TeammateRegistry::reserved`].
    async fn claim(self: &Arc<Self>, desired: &str) -> Result<MemberName, SpawnError> {
        let taken = self.taken().await?;
        let mut reserved = self
            .reserved
            .lock()
            .expect("the reserved names are never poisoned");
        let name = resolve_unique(
            desired,
            taken
                .iter()
                .map(String::as_str)
                .chain(reserved.iter().map(String::as_str)),
        )?;
        reserved.insert(name.as_str().to_owned());

        Ok(name)
    }

    /// Gives back a name whose spawn **failed**, which is the only kind of
    /// claim that is ever given back.
    ///
    /// A failed spawn registered nothing, so a later teammate taking the name
    /// evicts nobody; a spawn that succeeded holds its name for the life of
    /// the registry, for the reason on [`TeammateRegistry::reserved`].
    fn release(&self, name: &MemberName) {
        self.reserved
            .lock()
            .expect("the reserved names are never poisoned")
            .remove(name.as_str());
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
    ///
    /// The kills run **together**, and that is a bound rather than a tidy-up:
    /// a kill waits out one teammate's [`SETTLE`], so a serial loop costs the
    /// exit path that wait once per teammate — a team of six holding the
    /// process open for half a minute, where nothing about the waits makes
    /// them owe each other an order.
    pub async fn shutdown(&self) {
        self.cancel.cancel();

        let members: Vec<Arc<Member>> = self.members().values().map(Arc::clone).collect();
        futures::future::join_all(
            members
                .iter()
                .map(|member| member.surface.kill(&member.handle)),
        )
        .await;

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

    /// §4.3's assignment: the next colour round the palette, and the index only
    /// ever moves forward.
    ///
    /// Takes no name, because the colour a teammate keeps is the one recorded
    /// on its own [`Member`] and its own [`MemberRecord`]; this is asked once,
    /// at the spawn that mints it.
    fn color_for(&self) -> String {
        let mut index = self
            .next_color
            .lock()
            .expect("the palette is never poisoned");
        let color = PALETTE[*index % PALETTE.len()].to_owned();
        *index += 1;

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
    ///
    /// The read and the write are **one critical section**, and they have to
    /// be: this is a read-modify-write of a whole document, so two spawns
    /// running it at once both read a file without the other's member in it
    /// and the second write puts back a document missing the first — a
    /// teammate that is running, holds a mailbox, and no team file remembers.
    /// The reservation in [`TeammateRegistry::claim`] keeps their *names*
    /// apart; nothing about that keeps their *records* apart.
    ///
    /// A [`tokio::sync::Mutex`] because the section spans two blocking hops.
    /// It covers this process only — a real `claude` sharing the directory is
    /// held off by nothing here, and what keeps that case from being worse
    /// than a lost record is [`TeammateRegistry::write_team`]'s staged rename,
    /// which at least never shows anybody half a document. Locking the file
    /// across processes is the pane phase's problem, where a second writer
    /// starts existing.
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
        let writing = self.team_file.lock().await;
        let mut file = self.read_team().await?;
        file.members.retain(|member| member.name != record.name);
        file.members.push(record);
        self.write_team(file, &writing).await
    }

    /// Writes the team file whole, through a staged file and a rename: a reader
    /// sharing this directory — a real `claude` among them — sees the old
    /// document or the new one and never half of either.
    ///
    /// The staged name is unique per **process**, which is all it needs to be
    /// and less than it looks: two writes from *this* process would share it,
    /// and the second would rename a file the first had already renamed away.
    /// What keeps that from happening is the `team_file` lock — and it is
    /// taken as an **argument** rather than described in a sentence, so a
    /// second caller cannot forget to hold it: the guard is unused inside, and
    /// borrowing it is the point. `_writing` names what it is because the
    /// compiler is the reader that matters here.
    async fn write_team(
        &self,
        file: TeamFile,
        _writing: &tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<(), SpawnError> {
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
    ///
    /// This is also where a teammate's outbound identity is installed, and it
    /// has to be here: [`crate::subagent::Postbox::of`] takes the
    /// [`Teammate`] itself so that nobody building one can choose the name it
    /// stamps, and this is the first place the team and that value exist
    /// together. Installed before the runner starts, so the teammate's first
    /// turn already has somewhere to post.
    fn start(
        self: &Arc<Self>,
        spec: &SpawnSpec,
        handle: Handle,
        backend: Arc<dyn TeammateBackend>,
    ) {
        let recent = Arc::new(Mutex::new(VecDeque::with_capacity(RECENT_CALLS)));
        let alive = Arc::new(AtomicBool::new(true));
        let mut held: Option<Arc<runner::Runner>> = None;
        let mut tasks = self.tasks.lock().expect("the task list is never poisoned");

        if let Some(teammate) = handle.teammate() {
            teammate
                .engine()
                .install_postbox(Arc::new(crate::subagent::Postbox::of(self, teammate)));

            // Built before either task below is spawned, because building it is
            // what registers its subscription: a forwarding that subscribed
            // inside its own task would race the teammate's very first dialog
            // (**D-5**).
            let forwarding = posture::Forwarding::new(
                Arc::clone(teammate),
                posture::Posture::for_spawn(spec),
                self.dialogs
                    .lock()
                    .expect("the dialog surface is never poisoned")
                    .clone(),
            );
            tasks.push(tokio::spawn(forwarding.run(self.cancel.child_token())));

            let events = teammate.engine().subscribe_droppable();
            tasks.push(tokio::spawn(fold_calls(
                events,
                Arc::clone(teammate.tools()),
                Arc::clone(&recent),
                spec.name.as_str().to_owned(),
                self.cancel.child_token(),
            )));

            // Kept beyond the task that drives it, which is what makes
            // `Runner::awaiting_plan_approval` a seam rather than a method
            // nothing can reach: the loop borrows the value it runs on, so the
            // registry can still tell this teammate what it is waiting for.
            let runner = Arc::new(runner::Runner::new(
                Arc::clone(teammate),
                self.lead.clone(),
                spec.inbox(),
                spec.lead_inbox(),
                handle.surface(),
                self.cancel.child_token(),
            ));
            let running = Arc::clone(&runner);
            let alive = Arc::clone(&alive);
            tasks.push(tokio::spawn(async move {
                running.run().await;
                alive.store(false, Ordering::Relaxed);
            }));
            held = Some(runner);
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
                runner: held,
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
        mailbox::seed(&path).map_err(|error| SpawnError::Inbox {
            path: path.display().to_string(),
            source: error,
        })?;
        mailbox::write(&path, message)?;

        Ok(())
    })
    .await?;

    Ok(identity)
}

/// Takes a spawn prompt back out of an inbox a spawn never got to use.
///
/// [`None`] is nothing to do rather than an error: a backend that
/// [`owns`](TeammateBackend::owns_inbox) its inbox was never seeded here, and
/// unwinding what it wrote is its own — the registry does not know the root, and
/// would prune the wrong file if it guessed.
///
/// Reported rather than returned: the spawn has already failed, and a cleanup
/// that failed too is a line in the log rather than a second error to explain.
async fn unseed_inbox(spec: &SpawnSpec, seeded: Option<mailbox::Identity>) {
    let Some(seeded) = seeded else {
        return;
    };
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
    // The part the ring was last told about. A running call republishes its
    // part on every streaming update, so this is what keeps `describe` — which
    // formats a whole argument object into a fresh `String` — from running
    // once per update for a call the ring already names.
    let mut named = None;
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
            // Cheap first, and it is the common case by a wide margin: the
            // same part arriving again is the same call still streaming, and
            // there is nothing new to name.
            && named.as_ref() != Some(&part.id)
        {
            named = Some(part.id.clone());
            let line = tools
                .get(tool)
                .map_or_else(|| tool.clone(), |found| found.describe(input));
            let mut ring = recent.lock().expect("the call ring is never poisoned");
            // A different call that reads identically to the last one is still
            // one row: the ring is a live view of what a teammate is doing, and
            // two of the same line say nothing the one line did not.
            if ring.back() != Some(&line) {
                if ring.len() == RECENT_CALLS {
                    ring.pop_front();
                }
                ring.push_back(line);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, path::Path, sync::Arc, time::Duration};

    use ganja_team::{MemberName, TeamName, TeamsRoot, mailbox};

    use super::{
        DEFAULT_BACKEND, Delivery, Handle, InProcess, MemberBackend, SpawnRequest, SpawnSpec,
        TeammateBackend, TeammateRegistry, Unsupported, session_team,
    };
    use crate::{
        Storage, permission::Permissions, provider::FakeProvider, tool::Registry as Tools,
    };

    /// Why [`Never`] refuses.
    const NEVER: &str = "this door spawns nothing";

    /// A backend that spawns nothing at all, refusing in its own sentence.
    ///
    /// A fixture rather than a real pane backend, because a real one spawns:
    /// a test that leaned on `GanjaPane` refusing would split a pane into
    /// whichever tmux session the developer happens to be sitting in the day
    /// its body lands.
    #[derive(Debug)]
    struct Never;

    #[async_trait::async_trait]
    impl TeammateBackend for Never {
        fn backend(&self) -> MemberBackend {
            MemberBackend::Pane
        }

        async fn spawn(&self, _spec: &SpawnSpec) -> Result<Handle, Unsupported> {
            Err(Unsupported {
                backend: MemberBackend::Pane,
                reason: NEVER.to_owned(),
            })
        }

        async fn kill(&self, _handle: &Handle) {}

        fn delivery(&self) -> Delivery {
            Delivery::FireAndForget
        }
    }

    /// An empty registry over a tree that goes away with `home`.
    fn registry(home: &Path) -> Arc<TeammateRegistry> {
        Arc::new(TeammateRegistry::new(
            TeamsRoot::new(home.join("teams")),
            TeamName::parse("session-abcd1234").expect("a team name"),
            "01998ad0-0000-7000-8000-000000000000",
            home,
        ))
    }

    /// A backend that really starts a teammate, over a store under `home`.
    fn in_process(home: &Path) -> Arc<dyn TeammateBackend> {
        Arc::new(InProcess::new(
            Arc::new(FakeProvider::new("on it", Duration::ZERO)),
            Arc::new(Tools::new(Vec::new())),
            Storage::open(home.join("storage")),
            |_| Permissions::default(),
        ))
    }

    /// A spawn asking for `name` on `backend`, at its dullest.
    fn request(name: &str, backend: MemberBackend, home: &Path) -> SpawnRequest {
        SpawnRequest {
            name: name.to_owned(),
            backend,
            agent_type: "general".to_owned(),
            model: "recorder-model".to_owned(),
            color: None,
            prompt: "hold the fort".to_owned(),
            cwd: home.to_path_buf(),
            plan_mode_required: false,
            bypass: false,
        }
    }

    /// What the registry is still holding names for.
    fn reserved(registry: &TeammateRegistry) -> BTreeSet<String> {
        registry
            .reserved
            .lock()
            .expect("the reserved names are never poisoned")
            .clone()
    }

    /// **A name a completed spawn took is claimed for good.**
    ///
    /// The defect this answers is a stale snapshot rather than a lost lock:
    /// [`TeammateRegistry::claim`] compares against a `taken()` view read
    /// before its own await, so a spawn that *began and finished* inside that
    /// window is in neither that view nor — if a spent name were released — the
    /// reservation set. The next claimer would then resolve to the same name
    /// and its `start` would evict a teammate that is already running, which is
    /// exactly the orphan the reservation exists to prevent.
    ///
    /// Read off the set directly, and deliberately: staging that interleave
    /// through the public door would need one claimer's task to be woken and
    /// then starved for the whole of another spawn, which no scheduler here
    /// promises. The property that closes the window is that the set never
    /// shrinks on success, and that is a fact this can assert without a race.
    /// `two_spawns_of_one_name_at_once_get_two_teammates` covers the
    /// overlapping-spawn half through the door itself.
    #[tokio::test]
    async fn a_name_a_completed_spawn_took_stays_claimed() {
        let home = ganja_testkit::temp_dir();
        let registry = registry(home.path());

        let started = registry
            .spawn(
                in_process(home.path()),
                request("worker", DEFAULT_BACKEND, home.path()),
            )
            .await
            .expect("a teammate joins");
        assert_eq!(started.name.as_str(), "worker");

        assert!(
            reserved(&registry).contains("worker"),
            "a spent name given back is a name a stale claimer would take"
        );

        registry.shutdown().await;
        assert!(
            reserved(&registry).contains("worker"),
            "and a teammate that has been shut down still holds the name it \
             ran under: its transcript and its member record both still say so"
        );
    }

    /// A refused spawn's name is free again, because nothing was registered
    /// under it: there is no member for a later teammate of that name to evict,
    /// and holding it would refuse a name for no reason anybody could see.
    #[tokio::test]
    async fn a_name_a_refused_spawn_never_spent_is_free_again() {
        let home = ganja_testkit::temp_dir();
        let registry = registry(home.path());

        let refused = registry
            .spawn(
                Arc::new(Never),
                request("worker", MemberBackend::Pane, home.path()),
            )
            .await
            .expect_err("this backend spawns nothing");
        assert!(
            refused.to_string().contains(NEVER),
            "refused by the backend, not before it: {refused}"
        );

        assert!(
            reserved(&registry).is_empty(),
            "a spawn that started nothing kept a name nobody can use"
        );
    }

    /// §2.1's own example, and the property that makes it useful: one session
    /// id always names one team, so a resume rejoins rather than orphans.
    #[test]
    fn a_session_names_its_own_team_and_a_pre_uuid_id_falls_back_to_the_default() {
        assert_eq!(
            session_team("224cbeab-4e62-497c-aa8f-d05cc33ce7ba").as_str(),
            "session-224cbeab"
        );
        assert_eq!(
            session_team("224CBEAB-4e62-497c-aa8f-d05cc33ce7ba").as_str(),
            "session-224cbeab",
            "the directory name is one spelling, whichever case the id is in"
        );
        // The per-process counter P1 minted and W1 retired: eight hex digits
        // cannot be taken from it, so there is no team name to derive.
        assert_eq!(session_team("ses_0001").as_str(), "default");
        assert_eq!(session_team("").as_str(), "default");
    }

    /// The lead reading a `shutdown_approved` is what takes a member out of
    /// both the roster it renders and the document a resume would read.
    #[tokio::test]
    async fn retiring_a_teammate_forgets_it_and_rewrites_the_team_file_without_it() {
        let home = tempfile::tempdir().expect("a temporary home");
        let registry = registry(home.path());
        registry
            .spawn(
                in_process(home.path()),
                request("w1", DEFAULT_BACKEND, home.path()),
            )
            .await
            .expect("the teammate starts");

        assert_eq!(registry.view().members.len(), 2, "the lead and w1");
        assert!(registry.retire("w1").await.expect("the team file rewrites"));

        assert_eq!(
            registry.view().members.len(),
            1,
            "only the lead is left in the roster"
        );
        let document = std::fs::read_to_string(registry.root().config_path(registry.team()))
            .expect("the team file is on disk");
        assert!(
            !document.contains("\"w1\""),
            "a retired member is out of the document too:\n{document}"
        );
        assert!(
            !registry
                .retire("w1")
                .await
                .expect("a second retire is fine"),
            "a shutdown read twice is ordinary rather than an error"
        );
    }

    /// A backend that hands out a pane-shaped handle and remembers every
    /// handle it is asked to end and every launch it is asked for — what a
    /// real pane backend does, minus tmux. `launch` also reads the team file
    /// as a pane's process would, so a test can see whether the record was
    /// there yet.
    #[derive(Debug, Default)]
    struct Recording {
        killed: std::sync::Mutex<Vec<(String, String)>>,
        /// `(name, whether the team file named it at launch time)`.
        launched: std::sync::Mutex<Vec<(String, bool)>>,
        /// Refuse every launch, so the unwind can be watched.
        refuse_launch: bool,
    }

    /// Why a refusing [`Recording`] refuses.
    const UNLAUNCHABLE: &str = "the launch line could not be typed";

    impl Recording {
        fn killed(&self) -> Vec<(String, String)> {
            self.killed
                .lock()
                .expect("the kill log is never poisoned")
                .clone()
        }

        fn launched(&self) -> Vec<(String, bool)> {
            self.launched
                .lock()
                .expect("the launch log is never poisoned")
                .clone()
        }
    }

    #[async_trait::async_trait]
    impl TeammateBackend for Recording {
        fn backend(&self) -> MemberBackend {
            MemberBackend::Pane
        }

        async fn spawn(&self, _spec: &SpawnSpec) -> Result<Handle, Unsupported> {
            Ok(Handle::Pane {
                pane_id: "%7".to_owned(),
                birth: "48213".to_owned(),
            })
        }

        async fn launch(&self, spec: &SpawnSpec, handle: &Handle) -> Result<(), Unsupported> {
            assert!(
                matches!(handle, Handle::Pane { pane_id, .. } if pane_id == "%7"),
                "launched with the handle spawn minted: {handle:?}"
            );
            let recorded = std::fs::read_to_string(spec.root.config_path(&spec.team))
                .is_ok_and(|document| document.contains(&format!("\"{}\"", spec.name)));
            self.launched
                .lock()
                .expect("the launch log is never poisoned")
                .push((spec.name.as_str().to_owned(), recorded));
            if self.refuse_launch {
                return Err(Unsupported {
                    backend: MemberBackend::Pane,
                    reason: UNLAUNCHABLE.to_owned(),
                });
            }

            Ok(())
        }

        async fn kill(&self, handle: &Handle) {
            let Handle::Pane { pane_id, birth } = handle else {
                panic!("a pane backend was asked to end something it did not start: {handle:?}");
            };
            self.killed
                .lock()
                .expect("the kill log is never poisoned")
                .push((pane_id.clone(), birth.clone()));
        }

        fn delivery(&self) -> Delivery {
            Delivery::Acknowledged
        }
    }

    /// §6.2's other half: reading a `shutdown_approved` ends the surface the
    /// member ran on — through the backend that spawned it and against the
    /// handle recorded at spawn, exactly once, and never again for a name the
    /// registry no longer holds.
    #[tokio::test]
    async fn retiring_a_teammate_ends_its_surface_through_the_recorded_handle_once() {
        let home = tempfile::tempdir().expect("a temporary home");
        let registry = registry(home.path());
        let backend = Arc::new(Recording::default());
        registry
            .spawn(
                Arc::clone(&backend) as Arc<dyn TeammateBackend>,
                request("w1", MemberBackend::Pane, home.path()),
            )
            .await
            .expect("the recording backend spawns");
        assert!(backend.killed().is_empty(), "spawning ends nothing");

        assert!(registry.retire("w1").await.expect("the retire lands"));
        assert_eq!(
            backend.killed(),
            [("%7".to_owned(), "48213".to_owned())],
            "the recorded pair, through the backend that minted it"
        );

        assert!(
            !registry
                .retire("w1")
                .await
                .expect("a second retire is fine")
        );
        registry.shutdown().await;
        assert_eq!(
            backend.killed().len(),
            1,
            "a member already retired is not ended a second time by anybody"
        );
    }

    /// §4.1's step order as the registry keeps it: the surface is launched
    /// **after** its record is in the team file — a pane's process reads that
    /// record first — and a launch that is refused unwinds the whole spawn:
    /// the handle is killed, the record and the seeded prompt are taken back
    /// out, and the name is free again.
    #[tokio::test]
    async fn a_surface_is_launched_after_its_record_exists_and_a_refused_launch_unwinds() {
        let home = tempfile::tempdir().expect("a temporary home");
        let registry = registry(home.path());

        let backend = Arc::new(Recording::default());
        registry
            .spawn(
                Arc::clone(&backend) as Arc<dyn TeammateBackend>,
                request("w1", MemberBackend::Pane, home.path()),
            )
            .await
            .expect("the recording backend spawns and launches");
        assert_eq!(
            backend.launched(),
            [("w1".to_owned(), true)],
            "launched once, and the team file already named it"
        );
        assert!(backend.killed().is_empty());
        assert!(
            reserved(&registry).contains("w1"),
            "a launched spawn spends its name"
        );

        let refusing = Arc::new(Recording {
            refuse_launch: true,
            ..Recording::default()
        });
        let refused = registry
            .spawn(
                Arc::clone(&refusing) as Arc<dyn TeammateBackend>,
                request("w2", MemberBackend::Pane, home.path()),
            )
            .await
            .expect_err("a refused launch is a refused spawn");
        assert!(
            refused.to_string().contains(UNLAUNCHABLE),
            "refused in the backend's own words: {refused}"
        );
        assert_eq!(
            refusing.launched(),
            [("w2".to_owned(), true)],
            "the record was there when the launch was asked for"
        );
        assert_eq!(
            refusing.killed(),
            [("%7".to_owned(), "48213".to_owned())],
            "the handle a refused launch leaves is ended"
        );
        assert!(
            !reserved(&registry).contains("w2"),
            "and the name is free again: {:?}",
            reserved(&registry)
        );
        let document = std::fs::read_to_string(registry.root().config_path(registry.team()))
            .expect("the team file is on disk");
        assert!(
            !document.contains("\"w2\""),
            "the record a refused launch had written is taken back out:\n{document}"
        );
        assert!(
            document.contains("\"w1\""),
            "without touching the member that did launch:\n{document}"
        );
        let inbox = registry.root().inbox_path(
            registry.team(),
            &MemberName::parse("w2").expect("a member name"),
        );
        assert!(
            mailbox::read(&inbox)
                .map(|held| held.valid.is_empty())
                .unwrap_or(true),
            "and the seeded prompt is gone from an inbox nothing will read"
        );
        assert_eq!(
            registry.view().members.len(),
            2,
            "the roster holds the lead and w1, never w2"
        );
    }
}
