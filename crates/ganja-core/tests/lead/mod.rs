//! The ground both team suites stand their lead on.
//!
//! `team_discipline.rs` and `team_tasks.rs` each drive a persistent engine that
//! leads a team, and each wraps it in a `Lead` of its own — one recording what
//! was announced and holding requests mid-turn, the other reading tool rosters
//! — so the harnesses stay theirs. What is shared is what came before them: the
//! temporary home, the store, the teams root, the team name, the registry wired
//! to those, and the two calls every test in either binary makes.
//!
//! Sharing them is not only about the lines. Both binaries assert about the
//! same team under the same lead session id, and a fixture that drifted on one
//! side would go on passing while asserting about a different team.
//!
//! Here rather than in `ganja-testkit` because the neighbouring entry there
//! does not fit: `team_with` builds a `Teammates` door beside the registry, and
//! a door neither binary opens is one both would have to explain.
//!
//! `tests/lead/mod.rs` rather than `tests/lead.rs`, because cargo makes a test
//! binary of every `tests/*.rs` and a binary holding no tests is a target
//! somebody has to account for.

use std::sync::Arc;

use ganja_core::protocol::Command;
use ganja_core::teammate::TeammateRegistry;
use ganja_core::{Engine, Storage};
use ganja_team::task::Store;
use ganja_team::{TeamName, TeamsRoot};
use ganja_testkit::{LEAD_SESSION_ID, TEAM};

/// A home that goes away with the test, the store an engine is to be built on,
/// and the team it is to be wired to.
///
/// A tuple rather than a struct for `ganja_testkit::team`'s reason — every
/// caller wants all five and names them itself — with the home **last**,
/// because that is where both sides declare it: the engine's storage lives
/// under it, and taking the directory away while an engine still holds it is
/// the reverse of the safe order.
pub fn ground() -> (TeamsRoot, TeamName, Arc<TeammateRegistry>, Storage, tempfile::TempDir) {
    let home = ganja_testkit::temp_dir();
    let storage = Storage::open(home.path().join("storage"));
    let root = TeamsRoot::new(home.path().join("teams"));
    let team = TeamName::parse(TEAM).expect("a team name");
    let registry =
        Arc::new(TeammateRegistry::new(root.clone(), team.clone(), LEAD_SESSION_ID, home.path()));

    (root, team, registry, storage, home)
}

/// The team's task documents, read the way any other process on the machine
/// would read them.
pub fn store(root: &TeamsRoot, team: &TeamName) -> Store {
    Store::new(root.tasks_dir(team))
}

/// Sends `text` the way a person typing it does.
pub async fn prompt(engine: &Engine, text: &str) {
    engine
        .send(Command::SendPrompt {
            text: text.to_owned(),
            mentions: Vec::new(),
            skills: Vec::new(),
            session_mentions: Vec::new(),
            peers: Vec::new(),
        })
        .await
        .expect("an idle engine accepts a prompt");
}
