//! Engine core for ganja: session orchestration, providers, and the agent loop
//! that drives them.
//!
//! This crate carries no terminal-backend dependency — no `ratatui`, no
//! `crossterm` — so the engine stays testable without a terminal and can later
//! be driven over a network transport. CI enforces the rule by asserting that
//! `cargo tree -p ganja-core -e normal` never mentions `ratatui`.
//!
//! Four things the engine is built on are crates of their own, and are
//! re-exported here under the module names they always had: the protocol
//! ([`ganja_protocol`]), the permission engine and project resolution
//! ([`ganja_permission`]), the tools with the read log behind them
//! ([`ganja_tool`]), and the vendor wires with the credentials and catalog
//! they need ([`ganja_provider`]). The split is what makes the one-way street
//! the compiler's rule — a tool cannot reach back into the engine, and neither
//! can a wire, because the engine is not in their dependency graph — and the
//! re-exports are what keep `ganja_core::tool`, `ganja_core::permission`,
//! `ganja_core::protocol`, `ganja_core::auth` and `ganja_core::catalog`
//! meaning what they mean everywhere they are already written.
//!
//! [`provider`] is the one of the four that did not leave whole: the half of
//! it that reads a [`Config`] — which provider a session runs as, and which
//! model it asks for — stayed, over a facade that re-exports the wires. That
//! module's own docs say why.
//!
//! The crate root names the engine's own types and nothing else. The crates
//! beneath are reachable as the modules they always were, for the callers that
//! want the engine; a caller that wants only one of them depends on it
//! directly. A frontend that only renders must be able to build against
//! `ganja-protocol` alone, and a root that flattens protocol types into the
//! engine's vocabulary invites the opposite.

pub mod agent;
pub mod attachment;
pub mod command;
pub mod config;
pub mod engine;
/// Commands a config asks this build to run at nine named moments of a
/// session. Engine-side by necessity rather than by taste: a headless `run`
/// fires the same hooks a screen does, and a `PreToolUse` that can refuse a
/// call has to sit where the call is executed.
pub mod hook;
pub mod instruction;
/// Background shell jobs — `bash` calls run with `run_in_background: true` —
/// outliving the turns that start them. The trait every caller reaches this
/// through ([`tool::job::Jobs`]) lives in `ganja-tool`, the same seam
/// `tool::task::Subagents` draws; this module is the one implementation.
pub mod job;
pub mod lsp;
pub mod mcp;
/// Claude Code plugins: the `.claude-plugin` manifest and marketplace
/// shapes, the install store under the config home, and the merge that turns
/// an installed plugin into a config contributor. Engine-side because the
/// contributions land in [`Config`] at load, before any
/// frontend exists to ask.
pub mod plugin;
pub mod provider;
pub mod session;
/// Runs the second agent loop a `task` call delegates to, and starts the
/// teammates that call's other door asks for.
///
/// Crate-private: what a frontend may know about a subagent is the answer that
/// comes back, and the trait that answer arrives through
/// ([`tool::task::Subagents`]) is one of the two parts of this that are public.
/// The other is the teammate half — [`Teammates`], [`Backends`] and
/// [`Postbox`], re-exported at the root below — because those are values a
/// frontend leading a team **assembles**, where a subagent is one it only ever
/// reads the answer of.
///
/// Since **D538** [`Backends`] is assembled *outside* the engine and is the
/// clearest case of that split: what a pane or a foreign CLI's TUI needs — a
/// tmux server, the shell a pane is split into, how wide the column opens — is
/// a frontend's to hold, so the frontend builds those implementations and
/// [`Engine::with_teammates`] adds only the in-process one it can build out of
/// what it already has.
pub(crate) mod subagent;
/// A teammate's own conversation, running in the lead's process: a second
/// [`Engine`] over a clone of the lead's [`Storage`] (**D500**). Public where
/// `subagent` is crate-private, because a teammate is a session a frontend
/// addresses by name and resumes tomorrow, not an answer that comes back from
/// a tool call.
pub mod teammate;

pub use agent::{Agent, AgentError, Registry as AgentRegistry};
pub use auth::{AuthError, Credential};
pub use catalog::{Cost, ModelInfo};
pub use config::{
    AgentConfig, AgentMode, AgentsConfig, CommandConfig, Config, ConfigError, HookCommand,
    HookHandler, HookMatcher, LspConfig, LspEntry, McpLocal, McpRemote, McpServer, Overrides,
    ThemeMode, WebfetchConfig,
};
pub use engine::{Engine, EngineError, Evicted};
pub use ganja_permission::{permission, project};
pub use ganja_protocol as protocol;
/// The credential store and the model table, as the modules they always were.
/// Both moved to [`ganja_provider`] with the wires that read them — the
/// provider/auth boundary carries no invariant anyone would gate — and both are
/// named here so no caller had to notice.
pub use ganja_provider::{auth, catalog};
/// The session store and the working-tree snapshots, as the modules they
/// always were before **D540** moved them below the engine rather than
/// inside it: `storage.rs` and `snapshot.rs` needed only a project's
/// worktree and the wire types a stored record decodes to, never the loop
/// that calls them, which is what makes `ganja-storage` a leaf rather than a
/// module here. `crate::snapshot` and `crate::storage` keep meaning what
/// they always meant to every caller that reads them.
pub use ganja_storage::{snapshot, storage};
/// Claude's own team documents — the team file, the member records and the
/// mailbox — as the module they would have been if they had not needed a crate
/// of their own ([`crate::teammate`]'s neighbour, and its store).
///
/// Named here for the reason [`protocol`] and [`tool`] are: a frontend that
/// leads a team has to be able to *write* one of these documents in a fixture,
/// and reaching a second path dependency to do it would be a build-file change
/// in every crate that ever tests a lead. What it deliberately does **not**
/// invite is decoding one outside the engine: the classification a lead does
/// over its own inbox lives in [`crate::teammate::lead_inbox`], beside the §6.1 loop
/// that answers it, and a second copy of it in a frontend would be two readings
/// of one format.
pub use ganja_team as team;
pub use ganja_tool as tool;
pub use ganja_tool::watch;
pub use lsp::Lsp;
pub use mcp::{Servers as McpServers, Status as McpStatus};
pub use snapshot::{RevertState, Snapshots};
/// `SessionId` is the one protocol type the root names, and the exception is
/// deliberate rather than drift: the crate doc above says the root carries
/// the engine's own types and nothing else, and this *was* one — it lived in
/// `storage` until events began naming their session, at which point a wire
/// type had to move to [`ganja_protocol`]. The root keeps naming it because
/// callers outside that change's blast radius were already reading it here,
/// and the curation rule is about not inviting new flattening, not about
/// breaking the readers the old shape has. `storage` itself is
/// [`ganja_storage::storage`] now (**D540**), reached through the re-export
/// two lines above; this re-export's own path is unaffected by that move.
pub use storage::SessionId;
pub use storage::{SessionInfo, Storage, StorageError};
/// The two seams a session that leads a team installs into its tool contexts:
/// the `task` tool's teammate door, and the one `send_message` posts through.
/// Named at the root for the reason [`Snapshots`] and [`McpServers`] are — a
/// frontend builds them and hands them to an engine — while everything else in
/// `subagent` stays crate-private. [`Caller`] is what one spawn's calling turn
/// brings, and [`SpawnAsker`] is where a spawn the rules do not settle is put
/// in front of somebody. [`Incoming`], [`NotReceived`], [`SocketMessage`]
/// and [`SocketDelivered`] are the socket route's half (**D505**): what
/// `ganja-serve` hands [`Engine::receive_peer_message`] and reads back, and
/// the two wire bodies both ends of `uds:` delivery spell. [`SocketReceipt`]
/// and [`ReceiptStatus`] are the same route's settlement half (**D534**):
/// what a sender's own socket answers `POST /peer/receipt` with.
pub use subagent::{
    Backends, Caller, Incoming, NotReceived, Postbox, ReceiptStatus, SocketDelivered,
    SocketMessage, SocketReceipt, SpawnAsk, SpawnAsker, Teammates,
};
