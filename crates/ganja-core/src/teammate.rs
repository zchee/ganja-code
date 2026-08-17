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
//! # Scope
//!
//! This module is the construction path and nothing else. The backend trait,
//! the registry that owns a teammate's lifetime, the §6.1 runner and the
//! permission posture are the later lanes' — they build against
//! [`crate::teammate::Teammate::new`].
//!
//! [`Registry`]: crate::tool::Registry

use std::{sync::Arc, time::Duration};

use crate::{Engine, Storage, permission::Permissions, provider::Provider, tool::Registry};

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
            engine: Engine::persistent(provider, model, tools, permissions, storage),
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
    /// Returns whether the engine really went idle within `limit`.
    pub async fn shutdown(&self, limit: Duration) -> bool {
        let settled = self.engine.settle(limit).await;
        self.engine.shutdown_jobs().await;

        settled
    }
}
