//! What ganja is allowed to do, and where.
//!
//! [`permission`] is the crate's subject: a call becomes one or more patterns,
//! the last matching rule wins, and every pattern has to come back allowed for
//! the call to run without asking. Its own crate because the answer must not
//! depend on the loop that asks — a tool checks a write for containment, the
//! engine raises a dialog, and a stored answer outlives both — and because
//! nothing in here may reach back for a session to consult.
//!
//! [`project`] rides along, and the crate name is a small lie about that. It is
//! here because it is what the rules are keyed by: which worktree this is
//! decides where the stored answers live and what counts as outside the
//! project. A few hundred settled lines that only this crate and the engine
//! read do not earn a crate of their own, and the alternative — a micro-crate
//! with one reader — buys a truer name at the cost of a manifest nobody
//! benefits from.

pub mod permission;
pub mod project;

// The inner module's name is load-bearing for `ganja-core`'s facade —
// `ganja_core::permission` must keep resolving — so the
// `ganja_permission::permission` stutter exists by construction. These
// crate-root re-exports are how a direct consumer avoids paying it, and
// `Action` belongs here with its siblings because rules are the crate's
// primary vocabulary.
pub use permission::{Action, CallDecision, Decision, PermissionConfig, Permissions};
pub use project::{Project, ProjectError};
