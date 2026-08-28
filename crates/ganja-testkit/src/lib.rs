//! Shared scaffolding for `ganja-core`'s integration suites.
//!
//! `crates/ganja-core/tests/*.rs` is a family of standalone binaries, several
//! of them deliberately one-test-per-binary (see
//! `ganja-core/tests/AGENTS.md`). Before this crate existed, each one
//! rebuilt the same handful of fixtures from scratch: a [`Provider`] double
//! that plays back a script and records what it was asked, a [`Tool`] double
//! that records a call or blocks until cancelled, the drain loop that
//! collects a turn's events (optionally answering permission dialogs along
//! the way), the storage builders that seed a session directly on disk, the
//! teammate fixtures P25's suites share, and the private tmux server every
//! pane suite runs against.
//!
//! This crate exists to hold exactly that — nothing that is genuinely
//! specific to one suite (a bun fixture's own spawn helper, a
//! provider-failure-and-repeat schedule only one file needs) belongs here;
//! it stays in the file that needs it. See each module for what moved and
//! why.
//!
//! [`Provider`]: ganja_core::provider::Provider
//! [`Tool`]: ganja_tool::Tool

mod agent;
mod drain;
mod fs;
mod log;
mod provider;
mod session;
mod subagent;
mod teammate;
pub mod tmux;
mod tool;

pub use agent::agent_registry;
pub use drain::{drain, drain_allowing, drain_answering};
pub use fs::{Homes, plant, redirect_xdg_data_home, temp_dir};
pub use log::LogCapture;
pub use provider::{ScriptedProvider, says, tool_call};
pub use session::{
    PRE_UUID_ID, entries, plant_preuuid_store, seed_message, seed_session, seeded_session_info,
    set_aside_of,
};
pub use subagent::{RecordingSpawner, ScriptedSubagents};
pub use teammate::{
    AllowSpawn, LEAD_SESSION_ID, RecordedSpawns, RunnerHarness, TASK, TEAM, backends, caller,
    caller_with, eventually, externals, flooded_inbox, seed_team_file, spawn, spawn_with_prompt,
    team, team_file, team_with, teammates_recorded,
};
pub use tool::{BlockingTool, RecorderTool, placeholder_schema, tool_ctx};
