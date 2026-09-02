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
//! own [`ganja_protocol::team::MemberBackend`], whose six spellings are
//! exactly the argument both doors take:
//!
//! | Door | Argument | Default |
//! |---|---|---|
//! | the `task` tool | `name`, `backend: "in-process" \| "ganja" \| "claude" \| "codex" \| "agy" \| "grok"` | `ganja` |
//! | `/teammate spawn <name>` | `--backend in-process\|ganja\|claude\|codex\|agy\|grok` | `ganja` |
//!
//! **The backend is an explicit argument on both doors, never inferred**, and
//! the default is a fixed value rather than a guess (**Dv-1**).
//! `$TMUX` governs whether a pane backend *can run*, not which backend is
//! chosen: a session without it refuses `ganja` and `claude` readably rather
//! than falling back to `in-process`, because a person who asked for a window
//! and silently got none has been lied to — and since Dv-1 that refusal is
//! what a session with no tmux gets for an **unnamed** backend too, since
//! `ganja` is what unnamed means. Both pane values refuse
//! **identically** — one [`crate::teammate::Unsupported`] carrying
//! `ganja_teammate_local::tmux::REFUSED_NO_TMUX`, since a door that spawned where
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
//! [`crate::teammate::posture`]'s. The pane and foreign-CLI bodies are **not**
//! here at all since **D539**: they live in `ganja-teammate-local`, a member
//! *above* this crate, because every one of them needs a tmux server, a shell
//! to split into or somebody else's binary on `PATH` — machine-bound facts an
//! engine has no business holding. Both pane values are shipped and real, one
//! splitting a `ganja` of this very build and one a `claude` off `PATH`; what
//! stays here is the vocabulary they are named by and the seam they plug
//! into.
//!
//! [`Registry`]: crate::tool::Registry

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use etcetera::base_strategy::{BaseStrategy as _, Xdg};
use futures::StreamExt as _;
use ganja_protocol::team::{MemberBackend, MemberView, TeamView};
use ganja_team::team::resolve_unique;
use ganja_team::{
    MailboxMessage, MemberName, MemberRecord, NameError, Spawn, Surface, TeamFile, TeamName,
    TeamsRoot, lock, mailbox, record,
};
use tempfile::NamedTempFile;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::engine::Evicted;
use crate::permission::Permissions;
use crate::protocol::{Event, PartBody, ToolState};
use crate::provider::Provider;
use crate::tool::Registry;
use crate::{Engine, Storage};

/// The two engine-native guards a lead's turn loop runs under while it holds
/// a team: the continuation blocker and the name nag.
pub(crate) mod discipline;
/// Which session a name points at, and what a person is told about the name
/// they typed (**D528**, **D529**'s reminder half).
pub mod identity;
/// The receiver-side admission gate: what a lead does with a peer message
/// from outside its own team before anything is delivered (**D523**–**D525**).
pub mod inbound;
/// The §6.2 pass the lead makes over its own inbox.
pub mod lead_inbox;
/// What a process that *is* a member holds: its postbox, and its asks on their
/// way to the lead over §5's frames.
pub mod member;
/// What every postbox shares: one classification of the frame vocabulary, and
/// the write tail a local delivery ends in.
///
/// `pub` rather than crate-private since **D539**: the two shim deliveries in
/// `ganja-teammate-local` write a foreign teammate's inbox under this module's
/// own `INBOX_CEILING`, so the ceiling every postbox bounds a write by has one
/// spelling across the split.
pub mod postbox;
/// What a teammate may do, and who answers when it asks (**D-5**).
pub mod posture;
/// What a teammate is told before its task (**D514**): the frame every
/// backend's preamble shares, and the `send_message` one the two native
/// surfaces seed.
pub mod preamble;
/// Held-settlement receipts (**D534**): the sender-side outstanding-id
/// registry, the receiver-side `HeldId` association, and the best-effort
/// client that carries a settlement back over the sender's own socket.
pub mod receipts;
/// The §6.1 loop that drives one in-process teammate.
pub mod runner;
/// The engine's half of the shared task list: `ganja-team`'s store behind
/// `ganja-tool`'s four task tools, acted on under one bound identity.
pub mod tasklist;

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
    /// and then given `CANCELLED` to unwind. Waiting is the courtesy a
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

/// The six spellings the `backend` argument takes, in the order a refusal
/// lists them: P25's three surfaces, then P27's three shim CLIs.
///
/// Written out rather than derived from [`MemberBackend`]'s serde renaming,
/// and checked against it by `every_backend_value_is_spelled_the_way_it_is
/// _serialized`: the argument's vocabulary and the document's have to agree,
/// and a test saying so is cheaper than a reader assuming it.
///
/// The `ganja` surface was spelled `pane` until Dv-1, and **no alias was
/// kept**: `pane` is refused by the ordinary unknown-name sentence, which
/// lists these six. An alias would have been a second spelling of one surface
/// living in the grammar forever, where a refusal that names the list teaches
/// the new word once.
pub const BACKENDS: [&str; 6] = ["in-process", "ganja", "claude", "codex", "agy", "grok"];

/// What a door spawns when nobody named a backend (**D501**, amended by
/// **Dv-1**).
///
/// A teammate with a window of its own, which is what somebody starting one
/// without saying where almost always meant: a pane they can watch. It was
/// `in-process` until Dv-1.
///
/// **Absence still infers nothing**, which is D501's rule and is worth
/// separating from the choice of default. This value is what an unnamed
/// backend *is*, unconditionally — not a guess informed by whether `$TMUX` is
/// set or a `claude` is on the path. A session that cannot reach tmux is
/// therefore **refused by name at spawn** rather than quietly served an
/// in-process teammate: a person who asked for a teammate and got a different
/// kind of teammate has been told something untrue about their own session.
/// `in-process` stays selectable by name for anybody who wants it.
pub const DEFAULT_BACKEND: MemberBackend = MemberBackend::Ganja;

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
const CANCELLED: Duration = Duration::from_secs(1);

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
/// under a row in `/teammate`, not a log. The full account of a teammate's work is
/// its own transcript, which is a root session anybody can open.
pub const RECENT_CALLS: usize = 8;

/// §4.3's palette, assigned round-robin and memoized per name.
const PALETTE: [&str; 4] = ["blue", "green", "pink", "purple"];

/// Where the teams live under a config home — Claude's
/// `$CLAUDE_CONFIG_DIR/teams` (§2.1), read and written under ganja's own home
/// by [`TeammateRegistry::for_session`] and under claude's by
/// [`teams_root`].
///
/// One constant for both, because it is one fact about somebody else's
/// document: a build that spelled the directory twice could come to spell it
/// two ways, and the two sides of a round trip would then never meet.
pub const TEAMS_DIR: &str = "teams";

/// The variable naming the directory a real `claude` keeps its own things in,
/// and therefore the parent of the teams directory it reads (§2.1).
///
/// It reaches further than the teams directory, and a caller that sets one for
/// a session should know it: a real `claude` derives the identity of its
/// **credential store** from this path too — on macOS the keychain service is
/// `Claude Code-credentials` under the default home and
/// `Claude Code-credentials-<eight hex of the path>` under any other — which is
/// how one variable serves several accounts. Nothing here needs to act on that,
/// because a pane under the user's own config home reads the store that user
/// logged into; it is recorded because a *fresh* config home is a fresh login,
/// and a pane that starts, reads its inbox and then refuses to take a turn looks
/// nothing like an authentication problem until somebody knows this.
pub const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";

/// Where a `claude` with no [`CLAUDE_CONFIG_DIR_ENV`] keeps them, under the
/// user's home.
///
/// **This plan's assumption, not the reference's**: §2.1 spells the root as
/// `$CLAUDE_CONFIG_DIR/teams` and never says what an unset variable falls back
/// to. It is recorded as a constant so that being wrong costs one line, and so
/// that a reader can see it is a guess rather than a citation.
pub const CLAUDE_CONFIG_HOME_DIRECTORY: &str = ".claude";

/// What a lead or a claude spawn says when there is no directory to reach
/// claude's teams through.
pub const REFUSED_NO_CONFIG_DIR: &str = "there is no directory to reach claude's teams through: neither CLAUDE_CONFIG_DIR nor a home \
     directory could be resolved";

/// Where a real `claude` reads and writes its teams (§2.1).
///
/// `$CLAUDE_CONFIG_DIR/teams`, else `~/.claude/teams`. [`None`] when neither
/// the variable nor a home directory can be had, which is what
/// [`REFUSED_NO_CONFIG_DIR`] says out loud.
///
/// **In core rather than in the claude backend that spawns against it**, since
/// D538: the two readers that are not that backend are the lead's own inbox
/// pass ([`crate::teammate::lead_inbox::LeadInbox`]) and the engine's held-entry
/// prune, and both need it whether or not this session ever spawns a `claude`
/// pane. Where a foreign agent keeps its documents is a fact about the machine,
/// not a fact about one way of starting a teammate.
#[must_use]
pub fn teams_root() -> Option<TeamsRoot> {
    // The home comes off the same strategy `config::config_home` asks, rather
    // than off `$HOME` directly: one answer about where this machine's home is,
    // whichever of the two directories is being resolved.
    let home = Xdg::new().ok().map(|base| base.home_dir().to_path_buf());

    claude_root_under(std::env::var_os(CLAUDE_CONFIG_DIR_ENV), home)
}

/// [`teams_root`]'s decision, over values rather than over the environment, so
/// a test can hold both cases without touching the process it runs in.
///
/// An empty variable is treated as unset — the shape every other environment
/// read in this tree keeps (`config_home`'s `CONFIG_HOME_ENV`), because
/// `CLAUDE_CONFIG_DIR=` in a shell profile means "I did not set this" far more
/// often than it means "the root directory".
pub(crate) fn claude_root_under(
    config_dir: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
) -> Option<TeamsRoot> {
    let named = config_dir.filter(|value| !value.is_empty()).map(PathBuf::from);

    named
        .or_else(|| home.map(|home| home.join(CLAUDE_CONFIG_HOME_DIRECTORY)))
        .map(|home| TeamsRoot::new(home.join(TEAMS_DIR)))
}

/// Pushes one line onto a member's ring.
///
/// Shared by every writer — `fold_calls` here, and the two shim loops that
/// have no engine stream to fold from — so they cannot come to disagree about
/// the cap or about what counts as a repeat: a line identical to the one
/// already at the back says nothing the first said, and the ring is a live view
/// rather than a log.
///
/// **In core rather than in the shim module that used to own it**, since D538:
/// the ring belongs to `Member`, and a function writing the registry's own
/// state had no business living inside one backend's file.
pub fn push_recent(ring: &Mutex<VecDeque<String>>, line: String) {
    let mut ring = ring.lock().expect("the call ring is never poisoned");
    if ring.back() == Some(&line) {
        return;
    }
    if ring.len() == RECENT_CALLS {
        ring.pop_front();
    }
    ring.push_back(line);
}

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
/// [`UnknownBackend`], naming the value and listing the six — an unknown
/// backend is a typo somebody can fix, and the fix is the list.
pub fn parse_backend(value: &str) -> Result<MemberBackend, UnknownBackend> {
    match value {
        "in-process" => Ok(MemberBackend::InProcess),
        "ganja" => Ok(MemberBackend::Ganja),
        "claude" => Ok(MemberBackend::Claude),
        "codex" => Ok(MemberBackend::Codex),
        "agy" => Ok(MemberBackend::Agy),
        "grok" => Ok(MemberBackend::Grok),
        other => Err(UnknownBackend { value: other.to_owned() }),
    }
}

/// How a backend is spelled as an argument.
///
/// An exhaustive match rather than a lookup, so a seventh surface is a build
/// failure here instead of a value that prints as nothing.
#[must_use]
pub const fn backend_name(backend: MemberBackend) -> &'static str {
    match backend {
        MemberBackend::InProcess => BACKENDS[0],
        MemberBackend::Ganja => BACKENDS[1],
        MemberBackend::Claude => BACKENDS[2],
        MemberBackend::Codex => BACKENDS[3],
        MemberBackend::Agy => BACKENDS[4],
        MemberBackend::Grok => BACKENDS[5],
    }
}

/// The posture a shim teammate is pinned to, in the terms a person consenting
/// to it has to read (**D508(c)**).
///
/// [`None`] for P25's three surfaces, and that absence is the honest answer
/// rather than a missing sentence: an in-process or `ganja`/`claude` pane
/// teammate forwards its dialogs to the lead, so its bounds are the lead's
/// own rules and a person stays in the loop for every one of them. A shim
/// asks **ganja** nothing after spawn, on either of its doors: a headless CLI
/// child has no channel to ask through, and a CLI's native TUI in a pane
/// (**D512**) puts that CLI's *own* prompts in front of a person — answered
/// there, under the CLI's rules, never routed through the lead's — so the
/// spawn dialog is the last moment ganja can tell anybody anything, and this
/// is what they are told. What a pane adds to that — whose prompts show
/// there, and that the lead hears nothing back — is the backend's own
/// sentence, [`TeammateBackend::surface_line`], read beside this one rather
/// than folded into it, because the bound is the same on both doors and this
/// sentence is pinned against the probe that measured the bound.
///
/// One table with two readers, so a dialog and a ring line cannot come to
/// describe one grant differently: the spawn dialog's `args` carry it under
/// `posture`, and the registry's ring line opens with it.
///
/// # What these sentences may say
///
/// Each names what the posture **bounds**, never which flag was passed: a
/// person consenting to "reads this project" when the sandbox reads the whole
/// disk has consented under a wrong description. And under the plan's
/// acceptance-sequencing rule no bound ships ahead of the probe that measured
/// it, so a sentence here says `unmeasured` where its wave has not yet
/// measured, rather than guessing generously.
///
/// An exhaustive match, so a seventh backend that forgets to answer is a build
/// failure rather than a teammate spawning with no posture disclosed.
#[must_use]
pub const fn posture_line(backend: MemberBackend) -> Option<&'static str> {
    match backend {
        MemberBackend::InProcess | MemberBackend::Ganja | MemberBackend::Claude => None,
        MemberBackend::Agy => Some(POSTURE_AGY),
        MemberBackend::Codex => Some(POSTURE_CODEX),
        MemberBackend::Grok => Some(POSTURE_GROK),
    }
}

/// agy's pinned posture, **measured** — and the only one of the three that
/// describes the *absence* of a bound (**Dv-7**, amending **D508(a)**).
///
/// Every other sentence in this table names what a sandbox denies. This one
/// names what nothing denies, and it is written that way on purpose: W4
/// measured `--sandbox` as a bound on agy's terminal and not on its
/// filesystem — agy's own `write_to_file` wrote to an absolute path outside
/// the working directory in 2 of 2 runs of that flag set — and Dv-7's user
/// directive was to ship the backend anyway, at the honest posture, rather
/// than to build a write tier or a ganja-side sandbox for it. So the consent
/// surface has to say that plainly. A person approving this spawn is
/// approving a foreign agent that can write anywhere they can, and a sentence
/// that opened with "sandbox=" and then qualified itself would be read as a
/// bound by everybody who skims.
///
/// The read clause carries its consequence for the reason codex's and grok's
/// do — whole-disk read is the ability to read a credential, and a bound
/// stated without what it enables is a bound nobody can price. The write
/// clause carries the consequence a reader would otherwise have to know this
/// codebase to work out: these writes are **not** in the snapshot chain
/// `/undo` walks, because that chain is built from this build's own tool
/// calls and a foreign CLI's writes never pass through them. Somebody who
/// believes an agy teammate's edits are revertable the way a ganja
/// teammate's are has consented under a wrong description.
///
/// What the sentence deliberately does **not** claim is a network bound. It
/// is unmeasured here, and under a floor this open it would change nothing a
/// reader could act on.
///
/// The opening clause and the two after the dash are **Dv-7's own words**,
/// kept rather than improved: the amendment is a user directive about what a
/// person is told, so the sentence they approved is the sentence that ships.
/// Only the `/undo` rider is this file's, and that clause is Dv-7's
/// requirement too — recorded there as "the dialog says so", which this table
/// is the one source of.
const POSTURE_AGY: &str = "sandbox: terminal bounded, no enforced filesystem bound — may read any file you \
     can, including credentials, and write anywhere you can; those writes are outside the \
     snapshot chain /undo walks";

/// codex's pinned posture, **measured** — the strongest floor of the three.
///
/// Every clause was taken rather than inferred, and by two instruments that
/// agree: `codex sandbox` ran a write, a read and a network attempt under
/// exactly the composed override, and the vendor's own persisted rollout for a
/// probe turn recorded the same profile — `file_system: restricted` with one
/// `root`/`read` entry, `network: restricted`.
///
/// Writes are denied everywhere, the child's own cwd included. Reads are the
/// whole disk, which is why the sentence says what that *enables*: a bound
/// stated without its consequence is a bound nobody can price, and whole-disk
/// read is the ability to read a credential. The network clause is the one
/// that separates this floor from grok's — there, whole-disk read sits beside
/// an unbounded network and the pair is exfiltration; here the second half is
/// closed, so the sentence ends by saying so rather than by borrowing grok's
/// words.
const POSTURE_CODEX: &str = "sandbox=read-only: writes denied, whole-disk read, network denied — may read any file you \
     can, including credentials, but has no network to send them over";

/// grok's pinned posture, **measured** — every clause of it, as of W5.
///
/// Long because every clause is load-bearing. The write bound is real but
/// narrow, and "temp" is spelled out because a reader pictures `/tmp` and on
/// macOS it is also the per-user folder root. The read scope is the whole disk,
/// which is what makes the second half necessary: a bound stated without what
/// it *enables* is a bound nobody can price, and whole-disk read plus an
/// unbounded network is the ability to read a credential and post it somewhere.
/// The `(macOS)` qualifier belongs to both halves — one Linux-only switch is
/// why neither holds here.
///
/// The last clause is W5's gating measurement and it is what a person consents
/// to rather than a detail: a pure-read turn **completed**, with the read tool
/// call reaching terminal status, and a write turn and a shell turn on the same
/// conversation each ended `stop_reason: "cancelled"` with the tool named. So a
/// grok teammate is a read-and-answer teammate that stops mid-answer the moment
/// it wants anything else — bounded, mailed, and survivable, but not silent
/// about it. Said in the dialog and the ring because somebody agreeing to a
/// teammate that may stop mid-answer should be agreeing to that.
const POSTURE_GROK: &str = "sandbox=read-only: writes denied outside ~/.grok and temp, whole-disk \
     read, no network bound (macOS) — may read any file you can, including credentials, and may \
     send them anywhere; reading takes no approval, and a tool request that needs one ends the \
     turn";

/// `a`, `b` and `c` — the list a refusal ends with.
fn spell(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [only] => format!("{only:?}"),
        [rest @ .., last] => format!(
            "{} and {last:?}",
            rest.iter().map(|name| format!("{name:?}")).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// A backend this build cannot spawn on, and why.
///
/// Carries the backend rather than only a sentence, so a caller may act on
/// *which* surface was refused — and still has one sentence to show when it
/// does not care.
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
    /// The lead's session, which §4.1 passes a pane as `--parent-session-id`.
    ///
    /// The last field, and the last two left with it: the shell a pane is split
    /// into (**D520**) and the column's share of the width used to ride here
    /// too, and left with **D538** — they are properties of the *runtime* a
    /// frontend resolved once, not of one spawn, and they now reach the pane
    /// backends at assembly instead of through every spec.
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

    /// §2.2's derived `<name>@<team>` identity.
    #[must_use]
    pub fn agent_id(&self) -> String {
        self.name.agent_id(&self.team)
    }
}

/// What a spawn produced: the thing that has to be torn down again, and the
/// thing that knows how (**D538**).
///
/// The registry holds one of these per member and asks it every question it
/// used to answer by knowing which *kind* of member it had started. Which is
/// the whole ruling: a backend owns what it spawned, so adding a seventh
/// surface edits that surface's own file and nothing here.
///
/// # `Arc<Self>` receivers only
///
/// [`Spawned::start`] takes `self: Arc<Self>` rather than `&Arc<Self>`, which
/// is not a shape a trait object can dispatch on; the registry holds an
/// [`Arc<dyn Spawned>`](Spawned) and hands a clone of it in, because the tasks
/// that method spawns outlive the call.
///
/// # Why the in-process-only method has a default rather than a downcast
///
/// [`Spawned::awaiting_plan_approval`] is a question only the in-process
/// member can answer, and the answer for every other member is honestly
/// "nothing here". A downcast in the registry would put the kind test back
/// exactly where this ruling took it out.
#[async_trait]
pub trait Spawned: fmt::Debug + Send + Sync {
    /// What §2.2's overloaded `tmuxPaneId` records for this member.
    fn surface(&self) -> Surface;

    /// §4.1's step 6, called once, right after the member record is written,
    /// and nowhere else.
    ///
    /// The default does nothing, and that is the right answer for a member
    /// that is already running when it is spawned. A `claude` pane splits its
    /// window in [`TeammateBackend::spawn`] — that is what yields the identity
    /// a record has to name — and types its launch line **here**, because the
    /// record is the first thing the pane's process reads and a process
    /// launched before its record exists would read a team it is not yet a
    /// member of.
    ///
    /// A call rather than a watch from inside `spawn`, because a call has an
    /// unwind path and a poll does not: a launch line that could not be typed
    /// after the record was written would otherwise be a registered member
    /// holding an idle shell that nothing cleans up.
    ///
    /// # Errors
    ///
    /// [`Unsupported`] when the surface could not be started — the same
    /// vocabulary a refused `spawn` answers in, because to whoever asked it is
    /// the same fact: this surface cannot be had. The registry unwinds exactly
    /// as it does for a record that would not write.
    async fn launch(&self) -> Result<(), Unsupported> {
        Ok(())
    }

    /// Starts whatever watches this member, once [`Spawned::launch`] has
    /// succeeded, and hands back every task the registry must hold on its
    /// list.
    ///
    /// Empty for a member that runs itself in a process of its own. A task
    /// that is not returned is a task `shutdown` will not wait for, which is
    /// how a child gets reaped after the process it belonged to has returned.
    fn start(self: Arc<Self>) -> Vec<JoinHandle<()>>;

    /// Whether this member is still running, which is what the roster and the
    /// status bar count.
    fn alive(&self) -> bool;

    /// **D503**'s ring: what this member most recently did, newest last.
    fn recent(&self) -> Vec<String>;

    /// Ends it. Idempotent: a member that has already gone is nothing to end.
    async fn kill(&self);

    /// Tells this member it is waiting on the lead's answer to `request_id`,
    /// so that answer is applied rather than ignored as stale. Answers whether
    /// anybody was told.
    ///
    /// [`false`] for every member that keeps its own wait in its own process.
    fn awaiting_plan_approval(&self, _request_id: &str) -> bool {
        false
    }
}

/// What the registry lends a backend at spawn, so what it spawned can go on
/// answering to the team without holding the registry that holds it
/// (**D538**).
///
/// One value rather than five arguments, for the reason the two shim `Lent`s
/// it absorbs already gave: every field here is the *registry's*, and a
/// backend handed them one by one is a backend somebody could build with one
/// of them missing.
#[derive(Clone, Debug)]
pub struct Lent {
    /// Where this member answers.
    pub lead_inbox: PathBuf,
    /// A child of the registry's own token, so one cancel ends every member
    /// and no turn's cancel ends any of them.
    ///
    /// Every backend takes a [`CancellationToken::child_token`] of this per
    /// task it spawns rather than a clone, so the three shapes cancel at one
    /// depth and a member that later wants to end one of its own tasks can.
    pub cancel: CancellationToken,
    /// Where a member's permission dialogs are handed to the lead (**D-5**).
    /// [`None`] until a frontend attaches a surface, which every reader takes
    /// as a refusal rather than leaving an ask hanging.
    pub dialogs: Option<posture::DialogSurface>,
    /// Where a member whose pane stopped running puts the fact, for the lead's
    /// next pass to retire it on ([`TeammateRegistry::take_exited`]).
    pub exits: tokio::sync::mpsc::UnboundedSender<Exited>,
    /// The registry itself, for the one thing a member needs it for: a postbox
    /// is bound to the team it belongs to ([`crate::subagent::Postbox::of`]).
    ///
    /// [`Weak`](std::sync::Weak), and it has to be: the registry owns the member, so a strong
    /// reference here would be a cycle through the member map.
    pub registry: std::sync::Weak<TeammateRegistry>,
}

/// A member whose pane stopped running **after** readiness — the CLI quit,
/// crashed, or a person closed the pane (bead g9u's case, **D512** as
/// amended): what the loop that noticed hands the registry, for the lead's
/// next pass to retire the member on.
///
/// Carried through the registry rather than written as a frame because no
/// frame says it honestly: a `shutdown_approved` answers a request nobody
/// sent, and `teammate_terminated` is the lead's word to a teammate, never a
/// teammate's to the lead (§5). The lead's *model* is told in prose beside
/// this, by the loop itself, so the harness's bookkeeping and the model's
/// knowledge do not depend on each other arriving.
///
/// Backend-neutral, and in core since **D538**: the registry drains these
/// without knowing which kind of member posted one. That claim only became
/// true of the *type* with **D541**, which gave the `ganja` and `claude` pane
/// backends the same poll — until then the shape carried a mandatory
/// [`ganja_team::ShimCli`], which a pane running no CLI of ours had no honest
/// value for, so the one kind of member that could post one was a shim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exited {
    /// Which member.
    pub name: String,
    /// Which CLI it ran, and [`None`] for a `ganja` or `claude` pane, which
    /// runs no CLI this build shims for (**D541**). It is what
    /// [`lead_inbox::LeadInbox`]'s retirement rebuilds the member's surface
    /// from — [`ganja_team::Surface::Shim`] for a CLI, and
    /// [`ganja_team::Surface::Pane`] for none — so the record it takes out of
    /// the team file is the one the spawn put in.
    pub cli: Option<ganja_team::ShimCli>,
    /// Its backend, for a frontend that names members by it.
    pub backend: MemberBackend,
    /// The pane it ran in.
    pub pane_id: String,
    /// What the loop left of that pane — read off the pane itself, never
    /// assumed: a corpse tmux would not take away, or an id that now names
    /// somebody else's pane, is said rather than called closed.
    pub pane: PaneFate,
    /// The last non-empty line the pane showed, where the pane was still this
    /// member's to read and the capture found one: the CLI's own parting
    /// words. Never a recycled pane's screen.
    pub last_words: Option<String>,
}

impl Exited {
    /// The one sentence a frontend shows for this.
    #[must_use]
    pub fn notice(&self) -> String {
        let cli = backend_name(self.backend);
        let said = self
            .last_words
            .as_deref()
            .map(|words| format!(" — last line: {words}"))
            .unwrap_or_default();
        format!(
            "{name} ({cli}) exited in its pane{said}; {fate}",
            name = self.name,
            fate = self.pane_sentence(),
        )
    }

    /// What became of the pane and the member, as one clause.
    #[must_use]
    pub fn pane_sentence(&self) -> &'static str {
        match self.pane {
            PaneFate::Closed => "the pane was closed and the teammate retired",
            PaneFate::Left => {
                "the teammate is retired, and its dead pane could not be closed from here — close \
                 it by hand"
            }
            PaneFate::Recycled => {
                "the teammate is retired; its pane id now names another pane, which was left alone"
            }
        }
    }
}

/// What ending a member's pane left of it — the fact a sentence about "the
/// pane was closed" has to be read off rather than assumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneFate {
    /// Gone from the server: closed here, or already gone.
    Closed,
    /// Still on the server, dead, and not taken away — the kill was refused,
    /// or no listing could be had to check it against — so a person closes
    /// it; logged as such.
    Left,
    /// Its id now names a live pane under another pid: recycled, somebody
    /// else's, and not touched.
    Recycled,
}

/// One way of running a teammate: how one is *started*, and nothing about
/// what happens afterwards (**D538**).
///
/// A backend holds the host's own handles — the provider, the tool registry,
/// the store, a tmux server, a resolved binary — and turns a [`SpawnSpec`]
/// into a [`Spawned`], which owns everything from that moment on. It knows
/// nothing about the team file or the mailbox: registration is the registry's,
/// so a backend that refuses leaves nothing behind to unwind but what the
/// registry itself wrote.
///
/// The split is what lets the registry stop knowing which kind of member it
/// started. A backend is a *factory* that lives for the session; what it makes
/// lives for one teammate, holds that teammate's own ring, liveness and tasks,
/// and answers the four questions the registry used to answer by matching on a
/// handle's shape.
#[async_trait]
pub trait TeammateBackend: fmt::Debug + Send + Sync {
    /// Which surface this backend is the implementation of.
    fn backend(&self) -> MemberBackend;

    /// Yields a member the team file can name; nothing of the teammate's own
    /// work runs yet (§4.1 step 2).
    ///
    /// `lent` is what the registry gives every member to go on answering to
    /// the team with — see [`Lent`], and note the [`Weak`](std::sync::Weak)
    /// registry in it: a member holding a strong one would be a cycle through
    /// the map that owns it.
    ///
    /// # Errors
    ///
    /// [`Unsupported`] when this build, or this session, cannot have the
    /// surface asked for — a pane in a session with no tmux. Refused by name,
    /// never fallen back to.
    async fn spawn(&self, spec: &SpawnSpec, lent: Lent) -> Result<Arc<dyn Spawned>, Unsupported>;

    /// Whether this backend seeds its teammate's inbox itself, and the registry
    /// must therefore not.
    ///
    /// [`false`] for every backend whose inbox is the one [`SpawnSpec::inbox`]
    /// names, which is all of them but one: the registry writes §4.1's steps 4
    /// and 5 there before it spawns, and unwinds that write on every failing
    /// path.
    ///
    /// `ganja_teammate_local::claude::ClaudePane` answers [`true`], because its
    /// teammate reads a **different root** — `$CLAUDE_CONFIG_DIR/teams`, which
    /// nothing will persuade a real `claude` to look away from (§2.1) — and it
    /// writes a different message there, `ganja_teammate_local::claude::preamble`'s
    /// rather than the bare prompt. Two writers over one spawn were a defect
    /// both ways round: with the two roots pointed at one
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

    /// What this backend's teammate is told before its task (**D514**): the
    /// **first** message in its inbox, ahead of everything the lead will ever
    /// write — seeded by the registry right here in `TeammateRegistry::spawn`,
    /// or by the backend itself where it [owns the inbox](TeammateBackend::owns_inbox).
    ///
    /// Required rather than defaulted, and that is the point: the one
    /// paragraph that differs per backend is how — or whether — its teammate
    /// answers (ganja's `send_message`, a real `claude`'s `SendMessage`, a
    /// pane that cannot answer at all, a headless child whose answers are
    /// mail), and a backend that could be spawned without saying so would be
    /// a teammate reading a task with nobody to report to. The shared frame
    /// and the native channel are [`crate::teammate::preamble`]'s; each
    /// backend supplies its own sentence, in ganja's own words (**D497**).
    fn preamble(&self, spec: &SpawnSpec) -> String;

    /// What this backend can tell the lead about a message it handed over.
    fn delivery(&self) -> Delivery;

    /// What the *surface* adds to [`posture_line`]'s sentence, for the spawns
    /// where that sentence is not the whole of what a person is consenting to.
    ///
    /// The default is [`None`], and it is the right answer for every backend
    /// whose teammate goes on asking the lead afterwards, and for the headless
    /// shim, whose posture sentence already says all there is — a child that
    /// asks nobody. A shim in its CLI's native TUI (**D512**) answers with
    /// the two facts a pane changes: which of the CLI's own prompts now render
    /// in front of a person, and that the lead hears nothing back (v1 is
    /// send-only). Per CLI and not shared, because the first fact differs
    /// under each CLI's own flags (the lead's ruling 5 for P28).
    ///
    /// The same table has the same two readers [`posture_line`] has — the
    /// spawn dialog carries it under `surface`, and the registry's ring
    /// closes its spawn lines with it — so the two cannot describe one pane
    /// differently.
    fn surface_line(&self) -> Option<String> {
        None
    }
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

    /// The native channel: this teammate's engine holds ganja's own
    /// `send_message`, installed as its postbox the moment it is registered.
    fn preamble(&self, spec: &SpawnSpec) -> String {
        preamble::native(preamble::Names::of(spec), &spec.prompt)
    }

    async fn spawn(&self, spec: &SpawnSpec, lent: Lent) -> Result<Arc<dyn Spawned>, Unsupported> {
        Ok(Arc::new(InProcessMember {
            teammate: Arc::new(Teammate::deferring(
                spec.name.as_str(),
                Arc::clone(&self.provider),
                spec.model.clone(),
                (self.tools)(),
                (self.permissions)(spec),
                self.storage.clone(),
                self.defer_threshold,
            )),
            spec: spec.clone(),
            lent,
            recent: Arc::new(Mutex::new(VecDeque::with_capacity(RECENT_CALLS))),
            alive: Arc::new(AtomicBool::new(true)),
            runner: Mutex::new(None),
        }))
    }

    fn delivery(&self) -> Delivery {
        // The runner prunes an inbox entry when it takes the message into a
        // turn, so the lead can retire its queue entry on having watched that
        // happen rather than on having written it.
        Delivery::Acknowledged
    }
}

/// One teammate running in the lead's own process, from the moment
/// [`InProcess::spawn`] made it.
///
/// Everything the registry's own `start` used to do for this shape lives here
/// since **D538**, in the same order and for the same reasons.
struct InProcessMember {
    teammate: Arc<Teammate>,
    spec: SpawnSpec,
    lent: Lent,
    /// **D503**'s ring, folded from this teammate's own event stream.
    recent: Arc<Mutex<VecDeque<String>>>,
    /// Cleared when the runner's task ends, so a teammate that shut itself
    /// down stops being listed without the registry having to be told.
    alive: Arc<AtomicBool>,
    /// Kept beyond the task that drives it, which is what makes
    /// [`runner::Runner::awaiting_plan_approval`] a seam rather than a method
    /// nothing can reach: the loop borrows the value it runs on. Filled by
    /// [`Spawned::start`], which the registry calls exactly once.
    runner: Mutex<Option<Arc<runner::Runner>>>,
}

impl fmt::Debug for InProcessMember {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("InProcess").field(&self.teammate.name()).finish()
    }
}

#[async_trait]
impl Spawned for InProcessMember {
    fn surface(&self) -> Surface {
        Surface::InProcess
    }

    /// **The order is the contract**, and U-3 pins it: the postbox is
    /// installed before anything can prompt, the forwarding is *built* before
    /// either task is spawned because building it is what registers its
    /// subscription (**D-5**), the droppable ring subscription is registered
    /// before the runner can take a turn, and only then does the runner start.
    /// A subscription registered inside its own task would race the very first
    /// event it exists to see.
    ///
    /// Two subscribers, each with its own reason. The ring reads a
    /// **droppable** one (**D503**), so a reader that falls behind is evicted
    /// rather than allowed to backpressure the teammate's turn. The runner
    /// claims a **lossless** one, and must: the engine's birth queue is a
    /// lossless lane registered at construction, and an unclaimed one fills
    /// and then makes the teammate's own first turn wait on nobody.
    fn start(self: Arc<Self>) -> Vec<JoinHandle<()>> {
        let mut tasks = Vec::new();
        // The registry is `Weak` because it owns this member; a session tearing
        // down between the spawn and the start has nothing left to bind an
        // outbound identity to, and a member with no postbox is better than a
        // panic on the way out.
        let Some(registry) = self.lent.registry.upgrade() else {
            tracing::warn!(
                teammate = self.spec.name.as_str(),
                "the registry went away before its teammate could be started"
            );
            self.alive.store(false, Ordering::Relaxed);

            return tasks;
        };
        // This is where a teammate's outbound identity is installed, and it has
        // to be here: [`crate::subagent::Postbox::of`] takes the [`Teammate`]
        // itself so that nobody building one can choose the name it stamps, and
        // this is the first place the team and that value exist together.
        self.teammate
            .engine()
            .install_postbox(Arc::new(crate::subagent::Postbox::of(&registry, &self.teammate)));
        // And the team's shared list under this teammate's own name, for the
        // same reason and at the same moment: the team and the teammate first
        // exist together here, and a list built anywhere else could be given a
        // name its holder did not earn. The four tools it serves were lent by
        // `Engine::teammate_tools`, which says why they join there rather than
        // through the composition path the lead's own registration takes.
        self.teammate.engine().install_tasks(Arc::new(tasklist::TeamTasks::of(
            registry.root(),
            registry.team(),
            self.teammate.name(),
        )));

        let forwarding =
            posture::Forwarding::new(Arc::clone(&self.teammate), self.lent.dialogs.clone());
        tasks.push(tokio::spawn(forwarding.run(self.lent.cancel.child_token())));

        let events = self.teammate.engine().subscribe_droppable();
        tasks.push(tokio::spawn(fold_calls(
            events,
            Arc::clone(&self.teammate.tools),
            Arc::clone(&self.recent),
            self.spec.name.as_str().to_owned(),
            self.lent.cancel.child_token(),
        )));

        let runner = Arc::new(runner::Runner::new(
            Arc::clone(&self.teammate),
            self.spec.lead.clone(),
            self.spec.inbox(),
            self.lent.lead_inbox.clone(),
            self.surface(),
            self.lent.cancel.child_token(),
        ));
        *self.runner.lock().expect("the runner slot is never poisoned") = Some(Arc::clone(&runner));
        let alive = Arc::clone(&self.alive);
        tasks.push(tokio::spawn(async move {
            runner.run().await;
            alive.store(false, Ordering::Relaxed);
        }));

        tasks
    }

    fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn recent(&self) -> Vec<String> {
        self.recent.lock().expect("the call ring is never poisoned").iter().cloned().collect()
    }

    async fn kill(&self) {
        if !self.teammate.shutdown(SETTLE).await {
            tracing::warn!(
                teammate = self.teammate.name(),
                "a teammate was still working when its lifetime ended"
            );
        }
    }

    fn awaiting_plan_approval(&self, request_id: &str) -> bool {
        let Some(runner) = self.runner.lock().expect("the runner slot is never poisoned").clone()
        else {
            return false;
        };
        runner.awaiting_plan_approval(request_id);

        true
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
            .finish()
    }
}

/// What a door tells the model after a spawn.
///
/// Named for the report it is rather than for the act, since **D538** gave
/// [`Spawned`] to the trait a backend answers a spawn with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnReport {
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
    /// Its own variant rather than [`SpawnError::TeamFile`]: the team file
    /// and an inbox are two different documents in two different places, and a
    /// sentence naming the wrong one sends whoever reads it to look at a file
    /// that is fine. Its own variant
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
    /// The in-process half of the team file's hold, taken by
    /// [`TeammateRegistry::edit_team`] before the on-disk one and released
    /// after it.
    ///
    /// It is not what keeps two *processes* off one document — only
    /// [`ganja_team::lock`]'s directory does that, and a co-tenant lead is
    /// exactly such a process. What it buys is that crate's own in-process
    /// step, one layer up: this registry's threads queue on a mutex that
    /// answers immediately rather than each occupying a blocking-pool thread
    /// parked in the condvar behind [`ganja_team::lock::acquire_unseeded`].
    ///
    /// Private, and it has to be (**Dv-13**): a public lock is a public licence
    /// to hold the registry's one write barrier for an arbitrary span. Every
    /// read-modify-write a caller outside this crate needs is a door built on
    /// [`TeammateRegistry::edit_team`] — [`TeammateRegistry::unrecord`] and
    /// [`TeammateRegistry::mark_records_inactive`] among them — so the
    /// section's length is this file's to decide.
    team_file: tokio::sync::Mutex<()>,
    /// How many colours §4.3's palette has handed out, which is the whole of
    /// the assignment: a member's name is unique for the life of the registry
    /// by construction, so nothing ever asks twice about one name and no map
    /// from name to colour needs to sit beside the counter.
    next_color: Mutex<usize>,
    /// The tasks a spawn started, kept so a shutdown can wait for them to
    /// actually finish rather than only ask them to.
    tasks: Mutex<Vec<JoinHandle<()>>>,
    /// Where a member whose surface stopped running posts the fact, and where
    /// [`TeammateRegistry::take_exited`] drains it from ([`Exited`], **D512**
    /// as amended for bead g9u).
    ///
    /// A channel rather than the `Vec` this was until **D538**, for the reason
    /// the ruling moved the state at all: the writer is the member's own loop,
    /// which is now the member's to own, and an unbounded sender is what it can
    /// be handed without also being handed the registry that holds it. The
    /// contract the drain keeps is unchanged — every entry is acted on exactly
    /// once, by the one pass that takes it.
    exits: tokio::sync::mpsc::UnboundedSender<Exited>,
    /// The receiving half, behind a lock because a drain takes `&self`.
    ///
    /// `try_recv` in a loop rather than an `await`, so a pass that finds
    /// nothing returns rather than parking — and so no guard is ever held
    /// across an await.
    exited: Mutex<tokio::sync::mpsc::UnboundedReceiver<Exited>>,
    /// Where a teammate's permission dialogs are handed to the lead (**D-5**).
    ///
    /// [`None`] until a frontend attaches one, and a registry that never gets
    /// one refuses every ask its teammates raise rather than leaving them
    /// hanging — see [`crate::teammate::posture::Forwarding`]. Set rather than
    /// constructed with, because the value a frontend has to build is a channel
    /// it also drains, and a registry is useful to a test that has neither.
    dialogs: Mutex<Option<tokio::sync::mpsc::Sender<posture::Forwarded>>>,
    /// How many of this team's forwarded dialogs the person has not answered
    /// yet ([`TeammateRegistry::dialogs_waiting`]).
    ///
    /// Beside the surface rather than inside it because it outlives any one
    /// sender: [`TeammateRegistry::forward_dialogs_to`] may be called again,
    /// and a dialog raised through the old surface is still on the same
    /// person's screen. Every send hands out a
    /// [`Raised`](posture::Raised) against this counter and nothing else can
    /// touch it, so it cannot drift from what was really carried.
    waiting_dialogs: Arc<AtomicUsize>,
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
///
/// Five facts the registry minted and one object it did not (**D538**): the
/// ring, the liveness and the loop that used to sit here belong to whatever
/// the backend spawned, and are read through it.
struct Member {
    name: MemberName,
    agent_id: String,
    backend: MemberBackend,
    color: String,
    /// What the backend made, and what ends it.
    spawned: Arc<dyn Spawned>,
    /// The backend that made [`Member::spawned`], for the one question that is
    /// the *implementation's* rather than the member's: what it can tell the
    /// lead about a delivery.
    surface: Arc<dyn TeammateBackend>,
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
        let (exits, exited) = tokio::sync::mpsc::unbounded_channel();

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
            exits,
            exited: Mutex::new(exited),
            dialogs: Mutex::new(None),
            waiting_dialogs: Arc::default(),
        }
    }

    /// The members whose surfaces stopped running since the last call, each
    /// taken out so it is retired once (**D512** as amended for bead g9u).
    ///
    /// The lead's pass ([`lead_inbox::LeadInbox::poll`]) is the one caller: it
    /// retires each through [`TeammateRegistry::retire`], the same door a
    /// `shutdown_approved` takes, so a member that ended on its own and one
    /// that was asked to end leave the roster and the team file the same way.
    ///
    /// **Drains everything posted since the last call** — the contract the
    /// `Vec` this replaced kept, and the one U-5 pins.
    #[must_use]
    pub fn take_exited(&self) -> Vec<Exited> {
        let mut exited = self.exited.lock().expect("the exit channel is never poisoned");
        let mut taken = Vec::new();
        while let Ok(entry) = exited.try_recv() {
            taken.push(entry);
        }

        taken
    }

    /// What every backend is lent at spawn (**D538**): the team's own address,
    /// a cancellation of the registry's, the dialog surface a frontend
    /// attached, the exit channel, and a [`Weak`](std::sync::Weak) back to
    /// here for the one thing a postbox needs.
    fn lend(self: &Arc<Self>) -> Lent {
        Lent {
            lead_inbox: self.lead_inbox(),
            cancel: self.cancel.child_token(),
            dialogs: self.dialog_surface(),
            exits: self.exits.clone(),
            registry: Arc::downgrade(self),
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
        *self.dialogs.lock().expect("the dialog surface is never poisoned") = Some(lead);
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
    pub(crate) fn dialog_surface(&self) -> Option<posture::DialogSurface> {
        let lead = self.dialogs.lock().expect("the dialog surface is never poisoned").clone()?;

        Some(posture::DialogSurface::new(lead, Arc::clone(&self.waiting_dialogs)))
    }

    /// How many of this team's forwarded dialogs are still in front of the
    /// person, whichever kind of teammate raised them.
    ///
    /// Read by the lead's own turn loop, which must not push a synthetic
    /// instruction onto a screen that is already asking somebody a question
    /// (`crate::teammate::discipline::Facts::dialog_open`). Both carriers count
    /// here — [`crate::teammate::posture::Forwarding`] for an in-process
    /// teammate and [`crate::teammate::lead_inbox`] for a pane's frame — because
    /// the person answering cannot tell the two apart either.
    #[must_use]
    pub fn dialogs_waiting(&self) -> usize {
        self.waiting_dialogs.load(Ordering::Relaxed)
    }

    /// A child of the token [`TeammateRegistry::shutdown`] cancels, for work
    /// that outlives the pass that started it.
    ///
    /// A child rather than a clone for [`TeammateRegistry::lend`]'s reason: a
    /// holder may cancel its own without ending anybody else's. The one caller
    /// is [`crate::teammate::lead_inbox`]'s wait on a forwarded dialog, which
    /// has to end when the team does — and it is a child there so the arm can
    /// never be the thing that cancels the registry.
    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.cancel.child_token()
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
        self.members().values().any(|member| member.backend == backend)
    }

    /// Whether this registry holds **nobody** — the live state a session's
    /// teamless posture is read off (**D543**, 2026-08-30, `Engine::teamless`).
    ///
    /// Counts every member this registry holds rather than only the live
    /// ones, [`TeammateRegistry::holds_backend`]'s rule and for that rule's
    /// reason: a teammate whose surface has stopped is still a teammate
    /// until a retire drops it, and whatever it wrote is still owed a read.
    /// So a team does not flicker back to leading nobody in the window
    /// between an exit and the inbox pass that retires it; the state follows
    /// the retire, which is where the team really ends.
    #[must_use]
    pub fn leads_nobody(&self) -> bool {
        self.members().is_empty()
    }

    /// How many teammates are still running, which is what the status bar
    /// counts.
    #[must_use]
    pub fn running(&self) -> usize {
        self.members().values().filter(|member| member.spawned.alive()).count()
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
        members.extend(self.members().values().filter(|member| member.spawned.alive()).map(
            |member| MemberView {
                name: member.name.as_str().to_owned(),
                agent_id: member.agent_id.clone(),
                backend: member.backend,
                color: Some(member.color.clone()),
                is_lead: false,
                recent_calls: member.spawned.recent(),
            },
        ));

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
        self.members().get(teammate).map(|member| member.surface.delivery())
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
    /// has already settled itself — idempotent by [`Spawned::kill`]'s
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
        Ok(!self.retire_all(&[teammate.to_owned()]).await?.is_empty())
    }

    /// [`TeammateRegistry::retire`] over a whole sweep: every surface ended,
    /// and **one** read-modify-write of the team file rather than one per name.
    ///
    /// Answers the names this registry was holding, which is
    /// [`TeammateRegistry::retire`]'s answer widened rather than a second
    /// question.
    ///
    /// The batch is not a convenience. A sweep's drops are N whole
    /// read-modify-writes of one small document under N acquires of one lock,
    /// and every one of them is a window a co-tenant lead's `record` can land
    /// in and be waited out of; folding them into one hold shortens the sweep
    /// to a single window and does the same work. The kills stay per member and
    /// run **together**, for [`TeammateRegistry::shutdown`]'s reason: each waits
    /// out one teammate's [`SETTLE`], and nothing about those waits owes them an
    /// order.
    ///
    /// # Errors
    ///
    /// [`TeammateRegistry::unrecord`]'s, and the surfaces are ended either way
    /// — a failed rewrite leaves stale rows, never live panes.
    pub async fn retire_all(&self, teammates: &[String]) -> Result<Vec<String>, SpawnError> {
        let removed: Vec<(String, Arc<Member>)> = {
            let mut members = self.members();
            teammates
                .iter()
                .filter_map(|name| members.remove(name).map(|member| (name.clone(), member)))
                .collect()
        };
        for (teammate, member) in &removed {
            tracing::info!(
                teammate,
                spawned = ?member.spawned,
                "ending a retired teammate's surface"
            );
        }
        futures::future::join_all(removed.iter().map(|(_, member)| member.spawned.kill())).await;

        self.unrecord_all(teammates).await?;

        Ok(removed.into_iter().map(|(teammate, _)| teammate).collect())
    }

    /// Takes `teammate`'s record out of the team file, under the same lock a
    /// spawn's write holds. Answers whether the document named it.
    ///
    /// The half of a retire that outlives this process, and the unwind a
    /// refused launch owes — the one failing spawn path that runs after the
    /// record was written.
    ///
    /// `pub` since **D539** for a third caller outside this crate:
    /// `ganja_teammate_local::reaper` drops the record of a pane it swept, once
    /// it has proved what became of the pane. The whole read-modify-write is
    /// inside this method rather than spread across the caller, which is the
    /// shape every door onto this document takes — see
    /// [`TeammateRegistry::mark_records_inactive`] for the race that is what
    /// happens when it is not. A caller dropping *several* records has
    /// [`TeammateRegistry::unrecord_all`] rather than a loop over this one.
    pub async fn unrecord(&self, teammate: &str) -> Result<bool, SpawnError> {
        Ok(!self.unrecord_all(&[teammate.to_owned()]).await?.is_empty())
    }

    /// [`TeammateRegistry::unrecord`] for a sweep: every name in one hold of
    /// the lock, answering the ones the document really named.
    ///
    /// The one this crate's own doors are written on — `unrecord` is this with
    /// a single name — so a caller outside it that drops records in a loop is
    /// paying N acquires and N rewrites for what one hold does.
    pub async fn unrecord_all(&self, teammates: &[String]) -> Result<Vec<String>, SpawnError> {
        let dropping: BTreeSet<String> = teammates.iter().cloned().collect();

        Ok(self
            .edit_team(move |file| {
                let mut dropped = Vec::new();
                file.members.retain(|member| {
                    if dropping.contains(&member.name) {
                        dropped.push(member.name.clone());

                        return false;
                    }

                    true
                });

                // Nothing to rewrite, and rewriting anyway would stage and
                // rename a byte-identical document over a directory a real
                // `claude` may be reading.
                (!dropped.is_empty()).then_some(dropped)
            })
            .await?
            .unwrap_or_default())
    }

    /// Marks every record `recognized` claims inactive — not dropped — and
    /// answers the names it marked, in one hold of the team file's lock.
    ///
    /// The one door outside this crate onto a read-modify-write of the whole
    /// document (**D539**, **Dv-13**). Its caller is
    /// `ganja_teammate_local::reaper::retire_shim_records`, which recognizes
    /// the foreign-CLI teammates a *previous* lead left behind — a fact about
    /// somebody else's binary that this crate deliberately does not know, which
    /// is why the test arrives as a predicate rather than as a comparison
    /// written here.
    ///
    /// **Why the lock spans the whole operation.** This is a read-modify-write
    /// of an entire document, exactly as this registry's own `record` is, and
    /// the two run against each other: a read taken *outside* the lock can be
    /// written back over a `record` that landed in between, and the member row
    /// that spawn wrote is gone — a teammate that is running, holds a mailbox,
    /// and no team file remembers. So the lock is taken before the read and
    /// released after the write, with the caller's predicate running inside it.
    ///
    /// **Marked, not dropped**: a row already carrying `isActive: false` is
    /// left alone and not answered, so a second pass over the same document
    /// retires nothing. What that costs is stated at the caller — a retired
    /// name is not freed, because dropping the row would hand a dead
    /// teammate's identity to the next live one in a document a real `claude`
    /// may be reading.
    ///
    /// Two guards refuse ahead of any write, both by answering the empty list:
    ///
    /// * **Before the first spawn, and only then.** A registry that already
    ///   holds members would be marking its *own* live teammates inactive, so a
    ///   non-empty member map retires nothing. The precondition is enforced
    ///   here rather than asked of the caller, because it is this method's
    ///   safety property and not that caller's discipline.
    /// * **The co-tenant guard.** Two leads that start inside one 65-second
    ///   UUIDv7 bucket share a team name and therefore a team file, so a
    ///   document naming another lead's session is left entirely alone rather
    ///   than have a live co-tenant's members marked dead.
    ///
    /// # Errors
    ///
    /// [`SpawnError::TeamFile`] when the document could not be read or written,
    /// and [`SpawnError::Lost`] when the blocking hop that does it did not come
    /// back. Neither guard is an error: a refusal to touch the document is an
    /// empty answer, since every caller of this is best-effort startup work.
    pub async fn mark_records_inactive(
        &self,
        recognized: impl Fn(&MemberRecord) -> bool + Send + 'static,
    ) -> Result<Vec<String>, SpawnError> {
        // Bound to a `let` rather than tested inline, so the member map's guard
        // is released at the end of this statement and cannot be held across
        // the awaits below.
        let already_spawning = !self.members().is_empty();
        if already_spawning {
            tracing::debug!(
                "this lead has already spawned into its team, so no member record was retired"
            );

            return Ok(Vec::new());
        }

        let team = self.team.clone();
        let session = self.lead_session_id.clone();

        Ok(self
            .edit_team(move |file| {
                if file.lead_session_id != session {
                    tracing::info!(
                        team = team.as_str(),
                        "the team file names another lead's session, so its records were left \
                         alone"
                    );

                    return None;
                }

                let mut retired = Vec::new();
                for member in &mut file.members {
                    if member.is_lead() {
                        continue;
                    }
                    if !recognized(member) || member.is_active == Some(false) {
                        continue;
                    }
                    member.is_active = Some(false);
                    retired.push(member.name.clone());
                }

                // Nothing to rewrite, and rewriting anyway would stage and
                // rename a byte-identical document over a directory a real
                // `claude` may be reading — `unrecord`'s rule, for `unrecord`'s
                // reason.
                (!retired.is_empty()).then_some(retired)
            })
            .await?
            .unwrap_or_default())
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
        let Some(member) = self.members().get(teammate).map(Arc::clone) else {
            return false;
        };

        member.spawned.awaiting_plan_approval(&request_id.into())
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
    /// [`Spawned::launch`] is what starts the surface **after** the record
    /// exists, since a pane's process reads its record first. What a pane
    /// needs before either — its inbox and the task in it — is still written
    /// first. [`Spawned::start`] runs last, and every task it hands back joins
    /// the list a shutdown drains.
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
    /// therefore one unwinder: `ganja_teammate_local::claude::ClaudePane` seeds in
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
    ) -> Result<SpawnReport, SpawnError> {
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
            parent_session_id: self.lead_session_id.clone(),
        };

        // Skipped outright for a backend that seeds its own — see
        // [`TeammateBackend::owns_inbox`] for what two writers over one spawn
        // cost. `None` therefore means "there is nothing of ours in any inbox",
        // and every unwind below reads it that way rather than as a failure.
        let seeded = if backend.owns_inbox() {
            None
        } else {
            // The backend's preamble around the prompt, not the bare prompt
            // (**D514**): the first thing a teammate reads says who it is and
            // how it answers, and the record below keeps the prompt as typed.
            match seed_inbox(spec.inbox(), spec.lead.as_str().to_owned(), backend.preamble(&spec))
                .await
            {
                Ok(seeded) => Some(seeded),
                Err(error) => {
                    self.release(&spec.name);
                    return Err(error);
                }
            }
        };
        let spawned = match backend.spawn(&spec, self.lend()).await {
            Ok(spawned) => spawned,
            Err(unsupported) => {
                unseed_inbox(spec.inbox(), seeded, spec.name.as_str()).await;
                self.release(&spec.name);
                return Err(SpawnError::Unsupported(unsupported));
            }
        };
        if let Err(error) = self.record(&spec, spawned.surface()).await {
            spawned.kill().await;
            unseed_inbox(spec.inbox(), seeded, spec.name.as_str()).await;
            self.release(&spec.name);
            return Err(error);
        }
        if let Err(unsupported) = spawned.launch().await {
            spawned.kill().await;
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
            unseed_inbox(spec.inbox(), seeded, spec.name.as_str()).await;
            self.release(&spec.name);
            return Err(SpawnError::Unsupported(unsupported));
        }

        let report = SpawnReport {
            name: spec.name.clone(),
            agent_id: spec.agent_id(),
            backend: request.backend,
            delivery: backend.delivery(),
            note: SPAWNED,
        };
        // Every task the member started is registered here, which is what makes
        // `shutdown()` correct: it cancels, `join_all`s every kill, **and then
        // drains this list**. A task that was never pushed would leave a child
        // being reaped after the process it belonged to had returned.
        self.tasks
            .lock()
            .expect("the task list is never poisoned")
            .extend(Arc::clone(&spawned).start());
        // Registered, and never given back: a spent name stays reserved for
        // the life of the registry. Why that is the fix rather than an
        // oversight is on [`TeammateRegistry::reserved`].
        self.members().insert(
            spec.name.as_str().to_owned(),
            Arc::new(Member {
                name: spec.name.clone(),
                agent_id: spec.agent_id(),
                backend: spec.backend,
                color: spec.color.clone(),
                spawned,
                surface: backend,
            }),
        );

        Ok(report)
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
        let mut reserved = self.reserved.lock().expect("the reserved names are never poisoned");
        let name = resolve_unique(
            desired,
            taken.iter().map(String::as_str).chain(reserved.iter().map(String::as_str)),
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
        self.reserved.lock().expect("the reserved names are never poisoned").remove(name.as_str());
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
        futures::future::join_all(members.iter().map(|member| member.spawned.kill())).await;

        let tasks =
            std::mem::take(&mut *self.tasks.lock().expect("the task list is never poisoned"));
        for task in tasks {
            let _ = task.await;
        }
    }

    fn members(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Arc<Member>>> {
        self.members.lock().expect("the member map is never poisoned")
    }

    /// §4.3's assignment: the next colour round the palette, and the index only
    /// ever moves forward.
    ///
    /// Takes no name, because the colour a teammate keeps is the one recorded
    /// on its own [`Member`] and its own [`MemberRecord`]; this is asked once,
    /// at the spawn that mints it.
    fn color_for(&self) -> String {
        let mut index = self.next_color.lock().expect("the palette is never poisoned");
        let color = PALETTE[*index % PALETTE.len()].to_owned();
        *index += 1;

        color
    }

    /// Every name a new teammate may not be given: the team file's members —
    /// the lead among them, which is what keeps a teammate from taking the
    /// lead's name — and everything this registry has started, running or not.
    async fn taken(&self) -> Result<Vec<String>, SpawnError> {
        let mut taken: Vec<String> =
            self.read_team().await?.members.iter().map(|member| member.name.clone()).collect();
        taken.push(self.lead.as_str().to_owned());
        taken.extend(self.members().keys().cloned());

        Ok(taken)
    }

    /// The team file, or the team it would be if nothing has written one yet.
    ///
    /// `pub` since **D539**: `ganja_teammate_local::reaper`'s pane sweep reads
    /// the document to learn which members claim a pane, and every fact it
    /// needs — a record's name, agent id, `backendType` and surface, and the
    /// file's own `leadSessionId` — is [`ganja_team`]'s public shape already.
    /// A **read** door and nothing more: it takes no lock and hands out no way
    /// to write, so a caller that reads here and writes elsewhere is writing
    /// through a method that takes the lock itself. Locking a read would buy
    /// nothing anyway — every writer of this document replaces it by rename, so
    /// a reader sees one whole version or another and never half of one, which
    /// is the guarantee every foreign reader of the file already relies on.
    pub async fn read_team(&self) -> Result<TeamFile, SpawnError> {
        let path = self.root.config_path(&self.team);
        let team = self.team.clone();
        let session = self.lead_session_id.clone();
        let cwd = self.cwd.to_string_lossy().into_owned();

        blocking(move || {
            read_team_file(&path, || TeamFile::new(&team, session, cwd, record::now_millis()))
        })
        .await
    }

    /// The team file's one write door: read the document, hand it to `edit`,
    /// and write back exactly when `edit` asks for one.
    ///
    /// The read and the write are **one critical section**, and they have to
    /// be: this is a read-modify-write of a whole document, so two writers
    /// running it at once both read a file without the other's change in it
    /// and the second write puts back a document missing the first — a
    /// teammate that is running, holds a mailbox, and no team file remembers.
    ///
    /// **Two locks, in [`ganja_team::lock`]'s own order** — in-process first,
    /// disk second — because there are two kinds of racer and only one of them
    /// is a thread. This registry's own are held off by `team_file`; a
    /// **co-tenant lead** is another process, and the only thing that holds one
    /// of those off is §2.5's lock directory, the protocol a real `claude`
    /// sharing this directory takes for the inbox beside it. It is taken here
    /// through [`ganja_team::lock::acquire_unseeded`], which names the lock
    /// from the team's *directory* rather than from the document's real path,
    /// because a team file has no seed step for that path to be read out of.
    /// Both halves are released on every way out of the hop below, the
    /// directory's by its guard's [`Drop`].
    ///
    /// **A closure answering [`None`] stops the write**, which is what the
    /// callers here use to say "nothing changed": staging and renaming a
    /// byte-identical document over a directory a real `claude` may be reading
    /// is a rewrite with a reader and no change in it.
    ///
    /// One blocking hop covers lock, read, edit and write together, rather than
    /// a hop each: the guard waits a peer out by sleeping and releases by
    /// `rmdir`, so every part of its life belongs off the runtime. That is also
    /// why `edit` is `'static` and [`Send`] — it runs on that thread — and the
    /// reason a caller that wants the *names* it touched has it answer them
    /// rather than reach back into the document afterwards, when the hold is
    /// gone.
    ///
    /// # Hazards
    ///
    /// **`edit` runs under both halves of the hold**, so a closure that reached
    /// back into this document would wait for itself: the in-process mutex is
    /// already taken, and [`ganja_team::lock`] is not reentrant by that crate's
    /// own design. Everything the closures here need is in the [`TeamFile`]
    /// they are handed, which is the shape that keeps the section short as well
    /// as safe.
    ///
    /// # Errors
    ///
    /// [`SpawnError::TeamFile`] when the lock could not be taken or the
    /// document could not be read or written, and [`SpawnError::Lost`] when the
    /// blocking hop that does it did not come back.
    async fn edit_team<T>(
        &self,
        edit: impl FnOnce(&mut TeamFile) -> Option<T> + Send + 'static,
    ) -> Result<Option<T>, SpawnError>
    where
        T: Send + 'static,
    {
        let path = self.root.config_path(&self.team);
        let team = self.team.clone();
        let session = self.lead_session_id.clone();
        let cwd = self.cwd.to_string_lossy().into_owned();

        // Named rather than `_`, which would drop the guard where it stands and
        // hold nothing at all. The same is true of `_hold` below.
        let _writing = self.team_file.lock().await;

        blocking(move || {
            let failed = |doing: &'static str, source: Box<dyn std::error::Error + Send + Sync>| {
                SpawnError::TeamFile { doing, path: path.display().to_string(), source }
            };
            // The team's directory is what a team file has instead of a seed:
            // a lock named from a directory that is not there is `ENOENT`, and
            // the first spawn of a session is exactly the case where it is not.
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            std::fs::create_dir_all(parent).map_err(|error| failed("written", Box::new(error)))?;

            let _hold =
                lock::acquire_unseeded(&path).map_err(|error| failed("locked", Box::new(error)))?;

            let mut file =
                read_team_file(&path, || TeamFile::new(&team, session, cwd, record::now_millis()))?;
            let Some(answer) = edit(&mut file) else {
                return Ok(None);
            };
            write_team_file(&path, &file)?;

            Ok(Some(answer))
        })
        .await
    }

    /// Adds this teammate to the team file, under
    /// [`TeammateRegistry::edit_team`]'s hold.
    ///
    /// What this method knows is which record goes in; the read, both halves of
    /// the hold and the write back are that door's. Which is the whole point of
    /// there being a door: two spawns racing this would both read a file
    /// without the other's member in it, and the second write would put back a
    /// document missing the first — a teammate that is running, holds a
    /// mailbox, and no team file remembers. The reservation in
    /// [`TeammateRegistry::claim`] keeps their *names* apart; nothing about
    /// that keeps their *records* apart.
    ///
    /// The edit never declines, because a spawn is not a change that can turn
    /// out to be nothing: the record replaces whatever stood under its name, so
    /// there is always a document to write.
    async fn record(&self, spec: &SpawnSpec, surface: Surface) -> Result<(), SpawnError> {
        let record = MemberRecord::teammate(
            &spec.name,
            &spec.team,
            Spawn {
                agent_type: spec.agent_type.clone(),
                model: spec.model.clone(),
                color: spec.color.clone(),
                prompt: spec.prompt.clone(),
                plan_mode_required: spec.plan_mode_required,
                surface,
                cwd: spec.cwd.to_string_lossy().into_owned(),
            },
            record::now_millis(),
        );
        self.edit_team(move |file| {
            file.members.retain(|member| member.name != record.name);
            file.members.push(record);

            Some(())
        })
        .await?;

        Ok(())
    }
}

/// The team file at `path`, or `absent`'s answer where nothing has written one
/// yet.
///
/// Synchronous, and that is what it is for: the read a hold covers
/// ([`TeammateRegistry::edit_team`]) and the read that takes no hold
/// ([`TeammateRegistry::read_team`]) are this one body rather than two that
/// could come to disagree about what a missing document means.
///
/// `absent` is a closure rather than a value because a team file that is not
/// there is the ordinary case for exactly one read of a session — the first —
/// and the document it would be costs three clones and a clock read that every
/// other call would throw away.
///
/// # Errors
///
/// [`SpawnError::TeamFile`] when a document that is there cannot be read or
/// does not decode. A missing one is not an error: a team with no file yet is
/// a team with no members yet.
fn read_team_file(path: &Path, absent: impl FnOnce() -> TeamFile) -> Result<TeamFile, SpawnError> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|error| SpawnError::TeamFile {
            doing: "read",
            path: path.display().to_string(),
            source: Box::new(error),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(absent()),
        Err(error) => Err(SpawnError::TeamFile {
            doing: "read",
            path: path.display().to_string(),
            source: Box::new(error),
        }),
    }
}

/// Writes the team file whole, through a temporary file and a rename: a
/// reader sharing this directory — a real `claude` among them — sees the
/// old document or the new one and never half of either.
///
/// This is [`ganja_team::mailbox`]'s `write_atomically` against the other
/// document of the same interop pair, and it is deliberately the same
/// steps in the same order — temporary beside the target, `sync_all`, the
/// mode copy, `persist` — because the reader they are defending against is
/// literally the same process. Two properties are worth naming out loud:
///
/// * **The bytes are fsynced before the rename.** Without it a crash can
///   leave the *renamed* file present and empty, which is the one outcome
///   a foreign reader cannot tell from "the team has no members" — the
///   torn-write failure the rename exists to prevent, arriving through the
///   back door. The parent directory is **not** fsynced, for the reason
///   spelled out at the mailbox's own copy: a lost rename is
///   indistinguishable from the spawn never having happened, and a reader
///   still sees one whole document or the other.
/// * **The temporary cannot outlive the failure.** The staged name used to
///   be `<path>.json.new-<pid>`, and a rename that failed left it in the
///   team directory for good — beside a document a real `claude` walks.
///   Its life is now the value's: dropped on every path out, including
///   the one where `persist` hands it back.
///
/// Uniqueness per *process* used to be the staged name's job and is now the
/// crate's, but what makes concurrent writes safe was never the name — and is
/// no longer a witness argument either. It used to take the `team_file` guard
/// by reference so a caller could not forget to hold it; a witness only ever
/// proves something about a caller in *this* process, and the writer this
/// document really has to survive is a co-tenant lead in another one. So the
/// parameter was traded for a door: this is private, and
/// [`TeammateRegistry::edit_team`] — which takes both halves of the hold first
/// — is the only thing that calls it.
///
/// # Errors
///
/// [`SpawnError::TeamFile`] when the document could not be encoded, staged,
/// synced or renamed into place.
fn write_team_file(path: &Path, file: &TeamFile) -> Result<(), SpawnError> {
    let failed = |doing: &'static str, source: Box<dyn std::error::Error + Send + Sync>| {
        SpawnError::TeamFile { doing, path: path.display().to_string(), source }
    };
    let document = record::document(file).map_err(|error| failed("encoded", Box::new(error)))?;
    // The temporary has to land in the directory the target is in, or
    // `persist` is a cross-device copy rather than a rename and the
    // atomicity this exists for is gone.
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|error| failed("written", Box::new(error)))?;

    let mut staged =
        NamedTempFile::new_in(parent).map_err(|error| failed("written", Box::new(error)))?;
    staged.write_all(document.as_bytes()).map_err(|error| failed("written", Box::new(error)))?;
    staged.as_file().sync_all().map_err(|error| failed("written", Box::new(error)))?;
    // A temporary is created `0600` and a rename carries that mode
    // onto the target. The team file is *shared* — that is the whole
    // premise of the crate it belongs to — so an existing document's
    // bits are copied across rather than narrowed under a peer that
    // was reading it.
    //
    // A file this *creates* keeps the `0600`, where the `fs::write`
    // this replaces took the umask's answer. Named rather than
    // inherited from the crate's default: the document records every
    // teammate's prompt, model and working directory, and the only
    // reader that has ever mattered — a real `claude` sharing the
    // directory — runs as this same user.
    if let Ok(existing) = std::fs::symlink_metadata(path)
        && existing.file_type().is_file()
    {
        staged
            .as_file()
            .set_permissions(existing.permissions())
            .map_err(|error| failed("written", Box::new(error)))?;
    }
    staged.persist(path).map_err(|error| failed("written", Box::new(error.error)))?;

    Ok(())
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

/// The same hop for every caller whose failure is a sentence rather than a
/// [`SpawnError`]: a lost blocking task and the work's own error collapse to
/// one string, which is what each call site was spelling by hand.
pub async fn blocking_io<T, E, F>(work: F) -> Result<T, String>
where
    T: Send + 'static,
    E: std::fmt::Display + Send + 'static,
    F: FnOnce() -> Result<T, E> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| error.to_string())
        .and_then(|done| done.map_err(|error| error.to_string()))
}

/// §4.1's steps 4 and 5: the inbox exists, and the task is in it.
///
/// The prompt travels through the mailbox rather than the command line, which
/// is what makes "here is your task" and "here is a follow-up" one channel with
/// one ordering and one lock. Returns what identifies the entry, so a spawn
/// that fails afterwards can take it back out. Over values rather than a
/// [`SpawnSpec`], because `ganja_teammate_local::claude::ClaudePane` seeds a
/// different root with a different message through this same body.
pub async fn seed_inbox(
    inbox: PathBuf,
    from: String,
    text: String,
) -> Result<mailbox::Identity, SpawnError> {
    let message = MailboxMessage::new(from, text, record::now_iso8601());
    let identity = mailbox::identity(&message);

    blocking(move || {
        mailbox::seed(&inbox).map_err(|error| SpawnError::Inbox {
            path: inbox.display().to_string(),
            source: error,
        })?;
        mailbox::write_bounded(&inbox, message, Some(postbox::INBOX_CEILING))?;

        Ok(())
    })
    .await?;

    Ok(identity)
}

/// Takes a spawn prompt back out of an inbox a spawn never got to use.
///
/// [`None`] is nothing to do rather than an error: a backend that
/// [`owns`](TeammateBackend::owns_inbox) its inbox was never seeded here, and
/// unwinding what it wrote goes through this same body with its own root and
/// identity (`ganja_teammate_local::claude::ClaudePane`).
///
/// Reported rather than returned: the spawn has already failed, and a cleanup
/// that failed too is a line in the log rather than a second error to explain.
pub async fn unseed_inbox(inbox: PathBuf, seeded: Option<mailbox::Identity>, teammate: &str) {
    let Some(seeded) = seeded else {
        return;
    };
    let pruned = inbox.clone();
    let outcome = blocking_io(move || mailbox::prune_delivered(&pruned, &[seeded])).await;

    if let Err(error) = outcome {
        tracing::warn!(
            teammate,
            inbox = %inbox.display(),
            %error,
            "a refused spawn left its prompt in an inbox"
        );
    }
}

/// Folds a teammate's own event stream into the ring `/teammate` draws (**D503**).
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
            let line = crate::subagent::describe_call(&tools, tool, input);
            // A different call that reads identically to the last one is still
            // one row: the ring is a live view of what a teammate is doing, and
            // two of the same line say nothing the one line did not. Shared
            // with the shim's own writer since P27, so the two cannot come to
            // disagree about the cap or about what counts as a repeat.
            push_recent(&recent, line);
        }
    }
}

#[cfg(test)]
#[path = "teammate_tests.rs"]
pub(crate) mod tests;
