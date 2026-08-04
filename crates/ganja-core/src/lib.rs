//! Engine core for ganja: session orchestration, providers, tools, and the
//! serde-serializable command/event protocol that frontends speak.
//!
//! This crate carries no terminal-backend dependency — no `ratatui`, no
//! `crossterm` — so the engine stays testable without a terminal and can later
//! be driven over a network transport. CI enforces the rule by asserting that
//! `cargo tree -p ganja-core -e normal` never mentions `ratatui`.

pub mod agent;
pub mod auth;
pub mod catalog;
pub mod command;
pub mod config;
pub mod engine;
pub mod instruction;
pub mod permission;
pub mod project;
pub mod protocol;
pub mod provider;
pub mod session;
pub mod storage;
pub mod tool;

pub use agent::{Agent, AgentError, Registry as AgentRegistry};
pub use auth::{AuthError, Credential};
pub use catalog::{Cost, ModelInfo};
pub use config::{
    AgentConfig, AgentMode, CommandConfig, Config, ConfigError, Overrides, PermissionConfig,
    ThemeMode,
};
pub use engine::{Engine, EngineError};
pub use instruction::system_prompt;
pub use permission::{Decision, Permissions};
pub use project::{Project, ProjectError};
pub use protocol::{
    Command, Event, FinishReason, Mention, Message, MessageId, MessageTime, Part, PartBody, PartId,
    PermissionId, PermissionReply, Role, ToolState, Usage,
};
pub use storage::{SessionId, SessionInfo, Storage, StorageError};
pub use tool::{Registry, Tool, ToolCtx, ToolDefinition, ToolError, ToolOutput};
