//! What every filesystem-driving binary in this directory rebuilds otherwise:
//! a teams root nobody else can reach, one member's inbox under it, the naive
//! spelling of that inbox's lock, and a message or a spawn with the fields no
//! test here asserts on already filled in.
//!
//! Not a test binary — `tests/support/` is a directory module, compiled only
//! into the binaries that declare `mod support;`. Not `ganja-testkit` either:
//! that crate depends on `ganja-core`, which depends on this one, and a
//! dev-dependency back up that edge would be a cycle.

// Compiled once per binary, and each binary uses its own subset.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use ganja_team::{LEAD, MailboxMessage, MemberName, Spawn, Surface, TeamName, TeamsRoot, record};

/// A root nothing else can reach, and the team a test's paths live under.
///
/// The temporary home comes back too, for the caller to keep alive: a
/// `TempDir` removes its tree on drop.
pub fn root(team: &str) -> (tempfile::TempDir, TeamsRoot, TeamName) {
    let home = tempfile::tempdir().expect("a temp directory");
    let root = TeamsRoot::new(home.path().join("teams"));
    let team = TeamName::parse(team).expect("a valid team name");

    (home, root, team)
}

/// One member's inbox path under `root`. The inbox itself is made by the
/// first write, which is what seeds it.
pub fn inbox_of(root: &TeamsRoot, team: &TeamName, member: &str) -> PathBuf {
    root.inbox_path(
        team,
        &MemberName::parse(member).expect("a valid member name"),
    )
}

/// `${path}.lock`, spelled the way a peer that never canonicalized anything
/// would spell it — through the same symlinks, at the same file.
pub fn naive_lock_of(inbox: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lock", inbox.display()))
}

/// One message from the lead, timestamped now.
pub fn message(text: &str) -> MailboxMessage {
    MailboxMessage::new(LEAD, text, record::now_iso8601())
}

/// A spawn with every field a test here does not assert on filled in.
pub fn spawn(prompt: &str, surface: Surface) -> Spawn {
    Spawn {
        agent_type: "general-purpose".to_owned(),
        model: "claude-opus-5[1m]".to_owned(),
        color: "blue".to_owned(),
        prompt: prompt.to_owned(),
        plan_mode_required: false,
        surface,
        cwd: "/w".to_owned(),
    }
}
