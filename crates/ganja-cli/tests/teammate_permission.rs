//! A pane teammate's permission ask reaches its lead's dialog, and the lead's
//! answer lets the call run (**AC-8**, **D-5**).
//!
//! Upstream opencode has no counterpart; the reference is Claude Code's §5
//! permission frames riding the §2 mailbox between two processes. What is
//! pinned here is the whole loop with **both** processes being the shipped
//! binary: a real lead in a private tmux server spawns a real pane with
//! `/teammate spawn w1 --backend ganja`, the pane's scripted turn calls `bash` —
//! a tool the builtin rules ask about — and under `ForwardToLead` no dialog
//! opens in the pane: the ask crosses to the lead as a `permission_request`,
//! the lead's own pass puts it in front of the same dialog its in-process
//! teammates use, the person at the lead answers it, and the answer crosses
//! back as a `permission_response` that the pane's engine takes as the reply.
//! The call then runs, and the file it writes is the proof — read off the
//! filesystem, because a screen is only ever sent the cells that changed.
//!
//! One test, per the plan's own table: two processes on a private server is
//! the whole of what this binary is for. Hard-fails without tmux, for the
//! reason `teammate_pane_lifecycle.rs` gives.

#![cfg(unix)]

mod pane_lead;

use std::fs;
use std::time::Instant;

use pane_lead::{DEADLINE, DIALOG_OPTIONS, Homes, Lead, TEAMMATE, wait_for};
use serde_json::json;

/// What the pane's shell call writes, appearing nowhere else.
const RAN: &str = "ac8-forwarded-ask-allowed-zarquon";

/// The scripted turn after the call, so the pane's turn has an end.
const CLOSING: &str = "script-finished-zarquon";

#[test]
fn a_panes_ask_reaches_the_leads_dialog_and_the_leads_answer_lets_the_call_run() {
    let homes = Homes::new();
    let marker = homes.project().join("ran.txt");
    // The pane's turn: one `bash` call the rules ask about, then a closing
    // word. `GANJA_FAKE_SCRIPT` names it in the server's environment, so it is
    // what a spawned pane plays; the lead runs no turn of its own here.
    let script = homes.script(
        "pane.json",
        json!([
            {"tool_calls": [{
                "name": "bash",
                "args": {"command": format!("echo {RAN} > {}", marker.display())}
            }]},
            {"text": CLOSING}
        ]),
    );
    let lead = Lead::start(&homes, &script, &[], &[]);

    // This lead's socket is in the fixture's own directory rather than in the
    // developer's `/tmp/ganja-<uid>/`, where it would show in their
    // `sessions --live` and leave a `.lock` behind for good. Pinned on both
    // shapes of lead this fixture starts, because the flag reaches them by
    // different roads — an argv word for `Tmux::lead`, a shell-quoted word in
    // the window command here — and a road that lost it reads as an empty
    // directory rather than as a failure anywhere else.
    let bound = wait_for("the lead to bind its socket", || {
        let found = pane_lead::bound_sockets(&homes);
        (!found.is_empty()).then_some(found)
    });
    assert_eq!(bound.len(), 1, "one session is one socket: {bound:?}");

    // The person's door, exactly as AC-11 spells it, plus the task. Nothing
    // is asked at spawn: the pane works inside the project, so the spawn gate
    // has nothing to raise.
    lead.type_line(&format!("/teammate spawn {TEAMMATE} --backend ganja write the marker"));
    let (pane, _) = lead.wait_for_teammate_pane();

    // The lead's dialog, about the pane's call: the one dialog in either
    // window. Its title names the teammate, which is the lead-side pass's
    // own spelling for a forwarded ask.
    let dialog = lead.wait_for_screen(lead.pane(), |screen| screen.contains(DIALOG_OPTIONS));
    // Scoped to the **title's** own separator (`app.rs`'s `"{teammate} · {title}"`)
    // rather than to the name anywhere on the screen: a screen-wide `contains`
    // was non-vacuous only because the dialog overlay happens to blank the
    // transcript, so it would have gone quiet the day it stopped doing that.
    assert!(
        dialog.contains(&format!("{TEAMMATE} \u{b7} ")),
        "the lead's dialog titles the ask with the teammate that raised it:\n{dialog}"
    );
    assert!(
        !lead.screen(&pane).contains(DIALOG_OPTIONS),
        "and the pane draws no dialog of its own:\n{}",
        lead.screen(&pane)
    );
    assert!(!marker.exists(), "nothing ran before the lead answered");

    lead.press("y");

    // The answer crossed back and the call ran: the file says so.
    let started = Instant::now();
    loop {
        if let Ok(written) = fs::read_to_string(&marker)
            && written.contains(RAN)
        {
            break;
        }
        assert!(
            started.elapsed() < DEADLINE,
            "the call never ran after the lead allowed it; the pane shows:\n{}\nthe lead shows:\n{}",
            lead.screen(&pane),
            lead.screen(lead.pane()),
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // And the pane's turn ran to its end, past the call.
    lead.wait_for_screen(&pane, |screen| screen.contains(CLOSING));
}
