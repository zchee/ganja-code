//! Engine core for ganja: session orchestration, providers, and the agent loop
//! that drives them.
//!
//! This crate carries no terminal-backend dependency — no `ratatui`, no
//! `crossterm` — so the engine stays testable without a terminal and can later
//! be driven over a network transport. CI enforces the rule by asserting that
//! `cargo tree -p ganja-core -e normal` never mentions `ratatui`.
//!
//! Three things the engine is built on are crates of their own, and are
//! re-exported here under the module names they always had: the protocol
//! ([`ganja_protocol`]), the permission engine and project resolution
//! ([`ganja_permission`]), and the tools with the read log behind them
//! ([`ganja_tool`]). The split is what makes the one-way street the compiler's
//! rule — a tool cannot reach back into the engine, because the engine is not in
//! its dependency graph — and the re-exports are what keep `ganja_core::tool`,
//! `ganja_core::permission` and `ganja_core::protocol` meaning what they mean
//! everywhere they are already written.
//!
//! The crate root names the engine's own types and nothing else. The three
//! crates beneath are reachable as the modules they always were, for the
//! callers that want the engine; a caller that wants only one of them depends
//! on it directly. A frontend that only renders must be able to build against
//! `ganja-protocol` alone, and a root that flattens protocol types into the
//! engine's vocabulary invites the opposite.

pub mod agent;
pub mod auth;
pub mod catalog;
pub mod command;
pub mod config;
pub mod engine;
pub mod instruction;
pub mod lsp;
pub mod mcp;
pub mod provider;
pub mod session;
pub mod snapshot;
pub mod storage;
/// Runs the second agent loop a `task` call delegates to.
///
/// Crate-private: what a frontend may know about a subagent is the answer that
/// comes back, and the trait that answer arrives through
/// ([`tool::task::Subagents`]) is the only part of this that is public.
pub(crate) mod subagent;

pub use agent::{Agent, AgentError, Registry as AgentRegistry};
pub use auth::{AuthError, Credential};
pub use catalog::{Cost, ModelInfo};
pub use config::{
    AgentConfig, AgentMode, CommandConfig, Config, ConfigError, LspConfig, LspEntry, McpLocal,
    McpRemote, McpServer, Overrides, ThemeMode, WebfetchConfig,
};
pub use engine::{Engine, EngineError, Evicted};
pub use ganja_permission::{permission, project};
pub use ganja_protocol as protocol;
pub use ganja_tool as tool;
pub use ganja_tool::watch;
pub use instruction::system_prompt;
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
/// breaking the readers the old shape has.
pub use storage::SessionId;
pub use storage::{SessionInfo, Storage, StorageError};
