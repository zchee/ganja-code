//! Engine core for ganja: session orchestration, providers, tools, and the
//! serde-serializable command/event protocol that frontends speak.
//!
//! This crate carries no terminal-backend dependency — no `ratatui`, no
//! `crossterm` — so the engine stays testable without a terminal and can later
//! be driven over a network transport. CI enforces the rule by asserting that
//! `cargo tree -p ganja-core -e normal` never mentions `ratatui`.

pub mod engine;
pub mod protocol;
pub mod provider;

pub use engine::{Engine, EngineError};
pub use protocol::{Command, Event, FinishReason};
