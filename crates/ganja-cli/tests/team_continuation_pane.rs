//! **W4's acceptance drill.** A real `ganja`, in a pane, auto-continues while
//! its team holds work — and stops after five.
//!
//! The guard is engine-native and unit-tested at three heights already; what
//! only a whole binary can show is that it survives everything between the
//! agent loop and a person's terminal: the turn really keeps going, and the
//! session really becomes idle again afterwards rather than spinning. A guard
//! that quietly never fired — or one that never stopped — would pass every
//! in-process test and be unusable.
//!
//! Spec: this port's own team-orchestration behavior specification (decision
//! 11). Neither upstream has an auto-continuation of any kind, so there is
//! nothing of either's to port.
//!
//! **Hard-fails without tmux**, for `team_tasks_pane.rs`'s reason: a pane test
//! that skipped where there is no tmux would be green on exactly the machines
//! where nothing was tested.
//!
//! # What is asserted, in order
//!
//! 1. The lead files a task and its turn **ends** — a session leading nobody
//!    is not a team with work stranded in it.
//! 2. A teammate joins, so the registry holds somebody running.
//! 3. The next turn would end the same way and does not: the guard continues
//!    it, five times, each one saying so once in the log.
//! 4. The sixth is refused and the turn ends — the status bar reads idle
//!    again, so the session is back with the person rather than talking to
//!    itself.
//!
//! # Every line is typed into an idle lead, and the composer does not say so
//!
//! The composer's placeholder is drawn whenever the buffer is empty, a
//! streaming reply included, so it is no sign that a turn has ended. Read as
//! one, it let two lines land inside running turns (bead `d61w`, seen once
//! under a five-thread workspace run): the spawn line while the first turn's
//! last request was still streaming — a spawn completes inside a running
//! turn, and that turn's own tail then finds a live team and spends a
//! continuation — and `CARRY_ON` as a steer into that continuation, which
//! puts the budget back by design (`teammate::discipline`: a continuation
//! loses to a person). Six continuations in the log, one breaker, and nothing
//! the engine did wrong: the drill had measured a turn it never meant to
//! start. So the lead is waited on before the spawn line — [`pane_lead::idle`],
//! the status bar's own word, set by the handler that clears the frontend's
//! turn flag — and the spawn is waited on before `CARRY_ON`, so the one turn
//! the guard is about begins as a prompt, with the team already live.
//!
//! # One script, entries that do not care where they land
//!
//! A fake-provider script is played one entry per **request**, and a session
//! makes requests this drill never asks for — a title, at a moment of its own
//! choosing, and the teammate's own turn shares this process's provider. So
//! only the first entry does anything, and every entry after it is the same
//! plain reply: whichever request lands on whichever, the answer is a step
//! with no tool calls, which is exactly the step the guard is about. Pinning
//! an index would be pinning a race (W3's finding, kept).

#![cfg(unix)]

use std::fs;
use std::time::Duration;

use serde_json::json;

mod pane_lead;

use pane_lead::{Homes, SPAWN_NOTICE, Tmux};

/// How long each stage is given: a debug `ganja` starting cold in a pane, then
/// seven provider round trips against a scripted provider.
const DEADLINE: Duration = Duration::from_secs(60);

/// What the engine traces once per auto-continuation.
const CONTINUED: &str = "the team still holds work, so the turn continues";

/// What it traces when the breaker trips, exactly once.
const STOPPED: &str = "the turn stopped auto-continuing and handed the session back";

/// The note the first continuation carries. Counted on its own because a
/// budget put back looks like exactly this line twice — which is what the
/// flake this drill once had looked like — and a count that names the cause
/// is worth more than one that is off by one.
const FIRST: &str = "auto-continuation 1 of 5";

/// The constant the breaker is set to. Restated here rather than imported
/// because this drill is about the shipped binary's behavior, and a test that
/// read the same constant the code reads could not notice the two coming
/// apart.
const MAX_CONTINUATIONS: usize = 5;

/// The teammate's name.
const MEMBER: &str = "w1";

/// What the lead files.
const SUBJECT: &str = "port the parser";

/// The lead's first prompt: the one turn that files the work.
const FILE_PROMPT: &str = "file the work";

/// Its second, after the teammate has joined. Answered with a plain reply, so
/// the only reason the turn does not end here is the guard.
const CARRY_ON: &str = "anything else";

/// The script both conversations play.
const SCRIPT: &str = "turns.json";

/// The homes, the script, and this drill's reads of what the lead wrote.
struct Fixture {
    homes: Homes,
}

impl Fixture {
    fn new() -> Self {
        let homes = Homes::new();
        // Entry one files the task. Everything after it is the same plain
        // reply — a step with no tool calls, which is the step a turn ends on
        // and therefore the step the guard has an opinion about. Generous in
        // number: the run needs one opening step, five continuations and a
        // title, and an entry too many costs nothing while an entry too few
        // would end the turn for the wrong reason.
        let mut turns = vec![json!({
            "text": "Filing it.",
            "tool_calls": [{"name": "task_create", "args": {
                "subject": SUBJECT,
                "description": "start from the spec",
            }}],
        })];
        turns.extend(std::iter::repeat_n(json!({"text": "Nothing more from me."}), 24));
        homes.script(SCRIPT, json!(turns));

        Self { homes }
    }

    /// The environment the server is born from, and so what every pane
    /// inherits. No script: both conversations here are the lead's own, and
    /// the one it plays is on its own process.
    fn server_env(&self) -> Vec<(&'static str, String)> {
        pane_lead::server_env(&self.homes, None)
    }

    /// What the lead's own pane is additionally given (`-e`).
    fn lead_env(&self) -> Vec<(&'static str, String)> {
        pane_lead::lead_env(&self.homes, Some(&self.homes.project().join(SCRIPT)))
    }

    /// How many times the lead's log says `needle`.
    fn logged(&self, needle: &str) -> usize {
        pane_lead::log_text(&pane_lead::log_dir(&self.homes)).matches(needle).count()
    }

    /// The tail of the log, for a wait that timed out.
    fn log_tail(&self) -> String {
        pane_lead::log_tail(&pane_lead::log_dir(&self.homes))
    }

    /// Whether the lead's team directory holds any task document yet.
    fn filed_anything(&self) -> bool {
        let Ok(teams) = fs::read_dir(self.homes.config_home().join("teams")) else {
            return false;
        };

        teams
            .filter_map(Result::ok)
            .filter_map(|team| fs::read_dir(team.path().join("tasks")).ok())
            .any(|tasks| {
                tasks.filter_map(Result::ok).any(|entry| {
                    entry.path().extension().is_some_and(|extension| extension == "json")
                })
            })
    }
}

/// A real `ganja` auto-continues while its team holds work, and the breaker
/// stops it after five.
#[test]
fn a_lead_auto_continues_for_its_team_and_the_breaker_halts_it_at_five() {
    let fixture = Fixture::new();
    let tmux = Tmux::start(&fixture.homes, &fixture.server_env(), DEADLINE);

    // The lead, in a pane of its own in the project directory — so tmux gives
    // it `TMUX` and `TMUX_PANE` itself.
    let lead = tmux.lead(&fixture.homes, &fixture.lead_env());

    // 1. The work is filed before there is anybody to do it, which is the
    // order the pipeline actually runs in — and that turn **ends**: open work
    // with nobody running is a list, not a stranded team.
    // The two waits are one fact each, in this order: the bar reads `ready`
    // before a turn starts too, so what makes the second wait mean "ended"
    // is the first having proved a tool ran.
    tmux.type_line(&lead, FILE_PROMPT);
    tmux.wait_for("the lead to file the task", &lead, || fixture.filed_anything().then_some(()));
    tmux.wait_for("the first turn to end with no continuation", &lead, || {
        pane_lead::idle(&tmux.screen(&lead)).then_some(())
    });
    assert_eq!(
        fixture.logged(CONTINUED),
        0,
        "nobody was running, so nothing was stranded: {}",
        fixture.log_tail(),
    );

    // 2. Somebody joins. In-process rather than a second pane on purpose: what
    // the guard reads is the registry, and a second binary would only add a
    // second thing that can be slow — and it is what keeps the lead's inbox
    // empty for the rest of the drill, since a member whose script calls no
    // tool writes nothing there, where a backend that reports back would
    // steer the turn below from the other side and put the budget back the
    // same way a typed line does. Typed into an idle lead — the wait above —
    // so the spawn cannot complete inside the first turn and make *that* turn
    // the one that continues; and waited for, so the next line reaches a
    // registry that already holds the member (running from its insertion, for
    // this backend) rather than racing the spawn.
    tmux.type_line(&lead, &format!("/teammate spawn {MEMBER} --backend in-process"));
    tmux.wait_for("the spawn to be reported", &lead, || {
        tmux.screen(&lead).contains(&format!("{MEMBER} {SPAWN_NOTICE}")).then_some(())
    });

    // 3. The next turn would end exactly as the first one did. It does not. A
    // prompt rather than a steer: a spawn starts no turn on the lead, so this
    // is the one turn the guard is about, begun with the team already live.
    tmux.type_line(&lead, CARRY_ON);
    tmux.wait_for("the breaker to trip", &lead, || (fixture.logged(STOPPED) > 0).then_some(()));

    // Ahead of the count, so a budget put back reads as what it is rather
    // than as an off-by-one.
    assert_eq!(
        fixture.logged(FIRST),
        1,
        "counted from one exactly once — twice is a budget put back: {}",
        fixture.log_tail(),
    );
    assert_eq!(
        fixture.logged(CONTINUED),
        MAX_CONTINUATIONS,
        "five auto-continuations, then the budget is spent: {}",
        fixture.log_tail(),
    );
    assert_eq!(
        fixture.logged(STOPPED),
        1,
        "and the breaker says so once, at the moment it trips: {}",
        fixture.log_tail(),
    );

    // 4. And the session is back with the person: the turn really ended
    // rather than continuing quietly with the notice suppressed. Meaningful
    // only after the breaker was seen to trip, for step 1's reason.
    tmux.wait_for("the lead to be idle again after the breaker", &lead, || {
        pane_lead::idle(&tmux.screen(&lead)).then_some(())
    });
}
