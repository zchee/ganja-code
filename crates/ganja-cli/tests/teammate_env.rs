//! What a `ganja` pane's command line and environment carry from its lead —
//! and what they must not (**D502**, §10.2 step 6, §10.12-4).
//!
//! Upstream opencode has no counterpart. The reference is Claude Code's
//! §10.10: a new pane inherits the tmux **server's** environment, not the
//! client's, so a lead has to carry its config home to the pane explicitly —
//! and must never carry a secret, because `argv` is `ps(1)`-visible to every
//! user on the machine. `ganja-teammate-local/tests/teammate_pane_env.rs` pins the
//! mechanism with a fake pane child that reports what it received; this binary
//! is the same two facts with **both** processes being the shipped binary,
//! read off the outside: `ps` for the pane's argv, the store and the team file
//! for whether the pane joined the team.
//!
//! Every variable travels to a child — the server, the lead, the pane — and
//! nothing here calls `set_var`, so the binary holds two tests.

#![cfg(unix)]

mod pane_lead;

use std::fs;

use ganja_core::protocol::{PartBody, Role};
use pane_lead::{Homes, Lead, TEAMMATE, argv_of, wait_for};
use serde_json::json;

/// A credential the lead holds, and the string the test greps a pane's
/// command line for.
const CANARY: &str = "sk-ant-CANARY-b1a5f7-never-on-a-launch-line";

/// The prompt the lead types, which carries the canary too: §4.1 step 5 puts
/// the prompt in the mailbox and never on the line, and this is what would
/// show if it did.
const PROMPT: &str = "the task mentions sk-ant-CANARY-b1a5f7-never-on-a-launch-line on purpose";

/// The one word the pane's turn says, so the pane's session can be told from
/// the lead's by what it holds.
const REPLY: &str = "pane-turn-zarquon";

/// The pane's script: one turn, no tool, no dialog — the turn is the proof
/// that the pane joined the team, since it plays only once the seed reached
/// it through the team's own inbox.
fn one_turn(homes: &Homes) -> std::path::PathBuf {
    homes.script("pane.json", json!([{"text": REPLY}]))
}

/// The pane's process, once the launch line has replaced the shell tmux
/// forked: `exec` keeps the pid, so the pane's pid is the binary's.
fn pane_argv(lead: &Lead) -> String {
    let (_, pid) = lead.wait_for_teammate_pane();

    wait_for("the pane's shell exec'd the binary", || {
        let argv = argv_of(pid);
        argv.contains("--agent-id").then_some(argv)
    })
}

/// **The argv-secrets pin.** A credential in the lead's environment, and a
/// canary in the spawn prompt, reach neither the pane's command line nor
/// tmux's record of it: the line is the five flags, the prompt travels the
/// mailbox, and the environment the pane is given is a closed list of names.
#[test]
fn a_secret_the_lead_holds_never_reaches_a_panes_command_line() {
    let homes = Homes::new();
    let script = one_turn(&homes);
    let lead = Lead::start(
        &homes,
        &script,
        &[],
        &[("ANTHROPIC_API_KEY", CANARY), ("GANJA_SERVER_PASSWORD", CANARY)],
    );

    lead.type_line(&format!("/teammate spawn {TEAMMATE} --backend ganja {PROMPT}"));
    let argv = pane_argv(&lead);

    assert!(
        !argv.contains(CANARY),
        "a credential the lead holds is on the pane's command line: {argv}"
    );
    assert!(!argv.contains("mentions"), "the prompt travels the mailbox, never the line: {argv}");
    for flag in
        ["--agent-id", "--agent-name", "--team-name", "--agent-color", "--parent-session-id"]
    {
        assert!(argv.contains(flag), "{flag} is on the line: {argv}");
    }
    assert!(
        !argv.contains("--auto"),
        "a lead never puts the bypass trio on a pane's line (D513): {argv}"
    );
    assert!(
        !lead.global_has("ANTHROPIC_API_KEY"),
        "and the server the pane inherits from never had the credential"
    );
    // The other direction, on the one table a member's launch actually
    // inherits (§10.10): the kitty-probe kill switch (**D517**) is on the
    // **server's** environment, so every pane spawned here starts with it.
    // Set on the lead alone it was the lead's alone, and each member pane
    // opened by blocking a stage for up to two seconds on a query this tmux
    // never answers — `pane.rs`'s carried environment does not name the
    // variable, so there is no other way in. Asserted through tmux's own
    // record rather than by reading a pane's environment, which no portable
    // call can do: `ps` shows argv, and this is not on it.
    assert!(
        lead.global_has("GANJA_DISABLE_TERM_PROBE"),
        "every pane this server makes inherits the probe kill switch (D517)"
    );
}

/// **The failure D502's allowlist fixes.** The tmux server is born without
/// the lead's config home; the lead alone holds it, and its team lives under
/// it. A pane inheriting only the server's environment would resolve another
/// home, find no team and no record, and refuse — the one below joins the
/// lead's team, takes its seeded task, and writes a session row into the
/// lead's store, because the launch carried the variable.
#[test]
fn a_pane_joins_the_team_when_the_tmux_server_predates_the_config_home_export() {
    let homes = Homes::new();
    let script = one_turn(&homes);
    // A home nothing in the server's environment resolves to: not under the
    // `XDG_CONFIG_HOME` the server does carry, so a pane without the variable
    // would look somewhere else entirely.
    let home = homes.data().join("elsewhere").join("ganja-home");
    fs::create_dir_all(&home).expect("the lead's home is creatable");
    let lead = Lead::start(
        &homes,
        &script,
        &["GANJA_CONFIG_HOME"],
        &[("GANJA_CONFIG_HOME", &home.display().to_string())],
    );
    assert!(
        !lead.global_has("GANJA_CONFIG_HOME"),
        "the server predates the export, by construction"
    );

    lead.type_line(&format!("/teammate spawn {TEAMMATE} --backend ganja say the word"));
    let (pane, _) = lead.wait_for_teammate_pane();

    // The pane's turn ran, which it can only have done from the lead's team's
    // inbox: the reply is on its screen, and the row it wrote is in the
    // lead's store — a root, carrying the seed as the lead's attributed words.
    lead.wait_for_screen(&pane, |screen| screen.contains(REPLY));
    let storage = homes.store();
    let seeded = wait_for("the pane's session row reached the store", || {
        storage
            .list_sessions()
            .expect("the store lists")
            .into_iter()
            .find(|session| {
                storage
                    .load_transcript(&session.id)
                    .is_ok_and(|transcript| {
                        transcript.iter().any(|message| {
                            message.role == Role::User
                                && message.parts.iter().any(|part| {
                                    matches!(&part.body, PartBody::Peer { from, .. } if from == "team-lead")
                                })
                        })
                    })
            })
    });
    assert!(seeded.parent.is_none(), "a teammate's row is a root");

    // And the team file under the lead's own home names the pane, by the id
    // tmux gave it.
    let teams = fs::read_dir(home.join("teams"))
        .expect("the lead's home holds its teams")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    assert_eq!(teams.len(), 1, "one lead, one team: {teams:?}");
    let document = fs::read_to_string(teams[0].join("config.json")).expect("the team file reads");
    assert!(
        document.contains(&format!("\"{pane}\"")),
        "the record names the pane {pane}:\n{document}"
    );
}
